//! Push-to-talk in the app: hold the hotkey, speak, let go.
//!
//! Capture and Whisper live here; what the words mean is `harness_core::voice`. The flow:
//!
//! 1. Hotkey down: show the voice bar, start recording, and start warming Laya and Whisper
//!    if they are on disk but not loaded — the person is still talking.
//! 2. Hotkey up: transcribe, build a [`Snapshot`] of the harness, interpret.
//! 3. Settle it: a yes/no answers the pending question; an action that must be confirmed
//!    becomes the pending question; a computer action runs here; any other action goes to
//!    the main window as `voice://run`, which carries it out through the same handlers as
//!    its buttons. Everything is reported as `voice://heard` for the bar and the chat.

#![cfg_attr(not(feature = "voice"), allow(dead_code, unused_imports))]

pub mod stt;

#[cfg(feature = "voice")]
pub mod audio;

use harness_core::voice::browser::{BrowserClient, BrowserConfig};
use harness_core::voice::laya::{LayaClient, LayaConfig, LayaState};
use harness_core::voice::speech::{self, SpeechClient, SpeechConfig};
use harness_core::voice::{self, computer, Interpretation, Outcome, Snapshot, VoiceAction};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

use crate::{settings, AppState};

pub const HUD_LABEL: &str = "voice-hud";
/// A question left unanswered this long is dropped: a "yes" much later is about
/// something else.
const PENDING_FOR: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Off until the person turns it on: it asks for the microphone.
    pub enabled: bool,
    /// Held to talk, e.g. `Alt+Space`.
    pub hotkey: String,
    pub stt_model: stt::ModelSize,
    /// Laya must be at least this sure before anything is done on its word.
    pub confidence: f64,
    /// Laya is unloaded after this long unused, to give back its memory.
    pub laya_idle_minutes: u32,
    /// Say answers aloud, not only show them.
    pub speak_replies: bool,
    /// How long a pause ends a command while the key is held.
    pub pause_ms: u32,
    /// Requests the exact commands don't cover go to the voice agent, which plans and
    /// does several steps. Off: they go to Laya and the chat box as before.
    pub agent: bool,
    /// The voice agent's model, a Claude CLI alias or id.
    pub agent_model: String,
    /// Let the voice agent hand web tasks to the browser agent, in its own Chrome window.
    pub browser: bool,
    /// The browser agent's model. Browsing takes many steps; Haiku gets lost.
    pub browser_model: String,
    /// The person, in their own words: how they write, who people are. Both agents read
    /// it, so emails come out in their voice.
    pub about_me: String,
    /// How replies are spoken: "natural" (Kokoro, on the Mac) or "system" (macOS voices).
    pub speech_engine: String,
    /// The natural voice, e.g. `bm_george`.
    pub speech_voice: String,
    /// A macOS voice by name; empty picks the best British one installed.
    pub system_voice: String,
    /// 0.8 to 1.25; 1 is normal.
    pub speech_rate: f64,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hotkey: "Alt+Space".into(),
            stt_model: stt::ModelSize::Base,
            confidence: 0.75,
            laya_idle_minutes: 10,
            speak_replies: true,
            pause_ms: 700,
            agent: true,
            agent_model: "haiku".into(),
            browser: true,
            browser_model: "sonnet".into(),
            about_me: String::new(),
            speech_engine: "natural".into(),
            speech_voice: speech::DEFAULT_VOICE.into(),
            system_voice: String::new(),
            speech_rate: 1.0,
        }
    }
}

impl VoiceSettings {
    pub fn validated(mut self) -> Result<Self, String> {
        self.hotkey = self.hotkey.trim().to_string();
        if self.hotkey.is_empty() || self.hotkey.len() > 40 {
            return Err("give the voice hotkey, e.g. Alt+Space".into());
        }
        if self
            .hotkey
            .parse::<tauri_plugin_global_shortcut::Shortcut>()
            .is_err()
        {
            return Err(format!(
                "`{}` is not a shortcut I can register",
                self.hotkey
            ));
        }
        if !(0.5..=0.99).contains(&self.confidence) {
            return Err("the confidence must be between 0.5 and 0.99".into());
        }
        if !(1..=240).contains(&self.laya_idle_minutes) {
            return Err("unload Laya after 1 to 240 minutes".into());
        }
        if !(300..=2000).contains(&self.pause_ms) {
            return Err("the pause must be between 0.3 and 2 seconds".into());
        }
        self.agent_model = self.agent_model.trim().to_string();
        self.browser_model = self.browser_model.trim().to_string();
        for model in [&self.agent_model, &self.browser_model] {
            let model_ok = !model.is_empty()
                && model.len() <= 100
                && !model.starts_with('-')
                && model
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.[]/".contains(c));
            if !model_ok {
                return Err(format!("`{model}` does not look like a model name"));
            }
        }
        if !matches!(self.speech_engine.as_str(), "natural" | "system") {
            self.speech_engine = "natural".into();
        }
        if !speech::is_voice(&self.speech_voice) {
            self.speech_voice = speech::DEFAULT_VOICE.into();
        }
        self.system_voice = self.system_voice.trim().chars().take(100).collect();
        if !(0.8..=1.25).contains(&self.speech_rate) {
            return Err("the speaking speed must be between 0.8 and 1.25".into());
        }
        self.about_me = self.about_me.trim().to_string();
        if self.about_me.chars().count() > 4000 {
            return Err("keep “About you” under 4,000 characters".into());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub action: VoiceAction,
    pub describe: String,
    #[serde(skip)]
    since: Option<Instant>,
}

/// Listening while the key is held: the microphone, and the worker that turns each
/// spoken piece into an action, in order, while the person keeps talking.
#[cfg(feature = "voice")]
struct Session {
    listener: audio::Listener,
    worker: tauri::async_runtime::JoinHandle<()>,
}

pub struct Voice {
    pub laya: Arc<LayaClient>,
    #[cfg(feature = "voice")]
    stt: tokio::sync::Mutex<Option<Arc<stt::Stt>>>,
    #[cfg(feature = "voice")]
    session: std::sync::Mutex<Option<Session>>,
    pending: std::sync::Mutex<Option<Pending>>,
    /// Installed apps, and when they were last looked up.
    apps: std::sync::Mutex<Option<(Instant, Vec<String>)>>,
    /// The voice agent, started on first use and kept for follow-ups, with what it was
    /// started with.
    agent: tokio::sync::Mutex<Option<(String, voice::agent::VoiceAgent)>>,
    /// The voice browser: its own Chrome window.
    pub browser: Arc<BrowserClient>,
    /// The browser agent, kept between tasks so "now the second one" makes sense.
    browse_agent: Arc<tokio::sync::Mutex<Option<(String, voice::agent::VoiceAgent)>>>,
    /// The natural voice for spoken replies.
    pub speech: Arc<SpeechClient>,
    /// The browser task running in the background, and what it is.
    browse_job: std::sync::Mutex<Option<(String, tauri::async_runtime::JoinHandle<()>)>>,
}

impl Voice {
    pub fn new(settings: &VoiceSettings) -> Self {
        let mut config = LayaConfig::new(LayaConfig::bundled_script());
        config.idle = Duration::from_secs(settings.laya_idle_minutes as u64 * 60);
        let laya = LayaClient::new(config);
        Self {
            laya,
            #[cfg(feature = "voice")]
            stt: tokio::sync::Mutex::new(None),
            #[cfg(feature = "voice")]
            session: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(None),
            apps: std::sync::Mutex::new(None),
            agent: tokio::sync::Mutex::new(None),
            browser: BrowserClient::new(BrowserConfig::new(
                BrowserConfig::bundled_script(),
                harness_core::extensions::app_data_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("voice-browser"),
            )),
            speech: SpeechClient::new(SpeechConfig::new(
                SpeechConfig::bundled_script(),
                models_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("speech"),
            )),
            browse_agent: Arc::new(tokio::sync::Mutex::new(None)),
            browse_job: std::sync::Mutex::new(None),
        }
    }

    /// The browser task running now, if any.
    fn browsing(&self) -> Option<String> {
        let mut job = self.browse_job.lock().expect("not poisoned");
        if job
            .as_ref()
            .is_some_and(|(_, handle)| handle.inner().is_finished())
        {
            *job = None;
        }
        job.as_ref().map(|(task, _)| task.clone())
    }

    /// The apps "open …" can mean, looked up again every few minutes so a new install is
    /// found without a restart.
    fn apps(&self) -> Vec<String> {
        let mut cached = self.apps.lock().expect("not poisoned");
        match cached.as_ref() {
            Some((at, apps)) if at.elapsed() < Duration::from_secs(300) => apps.clone(),
            _ => {
                let apps = installed_apps();
                *cached = Some((Instant::now(), apps.clone()));
                apps
            }
        }
    }

    fn pending(&self) -> Option<Pending> {
        let mut pending = self.pending.lock().expect("not poisoned");
        if pending
            .as_ref()
            .and_then(|p| p.since)
            .is_some_and(|t| t.elapsed() > PENDING_FOR)
        {
            *pending = None;
        }
        pending.clone()
    }

    fn set_pending(&self, value: Option<Pending>) {
        *self.pending.lock().expect("not poisoned") = value;
    }
}

pub fn models_dir() -> Option<PathBuf> {
    Some(harness_core::extensions::app_data_dir()?.join("models"))
}

/// Apps installed on this Mac, by name. Folders of apps (Utilities, Setapp) are looked
/// into one level. Elsewhere, empty: "open …" then takes the name as said.
pub fn installed_apps() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    /// `.app` bundles in `dir`, and one level into plain folders when `deeper`.
    fn scan(dir: &std::path::Path, deeper: bool, apps: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(app) = name.strip_suffix(".app") {
                apps.push(app.to_string());
            } else if deeper && !name.starts_with('.') && entry.path().is_dir() {
                scan(&entry.path(), false, apps);
            }
        }
    }
    let mut apps = vec!["Finder".to_string()];
    for root in &roots {
        scan(root, true, &mut apps);
    }
    apps.sort();
    apps.dedup();
    apps
}

/// Bring the main window forward, for text that was put in its chat box.
fn bring_forward(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

// ---------------------------------------------------------------- events

#[derive(Clone, Serialize)]
struct Phase<'a> {
    phase: &'a str,
    message: Option<String>,
}

fn phase(app: &AppHandle, phase: &str, message: Option<String>) {
    let _ = app.emit("voice://state", Phase { phase, message });
}

#[derive(Clone, Serialize)]
pub struct Heard {
    pub interpretation: Interpretation,
    /// The question now waiting for a yes or no, if any.
    pub pending: Option<Pending>,
    /// What was done here and now, if anything: "Opened Safari".
    pub done: Option<String>,
    /// Something that went wrong doing it.
    pub error: Option<String>,
    pub speak: bool,
}

#[derive(Clone, Serialize)]
struct Run {
    action: VoiceAction,
}

#[derive(Clone, Serialize)]
struct Progress {
    what: &'static str,
    file: Option<String>,
    received: u64,
    total: Option<u64>,
}

// ---------------------------------------------------------------- the snapshot

/// The harness as voice sees it, from the app's open projects.
async fn snapshot(state: &AppState) -> (Snapshot, Option<PathBuf>, Vec<String>) {
    let active = state.active.lock().await.clone();
    let projects = state.projects.lock().await;
    let mut snapshot = Snapshot::default();
    for (key, session) in projects.iter() {
        let name = session
            .project_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| key.clone());
        snapshot.projects.push(voice::ProjectRef {
            root: key.clone(),
            name,
        });
    }
    snapshot.projects.sort_by(|a, b| a.name.cmp(&b.name));
    snapshot.active_project = active.clone();
    let Some(session) = active.as_ref().and_then(|key| projects.get(key)) else {
        return (snapshot, None, Vec::new());
    };
    let harness = Arc::clone(&session.harness);
    let root = session.project_root.clone();
    drop(projects);

    let numbers = harness.worker_numbers().await;
    let pending: std::collections::HashSet<String> = harness
        .pending_merges()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut workers = Vec::new();
    for record in harness.workers().await {
        let risk = match harness.verification(&record.id).await {
            Some(harness_core::engine::VerificationState::Done { report }) => {
                Some(if report.verified {
                    report.risk.as_str().to_string()
                } else {
                    "unverified".to_string()
                })
            }
            _ => None,
        };
        let status = serde_json::to_value(record.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        workers.push(voice::WorkerRef {
            number: numbers.get(&record.id).copied().unwrap_or(0),
            merge_pending: pending.contains(&record.id),
            risk,
            role: record.role,
            status,
            task: record.task,
            id: record.id,
        });
    }
    workers.sort_by_key(|w| w.number);
    snapshot.workers = workers;

    let plans = harness.plans().await;
    let live = plans
        .iter()
        .find(|p| {
            matches!(
                p.status,
                harness_core::plan::PlanStatus::Draft | harness_core::plan::PlanStatus::Running
            )
        })
        .or_else(|| plans.last());
    snapshot.plan = live.map(|plan| voice::PlanRef {
        title: plan.title.clone(),
        status: serde_json::to_value(plan.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        landed: plan
            .steps
            .iter()
            .filter(|s| s.state == harness_core::plan::StepState::Landed)
            .count(),
        total: plan.steps.len(),
    });
    snapshot.night = harness.night_report().await.map(|night| voice::NightRef {
        status: serde_json::to_value(night.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        kept: night.kept(),
        tried: night.experiments.len(),
        baseline: night.baseline,
        best: night.best,
        proposed: night.proposed_as.is_some(),
    });
    snapshot.autonomy = harness.autonomy().await;
    snapshot.landed = harness.landed_workers().await;

    let roles = harness
        .list_roles()
        .await
        .into_iter()
        .map(|r| r.name)
        .collect();
    (snapshot, Some(root), roles)
}

// ---------------------------------------------------------------- the voice bar

fn hud(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(HUD_LABEL)
}

/// Create the voice bar, hidden. It is shown while listening and answering.
pub fn create_hud(app: &AppHandle) -> tauri::Result<()> {
    if hud(app).is_some() {
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(app, HUD_LABEL, tauri::WebviewUrl::App("index.html".into()))
        .title("Harness voice")
        .inner_size(460.0, 280.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focused(false)
        .accept_first_mouse(true)
        .visible(false)
        .build()?;
    #[cfg(target_os = "macos")]
    bar_panel::make(&window)?;
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

/// Show the bar at the top of the screen the pointer is on, which is the one being
/// used, not necessarily the one Harness is on.
fn show_hud(app: &AppHandle) {
    let Some(window) = hud(app) else { return };
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|at| app.monitor_from_point(at.x, at.y).ok().flatten())
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    if let Some(monitor) = monitor {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let width = (460.0 * scale) as i32;
        let x = monitor.position().x + (size.width as i32 - width) / 2;
        let y = monitor.position().y + (48.0 * scale) as i32;
        let _ = window.set_position(tauri::PhysicalPosition { x, y });
    }
    #[cfg(target_os = "macos")]
    bar_panel::show(app);
    #[cfg(not(target_os = "macos"))]
    let _ = window.show();
}

/// How to say a reply: the natural voice's audio, or which system voice to use.
#[derive(Serialize)]
pub struct Spoken {
    /// A WAV file, base64, when the natural voice spoke it.
    wav: Option<String>,
    /// Otherwise the system voice to use; empty means the best British one.
    system_voice: String,
    rate: f64,
    /// Why the natural voice wasn't used, when it was chosen.
    fallback: Option<String>,
}

/// Speak a reply. The natural voice if it's chosen and can run; the system voice if not,
/// so a reply is never silent.
#[tauri::command]
pub async fn voice_say(app: AppHandle, text: String) -> Spoken {
    let settings = settings::load().voice;
    let mut spoken = Spoken {
        wav: None,
        system_voice: settings.system_voice.clone(),
        rate: settings.speech_rate,
        fallback: None,
    };
    if settings.speech_engine != "natural" {
        return spoken;
    }
    let speech = Arc::clone(&app.state::<Voice>().speech);
    // A reply never waits for the first download; Settings starts that.
    if !speech.is_ready() && !speech.downloaded() {
        spoken.fallback = Some("the natural voice isn't downloaded yet".into());
        return spoken;
    }
    match speech
        .say(&text, &settings.speech_voice, settings.speech_rate)
        .await
    {
        Ok(wav) => spoken.wav = Some(wav),
        Err(error) => {
            tracing::warn!("natural voice failed: {error:#}");
            spoken.fallback = Some(format!("{error:#}"));
        }
    }
    spoken
}

/// Settings: which natural voices there are.
#[tauri::command]
pub fn voice_speech_voices() -> Vec<(String, String, String)> {
    speech::VOICES
        .iter()
        .map(|(id, name, about)| (id.to_string(), name.to_string(), about.to_string()))
        .collect()
}

/// The Stop button while the browser is working.
#[tauri::command]
pub async fn voice_stop_browsing(app: AppHandle) -> bool {
    stop_browsing(&app).await
}

/// Show the voice browser's window, for the person to sign in to the sites it should use.
#[tauri::command]
pub async fn voice_open_browser(app: AppHandle) -> Result<(), String> {
    let voice = app.state::<Voice>();
    voice.browser.check_installed()?;
    voice
        .browser
        .open("https://accounts.google.com")
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn voice_hide_hud(app: AppHandle) {
    #[cfg(target_os = "macos")]
    bar_panel::hide(&app);
    #[cfg(not(target_os = "macos"))]
    if let Some(window) = hud(&app) {
        let _ = window.hide();
    }
}

/// On macOS the bar is a non-activating panel, like Spotlight's. An ordinary window,
/// even one on every desktop, is not drawn over another app's full-screen space, and
/// showing it would switch to Harness. A panel floats over whatever is in front,
/// on every desktop and full-screen app, and clicking it doesn't bring Harness forward.
#[cfg(target_os = "macos")]
mod bar_panel {
    use super::HUD_LABEL;
    use tauri::AppHandle;
    use tauri_nspanel::{tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt};

    tauri_panel! {
        panel!(VoiceBarPanel {
            config: {
                can_become_key_window: true,
                is_floating_panel: true
            }
        })
    }

    /// Called from setup, on the main thread.
    pub fn make(window: &tauri::WebviewWindow) -> tauri::Result<()> {
        let panel = window.to_panel::<VoiceBarPanel>()?;
        panel.set_level(PanelLevel::PopUpMenu.value());
        panel
            .add_style_mask(StyleMask::empty().nonactivating_panel().into())
            .map_err(|error| tauri::Error::Anyhow(error.into()))?;
        panel.set_collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary()
                .stationary()
                .ignores_cycle()
                .into(),
        );
        panel.set_hides_on_deactivate(false);
        panel.set_floating_panel(true);
        Ok(())
    }

    /// AppKit calls must happen on the main thread; voice runs on others.
    pub fn show(app: &AppHandle) {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Ok(panel) = handle.get_webview_panel(HUD_LABEL) {
                panel.show();
            }
        });
    }

    pub fn hide(app: &AppHandle) {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Ok(panel) = handle.get_webview_panel(HUD_LABEL) {
                panel.hide();
            }
        });
    }
}

// ---------------------------------------------------------------- the hotkey

/// Register (or re-register) the push-to-talk hotkey from the settings.
pub fn apply_hotkey(app: &AppHandle, settings: &VoiceSettings) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let shortcuts = app.global_shortcut();
    shortcuts.unregister_all().map_err(|e| e.to_string())?;
    if settings.enabled && cfg!(feature = "voice") {
        shortcuts
            .register(settings.hotkey.as_str())
            .map_err(|e| format!("could not register {}: {e}", settings.hotkey))?;
    }
    Ok(())
}

/// The global-shortcut plugin, wired to push-to-talk: press starts, release finishes.
pub fn hotkey_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use tauri_plugin_global_shortcut::ShortcutState;
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _shortcut, event| {
            let app = app.clone();
            match event.state {
                ShortcutState::Pressed => {
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = start(&app).await {
                            show_hud(&app);
                            phase(&app, "error", Some(err));
                        }
                    });
                }
                ShortcutState::Released => {
                    tauri::async_runtime::spawn(async move {
                        let _ = finish(&app).await;
                    });
                }
            }
        })
        .build()
}

// ---------------------------------------------------------------- listening

/// Warm what is on disk but not loaded, while the person is still talking.
fn warm(app: &AppHandle) {
    let voice = app.state::<Voice>();
    let laya = Arc::clone(&voice.laya);
    match laya.state() {
        LayaState::Stopped {
            downloaded: Some(true),
        } => {
            tauri::async_runtime::spawn(async move {
                let _ = laya.load().await;
            });
        }
        // Not asked yet whether the weights are on disk (Settings was never opened).
        LayaState::Stopped { downloaded: None } => {
            tauri::async_runtime::spawn(async move {
                if matches!(laya.probe().await, Ok(true)) {
                    let _ = laya.load().await;
                }
            });
        }
        _ => {}
    }
    #[cfg(feature = "voice")]
    {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = whisper(&app).await;
        });
    }
    let settings = settings::load().voice;
    let natural = Arc::clone(&voice.speech);
    if settings.speech_engine == "natural" && natural.downloaded() && !natural.is_ready() {
        tauri::async_runtime::spawn(async move {
            let _ = natural.load().await;
        });
    }
    if settings.agent {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = ensure_agent(&app, &settings).await {
                tracing::warn!("voice agent did not start: {error}");
            }
        });
    }
}

/// The Whisper model, loaded on first use. Errors if it is not downloaded yet.
#[cfg(feature = "voice")]
async fn whisper(app: &AppHandle) -> Result<Arc<stt::Stt>, String> {
    let voice = app.state::<Voice>();
    let size = settings::load().voice.stt_model;
    let mut slot = voice.stt.lock().await;
    if let Some(stt) = slot.as_ref().filter(|s| s.size == size) {
        return Ok(Arc::clone(stt));
    }
    let dir = models_dir().ok_or("no app data folder")?;
    if !stt::is_downloaded(&dir, size) {
        return Err(format!(
            "the speech model ({}) is not downloaded — Settings › Voice",
            size.as_str()
        ));
    }
    let path = stt::model_path(&dir, size);
    let loaded = tokio::task::spawn_blocking(move || stt::Stt::load(&path, size))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    let loaded = Arc::new(loaded);
    *slot = Some(Arc::clone(&loaded));
    Ok(loaded)
}

#[cfg(feature = "voice")]
pub async fn start(app: &AppHandle) -> Result<(), String> {
    let settings = settings::load().voice;
    if !settings.enabled {
        return Err("voice is off — Settings › Voice".into());
    }
    let voice = app.state::<Voice>();
    {
        let mut session = voice.session.lock().expect("not poisoned");
        if session.is_some() {
            return Ok(());
        }
        // Each piece is understood and acted on in turn, while listening goes on.
        let (pieces, mut incoming) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
        let level_app = app.clone();
        let listener = audio::Listener::start(
            Duration::from_millis(settings.pause_ms as u64),
            move |level| {
                let _ = level_app.emit("voice://level", level);
            },
            move |piece| {
                let _ = pieces.send(piece);
            },
        )
        .map_err(|e| format!("{e:#}"))?;
        let worker_app = app.clone();
        let worker = tauri::async_runtime::spawn(async move {
            while let Some(piece) = incoming.recv().await {
                if let Err(err) = understand(&worker_app, piece).await {
                    phase(&worker_app, "error", Some(err));
                }
            }
        });
        *session = Some(Session { listener, worker });
    }
    show_hud(app);
    phase(app, "listening", voice.pending().map(|p| p.describe));
    warm(app);
    Ok(())
}

#[cfg(not(feature = "voice"))]
pub async fn start(_app: &AppHandle) -> Result<(), String> {
    Err("this build has no voice support".into())
}

/// One spoken piece: write it down, work out what it means, act on it.
#[cfg(feature = "voice")]
async fn understand(app: &AppHandle, piece: Vec<f32>) -> Result<(), String> {
    let _ = app.emit("voice://busy", true);
    let result = async {
        let whisper = whisper(app).await?;
        let (snapshot, _, roles) = snapshot(&app.state::<AppState>()).await;
        let names: Vec<String> = snapshot.projects.iter().map(|p| p.name.clone()).collect();
        let prompt = stt::prompt(&roles, &names);
        let text = tokio::task::spawn_blocking(move || whisper.transcribe(&piece, &prompt))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))?;
        if !text.trim().is_empty() {
            hear(app, &text).await;
        }
        Ok(())
    }
    .await;
    let _ = app.emit("voice://busy", false);
    result
}

/// The key was let go: stop listening, finish what was being said, then rest.
#[cfg(feature = "voice")]
pub async fn finish(app: &AppHandle) -> Result<(), String> {
    let voice = app.state::<Voice>();
    let Some(session) = voice.session.lock().expect("not poisoned").take() else {
        return Ok(());
    };
    phase(app, "transcribing", None);
    let Session { listener, worker } = session;
    let stopped = tokio::task::spawn_blocking(move || listener.finish())
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| format!("{e:#}")));
    // The listener is gone, so the worker ends once the last piece is handled.
    let _ = worker.await;
    phase(app, "idle", None);
    stopped
}

#[cfg(not(feature = "voice"))]
pub async fn finish(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

/// Interpret words and settle the outcome: the shared path for speech and typed tests.
pub async fn hear(app: &AppHandle, text: &str) -> Heard {
    // "Open notes and show me the plan": each part in turn, when each is a command.
    let voice = app.state::<Voice>();
    if voice.pending().is_none() {
        let (mut snapshot, _, _) = snapshot(&app.state::<AppState>()).await;
        snapshot.apps = voice.apps();
        if let Some(parts) = voice::everyday::split_commands(text, &snapshot) {
            let mut last = None;
            for part in parts {
                last = Some(hear_one(app, &part).await);
            }
            if let Some(heard) = last {
                return heard;
            }
        }
    }
    hear_one(app, text).await
}

/// The voice agent's hands: the same checks and handlers as a spoken command.
struct AppHands {
    app: AppHandle,
}

impl voice::agent::Hands for AppHands {
    fn snapshot(&self) -> futures_util::future::BoxFuture<'static, voice::Snapshot> {
        let app = self.app.clone();
        Box::pin(async move {
            let (mut snapshot, _, _) = snapshot(&app.state::<AppState>()).await;
            snapshot.apps = app.state::<Voice>().apps();
            snapshot
        })
    }

    fn perform(
        &self,
        action: VoiceAction,
        confirm: bool,
        describe: String,
    ) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
        let app = self.app.clone();
        Box::pin(async move {
            if confirm {
                app.state::<Voice>().set_pending(Some(Pending {
                    action,
                    describe,
                    since: Some(Instant::now()),
                }));
                return Ok(String::new());
            }
            let (_, root, _) = snapshot(&app.state::<AppState>()).await;
            execute(&app, &action, root.as_deref())
                .await
                .map(|m| m.unwrap_or_default())
        })
    }

    fn chat_box(&self, text: String) -> futures_util::future::BoxFuture<'static, ()> {
        let app = self.app.clone();
        Box::pin(async move {
            bring_forward(&app);
            let _ = app.emit_to(
                "main",
                "voice://run",
                Run {
                    action: VoiceAction::AskHead { text },
                },
            );
        })
    }

    fn step(&self, line: String) {
        let _ = self.app.emit("voice://step", line);
    }

    fn browse(
        &self,
        task: String,
    ) -> futures_util::future::BoxFuture<'static, Result<String, String>> {
        let app = self.app.clone();
        Box::pin(async move { start_browsing(&app, task) })
    }

    fn lookup_email(
        &self,
        name: String,
    ) -> futures_util::future::BoxFuture<'static, Result<Vec<(String, String)>, String>> {
        Box::pin(async move {
            computer::lookup_emails(&name)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }
}

#[derive(Clone, Serialize)]
struct Browsing {
    /// The task running now; `None` when it has finished or stopped.
    task: Option<String>,
}

/// Start a browser task in the background. It reports on the voice bar as it goes and
/// says what it found when done. One at a time.
fn start_browsing(app: &AppHandle, task: String) -> Result<String, String> {
    let settings = settings::load().voice;
    if !settings.browser {
        return Err("browsing is turned off in Settings › Voice".into());
    }
    let voice = app.state::<Voice>();
    voice.browser.check_installed()?;
    if let Some(running) = voice.browsing() {
        return Err(format!(
            "the browser is still working on “{running}”. Say “stop browsing” first, or wait for it."
        ));
    }
    let job_app = app.clone();
    let job_task = task.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let app = job_app;
        let _ = app.emit(
            "voice://browsing",
            Browsing {
                task: Some(job_task.clone()),
            },
        );
        let outcome = run_browse_task(&app, &job_task, &settings).await;
        let (text, error) = match outcome {
            Ok(text) => (text, None),
            Err(error) => {
                tracing::warn!("browser task failed: {error}");
                (
                    String::new(),
                    Some(format!("The browser task failed: {error}")),
                )
            }
        };
        let voice = app.state::<Voice>();
        let heard = Heard {
            interpretation: Interpretation {
                transcript: job_task,
                outcome: Outcome::Reply {
                    text: if text.is_empty() {
                        "Stopped.".into()
                    } else {
                        text
                    },
                },
                source: voice::Source::Agent,
                confidence: None,
                laya_ms: None,
                reply: None,
            },
            pending: voice.pending(),
            done: None,
            error,
            speak: settings.speak_replies,
        };
        // Finished first, so the bar may hide once the answer has been read.
        let _ = app.emit("voice://browsing", Browsing { task: None });
        show_hud(&app);
        let _ = app.emit("voice://heard", heard);
    });
    *voice.browse_job.lock().expect("not poisoned") = Some((task, handle));
    Ok("Started: the browser assistant is working on it in its own Chrome window and will report back. Say you've started on it.".into())
}

async fn run_browse_task(
    app: &AppHandle,
    task: &str,
    settings: &VoiceSettings,
) -> Result<String, String> {
    let voice = app.state::<Voice>();
    let slot_lock = Arc::clone(&voice.browse_agent);
    let mut slot = slot_lock.lock().await;
    let key = format!("{}\n{}", settings.browser_model, settings.about_me);
    if slot.as_ref().is_some_and(|(k, _)| k != &key) {
        if let Some((_, old)) = slot.take() {
            old.shutdown().await;
        }
    }
    if slot.is_none() {
        let hands: Arc<dyn voice::agent::Hands> = Arc::new(AppHands { app: app.clone() });
        let agent = voice::agent::VoiceAgent::start_browser(
            hands,
            Arc::clone(&voice.browser),
            &settings.browser_model,
            &agent_dir().join("browser"),
            &settings.about_me,
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        *slot = Some((key, agent));
    }
    let (_, agent) = slot.as_mut().expect("just set");
    match agent.run_task(task, Duration::from_secs(15 * 60)).await {
        Ok(reply) => Ok(reply.text),
        Err(error) => {
            // A stuck or dead agent is replaced next time.
            if let Some((_, agent)) = slot.take() {
                agent.shutdown().await;
            }
            Err(format!("{error:#}"))
        }
    }
}

/// Stop the browser task: its agent is shut down (the next task starts a fresh one);
/// the window stays open.
async fn stop_browsing(app: &AppHandle) -> bool {
    let voice = app.state::<Voice>();
    let job = voice.browse_job.lock().expect("not poisoned").take();
    let Some((_, handle)) = job else {
        return false;
    };
    handle.abort();
    if let Some((_, agent)) = voice.browse_agent.lock().await.take() {
        agent.shutdown().await;
    }
    let _ = app.emit("voice://browsing", Browsing { task: None });
    true
}

/// The folder the voice agent runs in: its own, so no project's instructions reach it.
fn agent_dir() -> PathBuf {
    harness_core::extensions::app_data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("voice-agent")
}

/// Start the voice agent if it is not running, or is running another model.
async fn ensure_agent(app: &AppHandle, settings: &VoiceSettings) -> Result<(), String> {
    let voice = app.state::<Voice>();
    let mut slot = voice.agent.lock().await;
    let key = format!("{}\n{}", settings.agent_model, settings.about_me);
    if slot.as_ref().is_some_and(|(k, _)| k == &key) {
        return Ok(());
    }
    if let Some((_, old)) = slot.take() {
        old.shutdown().await;
    }
    let hands: Arc<dyn voice::agent::Hands> = Arc::new(AppHands { app: app.clone() });
    let agent = voice::agent::VoiceAgent::start(
        hands,
        &settings.agent_model,
        &agent_dir(),
        &settings.about_me,
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    *slot = Some((key, agent));
    Ok(())
}

/// Hand the words to the voice agent and wait for it. `None` if it could not run, so the
/// words go the old way instead.
async fn ask_agent(
    app: &AppHandle,
    text: &str,
    snapshot: &voice::Snapshot,
    settings: &VoiceSettings,
) -> Option<Interpretation> {
    if let Err(error) = ensure_agent(app, settings).await {
        tracing::warn!("voice agent unavailable: {error}");
        return None;
    }
    phase(app, "thinking", Some(text.to_string()));
    let voice = app.state::<Voice>();
    let mut slot = voice.agent.lock().await;
    let (_, agent) = slot.as_mut()?;
    match agent.ask(text, snapshot, Duration::from_secs(120)).await {
        Ok(reply) => {
            let said = reply.text.trim_start_matches("Done:").trim().to_string();
            Some(Interpretation {
                transcript: text.to_string(),
                outcome: Outcome::Reply {
                    text: if said.is_empty() {
                        "Done.".into()
                    } else {
                        said
                    },
                },
                source: voice::Source::Agent,
                confidence: None,
                laya_ms: Some(reply.ms),
                reply: None,
            })
        }
        Err(error) => {
            // A stuck or dead agent is replaced next time.
            tracing::warn!("voice agent failed: {error:#}");
            if let Some((_, agent)) = slot.take() {
                agent.shutdown().await;
            }
            None
        }
    }
}

async fn hear_one(app: &AppHandle, text: &str) -> Heard {
    let voice = app.state::<Voice>();
    let settings = settings::load().voice;
    let (mut snapshot, root, _) = snapshot(&app.state::<AppState>()).await;
    snapshot.apps = voice.apps();
    let pending = voice.pending();
    // Exact commands stay instant; anything else, with the agent on, is the agent's.
    let for_agent = settings.agent
        && pending.is_none()
        && voice::matcher::match_command(text, &snapshot, false).is_none()
        && !voice::matcher::normalize(text).is_empty();
    let from_agent = if for_agent {
        ask_agent(app, text, &snapshot, &settings).await
    } else {
        None
    };
    let interpretation = match from_agent {
        Some(interpretation) => interpretation,
        None => {
            voice::interpret(
                text,
                &snapshot,
                Some(&voice.laya),
                settings.confidence,
                pending.as_ref().map(|p| p.describe.as_str()),
            )
            .await
        }
    };

    let mut done = None;
    let mut error = None;
    match &interpretation.outcome {
        Outcome::Act {
            action: VoiceAction::Confirm,
            ..
        } => match pending {
            Some(p) => {
                voice.set_pending(None);
                match execute(app, &p.action, root.as_deref()).await {
                    Ok(message) => done = Some(message.unwrap_or(p.describe)),
                    Err(err) => error = Some(err),
                }
            }
            None => done = Some("There was nothing to confirm.".into()),
        },
        Outcome::Act {
            action: VoiceAction::Cancel,
            ..
        } => {
            voice.set_pending(None);
            done = Some("Cancelled.".into());
        }
        Outcome::Act {
            action,
            confirm: true,
            describe,
        } => {
            voice.set_pending(Some(Pending {
                action: action.clone(),
                describe: describe.clone(),
                since: Some(Instant::now()),
            }));
        }
        Outcome::Act {
            action,
            confirm: false,
            describe,
        } => match execute(app, action, root.as_deref()).await {
            Ok(message) => done = message.or_else(|| Some(describe.clone())),
            Err(err) => error = Some(err),
        },
        _ => {}
    }
    // Words for the head agent go into its chat box, to be edited and sent by hand.
    if matches!(interpretation.outcome, Outcome::ToHead { .. }) {
        bring_forward(app);
    }
    let heard = Heard {
        interpretation,
        pending: voice.pending(),
        done,
        error,
        speak: settings.speak_replies,
    };
    let _ = app.emit("voice://heard", heard.clone());
    heard
}

/// Carry out an action. Computer actions run here; harness actions go to the main
/// window, which runs them through the same handlers as its buttons.
async fn execute(
    app: &AppHandle,
    action: &VoiceAction,
    project: Option<&std::path::Path>,
) -> Result<Option<String>, String> {
    use VoiceAction::*;
    match action {
        OpenApp { .. }
        | OpenUrl { .. }
        | OpenFolder { .. }
        | RevealProject
        | OpenProjectInEditor
        | Search { .. }
        | Media { .. }
        | PlayPlaylist { .. }
        | PlayQuery { .. }
        | NewNote { .. }
        | Remind { .. }
        | System { .. } => {
            let opening = computer::validate(action, project).map_err(|e| format!("{e:#}"))?;
            computer::open(&opening)
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(computer::done_message(&opening))
        }
        DraftEmail { .. } => {
            let opening = computer::validate(action, project).map_err(|e| format!("{e:#}"))?;
            computer::open(&opening)
                .await
                .map_err(|e| format!("{e:#}"))?;
            Ok(Some(
                "The draft is open in Gmail — check it and press Send.".into(),
            ))
        }
        BrowserDo { step, describe } => {
            let voice = app.state::<Voice>();
            let result = voice
                .browser
                .run_step(step)
                .await
                .map_err(|e| format!("{e:#} — the page may have changed; ask again"))?;
            Ok(Some(format!("{describe}: done, {result}.")))
        }
        StopBrowsing => Ok(Some(if stop_browsing(app).await {
            "Stopped browsing.".into()
        } else {
            "Nothing was being browsed.".into()
        })),
        // Answered in words; nothing to run.
        Status { .. } => Ok(None),
        // "Tell Claude to …" is put in the chat box too, never sent unseen.
        AskHead { .. } => {
            bring_forward(app);
            app.emit_to(
                "main",
                "voice://run",
                Run {
                    action: action.clone(),
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(Some(
                "In the chat box — edit it and send when ready.".into(),
            ))
        }
        _ => {
            app.emit_to(
                "main",
                "voice://run",
                Run {
                    action: action.clone(),
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------- commands

#[derive(Serialize)]
pub struct VoiceStatus {
    /// Whether this build has capture and Whisper at all.
    built: bool,
    settings: VoiceSettings,
    whisper_downloaded: bool,
    whisper_megabytes: u32,
    laya: LayaState,
    /// The natural voice: loaded, on disk, or what to install.
    speech_ready: bool,
    speech_downloaded: bool,
    speech_hint: Option<String>,
    listening: bool,
    pending: Option<Pending>,
}

#[tauri::command]
pub async fn voice_status(app: AppHandle) -> Result<VoiceStatus, String> {
    let voice = app.state::<Voice>();
    let settings = settings::load().voice;
    let dir = models_dir();
    // The first look asks the helper whether Laya's weights are on disk.
    if matches!(voice.laya.state(), LayaState::Stopped { downloaded: None }) {
        let _ = voice.laya.probe().await;
    }
    #[cfg(feature = "voice")]
    let listening = voice.session.lock().expect("not poisoned").is_some();
    #[cfg(not(feature = "voice"))]
    let listening = false;
    Ok(VoiceStatus {
        built: cfg!(feature = "voice"),
        whisper_downloaded: dir
            .as_deref()
            .is_some_and(|d| stt::is_downloaded(d, settings.stt_model)),
        whisper_megabytes: settings.stt_model.megabytes(),
        settings,
        laya: voice.laya.state(),
        speech_ready: voice.speech.is_ready(),
        speech_downloaded: voice.speech.downloaded(),
        speech_hint: voice.speech.check_installed().err(),
        listening,
        pending: voice.pending(),
    })
}

/// Download (if needed) and load Whisper or Laya, reporting progress as `voice://progress`.
#[tauri::command]
pub async fn voice_prepare(app: AppHandle, what: String) -> Result<VoiceStatus, String> {
    match what.as_str() {
        "whisper" => {
            let size = settings::load().voice.stt_model;
            let dir = models_dir().ok_or("no app data folder")?;
            let progress_app = app.clone();
            stt::download(&dir, size, move |received, total| {
                let _ = progress_app.emit(
                    "voice://progress",
                    Progress {
                        what: "whisper",
                        file: Some(size.file_name()),
                        received,
                        total,
                    },
                );
            })
            .await
            .map_err(|e| format!("{e:#}"))?;
            #[cfg(feature = "voice")]
            whisper(&app).await?;
        }
        "laya" => {
            let laya = Arc::clone(&app.state::<Voice>().laya);
            let mut watch = laya.subscribe();
            let progress_app = app.clone();
            let forward = tauri::async_runtime::spawn(async move {
                while watch.changed().await.is_ok() {
                    if let LayaState::Loading {
                        file,
                        received,
                        total,
                    } = watch.borrow().clone()
                    {
                        let _ = progress_app.emit(
                            "voice://progress",
                            Progress {
                                what: "laya",
                                file,
                                received,
                                total,
                            },
                        );
                    }
                }
            });
            let result = laya.load().await;
            forward.abort();
            result.map_err(|e| format!("{e:#}"))?;
        }
        "speech" => {
            let speech = Arc::clone(&app.state::<Voice>().speech);
            speech.check_installed()?;
            let mut watch = speech.subscribe();
            let progress_app = app.clone();
            let forward = tauri::async_runtime::spawn(async move {
                while watch.changed().await.is_ok() {
                    if let Some(p) = watch.borrow().clone() {
                        let _ = progress_app.emit(
                            "voice://progress",
                            Progress {
                                what: "speech",
                                file: p.file,
                                received: p.received,
                                total: p.total,
                            },
                        );
                    }
                }
            });
            let result = speech.load().await;
            forward.abort();
            result.map_err(|e| format!("{e:#}"))?;
        }
        other => return Err(format!("nothing called {other} to prepare")),
    }
    voice_status(app).await
}

/// Mic button in the app: the same as the hotkey.
#[tauri::command]
pub async fn voice_start(app: AppHandle) -> Result<(), String> {
    start(&app).await
}

#[tauri::command]
pub async fn voice_stop(app: AppHandle) -> Result<(), String> {
    finish(&app).await
}

/// Answer the pending question by click.
#[tauri::command]
pub async fn voice_confirm(app: AppHandle, yes: bool) -> Result<Heard, String> {
    Ok(hear(&app, if yes { "yes" } else { "cancel" }).await)
}

/// What typed words would do, against the harness as it is. Does nothing.
#[tauri::command]
pub async fn voice_try(app: AppHandle, text: String) -> Result<Interpretation, String> {
    let voice = app.state::<Voice>();
    let settings = settings::load().voice;
    let (mut snapshot, _, _) = snapshot(&app.state::<AppState>()).await;
    snapshot.apps = voice.apps();
    // Never runs the agent here: it would really do things.
    if settings.agent && voice::matcher::match_command(&text, &snapshot, false).is_none() {
        return Ok(Interpretation {
            transcript: text.clone(),
            outcome: Outcome::Reply {
                text:
                    "Not an exact command — the voice agent would work out the steps and do them."
                        .into(),
            },
            source: voice::Source::Agent,
            confidence: None,
            laya_ms: None,
            reply: None,
        });
    }
    Ok(voice::interpret(
        &text,
        &snapshot,
        Some(&voice.laya),
        settings.confidence,
        None,
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_settings_refuse_what_would_not_work() {
        let ok = VoiceSettings::default().validated().unwrap();
        assert_eq!(ok.hotkey, "Alt+Space");
        assert!(!ok.enabled, "off until turned on");
        for broken in [
            VoiceSettings {
                hotkey: "".into(),
                ..VoiceSettings::default()
            },
            VoiceSettings {
                hotkey: "Banana+Q".into(),
                ..VoiceSettings::default()
            },
            VoiceSettings {
                confidence: 0.2,
                ..VoiceSettings::default()
            },
            VoiceSettings {
                laya_idle_minutes: 0,
                ..VoiceSettings::default()
            },
        ] {
            assert!(broken.validated().is_err());
        }
    }
}
