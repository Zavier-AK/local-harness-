//! Desktop shell for the harness.
//!
//! Deliberately thin. Every decision lives in `harness-core`; this file only owns the
//! session's lifetime, turns engine events into Tauri events for the UI, and exposes the
//! commands the UI calls. Keeping it this small is what lets the same engine be driven
//! headlessly by `harness-cli` and tested without a desktop.

use harness_core::engine::{Harness, WorkerRecord};
use harness_core::event::HarnessEvent;
use harness_core::isolation::Workspaces;
use harness_core::orchestrator::Orchestrator;
use harness_core::roles::RoleRegistry;
use harness_core::store::Store;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

/// Event channel the UI subscribes to.
const EVENT_CHANNEL: &str = "harness://event";

#[derive(Default)]
pub struct AppState {
    session: Mutex<Option<Session>>,
}

struct Session {
    orchestrator: Orchestrator,
    harness: Arc<Harness>,
}

#[derive(Serialize)]
pub struct SessionInfo {
    session_id: String,
    project_root: String,
    mcp_url: String,
    roles: Vec<RoleView>,
}

#[derive(Serialize)]
pub struct RoleView {
    name: String,
    provider: String,
    model: Option<String>,
    isolation: String,
    can_edit_files: bool,
    brief: Option<String>,
}

#[derive(Serialize)]
pub struct UsageView {
    provider: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_usd: f64,
    runs: u64,
}

/// Forward engine events to the webview, and feed them back to the engine so rate-limit
/// backpressure works the same here as it does in the CLI.
fn forward_events(
    app: AppHandle,
    harness: Arc<Harness>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            harness.note_event(&event).await;
            if let Err(err) = app.emit(EVENT_CHANNEL, &event) {
                tracing::warn!("dropping event, webview gone: {err}");
                break;
            }
        }
    });
}

#[tauri::command]
async fn start_session(
    app: AppHandle,
    state: State<'_, AppState>,
    project_root: String,
    roles_path: Option<String>,
    model: Option<String>,
) -> Result<SessionInfo, String> {
    let mut slot = state.session.lock().await;
    if slot.is_some() {
        return Err("a session is already running".into());
    }

    let project = PathBuf::from(&project_root);
    let roles_file = roles_path
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("roles.toml"));

    let registry = RoleRegistry::load(&roles_file).map_err(|e| format!("{e:#}"))?;

    // The session database lives beside the project, so history survives app restarts.
    let db_path = project.join(".harness").join("sessions.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).map_err(|e| e.to_string())?;
    let store = Store::open(&db_path).map_err(|e| format!("{e:#}"))?;

    let session_id = format!("s-{}", std::process::id());
    store
        .create_session(&session_id, None, &project.display().to_string())
        .map_err(|e| format!("{e:#}"))?;

    let (tx, worker_events) = tokio::sync::mpsc::unbounded_channel();
    let harness = Arc::new(Harness::new(
        registry,
        Workspaces::new(project.clone()),
        store,
        session_id.clone(),
        tx,
    ));

    let (orchestrator, orchestrator_events) =
        Orchestrator::start(Arc::clone(&harness), &project, model, Some(200))
            .await
            .map_err(|e| format!("{e:#}"))?;

    // Worker and orchestrator streams are separate; the UI keys them apart by run id.
    forward_events(app.clone(), Arc::clone(&harness), worker_events);
    forward_events(app, Arc::clone(&harness), orchestrator_events);

    let info = SessionInfo {
        session_id,
        project_root: project.display().to_string(),
        mcp_url: orchestrator.mcp_url(),
        roles: harness
            .list_roles()
            .into_iter()
            .map(|r| RoleView {
                name: r.name,
                provider: r.provider,
                model: r.model,
                isolation: r.isolation,
                can_edit_files: r.can_edit_files,
                brief: r.brief,
            })
            .collect(),
    };

    *slot = Some(Session { orchestrator, harness });
    Ok(info)
}

#[tauri::command]
async fn send_turn(state: State<'_, AppState>, text: String) -> Result<(), String> {
    let mut slot = state.session.lock().await;
    let session = slot.as_mut().ok_or("no session running")?;
    session.orchestrator.send(&text).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_workers(state: State<'_, AppState>) -> Result<Vec<WorkerRecord>, String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;
    Ok(session.harness.workers().await)
}

#[tauri::command]
async fn pending_merges(state: State<'_, AppState>) -> Result<Vec<(String, String)>, String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;
    Ok(session.harness.pending_merges().await)
}

/// The diff a worker left behind, so it can be read before it is landed.
///
/// Capped: a worker that regenerated a lockfile should not be able to wedge the drawer.
#[tauri::command]
async fn worker_patch(
    state: State<'_, AppState>,
    worker_id: String,
) -> Result<harness_core::isolation::Patch, String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;
    session
        .harness
        .worker_patch(&worker_id, 2_000)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Land a worker's branch. This is the only path that merges, and it exists only here —
/// the model cannot reach it, by design.
#[tauri::command]
async fn approve_merge(state: State<'_, AppState>, worker_id: String) -> Result<String, String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;
    session
        .harness
        .approve_merge(&worker_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn reject_merge(state: State<'_, AppState>, worker_id: String) -> Result<(), String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;
    session
        .harness
        .reject_merge(&worker_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn usage(state: State<'_, AppState>, hours: i64) -> Result<Vec<UsageView>, String> {
    let slot = state.session.lock().await;
    let session = slot.as_ref().ok_or("no session running")?;

    let rows = session
        .harness
        .usage_window(hours * 3600)
        .await
        .map_err(|e| format!("{e:#}"))?;

    Ok(rows
        .into_iter()
        .map(|r| UsageView {
            provider: r.provider,
            input_tokens: r.usage.input_tokens,
            output_tokens: r.usage.output_tokens,
            cache_creation_tokens: r.usage.cache_creation_input_tokens,
            cache_read_tokens: r.usage.cache_read_input_tokens,
            cost_usd: r.cost_usd,
            runs: r.runs,
        })
        .collect())
}

#[tauri::command]
async fn stop_session(state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.session.lock().await;
    if let Some(session) = slot.take() {
        session.orchestrator.shutdown().await.map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness_core=info,harness_app_lib=info".into()),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_session,
            send_turn,
            list_workers,
            worker_patch,
            pending_merges,
            approve_merge,
            reject_merge,
            usage,
            stop_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the harness app");
}
