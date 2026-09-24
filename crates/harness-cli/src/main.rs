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

    /// How much runs without you: ask, review, land-safe or land-most. Headless, so under
    /// `ask` delegations are declined — there is no one here to approve them.
    #[arg(long, global = true, default_value = "review", value_parser = parse_autonomy)]
    autonomy: harness_core::autonomy::Autonomy,

    /// Run plans the head agent proposes, unedited. Headless, there is no board to
    /// review them on; without this they are printed and left alone.
    #[arg(long, global = true)]
    run_plans: bool,

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

    /// Run a night shift: try one change at a time on its own branch, keep what scores
    /// better, and throw the rest away. Ctrl-C stops it; what was kept stays kept.
    Night {
        /// What to improve, in plain words.
        goal: String,

        /// Prints the score; the last number in its output counts.
        #[arg(long)]
        metric: String,

        /// Lower scores are better (the default is higher).
        #[arg(long)]
        lower: bool,

        /// Must keep passing for a change to be kept, e.g. the test suite.
        #[arg(long)]
        guard: Option<String>,

        /// The role that makes each change. Needs worktree isolation.
        #[arg(long, default_value = "builder")]
        role: String,

        #[arg(long, default_value = "20")]
        experiments: u32,

        #[arg(long, default_value = "8")]
        hours: f64,

        /// Seconds the metric and the guard may each run.
        #[arg(long, default_value = "900")]
        timeout: u64,

        /// Put the kept work up for review at the end, as an ordinary merge proposal.
        #[arg(long)]
        propose: bool,
    },

    /// What spoken words would make the harness do, without doing it — against the fixed
    /// example harness in `voice/phrases.toml`. `--eval` runs the whole labelled set.
    Voice {
        /// The words, as Whisper would write them.
        words: Option<String>,

        /// Run the labelled phrase set and report how often each step is right, wrong, or
        /// unsure, and how fast Laya is.
        #[arg(long)]
        eval: bool,

        /// Also ask Laya. Loads it first — the first time, that downloads about 1.7 GB.
        #[arg(long)]
        laya: bool,

        /// The confidence Laya must reach before anything is done on its word.
        #[arg(long, default_value = "0.75")]
        threshold: f64,

        /// The Laya helper script. Defaults to the one in this source tree.
        #[arg(long)]
        sidecar: Option<PathBuf>,

        /// Hand the words to the voice agent (a real Claude, Haiku by default), which
        /// plans the steps. Each step is printed, not done.
        #[arg(long)]
        agent: bool,

        #[arg(long, default_value = "haiku")]
        agent_model: String,
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
        HarnessEvent::PlanUpdated { plan } => {
            end_stream(streaming);
            let lanes: Vec<String> = plan
                .steps
                .iter()
                .map(|s| {
                    let state = serde_json::to_value(s.state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    if s.input.depends_on.is_empty() {
                        format!("{}={state}", s.input.id)
                    } else {
                        format!("{}(after {})={state}", s.input.id, s.input.depends_on.join(","))
                    }
                })
                .collect();
            println!("  ▦ plan \"{}\" [{:?}]: {}", plan.title, plan.status, lanes.join(" "));
        }
        HarnessEvent::PlanFinished { title, outcome, .. } => {
            end_stream(streaming);
            println!("  ▦ plan \"{title}\" finished — {outcome}");
        }
        HarnessEvent::DelegationRequested { worker_id, role, task } => {
            end_stream(streaming);
            println!("  ? {worker_id} [{role}] awaits approval: {}", task.lines().next().unwrap_or_default());
        }
        HarnessEvent::DelegationDeclined { worker_id, reason } => {
            end_stream(streaming);
            println!("  ⊘ {worker_id} declined: {reason}");
        }
        HarnessEvent::MergeLanded { worker_id, branch, commit, automatic, .. } => {
            end_stream(streaming);
            let how = if *automatic { "landed automatically" } else { "merged" };
            println!("  ⇲ {worker_id} {how}: {branch} at {}", &commit[..commit.len().min(10)]);
        }
        HarnessEvent::MergeNotLanded { worker_id, reason } => {
            end_stream(streaming);
            println!("  ⏸ {worker_id} could not land by itself, waiting for review: {reason}");
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
        // Each publish of a night's report adds exactly one thing; print that thing.
        HarnessEvent::NightUpdated { report } => {
            use harness_core::night::NightStatus;
            end_stream(streaming);
            if let Some(worker) = &report.proposed_as {
                println!("  ☾ proposed for review as {worker}");
            } else if report.status != NightStatus::Running {
                println!(
                    "  ☾ night shift {}: {} — {}",
                    if report.status == NightStatus::Stopped { "stopped" } else { "finished" },
                    report.ended_because.as_deref().unwrap_or("done"),
                    report.headline()
                );
            } else if let Some(e) = report.experiments.last() {
                println!(
                    "  ☾ #{} {} {}: {} — {}",
                    e.n,
                    if e.kept { "kept" } else { "thrown away" },
                    e.score.map(|s| s.to_string()).unwrap_or_else(|| "–".into()),
                    e.summary.lines().next().unwrap_or("(no summary)"),
                    e.reason
                );
            } else if let Some(base) = report.baseline {
                println!("  ☾ starting score {base}");
            } else {
                println!("  ☾ night shift on {}: {}", report.branch, report.config.goal);
            }
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
    run_plans: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut streaming = false;
        while let Some(event) = events.recv().await {
            // Backpressure only matters while the engine is still alive; rendering the
            // tail of the stream after it is gone is still worth doing.
            if let Some(harness) = harness.upgrade() {
                harness.note_event(&event).await;
                if let HarnessEvent::PlanUpdated { plan } = &event {
                    if run_plans && plan.status == harness_core::plan::PlanStatus::Draft {
                        if let Err(err) = harness.run_plan(&plan.id, None).await {
                            eprintln!("  ✗ could not run the plan: {err:#}");
                        }
                    }
                }
                // Headless: there is nobody to approve a delegation, so under `Ask` say so
                // and decline it rather than leave the head agent waiting forever.
                if let HarnessEvent::DelegationRequested { worker_id, .. } = &event {
                    let _ = harness
                        .decline_delegation(
                            worker_id,
                            "the CLI cannot ask for approval; run with --autonomy review, or use the app",
                        )
                        .await;
                }
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
    harness.set_autonomy(cli.autonomy).await;

    // Skills and MCP servers are an addition, never a reason not to run.
    if let Some(dir) = cli.extensions.clone().or_else(harness_core::extensions::default_dir) {
        match harness_core::extensions::Extensions::new(dir).worker_extras() {
            Ok(extras) => harness.set_extras(extras).await,
            Err(error) => tracing::warn!("skills and MCP servers not loaded: {error:#}"),
        }
    }
    Ok(harness)
}

/// The voice agent against the example harness, with hands that only say what they would do.
async fn voice_agent(words: Option<&str>, model: &str) -> Result<()> {
    use futures::future::BoxFuture;
    use harness_core::voice::{agent, eval as ev, Snapshot, VoiceAction};

    struct DryRun;
    impl agent::Hands for DryRun {
        fn snapshot(&self) -> BoxFuture<'static, Snapshot> {
            Box::pin(async { ev::fixture() })
        }
        fn perform(
            &self,
            action: VoiceAction,
            confirm: bool,
            describe: String,
        ) -> BoxFuture<'static, Result<String, String>> {
            let json = serde_json::to_string(&action).unwrap_or_default();
            println!(
                "  → {describe}{}   {json}",
                if confirm {
                    "  (asks for a yes first)"
                } else {
                    ""
                }
            );
            Box::pin(async { Ok(String::new()) })
        }
        fn chat_box(&self, text: String) -> BoxFuture<'static, ()> {
            println!("  → chat box: {text}");
            Box::pin(async {})
        }
        fn step(&self, _: String) {}
    }

    let words = words.context("say something: `harness-cli voice --agent \"…\"`")?;
    let cwd = std::env::temp_dir().join("harness-voice-agent");
    let started = std::time::Instant::now();
    let mut voice_agent =
        agent::VoiceAgent::start(std::sync::Arc::new(DryRun), model, &cwd).await?;
    eprintln!("  (agent up in {:.1}s)", started.elapsed().as_secs_f64());
    let snapshot = ev::fixture();
    for said in words.split(" || ") {
        println!("\n“{said}”");
        let reply = voice_agent
            .ask(said, &snapshot, std::time::Duration::from_secs(90))
            .await?;
        println!(
            "  says: {}\n  ({} steps, {:.1}s)",
            reply.text,
            reply.steps,
            reply.ms as f64 / 1000.0
        );
    }
    voice_agent.shutdown().await;
    println!("\n(against the example harness in voice/phrases.toml; nothing was done)");
    Ok(())
}

async fn voice(
    words: Option<&str>,
    eval: bool,
    use_laya: bool,
    threshold: f64,
    sidecar: Option<PathBuf>,
) -> Result<()> {
    use harness_core::voice::{self, eval as ev, laya};

    let client = if use_laya {
        let config = laya::LayaConfig::new(sidecar.unwrap_or_else(laya::LayaConfig::bundled_script));
        let client = laya::LayaClient::new(config);
        let mut progress = client.subscribe();
        let watcher = tokio::spawn(async move {
            while progress.changed().await.is_ok() {
                if let laya::LayaState::Loading { file: Some(file), received, total: Some(total) } =
                    progress.borrow().clone()
                {
                    eprint!("\r  loading Laya: {file} {:.0}%   ", received as f64 / total.max(1) as f64 * 100.0);
                }
            }
        });
        let started = std::time::Instant::now();
        client.load().await.context("loading Laya")?;
        watcher.abort();
        eprintln!("\r  Laya ready in {:.1}s                          ", started.elapsed().as_secs_f64());
        Some(client)
    } else {
        None
    };

    if eval {
        let report = ev::run(client.as_ref(), threshold).await;
        println!("{} phrases\n", report.phrases);
        let line = |name: &str, t: &ev::Tally| {
            println!("{name:<28} right {:>3}   wrong {:>3}   unsure {:>3}", t.right, t.wrong, t.unsure)
        };
        line("matcher", &report.matcher);
        if client.is_some() {
            if let Some(error) = &report.laya_error {
                println!("Laya failed: {error}");
            }
            for (t, tally) in &report.laya {
                line(&format!("Laya alone at {t:.2}"), tally);
            }
            if let Some((p50, p95)) = report.latency() {
                println!("\nLaya per decision: {p50} ms median, {p95} ms p95");
            }
            match report.suggested_threshold {
                Some(t) => println!("never wrong on this set from a threshold of {t:.2}"),
                None => println!("wrong at every threshold tried; keep the matcher and the head agent"),
            }
        }
        println!();
        line(&format!("all together at {threshold:.2}"), &report.combined);
        let misses: Vec<&ev::Row> = report.rows.iter().filter(|r| r.verdict != ev::Verdict::Right).collect();
        if !misses.is_empty() {
            println!("\nnot right:");
            for row in misses {
                let laya = row
                    .laya
                    .as_ref()
                    .map(|(got, c)| format!("  (Laya: {got}{})", c.map(|c| format!(" {c:.2}")).unwrap_or_default()))
                    .unwrap_or_default();
                println!(
                    "  {:<6} {:?} → {} (expected {}){laya}",
                    if row.verdict == ev::Verdict::Wrong { "WRONG" } else { "unsure" },
                    row.said,
                    row.combined,
                    row.expect
                );
            }
        }
        return Ok(());
    }

    let words = words.context("say something: `harness-cli voice \"what's waiting for me\"`, or --eval")?;
    let snapshot = ev::fixture();
    // Several commands in one breath are read one by one, as the app does.
    let parts = voice::everyday::split_commands(words, &snapshot).unwrap_or_else(|| vec![words.to_string()]);
    for part in parts {
        let heard = voice::interpret(&part, &snapshot, client.as_ref(), threshold, None).await;
        println!("{}", serde_json::to_string_pretty(&heard)?);
    }
    println!("\n(against the example harness in voice/phrases.toml; nothing was done)");
    Ok(())
}

fn parse_autonomy(text: &str) -> Result<harness_core::autonomy::Autonomy, String> {
    harness_core::autonomy::Autonomy::parse(text)
        .ok_or_else(|| format!("`{text}` is not one of ask, review, land-safe, land-most"))
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
        if let Some(out) = harness_core::hooks::run(which, &input).await? {
            println!("{out}");
        }
        return Ok(());
    }

    // Voice needs no project: it reads words against a fixed example harness.
    if let Command::Voice {
        words,
        eval,
        laya,
        threshold,
        sidecar,
        agent,
        agent_model,
    } = &cli.command
    {
        if *agent {
            return voice_agent(words.as_deref(), agent_model).await;
        }
        return voice(words.as_deref(), *eval, *laya, *threshold, sidecar.clone()).await;
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let harness = build_harness(&cli, tx).await?;

    match &cli.command {
        Command::Hook { .. } | Command::Voice { .. } => {
            unreachable!("handled before the harness is built")
        }

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
            let renderer = spawn_renderer(rx, Arc::downgrade(&harness), None, false);
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
            let renderer = spawn_renderer(worker_events, Arc::downgrade(&harness), None, cli.run_plans);
            let orchestrator_renderer =
                spawn_renderer(orchestrator_events, Arc::downgrade(&harness), Some(done_tx), cli.run_plans);

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
                let working = harness.workers().await.iter().any(|w| !w.status.is_terminal())
                    || harness.plans_running().await;
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

            let renderer = spawn_renderer(rx, Arc::downgrade(&harness), None, false);
            tokio::signal::ctrl_c().await?;
            server.shutdown().await;
            drop(harness);
            drain(renderer).await;
        }

        Command::Night {
            goal,
            metric,
            lower,
            guard,
            role,
            experiments,
            hours,
            timeout,
            propose,
        } => {
            use harness_core::night::{Direction, NightConfig, NightStatus};
            let renderer = spawn_renderer(rx, Arc::downgrade(&harness), None, false);
            harness
                .start_night(NightConfig {
                    goal: goal.clone(),
                    metric: metric.clone(),
                    direction: if *lower { Direction::Lower } else { Direction::Higher },
                    guard: guard.clone(),
                    role: role.clone(),
                    max_experiments: *experiments,
                    max_hours: *hours,
                    timeout_secs: *timeout,
                })
                .await?;

            let ctrl_c = tokio::signal::ctrl_c();
            tokio::pin!(ctrl_c);
            let mut stopping = false;
            loop {
                let done = harness
                    .night_report()
                    .await
                    .is_some_and(|report| report.status != NightStatus::Running);
                if done {
                    break;
                }
                tokio::select! {
                    _ = &mut ctrl_c, if !stopping => {
                        eprintln!("  ☾ stopping after the current experiment is cut short…");
                        harness.stop_night().await;
                        stopping = true;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
            }

            let report = harness.night_report().await.context("the night shift vanished")?;
            if report.kept() > 0 {
                println!("\nkept work is on {}", report.branch);
                if *propose {
                    harness.propose_night().await?;
                    // A proposal is verified like any other merge; wait for that to finish.
                    while harness.verifications_running().await > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                } else {
                    println!("(re-run with --propose to put it up for review, or merge the branch yourself)");
                }
            }
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
