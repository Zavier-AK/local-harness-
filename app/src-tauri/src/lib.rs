//! Desktop shell for the harness.
//!
//! Deliberately thin. Every decision lives in `harness-core`; this file only owns the
//! session's lifetime, turns engine events into Tauri events for the UI, and exposes the
//! commands the UI calls. Keeping it this small is what lets the same engine be driven
//! headlessly by `harness-cli` and tested without a desktop.

mod preview_probe;
mod settings;

use harness_core::engine::{Harness, WorkerRecord};
use harness_core::event::HarnessEvent;
use harness_core::extensions::{Extensions, ImportReport, SkillInfo, WorkerExtras};
use harness_core::isolation::Workspaces;
use harness_core::orchestrator::Orchestrator;
use harness_core::roles::RoleRegistry;
use harness_core::store::Store;
use harness_core::{DetectedBackend, FleetInspection, RoleModelPatch};
use preview_probe::{discover_dev_servers, parse_loopback_http_url, DevServer};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewUrl,
};
use std::collections::HashMap;
use tokio::sync::Mutex;

/// The first argument that makes this binary answer a Claude Code hook instead of
/// opening the app. See `main.rs`.
pub const HOOK_ARG: &str = "__harness-hook";

/// Answer a Claude Code worktree hook: read its JSON from stdin, act, and return the exit
/// code. `WorktreeCreate` prints the worktree path as its last stdout line, which is what
/// Claude Code reads; everything else goes to stderr.
pub fn run_hook(which: Option<&str>) -> i32 {
    use std::io::Read;
    let mut input = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("harness hook: reading stdin: {err}");
        return 1;
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("harness hook: {err}");
            return 1;
        }
    };
    let outcome = runtime.block_on(async {
        match which {
            Some("worktree-create") => harness_core::hooks::worktree_create(&input)
                .await
                .map(|path| println!("{}", path.display())),
            Some("worktree-remove") => harness_core::hooks::worktree_remove(&input).await,
            other => Err(anyhow::anyhow!("unknown hook {other:?}")),
        }
    });
    match outcome {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("harness hook: {err:#}");
            1
        }
    }
}

/// How Claude Code should invoke this app as a hook: this very binary, plus the flag.
fn hook_command() -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    Some(vec![exe.display().to_string(), HOOK_ARG.to_string()])
}

/// Event channel the UI subscribes to.
const EVENT_CHANNEL: &str = "harness://event";
const PREVIEW_LABEL: &str = "preview";

/// The default fleet, baked in so a project without a `roles.toml` can be given one
/// without the user hunting for a template.
const DEFAULT_ROLES: &str = include_str!("../../../roles.toml");

#[derive(Default)]
pub struct AppState {
    /// Open projects, keyed by canonical path. Several can be open at once; exactly one
    /// is in front.
    projects: Mutex<HashMap<String, Session>>,
    active: Mutex<Option<String>>,
    preview: std::sync::Mutex<Option<Webview>>,
}

/// One project's engine, plus what is needed to bring its head agent back after a
/// suspension.
struct Session {
    harness: Arc<Harness>,
    head: Head,
    project_root: PathBuf,
    roles_file: PathBuf,
    model: Option<String>,
}

/// The head agent is the expensive part of a project: a live `claude -p` process holding
/// the context floor. A project the user is not looking at should not be paying for one,
/// but its engine, worktrees and history stay put so switching back is cheap.
enum Head {
    Live(Box<Orchestrator>),
    /// Shut down. The backend conversation id is in the project's own store, so resuming
    /// picks the same conversation back up rather than starting over.
    Suspended,
}

impl Session {
    fn is_live(&self) -> bool {
        matches!(self.head, Head::Live(_))
    }

    async fn has_running_workers(&self) -> bool {
        self.harness
            .workers()
            .await
            .iter()
            .any(|worker| !worker.status.is_terminal())
    }

    /// Shut the head agent down, keeping everything else. Refuses while workers are still
    /// running: their results are reported through this session.
    async fn suspend(&mut self) -> bool {
        if self.has_running_workers().await {
            return false;
        }
        if let Head::Live(orchestrator) = std::mem::replace(&mut self.head, Head::Suspended) {
            if let Err(error) = orchestrator.shutdown().await {
                tracing::warn!("head agent did not shut down cleanly: {error:#}");
            }
            return true;
        }
        false
    }
}

#[derive(Serialize)]
pub struct SessionInfo {
    session_id: String,
    project_root: String,
    mcp_url: String,
    roles: Vec<RoleView>,
    backends: Vec<DetectedBackend>,
    resumed_head_session: bool,
}

/// One open project, as the switcher sees it.
#[derive(Serialize)]
pub struct ProjectView {
    project_root: String,
    name: String,
    active: bool,
    /// Whether its head agent is up. A suspended project costs nothing until reopened.
    live: bool,
    running_workers: usize,
    pending_merges: usize,
}

#[derive(Serialize)]
pub struct RoleView {
    name: String,
    provider: String,
    model: Option<String>,
    isolation: String,
    can_edit_files: bool,
    brief: Option<String>,
    available: bool,
    unavailable_reason: Option<String>,
}

/// What a directory offers before a session is started, so the UI can explain a problem
/// rather than failing with a raw error once the user has committed to starting.
#[derive(Serialize)]
pub struct ProjectStatus {
    exists: bool,
    is_git_repo: bool,
    has_roles_file: bool,
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

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewBounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl PreviewBounds {
    fn validate(self) -> Result<Self, String> {
        let values = [self.x, self.y, self.width, self.height];
        if values.iter().any(|value| !value.is_finite())
            || self.x < 0.0
            || self.y < 0.0
            || self.width < 1.0
            || self.height < 1.0
        {
            return Err("invalid preview bounds".into());
        }
        Ok(self)
    }
}

/// Forward engine events to the webview, and feed them back to the engine so rate-limit
/// backpressure works the same here as it does in the CLI.
fn forward_events(
    app: AppHandle,
    harness: Arc<Harness>,
    project: String,
    mut events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            harness.note_event(&event).await;
            if let Err(err) = app.emit(EVENT_CHANNEL, ProjectEvent { project: &project, event: &event })
            {
                tracing::warn!("dropping event, webview gone: {err}");
                break;
            }
        }
    });
}

/// An engine event, tagged with the project it came from.
///
/// Every project emits on one channel, and `HarnessEvent` carries only run and worker
/// ids — nothing that says which project a run belongs to. Without this tag two open
/// projects' streams would interleave with no way to separate them. Flattened, so the
/// event's own `type` discriminator is unchanged and existing handlers still match.
#[derive(Serialize, Clone)]
struct ProjectEvent<'a> {
    project: &'a str,
    #[serde(flatten)]
    event: &'a HarnessEvent,
}

/// Inspect a candidate project directory.
#[tauri::command]
async fn inspect_project(project_root: String) -> ProjectStatus {
    let project = PathBuf::from(&project_root);
    ProjectStatus {
        exists: project.is_dir(),
        // Worktree isolation needs a repository; without one, only `none`-isolation
        // roles could run, which is not worth starting a session over.
        is_git_repo: project.join(".git").exists(),
        has_roles_file: project.join("roles.toml").is_file(),
    }
}

/// Write the default fleet into a project that has none.
#[tauri::command]
async fn write_default_roles(project_root: String) -> Result<String, String> {
    let target = PathBuf::from(&project_root).join("roles.toml");
    if target.exists() {
        return Err(format!("{} already exists", target.display()));
    }

    std::fs::write(&target, DEFAULT_ROLES).map_err(|e| format!("{e}"))?;
    Ok(target.display().to_string())
}

fn roles_file(project_root: &str, roles_path: Option<String>) -> PathBuf {
    roles_path
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(project_root).join("roles.toml"))
}

/// Discover installed CLIs and local servers independently from the endpoints currently
/// named in roles.toml, then build compatible assignment choices for every role.
#[tauri::command]
async fn inspect_fleet(
    project_root: String,
    roles_path: Option<String>,
) -> Result<FleetInspection, String> {
    let roles_file = roles_file(&project_root, roles_path);
    let registry = RoleRegistry::load(&roles_file).map_err(|e| format!("{e:#}"))?;
    Ok(harness_core::detection::inspect_fleet(&registry).await)
}

/// Persist role assignments chosen from discovery, and apply them to a running session.
///
/// The core validates that the assignment is something this machine can actually run,
/// then edits only the fields it must, preserving the project's comments and role policy.
/// When a session is live the new fleet takes effect immediately: workers are spawned per
/// delegation, so nothing has to be restarted.
#[tauri::command]
async fn save_role_assignments(
    state: State<'_, AppState>,
    project_root: String,
    roles_path: Option<String>,
    patches: Vec<RoleModelPatch>,
) -> Result<FleetInspection, String> {
    let roles_file = roles_file(&project_root, roles_path);
    let registry = RoleRegistry::load(&roles_file).map_err(|e| format!("{e:#}"))?;
    let inspection = harness_core::detection::inspect_fleet(&registry).await;
    harness_core::detection::validate_patches(&registry, &inspection, &patches)
        .map_err(|e| format!("{e:#}"))?;
    harness_core::roles_patch::apply_role_patches_file(&roles_file, &patches)
        .map_err(|e| format!("{e:#}"))?;

    let updated = RoleRegistry::load(&roles_file).map_err(|e| format!("{e:#}"))?;

    // Push the new fleet into that project's live session, if it has one.
    let key = project_key(&project_root);
    let mut projects = state.projects.lock().await;
    if let Some(session) = projects.get_mut(&key) {
        session.harness.swap_registry(updated.clone()).await;

        // The head agent's brief is baked into its process argv and cannot be rewritten,
        // so it still describes the old fleet. Tell it what moved; its `list_roles` tool
        // reads live state, so one sentence is enough to keep it from planning around a
        // backend that is no longer there.
        let changed = patches
            .iter()
            .map(|patch| {
                let provider = patch
                    .provider
                    .clone()
                    .or_else(|| {
                        updated
                            .roles
                            .get(&patch.role_name)
                            .map(|role| role.provider.as_str().to_string())
                    })
                    .unwrap_or_default();
                format!("`{}` now runs {provider}/{}", patch.role_name, patch.model)
            })
            .collect::<Vec<_>>()
            .join("; ");

        // A native role's subagent definition was fixed when the head agent's process
        // started, so the Agent tool would keep running the old one. `delegate` always
        // reads the live fleet, so route a changed native role through it until the
        // project is reopened.
        let fixed: Vec<String> = patches
            .iter()
            .filter(|patch| {
                harness_core::native::native_roles(&registry).contains_key(&patch.role_name)
            })
            .map(|patch| format!("`{}`", patch.role_name))
            .collect();
        let fixed_note = if fixed.is_empty() {
            String::new()
        } else {
            format!(
                " Your Agent-tool definition for {} is fixed for this session, so until the \
                 project is reopened delegate to it with the `delegate` tool instead.",
                fixed.join(", ")
            )
        };

        if !changed.is_empty() {
            let notice = format!(
                "The fleet changed: {changed}. Call `list_roles` before your next \
                 delegation so you are planning against the current fleet.{fixed_note}"
            );
            if let Head::Live(orchestrator) = &mut session.head {
                if let Err(error) = orchestrator.send(&notice).await {
                    tracing::warn!("could not notify the head agent of the fleet change: {error:#}");
                }
            }
        }
    }

    Ok(harness_core::detection::inspect_fleet(&updated).await)
}

#[tauri::command]
async fn start_session(
    app: AppHandle,
    state: State<'_, AppState>,
    project_root: String,
    roles_path: Option<String>,
    model: Option<String>,
) -> Result<SessionInfo, String> {
    let key = project_key(&project_root);

    // Already open: bring it to the front, waking its head agent if it was suspended.
    {
        let mut projects = state.projects.lock().await;
        if let Some(session) = projects.get_mut(&key) {
            if !session.is_live() {
                session.harness.set_extras(current_extras()).await;
                let (orchestrator, events) = Orchestrator::start(
                    Arc::clone(&session.harness),
                    &session.project_root,
                    session.model.clone(),
                    Some(settings::load().max_turns),
                    session
                        .harness
                        .resumable_backend_session()
                        .await
                        .map_err(|e| format!("{e:#}"))?,
                    hook_command(),
                )
                .await
                .map_err(|e| format!("{e:#}"))?;
                forward_events(app.clone(), Arc::clone(&session.harness), key.clone(), events);
                session.head = Head::Live(Box::new(orchestrator));
            }
            let info = describe(&key, session).await;
            drop(projects);
            *state.active.lock().await = Some(key);
            return Ok(info);
        }
    }

    let project = PathBuf::from(&project_root);
    let roles_file = roles_file(&project_root, roles_path);
    let model = model.or_else(|| settings::load().default_model);

    if !roles_file.is_file() {
        return Err(format!("no roles.toml in {}", project.display()));
    }
    let registry = RoleRegistry::load(&roles_file).map_err(|e| format!("{e:#}"))?;

    // The session database lives beside the project, so history survives app restarts.
    let db_path = project.join(".harness").join("sessions.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).map_err(|e| e.to_string())?;
    let store = Store::open(&db_path).map_err(|e| format!("{e:#}"))?;
    let resume_session_id = store
        .latest_orchestrator_backend_session(&key)
        .map_err(|e| format!("{e:#}"))?;

    let session_id = format!(
        "s-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    store
        .create_session(&session_id, None, &key)
        .map_err(|e| format!("{e:#}"))?;

    let (tx, worker_events) = tokio::sync::mpsc::unbounded_channel();
    let workspaces = Workspaces::with_setup(project.clone(), registry.worktree.clone());
    // Worker worktrees live under the project; keep them out of its `git status`.
    if let Err(error) = workspaces.ensure_git_exclude().await {
        tracing::warn!("could not update .git/info/exclude: {error:#}");
    }
    let harness = Arc::new(Harness::new(
        registry,
        workspaces,
        store,
        session_id.clone(),
        tx,
    ));
    harness.set_extras(current_extras()).await;

    let (orchestrator, orchestrator_events) = Orchestrator::start(
        Arc::clone(&harness),
        &project,
        model.clone(),
        Some(settings::load().max_turns),
        resume_session_id,
        hook_command(),
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let resumed_head_session = orchestrator.resumed_backend_session();

    // Worker and orchestrator streams are separate; the UI keys them apart by run id, and
    // by project now that several can be open at once.
    forward_events(app.clone(), Arc::clone(&harness), key.clone(), worker_events);
    forward_events(app, Arc::clone(&harness), key.clone(), orchestrator_events);

    let info = SessionInfo {
        session_id,
        project_root: key.clone(),
        mcp_url: orchestrator.mcp_url(),
        backends: harness_core::detection::detect_backends().await,
        resumed_head_session,
        roles: harness
            .list_roles_probed()
            .await
            .into_iter()
            .map(|r| RoleView {
                name: r.name,
                provider: r.provider,
                model: r.model,
                isolation: r.isolation,
                can_edit_files: r.can_edit_files,
                brief: r.brief,
                available: r.available.unwrap_or(true),
                unavailable_reason: r.unavailable_reason,
            })
            .collect(),
    };

    state.projects.lock().await.insert(
        key.clone(),
        Session {
            harness,
            head: Head::Live(Box::new(orchestrator)),
            project_root: project,
            roles_file,
            model: model.clone(),
        },
    );
    *state.active.lock().await = Some(key);
    Ok(info)
}

/// A `SessionInfo` for a project that is already open.
async fn describe(key: &str, session: &Session) -> SessionInfo {
    SessionInfo {
        session_id: session.harness.session_id().to_string(),
        project_root: key.to_string(),
        mcp_url: match &session.head {
            Head::Live(orchestrator) => orchestrator.mcp_url(),
            Head::Suspended => String::new(),
        },
        backends: harness_core::detection::detect_backends().await,
        // True only of a fresh start; re-focusing an open project is not a resume.
        resumed_head_session: false,
        roles: session
            .harness
            .list_roles_probed()
            .await
            .into_iter()
            .map(|r| RoleView {
                name: r.name,
                provider: r.provider,
                model: r.model,
                isolation: r.isolation,
                can_edit_files: r.can_edit_files,
                brief: r.brief,
                available: r.available.unwrap_or(true),
                unavailable_reason: r.unavailable_reason,
            })
            .collect(),
    }
}

/// Canonical key for a project path. Canonicalized so the same project reached by two
/// spellings is one entry rather than two competing sessions over the same worktrees.
fn project_key(path: &str) -> String {
    let path = PathBuf::from(path);
    path.canonicalize().unwrap_or(path).display().to_string()
}

/// Which project a command is about: the one it names, or the one in front.
async fn key_for(state: &AppState, project: Option<String>) -> Result<String, String> {
    match project {
        Some(path) => Ok(project_key(&path)),
        None => state
            .active
            .lock()
            .await
            .clone()
            .ok_or_else(|| "no project is open".to_string()),
    }
}

/// The running session's fleet, re-probed.
///
/// `SessionInfo.roles` is a snapshot from session start; once a role can be reassigned
/// mid-session the UI needs to re-read it rather than trust that snapshot.
#[tauri::command]
async fn session_roles(
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<Vec<RoleView>, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    Ok(session
        .harness
        .list_roles_probed()
        .await
        .into_iter()
        .map(|r| RoleView {
            name: r.name,
            provider: r.provider,
            model: r.model,
            isolation: r.isolation,
            can_edit_files: r.can_edit_files,
            brief: r.brief,
            available: r.available.unwrap_or(true),
            unavailable_reason: r.unavailable_reason,
        })
        .collect())
}

#[tauri::command]
async fn send_turn(
    state: State<'_, AppState>,
    text: String,
    project: Option<String>,
) -> Result<(), String> {
    let key = key_for(&state, project).await?;
    let mut projects = state.projects.lock().await;
    let session = projects.get_mut(&key).ok_or("no session for that project")?;

    let Head::Live(orchestrator) = &mut session.head else {
        return Err("that project's head agent is suspended; open the project first".into());
    };
    orchestrator
        .send(&text)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn list_workers(
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<Vec<WorkerRecord>, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    Ok(session.harness.workers().await)
}

#[tauri::command]
async fn pending_merges(
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    Ok(session.harness.pending_merges().await)
}

/// The diff a worker left behind, so it can be read before it is landed.
///
/// Capped: a worker that regenerated a lockfile should not be able to wedge the drawer.
#[tauri::command]
async fn worker_patch(
    state: State<'_, AppState>,
    worker_id: String,
    project: Option<String>,
) -> Result<harness_core::isolation::Patch, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    session
        .harness
        .worker_patch(&worker_id, 2_000)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Land a worker's branch. This is the only path that merges, and it exists only here —
/// the model cannot reach it, by design.
#[tauri::command]
async fn approve_merge(
    state: State<'_, AppState>,
    worker_id: String,
    project: Option<String>,
) -> Result<String, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    session
        .harness
        .approve_merge(&worker_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn reject_merge(
    state: State<'_, AppState>,
    worker_id: String,
    project: Option<String>,
) -> Result<(), String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    session
        .harness
        .reject_merge(&worker_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Token burn over the trailing window, summed across every open project.
///
/// Each project keeps its own database, but the limit being measured is the
/// subscription's, which is per account. Reporting one project's burn would understate
/// it by however many other projects are open — which is exactly the situation the
/// project switcher creates.
#[tauri::command]
async fn usage(state: State<'_, AppState>, hours: i64) -> Result<Vec<UsageView>, String> {
    let projects = state.projects.lock().await;
    if projects.is_empty() {
        return Err("no project is open".into());
    }

    let mut totals: std::collections::BTreeMap<String, UsageView> = Default::default();
    for session in projects.values() {
        let rows = session
            .harness
            .usage_window(hours * 3600)
            .await
            .map_err(|e| format!("{e:#}"))?;

        for row in rows {
            let entry = totals.entry(row.provider.clone()).or_insert_with(|| UsageView {
                provider: row.provider.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                cost_usd: 0.0,
                runs: 0,
            });
            entry.input_tokens += row.usage.input_tokens;
            entry.output_tokens += row.usage.output_tokens;
            entry.cache_creation_tokens += row.usage.cache_creation_input_tokens;
            entry.cache_read_tokens += row.usage.cache_read_input_tokens;
            entry.cost_usd += row.cost_usd;
            entry.runs += row.runs;
        }
    }

    Ok(totals.into_values().collect())
}

/// Find already-running local HTTP servers. This command never manages server processes.
#[tauri::command]
async fn probe_preview_servers(
    project_root: String,
    worker_roots: Vec<String>,
    excluded_ports: Vec<u16>,
) -> Vec<DevServer> {
    discover_dev_servers(
        PathBuf::from(project_root),
        worker_roots.into_iter().map(PathBuf::from).collect(),
        excluded_ports.into_iter().collect::<HashSet<_>>(),
    )
    .await
}

#[tauri::command]
async fn create_preview(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    bounds: PreviewBounds,
) -> Result<String, String> {
    let url = parse_loopback_http_url(&url)?;
    let bounds = bounds.validate()?;
    let mut slot = state.preview.lock().map_err(|_| "preview lock poisoned")?;

    let webview = if let Some(webview) = slot.as_ref() {
        webview.clone()
    } else {
        let window = app
            .get_window("main")
            .ok_or_else(|| "main window is unavailable".to_string())?;
        let builder = WebviewBuilder::new(PREVIEW_LABEL, WebviewUrl::External(url.clone()))
            .on_navigation(preview_probe::is_loopback_http_url)
            .on_new_window(|_, _| NewWindowResponse::Deny);
        let webview = window
            .add_child(
                builder,
                LogicalPosition::new(bounds.x, bounds.y),
                LogicalSize::new(bounds.width, bounds.height),
            )
            .map_err(|error| error.to_string())?;
        *slot = Some(webview.clone());
        webview
    };
    drop(slot);

    set_webview_bounds(&webview, bounds)?;
    webview.show().map_err(|error| error.to_string())?;
    Ok(url.to_string())
}

#[tauri::command]
async fn set_preview_bounds(
    state: State<'_, AppState>,
    bounds: PreviewBounds,
) -> Result<(), String> {
    let bounds = bounds.validate()?;
    let webview = preview_webview(&state)?;
    set_webview_bounds(&webview, bounds)
}

#[tauri::command]
async fn set_preview_visible(state: State<'_, AppState>, visible: bool) -> Result<(), String> {
    let webview = match preview_webview(&state) {
        Ok(webview) => webview,
        // Hiding before the preview has been opened is already the desired state.
        Err(_) if !visible => return Ok(()),
        Err(error) => return Err(error),
    };
    if visible {
        webview.show()
    } else {
        webview.hide()
    }
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn navigate_preview(state: State<'_, AppState>, url: String) -> Result<String, String> {
    let url = parse_loopback_http_url(&url)?;
    preview_webview(&state)?
        .navigate(url.clone())
        .map_err(|error| error.to_string())?;
    Ok(url.to_string())
}

#[tauri::command]
async fn reload_preview(state: State<'_, AppState>) -> Result<(), String> {
    preview_webview(&state)?
        .reload()
        .map_err(|error| error.to_string())
}

fn preview_webview(state: &State<'_, AppState>) -> Result<Webview, String> {
    state
        .preview
        .lock()
        .map_err(|_| "preview lock poisoned".to_string())?
        .clone()
        .ok_or_else(|| "open the Preview tab first".to_string())
}

fn set_webview_bounds(webview: &Webview, bounds: PreviewBounds) -> Result<(), String> {
    webview
        .set_position(LogicalPosition::new(bounds.x, bounds.y))
        .and_then(|_| webview.set_size(LogicalSize::new(bounds.width, bounds.height)))
        .map_err(|error| error.to_string())
}

/// Subscription headroom, as far as it is actually knowable.
///
/// Deliberately separate from `usage`: that reports what this harness has spent, which is
/// a real measurement, while this reports what the vendors say is left, which only one of
/// them tells us. Keeping them apart is what stops a token count being read as a limit.
#[tauri::command]
async fn quotas(state: State<'_, AppState>) -> Result<QuotaReport, String> {
    let projects = state.projects.lock().await;
    let mut limited: Vec<String> = Vec::new();
    for session in projects.values() {
        for provider in session.harness.rate_limited_providers().await {
            if !limited.contains(&provider) {
                limited.push(provider);
            }
        }
    }

    // Claude's quota is account-wide, so whichever open project heard from it last has the
    // freshest figure.
    let mut claude = None;
    for session in projects.values() {
        if let Some(snapshot) = session.harness.claude_quota_snapshot().await {
            if claude.as_ref().is_none_or(|(seen, _): &(i64, _)| snapshot.0 > *seen) {
                claude = Some(snapshot);
            }
        }
    }

    Ok(QuotaReport {
        providers: harness_core::quota::all_quotas(claude),
        rate_limited: limited,
    })
}

#[derive(Serialize)]
pub struct QuotaReport {
    providers: Vec<harness_core::quota::ProviderQuota>,
    /// Providers that have hit a wall and are shedding to fallbacks right now.
    rate_limited: Vec<String>,
}

/// Stop the head agent's current turn. Its conversation survives: the next message
/// carries on from the point it was stopped.
#[tauri::command]
async fn stop_turn(state: State<'_, AppState>, project: Option<String>) -> Result<(), String> {
    let key = key_for(&state, project).await?;
    let mut projects = state.projects.lock().await;
    let session = projects.get_mut(&key).ok_or("no session for that project")?;
    let Head::Live(orchestrator) = &mut session.head else {
        return Ok(()); // Suspended: nothing is running to stop.
    };
    orchestrator.interrupt().await.map_err(|e| format!("{e:#}"))
}

/// Stop one worker. Its process is killed and anything it wrote is kept on its branch.
#[tauri::command]
async fn stop_worker(
    state: State<'_, AppState>,
    worker_id: String,
    project: Option<String>,
) -> Result<bool, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    Ok(session.harness.cancel_worker(&worker_id).await)
}

/// The head agent's conversation for a project, so it survives restarts and switching.
#[tauri::command]
async fn chat_history(
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<Vec<HarnessEvent>, String> {
    let key = key_for(&state, project).await?;
    let projects = state.projects.lock().await;
    let session = projects.get(&key).ok_or("no session for that project")?;
    session
        .harness
        .project_history(400)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Every open project, for the switcher.
///
/// The most-reported failure with tools like this is losing track of work — a session
/// left running in a project nobody is looking at. So this reports every project's live
/// worker counts, not just the one in front.
#[tauri::command]
async fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectView>, String> {
    let active = state.active.lock().await.clone();
    let projects = state.projects.lock().await;

    let mut out = Vec::new();
    for (key, session) in projects.iter() {
        let workers = session.harness.workers().await;
        out.push(ProjectView {
            project_root: key.clone(),
            name: PathBuf::from(key)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| key.clone()),
            active: active.as_deref() == Some(key.as_str()),
            live: session.is_live(),
            running_workers: workers.iter().filter(|w| !w.status.is_terminal()).count(),
            pending_merges: session.harness.pending_merges().await.len(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Bring a project to the front, suspending the one being left.
///
/// A project only suspends once its workers have settled: shutting the head agent down
/// while a worker is still running would strand the result nobody is left to report.
#[tauri::command]
async fn focus_project(
    app: AppHandle,
    state: State<'_, AppState>,
    project: String,
) -> Result<SessionInfo, String> {
    let key = project_key(&project);
    let previous = state.active.lock().await.clone();

    let mut projects = state.projects.lock().await;
    if !projects.contains_key(&key) {
        return Err("that project is not open".into());
    }

    if let Some(previous) = previous.filter(|previous| previous != &key) {
        if let Some(leaving) = projects.get_mut(&previous) {
            leaving.suspend().await;
        }
    }

    let session = projects.get_mut(&key).ok_or("that project is not open")?;
    if !session.is_live() {
        session.harness.set_extras(current_extras()).await;
        let resume = session
            .harness
            .resumable_backend_session()
            .await
            .map_err(|e| format!("{e:#}"))?;
        let (orchestrator, events) = Orchestrator::start(
            Arc::clone(&session.harness),
            &session.project_root,
            session.model.clone(),
            Some(settings::load().max_turns),
            resume,
            hook_command(),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
        forward_events(app, Arc::clone(&session.harness), key.clone(), events);
        session.head = Head::Live(Box::new(orchestrator));
    }

    let info = describe(&key, session).await;
    drop(projects);
    *state.active.lock().await = Some(key);
    Ok(info)
}

/// Close a project: shut its head agent down and forget it. Worktrees, branches and its
/// database stay on disk, so reopening it resumes rather than restarts.
#[tauri::command]
async fn close_project(state: State<'_, AppState>, project: Option<String>) -> Result<(), String> {
    let key = key_for(&state, project).await?;

    let mut projects = state.projects.lock().await;
    let Some(mut session) = projects.remove(&key) else {
        return Ok(());
    };
    if let Head::Live(orchestrator) = std::mem::replace(&mut session.head, Head::Suspended) {
        orchestrator.shutdown().await.map_err(|e| format!("{e:#}"))?;
    }
    drop(projects);

    let mut active = state.active.lock().await;
    if active.as_deref() == Some(key.as_str()) {
        *active = None;
    }
    Ok(())
}

#[tauri::command]
async fn stop_session(state: State<'_, AppState>) -> Result<(), String> {
    let keys: Vec<String> = state.projects.lock().await.keys().cloned().collect();
    for key in keys {
        close_project(state.clone(), Some(key)).await?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]

// ------------------------------------------------------------------ settings

#[tauri::command]
fn get_settings() -> settings::Settings {
    settings::load()
}

/// Takes effect for heads started from now on; a running head keeps the turn limit and
/// model it was started with.
#[tauri::command]
fn save_settings(settings: settings::Settings) -> Result<settings::Settings, String> {
    let settings = settings.validated()?;
    let path = settings::path().ok_or("no home directory to keep settings in")?;
    settings::save_to(&path, &settings)?;
    Ok(settings)
}

// ---------------------------------------------------------- tools and skills

fn extensions() -> Result<Extensions, String> {
    harness_core::extensions::default_dir()
        .map(Extensions::new)
        .ok_or_else(|| "no home directory to keep skills in".to_string())
}

/// The current skills and servers. A broken library costs the skills, not the session.
fn current_extras() -> WorkerExtras {
    match extensions().and_then(|ext| ext.worker_extras().map_err(|e| format!("{e:#}"))) {
        Ok(extras) => extras,
        Err(error) => {
            tracing::warn!("skills and MCP servers not loaded: {error}");
            WorkerExtras::default()
        }
    }
}

/// Hand the changed set to every open project, so its next worker gets it. Head agents
/// pick it up when they next start: a running process cannot load a plugin.
async fn apply_extras(state: &AppState) {
    let extras = current_extras();
    for session in state.projects.lock().await.values() {
        session.harness.set_extras(extras.clone()).await;
    }
}

fn describe_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
fn list_skills() -> Result<Vec<SkillInfo>, String> {
    extensions()?.skills().map_err(describe_error)
}

#[tauri::command]
async fn set_skill_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<Vec<SkillInfo>, String> {
    let ext = extensions()?;
    ext.set_skill_enabled(&name, enabled).map_err(describe_error)?;
    apply_extras(&state).await;
    ext.skills().map_err(describe_error)
}

#[tauri::command]
async fn create_skill(
    state: State<'_, AppState>,
    name: String,
    description: String,
) -> Result<String, String> {
    let path = extensions()?
        .create_skill(&name, &description)
        .map_err(describe_error)?;
    apply_extras(&state).await;
    Ok(path.join("SKILL.md").display().to_string())
}

#[tauri::command]
async fn import_skills_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<ImportReport, String> {
    let report = extensions()?
        .import_skills(std::path::Path::new(&path), Some(&path))
        .map_err(describe_error)?;
    apply_extras(&state).await;
    Ok(report)
}

#[tauri::command]
async fn import_skills_git(state: State<'_, AppState>, url: String) -> Result<ImportReport, String> {
    let report = extensions()?
        .import_skills_from_git(&url)
        .await
        .map_err(describe_error)?;
    apply_extras(&state).await;
    Ok(report)
}

#[tauri::command]
async fn remove_skill(state: State<'_, AppState>, name: String) -> Result<Vec<SkillInfo>, String> {
    let ext = extensions()?;
    ext.remove_skill(&name).map_err(describe_error)?;
    apply_extras(&state).await;
    ext.skills().map_err(describe_error)
}

#[tauri::command]
fn list_mcp_servers() -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
    extensions()?.mcp_servers().map_err(describe_error)
}

#[tauri::command]
async fn set_mcp_server(
    state: State<'_, AppState>,
    name: String,
    config: serde_json::Value,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
    let ext = extensions()?;
    ext.set_mcp_server(&name, config).map_err(describe_error)?;
    apply_extras(&state).await;
    ext.mcp_servers().map_err(describe_error)
}

#[tauri::command]
async fn remove_mcp_server(
    state: State<'_, AppState>,
    name: String,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
    let ext = extensions()?;
    ext.remove_mcp_server(&name).map_err(describe_error)?;
    apply_extras(&state).await;
    ext.mcp_servers().map_err(describe_error)
}

#[derive(Serialize)]
struct RoleTools {
    name: String,
    provider: String,
    isolation: String,
    tools: Vec<String>,
    /// Run by the head agent's own `Agent` tool rather than `delegate`.
    native: bool,
}

/// The fleet file for a project: the open session's, or `roles.toml` beside it.
async fn roles_file_for(state: &AppState, project_root: &str) -> PathBuf {
    let projects = state.projects.lock().await;
    projects
        .get(&project_key(project_root))
        .map(|session| session.roles_file.clone())
        .unwrap_or_else(|| roles_file(project_root, None))
}

fn describe_role_tools(registry: &RoleRegistry) -> Vec<RoleTools> {
    let native = harness_core::native::native_roles(registry);
    registry
        .roles
        .iter()
        .map(|(name, role)| RoleTools {
            name: name.clone(),
            provider: role.provider.as_str().to_string(),
            isolation: role.isolation.as_str().to_string(),
            tools: role.tools.clone(),
            native: native.contains_key(name),
        })
        .collect()
}

#[tauri::command]
async fn role_tools(
    state: State<'_, AppState>,
    project_root: String,
) -> Result<Vec<RoleTools>, String> {
    let file = roles_file_for(&state, &project_root).await;
    let registry = RoleRegistry::load(&file).map_err(describe_error)?;
    Ok(describe_role_tools(&registry))
}

/// Rewrite one role's allow-list in `roles.toml`, and apply it to the open session.
#[tauri::command]
async fn save_role_tools(
    state: State<'_, AppState>,
    project_root: String,
    role: String,
    tools: Vec<String>,
) -> Result<Vec<RoleTools>, String> {
    let file = roles_file_for(&state, &project_root).await;
    let before = RoleRegistry::load(&file).map_err(describe_error)?;
    harness_core::roles_patch::apply_tools_patch_file(&file, &role, &tools)
        .map_err(describe_error)?;
    let updated = RoleRegistry::load(&file).map_err(describe_error)?;

    let mut projects = state.projects.lock().await;
    if let Some(session) = projects.get_mut(&project_key(&project_root)) {
        session.harness.swap_registry(updated.clone()).await;
        // Delegated workers read the live fleet. A native role's definition was fixed
        // when the head agent started, so route it through `delegate` until then.
        if harness_core::native::native_roles(&before).contains_key(&role) {
            if let Head::Live(orchestrator) = &mut session.head {
                let notice = format!(
                    "The tools for `{role}` changed. Your Agent-tool definition for it is fixed \
                     for this session, so until the project is reopened delegate to `{role}` \
                     with the `delegate` tool instead."
                );
                if let Err(error) = orchestrator.send(&notice).await {
                    tracing::warn!("could not notify the head agent of the tools change: {error:#}");
                }
            }
        }
    }
    Ok(describe_role_tools(&updated))
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness_core=info,harness_app_lib=info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            app.manage(AppState::default());
            // Older plugin builds are safe to drop only now, before any worker can be
            // reading one.
            if let Ok(ext) = extensions() {
                let current = current_extras().plugin_dir;
                ext.prune_plugins(current.as_deref());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            inspect_project,
            write_default_roles,
            inspect_fleet,
            save_role_assignments,
            session_roles,
            list_projects,
            stop_turn,
            stop_worker,
            chat_history,
            quotas,
            focus_project,
            close_project,
            start_session,
            send_turn,
            list_workers,
            worker_patch,
            pending_merges,
            approve_merge,
            reject_merge,
            usage,
            probe_preview_servers,
            create_preview,
            set_preview_bounds,
            set_preview_visible,
            navigate_preview,
            reload_preview,
            stop_session,
            get_settings,
            save_settings,
            list_skills,
            set_skill_enabled,
            create_skill,
            import_skills_folder,
            import_skills_git,
            remove_skill,
            list_mcp_servers,
            set_mcp_server,
            remove_mcp_server,
            role_tools,
            save_role_tools,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the harness app");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_is_keyed_by_its_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("nested")).unwrap();

        let direct = project_key(&root.join("nested").display().to_string());
        let roundabout = project_key(&root.join("nested/../nested").display().to_string());

        // Two spellings of one project must not open two sessions racing over the same
        // worktrees and the same database.
        assert_eq!(direct, roundabout);
    }

    #[test]
    fn an_unopenable_path_still_produces_a_stable_key() {
        // Canonicalize fails on a path that does not exist. The key still has to be
        // deterministic, or the error surfaced to the user changes between calls.
        let key = project_key("/definitely/not/here");
        assert_eq!(key, project_key("/definitely/not/here"));
        assert!(key.contains("not/here"));
    }

    #[test]
    fn events_are_tagged_with_their_project_without_disturbing_the_event() {
        let event = HarnessEvent::WorkerStatusChanged {
            worker_id: "w-1".into(),
            status: harness_core::event::WorkerStatus::Running,
        };
        let json = serde_json::to_value(ProjectEvent {
            project: "/tmp/demo",
            event: &event,
        })
        .unwrap();

        assert_eq!(json["project"], "/tmp/demo");
        // Flattened: the discriminator and payload must be unchanged, or every existing
        // handler in the UI stops matching.
        assert_eq!(json["type"], "worker_status_changed");
        assert_eq!(json["worker_id"], "w-1");
        assert_eq!(json["status"], "running");
    }
}
