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
    fixture_with(ROLES).await
}

async fn fixture_with(roles: &str) -> Fixture {
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
        RoleRegistry::from_toml(roles).unwrap(),
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

/// The fleet is editable while a session runs.
///
/// Workers are spawned per delegation, so a swap needs no process restarted — the next
/// delegation simply resolves against the new registry. This is the property the fleet
/// editor depends on, so it is asserted against a real delegation rather than by reading
/// the registry back.
#[tokio::test]
async fn swapping_a_role_changes_what_the_next_delegation_spawns() {
    let mut fixture = fixture().await;

    // `reviewer` starts readonly on the mock backend.
    let before = fixture.harness.delegate("reviewer", "look", Vec::new()).await.unwrap();
    assert!(!before.is_error);
    let spawned_before = drain(&mut fixture.events)
        .into_iter()
        .find_map(|event| match event {
            HarnessEvent::WorkerSpawned { role, isolation, .. } => Some((role, isolation)),
            _ => None,
        })
        .expect("a worker should have been spawned");
    assert_eq!(spawned_before, ("reviewer".to_string(), "readonly".to_string()));

    // Move it into its own worktree, the way the fleet editor would.
    let swapped = RoleRegistry::from_toml(
        r#"
default_role = "builder"

[roles.builder]
provider = "mock"
isolation = "worktree"

[roles.reviewer]
provider = "mock"
isolation = "worktree"
tools = ["Read", "Write"]
"#,
    )
    .unwrap();
    fixture.harness.swap_registry(swapped).await;

    let after = fixture.harness.delegate("reviewer", "look again", Vec::new()).await.unwrap();
    assert!(!after.is_error);
    let spawned_after = drain(&mut fixture.events)
        .into_iter()
        .find_map(|event| match event {
            HarnessEvent::WorkerSpawned { role, isolation, .. } => Some((role, isolation)),
            _ => None,
        })
        .expect("a worker should have been spawned");
    assert_eq!(
        spawned_after,
        ("reviewer".to_string(), "worktree".to_string()),
        "the delegation after a swap must use the new fleet"
    );

    // A worktree role can be merged; the readonly one it replaced could not.
    assert!(after.branch.is_some(), "the swapped role should now produce a landable branch");
    assert!(before.branch.is_none());
}

/// A role that disappears in a swap stops being delegable, rather than silently running
/// under its old definition.
#[tokio::test]
async fn a_role_removed_by_a_swap_is_no_longer_delegable() {
    let fixture = fixture().await;
    fixture.harness.delegate("reviewer", "look", Vec::new()).await.unwrap();

    let without_reviewer = RoleRegistry::from_toml(
        r#"
default_role = "builder"

[roles.builder]
provider = "mock"
isolation = "worktree"
"#,
    )
    .unwrap();
    fixture.harness.swap_registry(without_reviewer).await;

    let error = fixture
        .harness
        .delegate("reviewer", "look again", Vec::new())
        .await
        .expect_err("a role the fleet no longer defines must not run");
    assert!(error.to_string().contains("no role named"), "got: {error}");
}

/// A person can stop a worker that has gone wrong, without losing what it wrote.
#[tokio::test]
async fn stopping_a_worker_kills_it_and_keeps_its_partial_work() {
    let f = fixture().await;
    let harness = Arc::clone(&f.harness);

    let running = tokio::spawn({
        let harness = Arc::clone(&harness);
        async move {
            harness
                .delegate("local_builder", "SLOW:half-done.txt:partial", vec![])
                .await
        }
    });

    // Wait until it is actually running, then stop it.
    let worker_id = loop {
        if let Some(worker) = harness
            .workers()
            .await
            .into_iter()
            .find(|w| w.status == WorkerStatus::Running)
        {
            break worker.id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    // Give the mock a moment to write its file before the stop lands.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(harness.cancel_worker(&worker_id).await, "a running worker should accept a stop");

    let record = tokio::time::timeout(std::time::Duration::from_secs(10), running)
        .await
        .expect("a stopped worker must finish promptly, not wait out its task")
        .unwrap()
        .unwrap();

    assert_eq!(record.status, WorkerStatus::Cancelled);
    assert!(record.is_error);
    assert!(
        record.summary.contains("do not retry"),
        "the head agent should be told not to redo deliberately stopped work"
    );

    // The partial work is on the branch, reviewable like any other.
    let branch = record.branch.expect("worktree workers keep a branch");
    let show = Command::new("git")
        .args(["show", &format!("{branch}:half-done.txt")])
        .current_dir(&f.root)
        .output()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show.stdout), "partial");

    // And a finished worker no longer accepts a stop.
    assert!(!harness.cancel_worker(&worker_id).await);
}

/// The head agent's conversation outlives the session that produced it.
#[tokio::test]
async fn the_head_agents_conversation_is_kept_across_sessions() {
    let f = fixture().await;
    let harness = &f.harness;
    harness.register_orchestrator("orchestrator-s1", None).await;

    harness
        .record_head_event(HarnessEvent::UserMessage {
            run_id: "orchestrator-s1".into(),
            text: "plan the release".into(),
        })
        .await;
    // What the agent says arrives through `note_event`, as it does from the real CLI.
    for event in [
        HarnessEvent::AssistantText {
            run_id: "orchestrator-s1".into(),
            text: "partial".into(),
            partial: true,
        },
        HarnessEvent::AssistantText {
            run_id: "orchestrator-s1".into(),
            text: "Here is the plan.".into(),
            partial: false,
        },
        // A worker's own text is not part of the head agent's conversation.
        HarnessEvent::AssistantText {
            run_id: "w-someone-else".into(),
            text: "worker chatter".into(),
            partial: false,
        },
    ] {
        harness.note_event(&event).await;
    }
    harness
        .record_head_event(HarnessEvent::TurnInterrupted {
            run_id: "orchestrator-s1".into(),
        })
        .await;

    let history = harness.project_history(100).await.unwrap();
    let kinds: Vec<String> = history
        .iter()
        .map(|e| serde_json::to_value(e).unwrap()["type"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        kinds,
        ["user_message", "assistant_text", "turn_interrupted"],
        "only settled head-agent events belong in the transcript"
    );
    match &history[1] {
        HarnessEvent::AssistantText { text, .. } => assert_eq!(text, "Here is the plan."),
        other => panic!("unexpected {other:?}"),
    }
}

/// A turn that failed or was stopped still spent quota, so it still counts.
#[tokio::test]
async fn a_stopped_head_turn_still_counts_against_the_window() {
    let f = fixture().await;
    f.harness.register_orchestrator("orchestrator-s1", None).await;

    f.harness
        .note_event(&HarnessEvent::RunFinished {
            run_id: "orchestrator-s1".into(),
            text: String::new(),
            usage: harness_core::event::Usage {
                input_tokens: 1200,
                output_tokens: 40,
                ..Default::default()
            },
            cost_usd: None,
            is_error: true,
        })
        .await;

    let rows = f.harness.usage_window(3600).await.unwrap();
    let claude = rows.iter().find(|r| r.provider == "claude").expect("usage recorded");
    assert_eq!(claude.usage.input_tokens, 1200);
}

/// Claude's own quota report is kept, and a refusal sheds work before a retry is wasted.
#[tokio::test]
async fn claudes_quota_report_is_kept_and_a_refusal_sheds_load() {
    let f = fixture().await;
    assert!(f.harness.claude_quota_snapshot().await.is_none());

    let window = |used: f64| harness_core::quota::QuotaWindow {
        label: "5h".into(),
        used_percent: used,
        resets_at: Some(1_790_122_800),
    };

    f.harness
        .note_event(&HarnessEvent::QuotaReport {
            run_id: "orchestrator-s1".into(),
            provider: "claude".into(),
            status: "allowed".into(),
            windows: vec![window(48.0)],
        })
        .await;
    let (_, windows) = f.harness.claude_quota_snapshot().await.unwrap();
    assert_eq!(windows[0].used_percent, 48.0);
    assert!(!f.harness.is_rate_limited("claude").await);

    f.harness
        .note_event(&HarnessEvent::QuotaReport {
            run_id: "orchestrator-s1".into(),
            provider: "claude".into(),
            status: "rejected".into(),
            windows: vec![window(100.0)],
        })
        .await;
    assert!(
        f.harness.is_rate_limited("claude").await,
        "a refused request should shed Claude roles to their fallbacks"
    );
    let (_, windows) = f.harness.claude_quota_snapshot().await.unwrap();
    assert_eq!(windows[0].used_percent, 100.0, "the newest report replaces the old one");
}

/// A native subagent — run inside the head agent's process by Claude Code — ends up as an
/// ordinary worker: its work committed to a branch, its spend counted, and its change put
/// in front of the person for review. Nothing lands until they approve it.
#[tokio::test]
async fn a_native_subagent_becomes_a_reviewable_worker() {
    let mut f = fixture().await;

    // What Claude Code does first: ask our hook for a worktree.
    let hook_input = serde_json::json!({ "cwd": f.root, "name": "agent-t1" }).to_string();
    let worktree = harness_core::hooks::worktree_create(&hook_input).await.unwrap();

    f.harness
        .note_event(&HarnessEvent::SubagentStarted {
            run_id: "orchestrator-s1".into(),
            task_id: "t1".into(),
            tool_use_id: "toolu_1".into(),
            subagent_type: "builder".into(),
            description: "Add a greeting".into(),
        })
        .await;
    assert_eq!(
        f.harness.worker("agent-t1").await.unwrap().status,
        WorkerStatus::Running
    );

    // The subagent works — in its worktree, never the checkout.
    tokio::fs::write(worktree.join("greeting.txt"), "hello\n").await.unwrap();

    f.harness
        .note_event(&HarnessEvent::SubagentFinished {
            run_id: "orchestrator-s1".into(),
            task_id: "t1".into(),
            status: "completed".into(),
            summary: "Added greeting.txt".into(),
            total_tokens: 3682,
        })
        .await;

    let worker = f.harness.worker("agent-t1").await.unwrap();
    assert_eq!(worker.status, WorkerStatus::Done);
    assert_eq!(worker.branch.as_deref(), Some("harness/agent-t1"));
    assert_eq!(worker.diff.as_ref().unwrap().files, ["greeting.txt"]);
    assert!(!worktree.exists(), "the worktree is removed once its work is committed");
    assert!(
        !f.root.join("greeting.txt").exists(),
        "nothing reaches the checkout before approval"
    );

    // Proposed for review automatically; the person still has to approve.
    let pending = f.harness.pending_merges().await;
    assert!(pending.iter().any(|(id, _)| id == "agent-t1"));
    let events = drain(&mut f.events);
    assert!(events.iter().any(|e| matches!(e, HarnessEvent::MergeRequested { worker_id, .. } if worker_id == "agent-t1")));

    // Its spend counts against the window; the head's own result would not include it.
    let claude = f.harness.usage_window(3600).await.unwrap();
    assert_eq!(claude.iter().find(|r| r.provider == "claude").unwrap().usage.input_tokens, 3682);

    // And approving lands it exactly like any other worker.
    f.harness.approve_merge("agent-t1").await.unwrap();
    assert_eq!(tokio::fs::read_to_string(f.root.join("greeting.txt")).await.unwrap(), "hello\n");
}

/// Claude Code's built-in subagents (Explore and friends) have no role and no worktree.
/// They still show in the rail, and never produce anything to merge.
#[tokio::test]
async fn a_built_in_subagent_is_shown_but_offers_nothing_to_merge() {
    let f = fixture().await;
    f.harness
        .note_event(&HarnessEvent::SubagentStarted {
            run_id: "orchestrator-s1".into(),
            task_id: "t2".into(),
            tool_use_id: "toolu_2".into(),
            subagent_type: "Explore".into(),
            description: "Find the retry logic".into(),
        })
        .await;
    f.harness
        .note_event(&HarnessEvent::SubagentFinished {
            run_id: "orchestrator-s1".into(),
            task_id: "t2".into(),
            status: "completed".into(),
            summary: "It is in src/prices.rs".into(),
            total_tokens: 900,
        })
        .await;

    let worker = f.harness.worker("agent-t2").await.unwrap();
    assert_eq!(worker.status, WorkerStatus::Done);
    assert!(worker.branch.is_none());
    assert!(f.harness.pending_merges().await.is_empty());
}

// ------------------------------------------------------------- verification

use harness_core::verify::{CheckKind, CheckStatus, Risk, VerificationReport};

/// Collect events until this worker's verification finishes.
async fn verification_of(
    f: &mut Fixture,
    worker_id: &str,
) -> (Vec<HarnessEvent>, VerificationReport) {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), f.events.recv())
            .await
            .expect("verification did not finish in time")
            .expect("event stream closed");
        let done = match &event {
            HarnessEvent::VerificationFinished { worker_id: id, report } if id == worker_id => {
                Some(report.clone())
            }
            _ => None,
        };
        seen.push(event);
        if let Some(report) = done {
            return (seen, report);
        }
    }
}

fn with_verify(verify: &str) -> String {
    format!("{ROLES}\n[verify]\n{verify}\n")
}

#[tokio::test]
async fn with_nothing_configured_a_merge_is_reported_unverified() {
    let mut f = fixture().await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (events, report) = verification_of(&mut f, &record.id).await;
    assert!(!report.verified, "nothing ran, so nothing may claim to be verified");
    assert!(report.reasons.iter().any(|r| r.contains("[verify]")));
    // Started, then the free signals check, then finished — in that order.
    let order: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            HarnessEvent::VerificationStarted { .. } => Some("started"),
            HarnessEvent::VerificationCheck { .. } => Some("check"),
            HarnessEvent::VerificationFinished { .. } => Some("finished"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["started", "check", "finished"]);
    assert!(f.harness.verification_line(&record.id).await.unwrap().contains("unverified"));
}

#[tokio::test]
async fn commands_run_against_exactly_what_would_be_merged() {
    // `test -f` only passes if the checkout holds the worker's file.
    let mut f = fixture_with(&with_verify("commands = [\"test -f notes.md\"]")).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (_, report) = verification_of(&mut f, &record.id).await;
    let command = report.checks.iter().find(|c| c.kind == CheckKind::Command).unwrap();
    assert_eq!(command.status, CheckStatus::Passed, "{command:?}");
    assert!(report.verified);
    assert_eq!(report.risk, Risk::Low);
    // The checkout is gone afterwards; the branch is still there to merge.
    assert!(!f.root.join(".harness/verify").join(&record.id).exists());
    f.harness.approve_merge(&record.id).await.unwrap();
}

#[tokio::test]
async fn a_failing_command_makes_the_merge_high_risk_and_says_why() {
    let mut f =
        fixture_with(&with_verify("commands = [\"echo 2 tests failed; exit 1\"]")).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (_, report) = verification_of(&mut f, &record.id).await;
    assert_eq!(report.risk, Risk::High);
    let command = report.checks.iter().find(|c| c.kind == CheckKind::Command).unwrap();
    assert_eq!(command.output.as_deref(), Some("2 tests failed"));
    assert!(report.reasons[0].contains("exit 1"), "{:?}", report.reasons);
    // The head agent can see it, and delegate a fix.
    let line = f.harness.verification_line(&record.id).await.unwrap();
    assert!(line.starts_with("high risk"), "{line}");
}

#[tokio::test]
async fn a_person_can_merge_while_checks_are_still_running() {
    let mut f = fixture_with(&with_verify("commands = [\"sleep 2\"]")).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    // A human decision always wins over a check that has not finished.
    f.harness.approve_merge(&record.id).await.unwrap();
    assert!(f.root.join("notes.md").exists());
    let (_, report) = verification_of(&mut f, &record.id).await;
    assert!(report.verified);
}

#[tokio::test]
async fn a_reviewer_that_does_not_answer_in_json_is_an_error_not_a_verdict() {
    // The mock echoes its prompt back, which contains the contract but no verdict.
    let mut f = fixture_with(&with_verify("reviewer = \"reviewer\"")).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (_, report) = verification_of(&mut f, &record.id).await;
    let review = report.checks.iter().find(|c| c.kind == CheckKind::Review).unwrap();
    assert_eq!(review.status, CheckStatus::Error);
    assert!(!report.verified);
}

/// An OpenAI-compatible server whose reply depends on the model asked for.
async fn review_server(replies: &'static [(&'static str, &'static str)]) -> String {
    use axum::{routing::get, routing::post, Json, Router};
    let app = Router::new()
        .route("/", get(|| async { "ok" }))
        .route(
            "/v1/models",
            get(move || async move {
                Json(serde_json::json!({
                    "data": replies.iter().map(|(model, _)| serde_json::json!({ "id": model })).collect::<Vec<_>>()
                }))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| async move {
                let model = body["model"].as_str().unwrap_or_default().to_string();
                let reply = replies.iter().find(|(m, _)| *m == model).map(|(_, r)| *r).unwrap_or("");
                Json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": reply } }],
                    "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}/v1")
}

fn with_reviewers(url: &str) -> String {
    format!(
        "{ROLES}
[roles.quick]
provider = \"openai_compat\"
base_url = \"{url}\"
model = \"quick\"
isolation = \"none\"

[roles.careful]
provider = \"openai_compat\"
base_url = \"{url}\"
model = \"careful\"
isolation = \"none\"

[verify]
reviewer = \"quick\"
escalate_to = \"careful\"
"
    )
}

#[tokio::test]
async fn a_worrying_first_review_escalates_and_the_second_has_the_last_word() {
    let url = review_server(&[
        ("quick", r#"Sure! {"risk_level": "medium", "summary": "not sure about the parser", "findings": [{"severity": "medium", "file": "notes.md", "line": 1, "message": "unclear"}]}"#),
        ("careful", r#"{"risk_level": "low", "summary": "fine on a closer look", "findings": []}"#),
    ])
    .await;
    let mut f = fixture_with(&with_reviewers(&url)).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (_, report) = verification_of(&mut f, &record.id).await;
    let reviews: Vec<_> = report.checks.iter().filter(|c| c.kind == CheckKind::Review).collect();
    assert_eq!(reviews.len(), 2, "the first review worried, so the second ran");
    assert_eq!(reviews[0].findings[0].file.as_deref(), Some("notes.md"));
    assert_eq!(report.risk, Risk::Low, "the escalated reviewer overrules the first");
    assert!(report.verified);
    assert!(reviews[1].reviewer.as_deref().unwrap().contains("careful"));
}

#[tokio::test]
async fn a_clean_first_review_spends_nothing_on_a_second() {
    let url = review_server(&[
        ("quick", r#"{"risk_level": "low", "summary": "trivial", "findings": []}"#),
        ("careful", r#"{"risk_level": "high", "summary": "should never be asked", "findings": []}"#),
    ])
    .await;
    let mut f = fixture_with(&with_reviewers(&url)).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    let (_, report) = verification_of(&mut f, &record.id).await;
    assert_eq!(report.checks.iter().filter(|c| c.kind == CheckKind::Review).count(), 1);
    assert_eq!(report.risk, Risk::Low);
}


// ------------------------------------------------------------------- autonomy

use harness_core::autonomy::Autonomy;

/// Wait for a worker to reach a terminal status.
async fn finished(f: &Fixture, worker_id: &str) -> harness_core::engine::WorkerRecord {
    for _ in 0..200 {
        if let Some(record) = f.harness.worker(worker_id).await {
            if record.status.is_terminal() {
                return record;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{worker_id} did not finish");
}

#[tokio::test]
async fn under_ask_a_delegation_waits_and_runs_the_task_the_person_approved() {
    let mut f = fixture().await;
    f.harness.set_autonomy(Autonomy::Ask).await;

    let id = f
        .harness
        .queue_for_approval("local_builder", "WRITE:original.txt:no", vec![])
        .await
        .unwrap();
    assert_eq!(f.harness.worker(&id).await.unwrap().status, WorkerStatus::AwaitingApproval);
    assert!(drain(&mut f.events)
        .iter()
        .any(|e| matches!(e, HarnessEvent::DelegationRequested { worker_id, .. } if worker_id == &id)));

    // The person edits the task before approving; the edit is what runs, under the same id.
    f.harness
        .approve_delegation(&id, Some("WRITE:edited.txt:yes".into()))
        .await
        .unwrap();
    let record = finished(&f, &id).await;
    assert_eq!(record.status, WorkerStatus::Done);
    assert_eq!(record.diff.unwrap().files, ["edited.txt"]);
    assert!(f.harness.take_approved(&id).await, "its outcome is owed to the head agent");
    assert!(!f.harness.take_approved(&id).await, "once");
    assert!(f.harness.approve_delegation(&id, None).await.is_err(), "approval is single-use");
}

#[tokio::test]
async fn a_declined_delegation_never_runs_and_says_why() {
    let mut f = fixture().await;
    let id = f.harness.queue_for_approval("local_builder", "WRITE:x.txt:no", vec![]).await.unwrap();
    f.harness.decline_delegation(&id, "wrong approach").await.unwrap();

    let record = f.harness.worker(&id).await.unwrap();
    assert_eq!(record.status, WorkerStatus::Cancelled);
    assert_eq!(record.summary, "Declined: wrong approach");
    assert!(drain(&mut f.events).iter().any(|e| matches!(
        e,
        HarnessEvent::DelegationDeclined { reason, .. } if reason == "wrong approach"
    )));
    assert!(!f.root.join(".harness/worktrees").join(&id).exists(), "nothing was prepared");
    // Stop on one that is waiting is the same as declining it.
    let other = f.harness.queue_for_approval("local_builder", "WRITE:y.txt:no", vec![]).await.unwrap();
    assert!(f.harness.cancel_worker(&other).await);
    assert_eq!(f.harness.worker(&other).await.unwrap().status, WorkerStatus::Cancelled);
}

fn passing_verify() -> String {
    with_verify("commands = [\"true\"]")
}

#[tokio::test]
async fn land_safe_lands_a_verified_low_risk_change_by_itself() {
    let mut f = fixture_with(&passing_verify()).await;
    f.harness.set_autonomy(Autonomy::LandSafe).await;
    let record = f.harness.delegate("local_builder", "WRITE:notes.md:hello", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();

    verification_of(&mut f, &record.id).await;
    let landed = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), f.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let HarnessEvent::MergeLanded { automatic, commit, .. } = event {
            break (automatic, commit);
        }
    };
    assert!(landed.0, "landed without a click");
    assert_eq!(landed.1.len(), 40, "the merge commit, so it can be undone");
    assert!(f.root.join("notes.md").exists());
    assert!(f.harness.pending_merges().await.is_empty());
    let line = f.harness.verification_line(&record.id).await.unwrap();
    assert!(line.contains("landed automatically"), "{line}");

    // Undo puts it back, once.
    f.harness.undo_merge(&record.id).await.unwrap();
    assert!(!f.root.join("notes.md").exists());
    assert!(drain(&mut f.events).iter().any(|e| matches!(e, HarnessEvent::MergeReverted { .. })));
    assert!(f.harness.undo_merge(&record.id).await.is_err());
}

#[tokio::test]
async fn nothing_lands_by_itself_that_should_wait() {
    // (level, verify config, what the worker writes)
    let cases = [
        // Review never lands.
        (Autonomy::Review, passing_verify(), "WRITE:notes.md:x"),
        // Unverified: nothing ran.
        (Autonomy::LandMost, ROLES.to_string(), "WRITE:notes.md:x"),
        // A failed check.
        (Autonomy::LandMost, with_verify("commands = [\"false\"]"), "WRITE:notes.md:x"),
        // Medium risk (code without tests) is above Land safe's ceiling.
        (Autonomy::LandSafe, passing_verify(), "WRITE:src/lib.rs:fn a() {}"),
        // High risk (a sensitive path) is above Land most's.
        (Autonomy::LandMost, passing_verify(), "WRITE:db/migrations/1.sql:drop table users;"),
    ];
    for (level, roles, task) in cases {
        let mut f = fixture_with(&roles).await;
        f.harness.set_autonomy(level).await;
        let record = f.harness.delegate("local_builder", task, vec![]).await.unwrap();
        f.harness.request_merge(&record.id).await.unwrap();
        verification_of(&mut f, &record.id).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(f.harness.pending_merges().await.len(), 1, "{level:?} / {task} should wait");
    }
}

#[tokio::test]
async fn land_most_takes_medium_risk_that_land_safe_would_not() {
    let mut f = fixture_with(&passing_verify()).await;
    f.harness.set_autonomy(Autonomy::LandMost).await;
    let record = f.harness.delegate("local_builder", "WRITE:src/lib.rs:fn a() {}", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();
    let (_, report) = verification_of(&mut f, &record.id).await;
    assert_eq!(report.risk, Risk::Medium);
    for _ in 0..50 {
        if f.harness.pending_merges().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(f.harness.pending_merges().await.is_empty());
    assert!(f.root.join("src/lib.rs").exists());
}

#[tokio::test]
async fn a_merge_that_fails_stays_proposed_for_the_person() {
    let f = fixture().await;
    let record = f.harness.delegate("local_builder", "WRITE:README.md:theirs", vec![]).await.unwrap();
    f.harness.request_merge(&record.id).await.unwrap();
    // A conflicting commit on the main line.
    tokio::fs::write(f.root.join("README.md"), "ours\n").await.unwrap();
    git(&f.root, &["commit", "-qam", "ours"]).await;

    assert!(f.harness.approve_merge(&record.id).await.is_err());
    assert_eq!(f.harness.pending_merges().await.len(), 1, "still reviewable after a conflict");
    // And the checkout is not left mid-merge with conflict markers in it.
    let status = Command::new("git").args(["status", "--porcelain"]).current_dir(&f.root).output().await.unwrap();
    assert!(status.stdout.is_empty(), "{}", String::from_utf8_lossy(&status.stdout));
    assert_eq!(tokio::fs::read_to_string(f.root.join("README.md")).await.unwrap(), "ours\n");
}
