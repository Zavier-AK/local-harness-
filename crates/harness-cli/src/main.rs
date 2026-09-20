//! Headless driver for the harness engine.
//!
//! Exists so the whole orchestration loop can be exercised without a desktop: the Tauri
//! app is a second front end over the identical engine, not a separate implementation.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use harness_core::engine::Harness;
use harness_core::event::HarnessEvent;
use harness_core::isolation::Workspaces;
use harness_core::mcp;
use harness_core::orchestrator::Orchestrator;
use harness_core::roles::RoleRegistry;
use harness_core::store::Store;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

/// How long a single orchestrator turn may run before the driver gives up on it.
const TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Parser)]
#[command(name = "harness", about = "Hierarchical multi-agent harness over subscription CLIs")]
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
        HarnessEvent::SessionStarted { provider, model, mcp_servers, .. } => {
            end_stream(streaming);
            println!(
                "  · session up (provider: {}, model: {}, mcp: [{}])",
                provider.as_deref().unwrap_or("?"),
                model.as_deref().unwrap_or("?"),
                mcp_servers.join(", ")
            );
        }
        HarnessEvent::AssistantText { text, partial: true, .. } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
            *streaming = true;
        }
        HarnessEvent::AssistantText { text, partial: false, .. } => {
            // Already shown as deltas when partial messages are enabled.
            if !*streaming {
                println!("{text}");
            }
        }
        HarnessEvent::ToolCall { name, .. } => {
            end_stream(streaming);
            println!("  → {name}");
        }
        HarnessEvent::WorkerSpawned { worker_id, role, provider, isolation, .. } => {
            end_stream(streaming);
            println!("  ⚙ worker {worker_id} [{role} via {provider}, {isolation}]");
        }
        HarnessEvent::WorkerFinished { worker_id, usage, diff, is_error, .. } => {
            end_stream(streaming);
            let files = diff.as_ref().map(|d| d.files_changed).unwrap_or(0);
            println!(
                "  ✓ worker {worker_id} {} ({} in / {} out, {files} files)",
                if *is_error { "FAILED" } else { "done" },
                usage.total_input(),
                usage.output_tokens
            );
        }
        HarnessEvent::MergeRequested { worker_id, branch, diff } => {
            end_stream(streaming);
            println!(
                "  ⏸ merge proposed for {worker_id} on {branch}: {} files, +{} -{} — awaiting approval",
                diff.files_changed, diff.insertions, diff.deletions
            );
        }
        HarnessEvent::ApiRetry { attempt, max_retries, error, retry_delay_ms, .. } => {
            end_stream(streaming);
            println!("  ! {error} (attempt {attempt}/{max_retries}, retrying in {retry_delay_ms}ms)");
        }
        HarnessEvent::RunFinished { usage, cost_usd, is_error, .. } => {
            end_stream(streaming);
            println!(
                "  · turn finished{} — {} in ({} cache-create, {} cache-read) / {} out{}",
                if *is_error { " WITH ERROR" } else { "" },
                usage.total_input(),
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
                usage.output_tokens,
                cost_usd.map(|c| format!(", notional ${c:.4}")).unwrap_or_default()
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
fn spawn_renderer(
    mut events: UnboundedReceiver<HarnessEvent>,
    harness: Arc<Harness>,
    turn_done: Option<tokio::sync::mpsc::UnboundedSender<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut streaming = false;
        while let Some(event) = events.recv().await {
            harness.note_event(&event).await;
            render(&event, &mut streaming);

            if let (HarnessEvent::RunFinished { .. }, Some(done)) = (&event, &turn_done) {
                let _ = done.send(());
            }
        }
    })
}

fn build_harness(cli: &Cli, events: harness_core::agents::EventSink) -> Result<Arc<Harness>> {
    let registry = RoleRegistry::load(&cli.roles)
        .with_context(|| format!("loading roles from {}", cli.roles.display()))?;

    let store = match &cli.db {
        Some(path) => Store::open(path)?,
        None => Store::in_memory()?,
    };

    let project = cli.project.canonicalize().unwrap_or_else(|_| cli.project.clone());
    let session_id = format!("s-{}", uuid_like());
    store.create_session(&session_id, None, &project.display().to_string())?;

    Ok(Arc::new(Harness::new(
        registry,
        Workspaces::new(project),
        store,
        session_id,
        events,
    )))
}

/// Small unique id without pulling uuid into this crate's surface.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
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
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let harness = build_harness(&cli, tx)?;

    match &cli.command {
        Command::Roles => {
            drop(rx);
            for role in harness.list_roles() {
                println!(
                    "{:<16} {:<14} {:<10} {}",
                    role.name,
                    format!("{}{}", role.provider, role.model.as_ref().map(|m| format!("/{m}")).unwrap_or_default()),
                    role.isolation,
                    if role.can_edit_files { "can edit" } else { "read-only" }
                );
            }
        }

        Command::RunWorker { role, task, context_file } => {
            let renderer = spawn_renderer(rx, Arc::clone(&harness), None);
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
            }
            drop(harness);
            let _ = renderer.await;
        }

        Command::Chat { turns, model, max_turns } => {
            // The orchestrator's own stream is separate from the worker stream; merge
            // them onto one renderer so the transcript reads in order.
            let worker_events = rx;
            let (orchestrator, orchestrator_events) = Orchestrator::start(
                Arc::clone(&harness),
                &cli.project,
                model.clone(),
                Some(*max_turns),
            )
            .await?;

            println!("MCP server: {}", orchestrator.mcp_url());

            let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
            let renderer = spawn_renderer(worker_events, Arc::clone(&harness), None);
            let orchestrator_renderer =
                spawn_renderer(orchestrator_events, Arc::clone(&harness), Some(done_tx));

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

            orchestrator.shutdown().await?;
            let _ = orchestrator_renderer.await;
            drop(harness);
            let _ = renderer.await;
        }

        Command::McpServe { bind } => {
            let server = mcp::serve_on(Arc::clone(&harness), bind.parse()?).await?;
            println!("url:   {}", server.url());
            println!("token: {}", server.token);
            println!("\nmcp-config:\n{}", server.claude_mcp_config());
            println!("\nCtrl-C to stop.");

            let renderer = spawn_renderer(rx, Arc::clone(&harness), None);
            tokio::signal::ctrl_c().await?;
            server.shutdown().await;
            drop(harness);
            let _ = renderer.await;
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
