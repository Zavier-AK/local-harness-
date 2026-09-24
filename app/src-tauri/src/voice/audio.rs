//! Microphone capture for push-to-talk, cut into commands at the pauses.
//!
//! While the key is held, audio is cut wherever the person pauses, and each piece is
//! handed on as soon as it ends — so "open Notes … show me the plan" does the first before
//! the second is said. The cpal stream lives on its own thread for its whole life (on some
//! platforms it cannot cross threads); pieces come out as 16 kHz mono, Whisper's input.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Whisper's input rate.
pub const RATE: u32 = 16_000;
/// A single piece is cut off here; a voice command is never this long.
pub const MAX_PIECE_SECONDS: u32 = 30;
/// Holding the key longer than this ends listening anyway.
pub const MAX_SESSION_SECONDS: u64 = 10 * 60;

/// Linear resampling. Plenty for speech going into Whisper, and no dependency.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let left = pos.floor() as usize;
            let right = (left + 1).min(input.len() - 1);
            let frac = (pos - left as f64) as f32;
            input[left] * (1.0 - frac) + input[right] * frac
        })
        .collect()
}

/// Root-mean-square level, 0..1, for the waveform and for telling speech from quiet.
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    mean.sqrt().min(1.0)
}

/// Cuts a stream of audio into spoken pieces at the pauses.
///
/// Speech is anything well above the room's own noise, which is learned from the quiet
/// between words. A piece ends after `pause` of quiet; it keeps a little of what came
/// just before the first word, so the first syllable is not clipped, and pieces with
/// hardly any speech in them (a click, a breath) are dropped.
pub struct Segmenter {
    frame: usize,
    pause_frames: usize,
    preroll: usize,
    tail: usize,
    min_voiced_frames: usize,
    max_len: usize,
    buffer: Vec<f32>,
    partial: Vec<f32>,
    noise: Option<f32>,
    start: Option<usize>,
    last_voice_end: usize,
    voiced_frames: usize,
    quiet_frames: usize,
}

/// Speech must be at least this loud whatever the room: a quiet room would otherwise make
/// a breath count.
const FLOOR: f32 = 0.006;

impl Segmenter {
    pub fn new(rate: u32, pause: Duration) -> Self {
        let rate = rate as usize;
        let frame = rate / 50; // 20 ms
        Self {
            frame,
            pause_frames: ((pause.as_millis() as usize) / 20).max(1),
            preroll: rate * 3 / 10,
            tail: rate / 5,
            min_voiced_frames: 12, // a quarter of a second of speech
            max_len: rate * MAX_PIECE_SECONDS as usize,
            buffer: Vec::new(),
            partial: Vec::with_capacity(frame),
            noise: None,
            start: None,
            last_voice_end: 0,
            voiced_frames: 0,
            quiet_frames: 0,
        }
    }

    /// Feed audio; get back every piece that ended in it.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut pieces = Vec::new();
        for &sample in samples {
            self.buffer.push(sample);
            self.partial.push(sample);
            if self.partial.len() == self.frame {
                let energy = level(&self.partial);
                self.partial.clear();
                if let Some(piece) = self.frame_done(energy) {
                    pieces.push(piece);
                }
            }
        }
        pieces
    }

    fn frame_done(&mut self, energy: f32) -> Option<Vec<f32>> {
        let noise = *self.noise.get_or_insert(energy);
        let voiced = energy > (noise * 3.0).max(FLOOR);
        let end = self.buffer.len();
        match self.start {
            None if voiced => {
                self.start = Some(end.saturating_sub(self.frame + self.preroll));
                self.voiced_frames = 1;
                self.quiet_frames = 0;
                self.last_voice_end = end;
                None
            }
            None => {
                // Learn the room while nobody speaks, and keep only enough for a preroll.
                self.noise = Some(noise * 0.95 + energy * 0.05);
                let excess = self.buffer.len().saturating_sub(self.preroll + self.frame);
                if excess > 0 {
                    self.buffer.drain(..excess);
                }
                None
            }
            Some(start) => {
                if voiced {
                    self.voiced_frames += 1;
                    self.quiet_frames = 0;
                    self.last_voice_end = end;
                } else {
                    self.quiet_frames += 1;
                }
                if self.quiet_frames >= self.pause_frames || end - start >= self.max_len {
                    self.cut()
                } else {
                    None
                }
            }
        }
    }

    fn cut(&mut self) -> Option<Vec<f32>> {
        let start = self.start.take()?;
        let end = (self.last_voice_end + self.tail).min(self.buffer.len());
        let piece = self.buffer[start..end].to_vec();
        self.buffer.drain(..end);
        let enough = self.voiced_frames >= self.min_voiced_frames;
        self.voiced_frames = 0;
        self.quiet_frames = 0;
        self.last_voice_end = 0;
        enough.then_some(piece)
    }

    /// The key was let go: whatever is being said counts now, without waiting for a pause.
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        self.start?;
        self.last_voice_end = self.buffer.len();
        self.cut()
    }
}

/// Listening in progress. Pieces are handed to `on_piece` as they end.
pub struct Listener {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<Result<()>>,
}

impl Listener {
    /// Start listening on the default input. `on_level` gets the level about 20 times a
    /// second; `on_piece` gets each spoken piece, 16 kHz mono, as soon as it ends. Both
    /// are called from the capture thread, so they should only hand things on.
    pub fn start(
        pause: Duration,
        on_level: impl Fn(f32) + Send + Sync + 'static,
        on_piece: impl Fn(Vec<f32>) + Send + 'static,
    ) -> Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<Result<()>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("voice-capture".into())
            .spawn(move || {
                let result = capture(stop_rx, &ready_tx, pause, on_level, on_piece);
                let _ = done_tx.send(result);
            })
            .context("could not start the capture thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop: stop_tx,
                done: done_rx,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => bail!("the microphone did not start"),
        }
    }

    /// Stop listening. The piece being spoken is handed on first.
    pub fn finish(self) -> Result<()> {
        let _ = self.stop.send(());
        self.done
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("listening did not stop"))?
    }
}

fn capture(
    stop: mpsc::Receiver<()>,
    ready: &mpsc::Sender<Result<()>>,
    pause: Duration,
    on_level: impl Fn(f32) + Send + Sync + 'static,
    on_piece: impl Fn(Vec<f32>) + Send + 'static,
) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Sample, SampleFormat};

    let fail = |ready: &mpsc::Sender<Result<()>>, message: String| -> Result<()> {
        let _ = ready.send(Err(anyhow!(message.clone())));
        bail!(message)
    };

    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        return fail(
            ready,
            "no microphone found — check System Settings › Sound › Input".into(),
        );
    };
    let config = match device.default_input_config() {
        Ok(config) => config,
        Err(err) => return fail(ready, format!("the microphone has no usable format: {err}")),
    };
    let channels = config.channels().max(1) as usize;
    let rate = config.sample_rate();

    // The audio callback only downmixes and hands chunks over; cutting happens here.
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<f32>>();
    let on_level = Arc::new(on_level);
    let last_level = Arc::new(Mutex::new(Instant::now()));

    fn push<T: Sample>(
        data: &[T],
        channels: usize,
        chunks: &mpsc::Sender<Vec<f32>>,
        on_level: &(dyn Fn(f32) + Send + Sync),
        last: &Mutex<Instant>,
    ) where
        f32: cpal::FromSample<T>,
    {
        let mono: Vec<f32> = data
            .chunks(channels)
            .map(|frame| frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32)
            .collect();
        if let Ok(mut last) = last.try_lock() {
            if last.elapsed() >= Duration::from_millis(50) {
                *last = Instant::now();
                on_level(level(&mono));
            }
        }
        let _ = chunks.send(mono);
    }

    let err_fn = |err: cpal::Error| tracing::warn!("microphone: {err}");
    macro_rules! stream {
        ($t:ty) => {{
            let (chunks, on_level, last) = (chunk_tx.clone(), on_level.clone(), last_level.clone());
            device.build_input_stream(
                config.clone().into(),
                move |data: &[$t], _: &_| push(data, channels, &chunks, &*on_level, &last),
                err_fn,
                None,
            )
        }};
    }
    let stream = match config.sample_format() {
        SampleFormat::F32 => stream!(f32),
        SampleFormat::I16 => stream!(i16),
        SampleFormat::I32 => stream!(i32),
        SampleFormat::U16 => stream!(u16),
        SampleFormat::I8 => stream!(i8),
        other => {
            return fail(
                ready,
                format!("the microphone's sample format {other} is not supported"),
            )
        }
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(err) => return fail(ready, format!("could not open the microphone: {err}")),
    };
    if let Err(err) = stream.play() {
        return fail(
            ready,
            format!("could not start the microphone: {err} — is microphone access allowed?"),
        );
    }
    drop(chunk_tx);
    let _ = ready.send(Ok(()));

    let mut segmenter = Segmenter::new(rate, pause);
    let started = Instant::now();
    let hand_on = |pieces: Vec<Vec<f32>>| {
        for piece in pieces {
            on_piece(resample(&piece, rate, RATE));
        }
    };
    loop {
        if stop.try_recv().is_ok() || started.elapsed() >= Duration::from_secs(MAX_SESSION_SECONDS)
        {
            break;
        }
        match chunk_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => hand_on(segmenter.push(&chunk)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stream);
    while let Ok(chunk) = chunk_rx.try_recv() {
        hand_on(segmenter.push(&chunk));
    }
    if let Some(piece) = segmenter.flush() {
        hand_on(vec![piece]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_duration_and_shape() {
        // One second of a 440 Hz tone at 48 kHz.
        let input: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin())
            .collect();
        let out = resample(&input, 48_000, RATE);
        assert_eq!(out.len(), 16_000);
        // Same tone: sample i at 16 kHz is sample 3i at 48 kHz.
        for i in [0usize, 100, 5_000, 15_999] {
            assert!((out[i] - input[i * 3]).abs() < 1e-3, "{i}");
        }
        assert_eq!(resample(&input[..10], 16_000, 16_000).len(), 10);
        assert!(resample(&[], 44_100, RATE).is_empty());
    }

    #[test]
    fn level_is_rms() {
        assert_eq!(level(&[]), 0.0);
        assert!((level(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
    }

    const R: u32 = 16_000;

    fn quiet(seconds: f32) -> Vec<f32> {
        // A little room noise, not digital silence.
        (0..(R as f32 * seconds) as usize)
            .map(|i| ((i * 7919 % 101) as f32 / 101.0 - 0.5) * 0.002)
            .collect()
    }

    fn speech(seconds: f32) -> Vec<f32> {
        (0..(R as f32 * seconds) as usize)
            .map(|i| (i as f32 / R as f32 * 220.0 * std::f32::consts::TAU).sin() * 0.2)
            .collect()
    }

    fn feed(segmenter: &mut Segmenter, parts: &[Vec<f32>]) -> Vec<Vec<f32>> {
        // In 10 ms chunks, the way audio arrives.
        let all: Vec<f32> = parts.concat();
        all.chunks(160)
            .flat_map(|chunk| segmenter.push(chunk))
            .collect()
    }

    #[test]
    fn each_pause_ends_a_piece_as_it_happens() {
        let mut s = Segmenter::new(R, Duration::from_millis(700));
        let pieces = feed(
            &mut s,
            &[quiet(0.5), speech(1.0), quiet(1.0), speech(0.6), quiet(0.9)],
        );
        assert_eq!(
            pieces.len(),
            2,
            "two commands, two pieces, both before letting go"
        );
        // Each holds its speech plus a short lead-in and tail, not the whole pause.
        let first = pieces[0].len() as f32 / R as f32;
        assert!((1.1..1.8).contains(&first), "{first}s");
        assert!(
            s.flush().is_none(),
            "nothing left once the pauses are through"
        );
    }

    #[test]
    fn letting_go_mid_sentence_hands_it_on() {
        let mut s = Segmenter::new(R, Duration::from_millis(700));
        assert!(
            feed(&mut s, &[quiet(0.3), speech(0.8), quiet(0.2)]).is_empty(),
            "no pause yet"
        );
        let last = s.flush().expect("the unfinished piece");
        assert!(last.len() as f32 / R as f32 > 0.8);
    }

    #[test]
    fn a_click_or_a_breath_is_not_a_command() {
        let mut s = Segmenter::new(R, Duration::from_millis(700));
        let pieces = feed(&mut s, &[quiet(0.5), speech(0.08), quiet(1.2)]);
        assert!(pieces.is_empty());
        assert!(s.flush().is_none());
    }

    #[test]
    fn a_shorter_pause_setting_cuts_sooner() {
        let mut quick = Segmenter::new(R, Duration::from_millis(300));
        let mut slow = Segmenter::new(R, Duration::from_millis(1200));
        let parts = [quiet(0.3), speech(0.6), quiet(0.5), speech(0.6), quiet(1.5)];
        assert_eq!(feed(&mut quick, &parts).len(), 2);
        assert_eq!(
            feed(&mut slow, &parts).len(),
            1,
            "a half-second gap is mid-sentence here"
        );
    }
}
