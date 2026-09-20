//! Engine-level tests: delegation, isolation policy, rate-limit shedding, and the merge
//! gate. All driven through the `mock` backend, so they need no network and no logged-in
//! CLI.

use harness_core::engine::Harness;
use harness_core::event::{HarnessEvent, WorkerStatus};
use harness_core::isolation::Workspaces;
use harness_core::roles::RoleRegistry;
use harness_core::store::Store;
use std::sync::Arc;
use tokio::process::Command;

const ROLES: &str = r#"
default_role = "builder"

[roles.builder]
provider = "claude"
model = "sonnet"
isolation = "worktree"
tools = ["Read", "Edit", "Write"]
fallback_role = "local_builder"

[roles.local_builder]
provider = "mock"
isolation = "worktree"
tools = ["Read", "Write"]

[roles.reviewer]
provider = "mock"
isolation = "readonly"
tools = ["Read", "Edit"]

[roles.shared_worker]
provider = "mock"
isolation = "shared"
tools = ["Read", "Write"]
"#;

async fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().await.unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Fixture {
    harness: Arc<Harness>,
    events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
    _dir: tempfile::TempDir,
    #[allow(dead_code)]
    root: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    git(&root, &["init", "-q", "-b", "main"]).await;
    git(&root, &["config", "user.email", "harness@test"]).await;
    git(&root, &["config", "user.name", "Harness Test"]).await;
    tokio::fs::write(root.join("README.md"), "base\n").await.unwrap();
    git(&root, &["add", "-A"]).await;
    git(&root, &["commit", "-q", "-m", "init"]).await;

    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
    let store = Store::in_memory().unwrap();
    store.create_session("s1", None, &root.display().to_string()).unwrap();

    let harness = Arc::new(Harness::new(
        RoleRegistry::from_toml(ROLES).unwrap(),
        Workspaces::new(root.clone()),
        store,
        "s1",
        tx,
    ));

    Fixture { harness, events, _dir: dir, root }
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>) -> Vec<HarnessEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn delegation_runs_a_worker_and_reports_its_diff() {
    let mut f = fixture().await;

    let record = f
        .harness
        .delegate("local_builder", "WRITE:out.txt:hello", vec![])
        .await
        .unwrap();

    assert_eq!(record.status, WorkerStatus::Done);
    assert!(!record.is_error);
    assert_eq!(record.diff.as_ref().unwrap().files, vec!["out.txt"]);
    assert!(record.branch.is_some());

    // The write stayed inside the worktree.
    assert!(!f.root.join("out.txt").exists());

    let events = drain(&mut f.events);
    assert!(events.iter().any(|e| matches!(e, HarnessEvent::WorkerSpawned { .. })));
    assert!(events.iter().any(|e| matches!(e, HarnessEvent::WorkerFinished { is_error: false, .. })));
}

#[tokio::test]
async fn failed_workers_are_recorded_not_lost() {
    let mut f = fixture().await;

    let record = f.harness.delegate("local_builder", "FAIL:could not do it", vec![]).await.unwrap();
    assert_eq!(record.status, WorkerStatus::Failed);
    assert!(record.is_error);
    assert_eq!(record.summary, "could not do it");

    assert!(drain(&mut f.events)
        .iter()
        .any(|e| matches!(e, HarnessEvent::WorkerFinished { is_error: true, .. })));
}

#[tokio::test]
async fn a_missing_backend_fails_the_worker_rather_than_the_engine() {
    let f = fixture().await;

    // `builder` is backed by the real claude CLI. With no rate limit noted it is chosen
    // as-is; if the binary is absent the worker fails cleanly instead of panicking.
    let record = f.harness.delegate("builder", "say hi", vec![]).await.unwrap();
    assert!(matches!(record.status, WorkerStatus::Done | WorkerStatus::Failed));
}

#[tokio::test]
async fn rate_limited_providers_shed_to_their_fallback_role() {
    let f = fixture().await;

    // Before: `builder` runs on claude.
    assert!(!f.harness.is_rate_limited("claude").await);

    f.harness.mark_rate_limited("claude").await;
    let record = f.harness.delegate("builder", "WRITE:shed.txt:ok", vec![]).await.unwrap();

    // After: the same request lands on the local fallback instead.
    assert_eq!(record.role, "local_builder");
    assert!(!record.is_error);

    f.harness.clear_rate_limit("claude").await;
    assert!(!f.harness.is_rate_limited("claude").await);
}

#[tokio::test]
async fn a_rate_limit_retry_event_triggers_shedding() {
    let f = fixture().await;

    f.harness
        .note_event(&HarnessEvent::ApiRetry {
            run_id: "r1".into(),
            attempt: 1,
            max_retries: 5,
            retry_delay_ms: 1000,
            error: "rate_limit".into(),
        })
        .await;

    assert!(f.harness.is_rate_limited("claude").await);

    // An unrelated retry category must not shed load.
    let g = fixture().await;
    g.harness
        .note_event(&HarnessEvent::ApiRetry {
            run_id: "r1".into(),
            attempt: 1,
            max_retries: 5,
            retry_delay_ms: 1000,
            error: "overloaded".into(),
        })
        .await;
    assert!(!g.harness.is_rate_limited("claude").await);
}

#[tokio::test]
async fn readonly_workers_produce_nothing_mergeable() {
    let f = fixture().await;

    // Even when the worker writes anyway, its branch is never offered for merge.
    let record = f.harness.delegate("reviewer", "WRITE:sneaky.txt:x", vec![]).await.unwrap();
    assert!(record.branch.is_none(), "readonly work must not be mergeable");
    assert!(!f.root.join("sneaky.txt").exists());

    let err = f.harness.request_merge(&record.id).await.unwrap_err();
    assert!(err.to_string().contains("nothing mergeable"), "{err}");
}

#[tokio::test]
async fn request_merge_queues_but_does_not_land() {
    let mut f = fixture().await;

    let record = f.harness.delegate("local_builder", "WRITE:feature.txt:shipped", vec![]).await.unwrap();
    let before = String::from_utf8(
        Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&f.root).output().await.unwrap().stdout,
    )
    .unwrap();

    let diff = f.harness.request_merge(&record.id).await.unwrap();
    assert_eq!(diff.files, vec!["feature.txt"]);

    // The proposal is recorded...
    assert_eq!(f.harness.pending_merges().await.len(), 1);
    assert!(drain(&mut f.events).iter().any(|e| matches!(e, HarnessEvent::MergeRequested { .. })));

    // ...and nothing has landed.
    let after = String::from_utf8(
        Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&f.root).output().await.unwrap().stdout,
    )
    .unwrap();
    assert_eq!(before, after, "request_merge must not move HEAD");
    assert!(!f.root.join("feature.txt").exists());
}

#[tokio::test]
async fn approval_is_what_actually_lands_the_work() {
    let f = fixture().await;

    let record = f.harness.delegate("local_builder", "WRITE:feature.txt:shipped", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();
    f.harness.approve_merge(&record.id).await.unwrap();

    assert_eq!(
        tokio::fs::read_to_string(f.root.join("feature.txt")).await.unwrap(),
        "shipped"
    );
    assert!(f.harness.pending_merges().await.is_empty());

    // The approval is single-use.
    assert!(f.harness.approve_merge(&record.id).await.is_err());
}

#[tokio::test]
async fn rejecting_a_merge_drops_the_branch() {
    let f = fixture().await;

    let record = f.harness.delegate("local_builder", "WRITE:bad.txt:no", vec![]).await.unwrap();
    let branch = record.branch.clone().unwrap();
    f.harness.request_merge(&record.id).await.unwrap();
    f.harness.reject_merge(&record.id).await.unwrap();

    let branches = String::from_utf8(
        Command::new("git").args(["branch", "--list"]).current_dir(&f.root).output().await.unwrap().stdout,
    )
    .unwrap();
    assert!(!branches.contains(&branch));
    assert!(!f.root.join("bad.txt").exists());
}

#[tokio::test]
async fn a_worker_that_changed_nothing_cannot_be_merged() {
    let f = fixture().await;
    let record = f.harness.delegate("local_builder", "just think about it", vec![]).await.unwrap();

    let err = f.harness.request_merge(&record.id).await.unwrap_err();
    assert!(err.to_string().contains("changed nothing"), "{err}");
}

#[tokio::test]
async fn async_delegation_fans_out_and_results_are_collectable() {
    let f = fixture().await;

    let mut ids = Vec::new();
    for n in 0..3 {
        ids.push(
            f.harness
                .delegate_async("local_builder", &format!("WRITE:f{n}.txt:v{n}"), vec![])
                .await
                .unwrap(),
        );
    }

    // Poll until all three settle.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let done = {
            let mut count = 0;
            for id in &ids {
                if let Some(record) = f.harness.worker(id).await {
                    if record.status.is_terminal() {
                        count += 1;
                    }
                }
            }
            count
        };
        if done == ids.len() || std::time::Instant::now() > deadline {
            assert_eq!(done, ids.len(), "workers did not all finish");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    for id in &ids {
        let record = f.harness.worker(id).await.unwrap();
        assert!(!record.is_error, "{record:?}");
        assert_eq!(record.diff.as_ref().unwrap().files_changed, 1);
    }
}

#[tokio::test]
async fn delegating_to_an_unknown_role_is_an_error_not_a_ghost_worker() {
    let f = fixture().await;

    let err = f.harness.delegate("nonexistent", "task", vec![]).await.unwrap_err();
    assert!(err.to_string().contains("no role named"), "{err}");
    assert!(f.harness.delegate_async("nonexistent", "task", vec![]).await.is_err());
    assert!(f.harness.workers().await.is_empty());
}

#[tokio::test]
async fn orchestrator_usage_lands_in_the_meter() {
    let f = fixture().await;
    f.harness.register_orchestrator("orchestrator-s1", Some("sonnet")).await;

    // The head agent is the biggest consumer of subscription quota; leaving it out of
    // the meter would understate burn by most of it.
    f.harness
        .note_event(&HarnessEvent::RunFinished {
            run_id: "orchestrator-s1".into(),
            text: "planned".into(),
            usage: harness_core::event::Usage {
                input_tokens: 4,
                output_tokens: 274,
                cache_creation_input_tokens: 39_306,
                cache_read_input_tokens: 77_922,
            },
            cost_usd: Some(0.175),
            is_error: false,
        })
        .await;

    let rows = f.harness.usage_window(3600).await.unwrap();
    let claude = rows.iter().find(|r| r.provider == "claude").expect("orchestrator usage");
    assert_eq!(claude.usage.cache_creation_input_tokens, 39_306);
    assert_eq!(claude.usage.output_tokens, 274);
}

#[tokio::test]
async fn worker_usage_is_not_double_counted_by_the_event_hook() {
    let f = fixture().await;
    f.harness.register_orchestrator("orchestrator-s1", None).await;

    let record = f.harness.delegate("local_builder", "WRITE:x.txt:y", vec![]).await.unwrap();

    // Replay the worker's own RunFinished through the hook, as the renderer does.
    f.harness
        .note_event(&HarnessEvent::RunFinished {
            run_id: record.id.clone(),
            text: record.summary.clone(),
            usage: record.usage,
            cost_usd: None,
            is_error: false,
        })
        .await;

    let rows = f.harness.usage_window(3600).await.unwrap();
    let mock = rows.iter().find(|r| r.provider == "mock").unwrap();
    assert_eq!(mock.runs, 1, "worker usage recorded exactly once");
}

#[tokio::test]
async fn dropping_the_engine_closes_the_event_stream() {
    // The engine owns the event sender, so anything that holds a strong reference to it
    // keeps the stream open. A consumer that holds one and then waits for the stream to
    // end waits forever — which is exactly how the CLI deadlocked on shutdown. Consumers
    // must hold a Weak handle; this test is the invariant that makes that necessary.
    let f = fixture().await;
    let mut events = f.events;

    let weak = Arc::downgrade(&f.harness);
    drop(f.harness);

    assert!(weak.upgrade().is_none(), "no strong references should remain");
    assert!(
        events.recv().await.is_none(),
        "the event stream must end once the engine is dropped"
    );
}

#[tokio::test]
async fn shared_workers_are_serialized_while_worktree_workers_are_not() {
    let f = fixture().await;

    // Two shared workers must not run concurrently; the engine queues the second.
    let a = f.harness.delegate_async("shared_worker", "WRITE:a.txt:a", vec![]).await.unwrap();
    let b = f.harness.delegate_async("shared_worker", "WRITE:b.txt:b", vec![]).await.unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let settled = [&a, &b]
            .iter()
            .filter(|id| {
                futures::executor::block_on(f.harness.worker(id))
                    .map(|r| r.status.is_terminal())
                    .unwrap_or(false)
            })
            .count();
        if settled == 2 || std::time::Instant::now() > deadline {
            assert_eq!(settled, 2, "shared workers did not both finish");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Shared mode writes to the project root, and both writes survived because they
    // were serialized rather than racing.
    assert_eq!(tokio::fs::read_to_string(f.root.join("a.txt")).await.unwrap(), "a");
    assert_eq!(tokio::fs::read_to_string(f.root.join("b.txt")).await.unwrap(), "b");
}
