//! Speech to text with whisper.cpp, on the device.
//!
//! The model is downloaded once into the app's data folder and loaded on first use. Its
//! prompt is primed with the harness's own words and the fleet's role and project names —
//! Whisper leans toward what the prompt makes likely, which is most of the difference
//! between "land safe" and "lance safe".

#![cfg_attr(not(feature = "voice"), allow(dead_code, unused_imports))]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ModelSize {
    #[serde(rename = "tiny.en")]
    Tiny,
    #[default]
    #[serde(rename = "base.en")]
    Base,
    #[serde(rename = "small.en")]
    Small,
}

impl ModelSize {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSize::Tiny => "tiny.en",
            ModelSize::Base => "base.en",
            ModelSize::Small => "small.en",
        }
    }

    pub fn file_name(self) -> String {
        format!("ggml-{}.bin", self.as_str())
    }

    pub fn url(self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.file_name()
        )
    }

    /// Roughly, for the Settings label; the download checks the real size.
    pub fn megabytes(self) -> u32 {
        match self {
            ModelSize::Tiny => 75,
            ModelSize::Base => 142,
            ModelSize::Small => 466,
        }
    }
}

pub fn model_path(dir: &Path, size: ModelSize) -> PathBuf {
    dir.join(size.file_name())
}

/// A smaller file than this is an error page, not a model.
const MIN_MODEL_BYTES: u64 = 30 * 1024 * 1024;

pub fn is_downloaded(dir: &Path, size: ModelSize) -> bool {
    std::fs::metadata(model_path(dir, size)).is_ok_and(|m| m.len() >= MIN_MODEL_BYTES)
}

/// Fetch the model, reporting `(received, total)` as it comes. Written to a `.part` file
/// and renamed only once complete and the right size, so an interrupted download never
/// looks like a model.
pub async fn download(
    dir: &Path,
    size: ModelSize,
    progress: impl Fn(u64, Option<u64>),
) -> Result<PathBuf> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let dest = model_path(dir, size);
    if is_downloaded(dir, size) {
        return Ok(dest);
    }
    tokio::fs::create_dir_all(dir).await?;
    let response = reqwest::get(size.url())
        .await
        .with_context(|| format!("downloading {}", size.url()))?;
    if !response.status().is_success() {
        bail!("downloading {} failed: {}", size.url(), response.status());
    }
    let total = response.content_length();
    let part = dest.with_extension("bin.part");
    let mut file = tokio::fs::File::create(&part).await?;
    let mut received = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("the download was interrupted")?;
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        progress(received, total);
    }
    file.flush().await?;
    drop(file);
    if total.is_some_and(|t| t != received) || received < MIN_MODEL_BYTES {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("the download was incomplete ({received} bytes)");
    }
    tokio::fs::rename(&part, &dest).await?;
    Ok(dest)
}

/// The words Whisper should expect.
pub fn prompt(roles: &[String], projects: &[String]) -> String {
    let mut prompt = String::from(
        "Commands for a coding app: status, what's waiting, worker, merge, approve, reject, \
         undo, delegation, autonomy, ask, review, land safe, land most, plan, night shift, \
         preview, settings, Claude.",
    );
    if !roles.is_empty() {
        prompt.push_str(&format!(" Roles: {}.", roles.join(", ").replace('_', " ")));
    }
    if !projects.is_empty() {
        prompt.push_str(&format!(" Projects: {}.", projects.join(", ")));
    }
    prompt
}

/// What Whisper writes for silence or noise, which is not something the person said.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // Whisper's habit on near-silence.
    if matches!(
        out.to_lowercase().trim_matches('.'),
        "you" | "thank you" | "thanks for watching"
    ) {
        return String::new();
    }
    out
}

#[cfg(feature = "voice")]
pub struct Stt {
    context: whisper_rs::WhisperContext,
    pub size: ModelSize,
}

#[cfg(feature = "voice")]
impl Stt {
    pub fn load(path: &Path, size: ModelSize) -> Result<Self> {
        let context = whisper_rs::WhisperContext::new_with_params(
            path.to_str().context("model path is not UTF-8")?,
            whisper_rs::WhisperContextParameters::default(),
        )
        .map_err(|e| anyhow::anyhow!("loading the Whisper model: {e}"))?;
        Ok(Self { context, size })
    }

    /// Blocking; call from a blocking thread.
    pub fn transcribe(&self, samples: &[f32], prompt: &str) -> Result<String> {
        use whisper_rs::{FullParams, SamplingStrategy};
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_initial_prompt(prompt);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        params.set_n_threads(threads as i32);

        // Whisper wants at least a second; pad short commands with silence.
        let mut audio = samples.to_vec();
        if audio.len() < 16_000 {
            audio.resize(16_000 + 1_600, 0.0);
        }
        let mut state = self
            .context
            .create_state()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        state
            .full(params, &audio)
            .map_err(|e| anyhow::anyhow!("transcribing: {e}"))?;
        let text: Vec<String> = state
            .as_iter()
            .map(|segment| {
                segment
                    .to_str_lossy()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            })
            .collect();
        Ok(clean(&text.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_markers_are_not_words() {
        assert_eq!(clean(" [BLANK_AUDIO] "), "");
        assert_eq!(
            clean("(keyboard clicking) approve worker three"),
            "approve worker three"
        );
        assert_eq!(clean("Thank you."), "");
        assert_eq!(clean("  Open   the plan. "), "Open the plan.");
    }

    #[test]
    fn the_prompt_names_the_fleet() {
        let p = prompt(
            &["builder".into(), "local_builder".into()],
            &["shop".into()],
        );
        assert!(p.contains("land safe"));
        assert!(p.contains("Roles: builder, local builder."));
        assert!(p.contains("Projects: shop."));
    }

    /// 16-bit PCM WAV to f32 samples, enough for the test file.
    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        let data = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("a data chunk")
            + 8;
        bytes[data..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect()
    }

    /// Runs whisper.cpp for real. Needs a model and a 16 kHz mono WAV:
    /// `WHISPER_TEST_MODEL=… WHISPER_TEST_WAV=… cargo test -- --ignored whisper_transcribes`.
    #[cfg(feature = "voice")]
    #[test]
    #[ignore]
    fn whisper_transcribes() {
        let model = std::env::var("WHISPER_TEST_MODEL").expect("WHISPER_TEST_MODEL");
        let wav = std::env::var("WHISPER_TEST_WAV").expect("WHISPER_TEST_WAV");
        let stt = Stt::load(Path::new(&model), ModelSize::Tiny).expect("the model loads");
        let samples = read_wav(Path::new(&wav));
        let started = std::time::Instant::now();
        let text = stt
            .transcribe(&samples, &prompt(&[], &[]))
            .expect("it transcribes");
        eprintln!(
            "{:.1}s of audio in {:?}: {text:?}",
            samples.len() as f32 / 16_000.0,
            started.elapsed()
        );
    }

    #[test]
    fn a_short_file_is_not_a_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            model_path(dir.path(), ModelSize::Tiny),
            b"<html>not found</html>",
        )
        .unwrap();
        assert!(!is_downloaded(dir.path(), ModelSize::Tiny));
        assert_eq!(ModelSize::Base.file_name(), "ggml-base.en.bin");
        assert_eq!(
            serde_json::to_string(&ModelSize::Small).unwrap(),
            "\"small.en\""
        );
    }
}
