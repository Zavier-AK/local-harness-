//! Microphone capture for push-to-talk.
//!
//! The cpal stream lives on its own thread for its whole life — on some platforms it
//! cannot cross threads — and hands back 16 kHz mono samples, which is what Whisper wants,
//! when told to stop.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Whisper's input rate.
pub const RATE: u32 = 16_000;
/// A take is cut off here; a voice command is never this long.
pub const MAX_SECONDS: u32 = 30;
/// Shorter than this is a slip of the key, not a command.
pub const MIN_SECONDS: f32 = 0.3;

pub struct Captured {
    /// 16 kHz mono.
    pub samples: Vec<f32>,
    pub seconds: f32,
}

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

/// Root-mean-square level, 0..1, for the waveform.
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    mean.sqrt().min(1.0)
}

/// A recording in progress.
pub struct Recorder {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<Result<Captured>>,
}

impl Recorder {
    /// Start recording from the default input. `on_level` is called about 20 times a
    /// second with the current level, from the audio thread.
    pub fn start(on_level: impl Fn(f32) + Send + Sync + 'static) -> Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<Result<Captured>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("voice-capture".into())
            .spawn(move || {
                let result = capture(stop_rx, &ready_tx, on_level);
                // If starting failed, `ready` carried the error already.
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

    /// Stop and hand back what was heard.
    pub fn finish(self) -> Result<Captured> {
        let _ = self.stop.send(());
        self.done
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("the recording did not finish"))?
    }
}

fn capture(
    stop: mpsc::Receiver<()>,
    ready: &mpsc::Sender<Result<()>>,
    on_level: impl Fn(f32) + Send + Sync + 'static,
) -> Result<Captured> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Sample, SampleFormat};

    let started = (|| -> Result<_> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no microphone found — check System Settings › Sound › Input")?;
        let config = device
            .default_input_config()
            .context("the microphone has no usable format")?;
        Ok((device, config))
    })();
    let (device, config) = match started {
        Ok(ok) => ok,
        Err(err) => {
            let message = format!("{err:#}");
            let _ = ready.send(Err(err));
            bail!(message);
        }
    };

    let channels = config.channels().max(1) as usize;
    let rate = config.sample_rate();
    let limit = (rate * MAX_SECONDS) as usize;
    let buffer: Arc<Mutex<Vec<f32>>> =
        Arc::new(Mutex::new(Vec::with_capacity((rate * 5) as usize)));
    let on_level = Arc::new(on_level);
    let last_level = Arc::new(Mutex::new(Instant::now()));

    // Downmix to mono as samples arrive; the level is reported from the same chunk.
    fn push<T: Sample>(
        data: &[T],
        channels: usize,
        limit: usize,
        buffer: &Mutex<Vec<f32>>,
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
        if let Ok(mut buffer) = buffer.lock() {
            let room = limit.saturating_sub(buffer.len());
            buffer.extend(mono.into_iter().take(room));
        }
    }

    let err_fn = |err: cpal::Error| tracing::warn!("microphone: {err}");
    macro_rules! stream {
        ($t:ty) => {{
            let (buffer, on_level, last) = (buffer.clone(), on_level.clone(), last_level.clone());
            device.build_input_stream(
                config.clone().into(),
                move |data: &[$t], _: &_| push(data, channels, limit, &buffer, &*on_level, &last),
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
            let message = format!("the microphone's sample format {other} is not supported");
            let _ = ready.send(Err(anyhow!(message.clone())));
            bail!(message);
        }
    };
    let stream = match stream.map_err(|e| anyhow!("could not open the microphone: {e}")) {
        Ok(stream) => stream,
        Err(err) => {
            let message = format!("{err:#}");
            let _ = ready.send(Err(err));
            bail!(message);
        }
    };
    if let Err(err) = stream.play() {
        let message =
            format!("could not start the microphone: {err} — is microphone access allowed?");
        let _ = ready.send(Err(anyhow!(message.clone())));
        bail!(message);
    }
    let _ = ready.send(Ok(()));

    // Until told to stop, or the cap is reached and the person is still holding the key.
    let _ = stop.recv_timeout(Duration::from_secs(MAX_SECONDS as u64 + 1));
    drop(stream);

    let raw = std::mem::take(
        &mut *buffer
            .lock()
            .map_err(|_| anyhow!("capture buffer poisoned"))?,
    );
    let seconds = raw.len() as f32 / rate as f32;
    Ok(Captured {
        samples: resample(&raw, rate, RATE),
        seconds,
    })
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
}
