//! A natural voice for spoken replies: Kokoro, run on the Mac in a Node helper
//! (`app/voice-sidecar/tts-server.mjs`). The system voice stays as the fallback.
//!
//! Kokoro is an open 82M text-to-speech model (Apache 2.0). Its British voices are the
//! point here: `bm_george` is deep and measured, closer to a film butler than to a screen
//! reader. The model (~90 MB) downloads once; each sentence takes a fraction of a second.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Kokoro's voices worth offering, British first: (id, name, what it sounds like).
pub const VOICES: &[(&str, &str, &str)] = &[
    ("bm_george", "George", "British, deep and measured"),
    ("bm_fable", "Fable", "British, warm"),
    ("bm_lewis", "Lewis", "British, lower and slower"),
    ("bm_daniel", "Daniel", "British, crisp"),
    ("bf_emma", "Emma", "British, clear"),
    ("bf_isabella", "Isabella", "British, bright"),
    ("am_michael", "Michael", "American, calm"),
    ("af_heart", "Heart", "American, friendly"),
];

pub const DEFAULT_VOICE: &str = "bm_george";

pub fn is_voice(id: &str) -> bool {
    VOICES.iter().any(|(v, _, _)| *v == id)
}

#[derive(Debug, Clone)]
pub struct SpeechConfig {
    pub node: PathBuf,
    /// `tts-server.mjs` in the voice sidecar folder.
    pub script: PathBuf,
    /// Where the model is kept once downloaded.
    pub cache: PathBuf,
}

impl SpeechConfig {
    pub fn new(script: PathBuf, cache: PathBuf) -> Self {
        Self {
            node: PathBuf::from("node"),
            script,
            cache,
        }
    }

    pub fn bundled_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/voice-sidecar/tts-server.mjs")
    }
}

/// Download progress, while the model is fetched the first time.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub file: Option<String>,
    pub received: u64,
    pub total: Option<u64>,
}

struct Helper {
    _child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

pub struct SpeechClient {
    config: SpeechConfig,
    helper: tokio::sync::Mutex<Option<Helper>>,
    ready: std::sync::atomic::AtomicBool,
    progress: tokio::sync::watch::Sender<Option<Progress>>,
    next_id: AtomicU64,
}

/// The first `say` may include the download.
const FIRST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// A sentence, once loaded, is well under a second; this allows for a slow Mac.
const SAY_TIMEOUT: Duration = Duration::from_secs(30);

impl SpeechClient {
    pub fn new(config: SpeechConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            helper: tokio::sync::Mutex::new(None),
            ready: std::sync::atomic::AtomicBool::new(false),
            progress: tokio::sync::watch::channel(None).0,
            next_id: AtomicU64::new(1),
        })
    }

    pub fn check_installed(&self) -> std::result::Result<(), String> {
        let dir = self
            .config
            .script
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        if !self.config.script.is_file() {
            return Err(format!(
                "the speech helper is missing: {}",
                self.config.script.display()
            ));
        }
        if !dir.join("node_modules/kokoro-js").is_dir() {
            return Err(format!("run `npm install` in {}", dir.display()));
        }
        Ok(())
    }

    /// Whether the model is on disk already, so loading it won't start a download.
    pub fn downloaded(&self) -> bool {
        self.config
            .cache
            .join("onnx-community/Kokoro-82M-v1.0-ONNX")
            .is_dir()
    }

    /// Whether the model is loaded, so a reply won't wait for a download.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<Progress>> {
        self.progress.subscribe()
    }

    async fn spawn(&self) -> Result<Helper> {
        self.check_installed().map_err(|hint| anyhow!(hint))?;
        std::fs::create_dir_all(&self.config.cache)
            .with_context(|| format!("creating {}", self.config.cache.display()))?;
        let mut child = Command::new(&self.config.node)
            .arg(&self.config.script)
            .current_dir(
                self.config
                    .script
                    .parent()
                    .unwrap_or(std::path::Path::new(".")),
            )
            .env("HARNESS_TTS_CACHE", &self.config.cache)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "starting {} — is Node 20+ installed?",
                    self.config.node.display()
                )
            })?;
        let stdin = child.stdin.take().context("helper has no stdin")?;
        let stdout = child.stdout.take().context("helper has no stdout")?;
        Ok(Helper {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }

    async fn call(&self, mut request: Value, timeout: Duration) -> Result<Value> {
        let mut guard = self.helper.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn().await?);
        }
        let helper = guard.as_mut().expect("just set");
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        request["id"] = json!(id);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        let exchange = async {
            helper
                .stdin
                .write_all(line.as_bytes())
                .await
                .context("the speech helper stopped")?;
            helper.stdin.flush().await?;
            loop {
                let Some(line) = helper.stdout.next_line().await? else {
                    bail!("the speech helper exited");
                };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message["event"] == "progress" {
                    self.progress.send_replace(Some(Progress {
                        file: message["file"].as_str().map(str::to_string),
                        received: message["received"].as_u64().unwrap_or(0),
                        total: message["total"].as_u64(),
                    }));
                    continue;
                }
                if message["id"].as_u64() == Some(id) {
                    return Ok(message);
                }
            }
        };
        let message = match tokio::time::timeout(timeout, exchange).await {
            Ok(Ok(message)) => message,
            Ok(Err(err)) => {
                *guard = None;
                self.ready.store(false, Ordering::Relaxed);
                return Err(err);
            }
            Err(_) => {
                *guard = None;
                self.ready.store(false, Ordering::Relaxed);
                bail!("the voice took longer than {}s", timeout.as_secs());
            }
        };
        if message["ok"].as_bool() == Some(true) {
            Ok(message)
        } else {
            Err(anyhow!(
                "{}",
                message["error"].as_str().unwrap_or("unknown error")
            ))
        }
    }

    /// Download if needed, and load. Slow the first time only.
    pub async fn load(&self) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }
        self.call(json!({ "op": "load" }), FIRST_TIMEOUT).await?;
        self.ready.store(true, Ordering::Relaxed);
        self.progress.send_replace(None);
        Ok(())
    }

    /// Speak `text` in `voice`: a WAV file, base64-encoded for the web view to play.
    pub async fn say(&self, text: &str, voice: &str, speed: f64) -> Result<String> {
        let voice = if is_voice(voice) {
            voice
        } else {
            DEFAULT_VOICE
        };
        let timeout = if self.is_ready() {
            SAY_TIMEOUT
        } else {
            FIRST_TIMEOUT
        };
        let reply = self
            .call(
                json!({ "op": "say", "text": text, "voice": voice, "speed": speed }),
                timeout,
            )
            .await?;
        self.ready.store(true, Ordering::Relaxed);
        reply["wav"]
            .as_str()
            .map(str::to_string)
            .context("the speech helper sent no audio")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voices_are_known_and_british_first() {
        assert!(is_voice(DEFAULT_VOICE));
        assert!(!is_voice("../../etc"));
        assert!(VOICES[0].0.starts_with("bm_"));
    }

    #[tokio::test]
    async fn a_missing_install_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let client = SpeechClient::new(SpeechConfig::new(
            dir.path().join("tts-server.mjs"),
            dir.path().join("cache"),
        ));
        assert!(client.check_installed().unwrap_err().contains("missing"));
        std::fs::write(dir.path().join("tts-server.mjs"), "").unwrap();
        assert!(client
            .check_installed()
            .unwrap_err()
            .contains("npm install"));
        assert!(client.say("hi", "bm_george", 1.0).await.is_err());
    }
}
