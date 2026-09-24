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

use harness_core::voice::laya::{LayaClient, LayaConfig, LayaState};
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
        }
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
    tauri::WebviewWindowBuilder::new(app, HUD_LABEL, tauri::WebviewUrl::App("index.html".into()))
        .title("Harness voice")
        .inner_size(460.0, 220.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        .build()?;
    Ok(())
}

fn show_hud(app: &AppHandle) {
    let Some(window) = hud(app) else { return };
    if let Ok(Some(monitor)) = window
        .current_monitor()
        .or_else(|_| window.primary_monitor())
    {
        let size = monitor.size();
        let scale = monitor.scale_factor();
        let width = (460.0 * scale) as i32;
        let x = monitor.position().x + (size.width as i32 - width) / 2;
        let y = monitor.position().y + (48.0 * scale) as i32;
        let _ = window.set_position(tauri::PhysicalPosition { x, y });
    }
    let _ = window.show();
}

#[tauri::command]
pub fn voice_hide_hud(app: AppHandle) {
    if let Some(window) = hud(&app) {
        let _ = window.hide();
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

async fn hear_one(app: &AppHandle, text: &str) -> Heard {
    let voice = app.state::<Voice>();
    let settings = settings::load().voice;
    let (mut snapshot, root, _) = snapshot(&app.state::<AppState>()).await;
    snapshot.apps = voice.apps();
    let pending = voice.pending();
    let interpretation = voice::interpret(
        text,
        &snapshot,
        Some(&voice.laya),
        settings.confidence,
        pending.as_ref().map(|p| p.describe.as_str()),
    )
    .await;

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
