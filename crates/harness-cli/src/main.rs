//! Headless driver for the harness engine.
//!
//! Exists so the whole orchestration loop can be exercised without a desktop: the Tauri
//! app is a second front end over the identical engine, not a separate implementation.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use harness_core::engine::Harness;
use harness_core::engine::RoleInfo as RoleRow;
use harness_core::event::HarnessEvent;
use harness_core::isolation::Workspaces;
use harness_core::mcp;
use harness_core::orchestrator::Orchestrator;
use harness_core::roles::RoleRegistry;
use harness_core::store::Store;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio::sync::mpsc::UnboundedReceiver;

/// How long a single orchestrator turn may run before the driver gives up on it.
const TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How long to wait for the event stream to close during shutdown.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "harness",
    about = "Hierarchical multi-agent harness over subscription CLIs"
)]
struct Cli {
    /// Project root. Workers' worktrees are created beneath it.
    #[arg(long, global = true, default_value = ".")]
    project: PathBuf,

    /// Role registry.
    #[arg(long, global = true, default_value = "roles.toml")]
    roles: PathBuf,

    /// Where to keep the session database. Defaults to in-memory.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Skills and MCP servers for Claude workers. Defaults to the desktop app's, so
    /// both use one library.
    #[arg(long, global = true)]
    extensions: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the fleet.
    Roles,

    /// Run a single worker directly, bypassing the orchestrator.
    RunWorker {
        role: String,
        task: String,
        #[arg(long)]
        context_file: Vec<String>,

        /// Print the worker's diff, not just its file list.
        #[arg(long)]
        patch: bool,
    },

    /// Start the head chat: a long-lived Claude session with delegation tools attached.
    Chat {
        /// Turns to send. Several turns reuse one process, which is the whole point —
        /// the context floor is paid once, not per turn.
        #[arg(required = true)]
        turns: Vec<String>,

        #[arg(long)]
        model: Option<String>,

        #[arg(long, default_value = "100")]
        max_turns: u32,
    },

    /// Run only the MCP server, so the tool surface can be inspected with curl.
    McpServe {
        /// Bind address. Defaults to an ephemeral loopback port.
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: String,
    },

    /// Answer a Claude Code worktree hook. Run by Claude Code itself, not by people.
    #[command(hide = true)]
    Hook { which: String },

    /// Token totals for the rolling window that governs a subscription.
    Usage {
        #[arg(long, default_value = "5")]
        hours: i64,
    },
}

/// Render events as they arrive. Partial deltas stream inline; everything else is a line.
fn render(event: &HarnessEvent, streaming: &mut bool) {
    use std::io::Write;

    // Close off an in-progress streamed line before printing anything structured.
    let end_stream = |streaming: &mut bool| {
        if *streaming {
            println!();
            *streaming = false;
        }
    };

    match event {
        HarnessEvent::SessionStarted {
            provider,
            model,
            mcp_servers,
            ..
        } => {
            end_stream(streaming);
            println!(
                "  · session up (provider: {}, model: {}, mcp: [{}])",
                provider.as_deref().unwrap_or("?"),
                model.as_deref().unwrap_or("?"),
                mcp_servers.join(", ")
            );
        }
        HarnessEvent::AssistantText {
            text,
            partial: true,
            ..
        } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
            *streaming = true;
        }
        HarnessEvent::AssistantText {
            text,
            partial: false,
            ..
        } => {
            // Already shown as deltas when partial messages are enabled.
            if !*streaming {
                println!("{text}");
            }
        }
        HarnessEvent::ToolCall { name, .. } => {
            end_stream(streaming);
            println!("  → {name}");
        }
        HarnessEvent::WorkerSpawned {
            worker_id,
            role,
            provider,
            isolation,
            ..
        } => {
            end_stream(streaming);
            println!("  ⚙ worker {worker_id} [{role} via {provider}, {isolation}]");
        }
        HarnessEvent::WorkerFinished {
            worker_id,
            usage,
            diff,
            is_error,
            ..
        } => {
            end_stream(streaming);
            let files = diff.as_ref().map(|d| d.files_changed).unwrap_or(0);
            println!(
                "  ✓ worker {worker_id} {} ({} in / {} out, {files} files)",
                if *is_error { "FAILED" } else { "done" },
                usage.total_input(),
                usage.output_tokens
            );
        }
        HarnessEvent::MergeRequested {
            worker_id,
            branch,
            diff,
        } => {
            end_stream(streaming);
            println!(
                "  ⏸ merge proposed for {worker_id} on {branch}: {} files, +{} -{} — awaiting approval",
                diff.files_changed, diff.insertions, diff.deletions
            );
        }
        HarnessEvent::VerificationCheck { worker_id, check } => {
            end_stream(streaming);
            println!("  ✔︎ check on {worker_id}: {} — {:?}: {}", check.name, check.status, check.summary);
        }
        HarnessEvent::VerificationFinished { worker_id, report } => {
            end_stream(streaming);
            println!(
                "  ◆ {worker_id}: {} risk{}{}",
                report.risk.as_str(),
                if report.verified { "" } else { ", unverified" },
                if report.reasons.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", report.reasons.join("; "))
                }
            );
        }
        HarnessEvent::ApiRetry {
            attempt,
            max_retries,
            error,
            retry_delay_ms,
            ..
        } => {
            end_stream(streaming);
            println!(
                "  ! {error} (attempt {attempt}/{max_retries}, retrying in {retry_delay_ms}ms)"
            );
        }
        HarnessEvent::RunFinished {
            usage,
            cost_usd,
            is_error,
            ..
        } => {
            end_stream(streaming);
            println!(
                "  · turn finished{} — {} in ({} cache-create, {} cache-read) / {} out{}",
                if *is_error { " WITH ERROR" } else { "" },
                usage.total_input(),
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
                usage.output_tokens,
                cost_usd
                    .map(|c| format!(", notional ${c:.4}"))
                    .unwrap_or_default()
            );
        }
        HarnessEvent::Error { message, .. } => {
            end_stream(streaming);
            eprintln!("  ✗ {message}");
        }
        _ => {}
    }
}

/// Drain events until the channel closes, feeding them back to the engine for
/// rate-limit backpressure.
///
/// `turn_done`, when provided, is signalled on every `RunFinished` so a caller can wait
/// for a turn to settle instead of guessing with a sleep.
///
/// The engine is held by [`Weak`] on purpose. `Harness` owns the event sender, so a
/// strong reference here would keep the channel open forever and this task would never
/// see the end of the stream — the caller's `drop(harness)` has to be able to actually
/// drop it.
fn spawn_renderer(
    mut events: UnboundedReceiver<HarnessEvent>,
    harness: Weak<Harness>,
    turn_done: Option<tokio::sync::mpsc::UnboundedSender<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut streaming = false;
        while let Some(event) = events.recv().await {
            // Backpressure only matters while the engine is still alive; rendering the
            // tail of the stream after it is gone is still worth doing.
            if let Some(harness) = harness.upgrade() {
                harness.note_event(&event).await;
            }
            render(&event, &mut streaming);

            if let (HarnessEvent::RunFinished { .. }, Some(done)) = (&event, &turn_done) {
                let _ = done.send(());
            }
        }
    })
}

/// Wait for a renderer to drain, without letting a missed drop hang the process.
async fn drain(renderer: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(DRAIN_TIMEOUT, renderer).await.is_err() {
        tracing::warn!("event stream did not close within {DRAIN_TIMEOUT:?}; exiting anyway");
    }
}

async fn build_harness(cli: &Cli, events: harness_core::agents::EventSink) -> Result<Arc<Harness>> {
    let registry = RoleRegistry::load(&cli.roles)
        .with_context(|| format!("loading roles from {}", cli.roles.display()))?;

    let store = match &cli.db {
        Some(path) => Store::open(path)?,
        None => Store::in_memory()?,
    };

    let project = cli
        .project
        .canonicalize()
        .unwrap_or_else(|_| cli.project.clone());
    let session_id = format!("s-{}", uuid_like());
    store.create_session(&session_id, None, &project.display().to_string())?;

    let workspaces = Workspaces::with_setup(project.clone(), registry.worktree.clone());
    // Worker worktrees live under the project; keep them out of its `git status`.
    if let Err(error) = workspaces.ensure_git_exclude().await {
        tracing::warn!("could not update .git/info/exclude: {error:#}");
    }

    let harness = Arc::new(Harness::new(registry, workspaces, store, session_id, events));

    // Skills and MCP servers are an addition, never a reason not to run.
    if let Some(dir) = cli.extensions.clone().or_else(harness_core::extensions::default_dir) {
        match harness_core::extensions::Extensions::new(dir).worker_extras() {
            Ok(extras) => harness.set_extras(extras).await,
            Err(error) => tracing::warn!("skills and MCP servers not loaded: {error:#}"),
        }
    }
    Ok(harness)
}

/// Small unique id without pulling uuid into this crate's surface.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness_core=info,harness_cli=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Hooks run before anything else: Claude Code is waiting on stdout for a path, and
    // building a harness here would load a roles file and open a database for nothing.
    if let Command::Hook { which } = &cli.command {
        use std::io::Read;
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        match which.as_str() {
            "worktree-create" => {
                let path = harness_core::hooks::worktree_create(&input).await?;
                println!("{}", path.display());
            }
            "worktree-remove" => harness_core::hooks::worktree_remove(&input).await?,
            other => anyhow::bail!("unknown hook `{other}`"),
        }
        return Ok(());
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let harness = build_harness(&cli, tx).await?;

    match &cli.command {
        Command::Hook { .. } => unreachable!("handled before the harness is built"),

        Command::Roles => {
            drop(rx);
            let roles = harness.list_roles_probed().await;

            let rows: Vec<(String, String, &RoleRow)> = roles
                .iter()
                .map(|role| {
                    let backend = format!(
                        "{}{}",
                        role.provider,
                        role.model
                            .as_ref()
                            .map(|m| format!("/{m}"))
                            .unwrap_or_default()
                    );
                    (role.name.clone(), backend, role)
                })
                .collect();

            // Size both columns to their contents; a long provider/model pair such as
            // `openai_compat/qwen3.6-35b-a3b` otherwise shunts every later column right.
            let name_w = rows
                .iter()
                .map(|(n, _, _)| n.len())
                .max()
                .unwrap_or(8)
                .max(8);
            let backend_w = rows
                .iter()
                .map(|(_, b, _)| b.len())
                .max()
                .unwrap_or(10)
                .max(10);

            for (name, backend, role) in &rows {
                println!(
                    "{name:<name_w$}  {backend:<backend_w$}  {:<9} {:<10} {}",
                    role.isolation,
                    if role.can_edit_files {
                        "can edit"
                    } else {
                        "read-only"
                    },
                    if role.available == Some(false) {
                        "UNAVAILABLE"
                    } else {
                        "ready"
                    },
                );
            }

            // The reasons are the actionable part: each one names what to install or start.
            let blocked: Vec<_> = roles
                .iter()
                .filter(|r| r.available == Some(false))
                .collect();
            if !blocked.is_empty() {
                println!("\n{} role(s) cannot run:", blocked.len());
                for role in blocked {
                    println!(
                        "  {}: {}",
                        role.name,
                        role.unavailable_reason
                            .as_deref()
                            .unwrap_or("backend not reachable")
                    );
                }
            }
        }

        Command::RunWorker {
            role,
            task,
            context_file,
            patch,
        } => {
            let renderer = spawn_renderer(rx, Arc::downgrade(&harness), None);
            let record = harness.delegate(role, task, context_file.clone()).await?;

            println!("\n--- result ---\n{}", record.summary);
            if let Some(diff) = &record.diff {
                println!(
                    "\n{} files changed (+{} -{}) on {}",
                    diff.files_changed,
                    diff.insertions,
                    diff.deletions,
                    record.branch.as_deref().unwrap_or("(no branch)")
                );

                if *patch {
                    match harness.worker_patch(&record.id, 2_000).await {
                        Ok(p) => {
                            println!("\n--- diff ---\n{}", p.text);
                            if p.truncated {
                                println!("\n… truncated; {} lines total", p.total_lines);
                            }
                        }
                        Err(err) => eprintln!("could not read the diff: {err:#}"),
                    }
                } else if record.branch.is_some() {
                    println!("(re-run with --patch to see the diff)");
                }
            }
            drop(harness);
            drain(renderer).await;
        }

        Command::Chat {
            turns,
            model,
            max_turns,
        } => {
            // The orchestrator's own stream is separate from the worker stream; merge
            // them onto one renderer so the transcript reads in order.
            let worker_events = rx;
            let (orchestrator, orchestrator_events) = Orchestrator::start(
                Arc::clone(&harness),
                &cli.project,
                model.clone(),
                Some(*max_turns),
                None,
                // This same binary answers Claude Code's worktree hook.
                std::env::current_exe()
                    .ok()
                    .map(|exe| vec![exe.display().to_string(), "hook".to_string()]),
            )
            .await?;

            println!("MCP server: {}", orchestrator.mcp_url());

            let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
            let renderer = spawn_renderer(worker_events, Arc::downgrade(&harness), None);
            let orchestrator_renderer =
                spawn_renderer(orchestrator_events, Arc::downgrade(&harness), Some(done_tx));

            let mut orchestrator = orchestrator;
            for (index, turn) in turns.iter().enumerate() {
                println!("\n=== turn {} ===\n> {turn}", index + 1);
                orchestrator.send(turn).await?;

                // Wait for this turn's result line before sending the next, so turns do
                // not interleave. The cap keeps a hung backend from wedging the run.
                match tokio::time::timeout(TURN_TIMEOUT, done_rx.recv()).await {
                    Ok(Some(())) => {}
                    Ok(None) => {
                        eprintln!("orchestrator stream closed; stopping");
                        break;
                    }
                    Err(_) => {
                        eprintln!("turn did not finish within {TURN_TIMEOUT:?}; stopping");
                        break;
                    }
                }
            }

            // Workers — native subagents especially — can still be running when the last
            // turn ends, and checks on a proposed merge run in the background after them.
            // Exiting now would cut both off and leave the verdict unprinted.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
            while std::time::Instant::now() < deadline {
                let working = harness.workers().await.iter().any(|w| !w.status.is_terminal());
                if !working && harness.verifications_running().await == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            orchestrator.shutdown().await?;
            drain(orchestrator_renderer).await;
            drop(harness);
            drain(renderer).await;
        }

        Command::McpServe { bind } => {
            let server = mcp::serve_on(Arc::clone(&harness), bind.parse()?).await?;
            println!("url:   {}", server.url());
            println!("token: {}", server.token);
            println!("\nmcp-config:\n{}", server.claude_mcp_config());
            println!("\nCtrl-C to stop.");

            let renderer = spawn_renderer(rx, Arc::downgrade(&harness), None);
            tokio::signal::ctrl_c().await?;
            server.shutdown().await;
            drop(harness);
            drain(renderer).await;
        }

        Command::Usage { hours } => {
            drop(rx);
            let rows = harness.usage_window(hours * 3600).await?;
            if rows.is_empty() {
                println!("no usage recorded in the last {hours}h");
            }
            for row in rows {
                println!(
                    "{:<16} {:>8} in ({:>7} cache-create, {:>7} cache-read) {:>8} out  {:>4} runs",
                    row.provider,
                    row.usage.total_input(),
                    row.usage.cache_creation_input_tokens,
                    row.usage.cache_read_input_tokens,
                    row.usage.output_tokens,
                    row.runs
                );
            }
        }
    }

    Ok(())
}
