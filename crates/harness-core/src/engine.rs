//! The delegation engine: the supervisor half of the supervisor pattern.
//!
//! Holds the fleet, prepares workspaces, runs workers, records what they cost, and keeps
//! the worker table the UI renders. The MCP layer is a thin adapter over this; nothing
//! here knows what MCP is.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::agents::{self, EventSink, WorkerSpec};
use crate::event::{DiffStat, HarnessEvent, Usage, WorkerStatus};
use crate::isolation::Workspaces;
use crate::roles::{Provider, RoleRegistry};
use crate::store::Store;

/// What the orchestrator learns about a role when it asks what its fleet can do.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoleInfo {
    pub name: String,
    pub provider: String,
    pub model: Option<String>,
    pub isolation: String,
    pub can_edit_files: bool,
    pub brief: Option<String>,
    /// Whether this role's backend is installed and reachable. `None` until probed.
    pub available: Option<bool>,
    pub unavailable_reason: Option<String>,
}

/// A worker's state, as the UI and the orchestrator both see it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkerRecord {
    pub id: String,
    pub role: String,
    pub status: WorkerStatus,
    pub task: String,
    pub summary: String,
    pub usage: Usage,
    pub diff: Option<DiffStat>,
    /// Present only for worktree-isolated workers whose changes could be landed.
    pub branch: Option<String>,
    pub is_error: bool,
}

pub struct Harness {
    /// Behind a lock because the fleet is editable while a session runs: swapping what
    /// backs a role takes effect on the next delegation, with no process restarted.
    registry: RwLock<RoleRegistry>,
    workspaces: Workspaces,
    store: Mutex<Store>,
    session_id: String,
    events: EventSink,
    workers: RwLock<HashMap<String, WorkerRecord>>,
    /// Merges the orchestrator has proposed, awaiting a human click.
    pending_merges: RwLock<HashMap<String, String>>,
    /// Providers currently known to be rate-limited, so delegation can shed load.
    rate_limited: RwLock<Vec<String>>,
    /// The head agent's run id, so its usage lands in the meter too.
    orchestrator_run: RwLock<Option<String>>,
    /// One stop switch per live worker, so a person can end a run that has gone wrong
    /// without closing the whole project.
    cancels: RwLock<HashMap<String, tokio::sync::watch::Sender<bool>>>,
    /// The latest subscription quota Claude reported, with when it arrived.
    claude_quota: RwLock<Option<(i64, Vec<crate::quota::QuotaWindow>)>>,
}

impl Harness {
    pub fn new(
        registry: RoleRegistry,
        workspaces: Workspaces,
        store: Store,
        session_id: impl Into<String>,
        events: EventSink,
    ) -> Self {
        Self {
            registry: RwLock::new(registry),
            workspaces,
            store: Mutex::new(store),
            session_id: session_id.into(),
            events,
            workers: RwLock::new(HashMap::new()),
            pending_merges: RwLock::new(HashMap::new()),
            rate_limited: RwLock::new(Vec::new()),
            orchestrator_run: RwLock::new(None),
            cancels: RwLock::new(HashMap::new()),
            claude_quota: RwLock::new(None),
        }
    }

    /// Register the head agent's run so [`Harness::note_event`] can attribute its usage.
    ///
    /// The orchestrator is the single largest consumer of subscription quota — it pays
    /// the context floor and every planning turn — so leaving it out of the meter would
    /// understate burn by most of it.
    pub async fn register_orchestrator(&self, run_id: &str, model: Option<&str>) {
        *self.orchestrator_run.write().await = Some(run_id.to_string());

        self.store
            .lock()
            .await
            .create_run(
                run_id,
                &self.session_id,
                "orchestrator",
                Provider::Claude.as_str(),
                model,
                crate::roles::Isolation::Readonly.as_str(),
            )
            .ok();
    }

    /// The head agent's most recent backend conversation for this project, if any.
    ///
    /// Resuming that conversation is what makes suspending a project cheap: the head
    /// agent comes back where it left off instead of paying the context floor again.
    pub async fn resumable_backend_session(&self) -> Result<Option<String>> {
        let root = self.workspaces.project_root();
        let key = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .display()
            .to_string();
        self.store
            .lock()
            .await
            .latest_orchestrator_backend_session(&key)
    }

    /// Stop a running worker. Returns false if it is not running, or already finished.
    ///
    /// Its process is killed, whatever it had written is committed to its branch so
    /// nothing is lost, and it finishes with status `Cancelled`.
    pub async fn cancel_worker(&self, worker_id: &str) -> bool {
        match self.cancels.read().await.get(worker_id) {
            Some(stop) => stop.send(true).is_ok(),
            None => false,
        }
    }

    /// The latest Claude quota this session has seen, as `(observed_at, windows)`.
    pub async fn claude_quota_snapshot(&self) -> Option<(i64, Vec<crate::quota::QuotaWindow>)> {
        self.claude_quota.read().await.clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The fleet as it stands, cloned out of its lock.
    pub async fn registry_snapshot(&self) -> RoleRegistry {
        self.registry.read().await.clone()
    }

    /// Replace the fleet for every delegation from here on.
    ///
    /// Workers are spawned per delegation, so a swap needs no process to be restarted —
    /// the next `delegate` resolves against the new registry. The head agent's brief is
    /// argv-baked into its long-lived process and cannot be rewritten, so callers should
    /// tell it what changed; its `list_roles` tool reads through to this, so it can
    /// always re-check.
    pub async fn swap_registry(&self, registry: RoleRegistry) {
        *self.registry.write().await = registry;
    }

    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    fn emit(&self, event: HarnessEvent) {
        let _ = self.events.send(event);
    }

    /// Persist an event as well as broadcasting it, so a session can be replayed.
    async fn record(&self, event: HarnessEvent) {
        if let Err(err) = self
            .store
            .lock()
            .await
            .append_event(&self.session_id, &event)
        {
            tracing::warn!("failed to persist event: {err}");
        }
        self.emit(event);
    }

    pub async fn list_roles(&self) -> Vec<RoleInfo> {
        self.registry
            .read()
            .await
            .roles
            .iter()
            .map(|(name, role)| RoleInfo {
                name: name.clone(),
                provider: role.provider.as_str().to_string(),
                model: role.model.clone(),
                isolation: role.isolation.as_str().to_string(),
                can_edit_files: role
                    .effective_tools()
                    .iter()
                    .any(|t| t == "Edit" || t == "Write"),
                brief: role.brief.clone(),
                available: None,
                unavailable_reason: None,
            })
            .collect()
    }

    /// The fleet, with each backend probed for whether it can actually run.
    ///
    /// Probed concurrently: a fleet with several unreachable local servers would
    /// otherwise stall session startup by the timeout, once per role.
    pub async fn list_roles_probed(&self) -> Vec<RoleInfo> {
        // Cloned rather than probed under the lock: a probe waits on a network timeout,
        // and holding the registry for that long would stall every delegation.
        let snapshot: Vec<(String, crate::roles::Role)> = self
            .registry
            .read()
            .await
            .roles
            .iter()
            .map(|(name, role)| (name.clone(), role.clone()))
            .collect();

        let probes = snapshot
            .iter()
            .map(|(name, role)| async move { (name.clone(), crate::availability::probe(role).await) });
        let results: Vec<_> = futures::future::join_all(probes).await;

        let mut roles = self.list_roles().await;
        for info in &mut roles {
            if let Some((_, probe)) = results.iter().find(|(name, _)| *name == info.name) {
                info.available = Some(probe.available);
                info.unavailable_reason = probe.reason.clone();
            }
        }
        roles
    }

    /// Note that a provider is rate-limited. Delegation to it sheds to fallbacks until
    /// [`Harness::clear_rate_limit`] is called.
    pub async fn mark_rate_limited(&self, provider: &str) {
        let mut limited = self.rate_limited.write().await;
        if !limited.iter().any(|p| p == provider) {
            tracing::warn!("{provider} is rate-limited; shedding to fallback roles");
            limited.push(provider.to_string());
        }
    }

    pub async fn clear_rate_limit(&self, provider: &str) {
        self.rate_limited.write().await.retain(|p| p != provider);
    }

    /// Providers currently shedding to fallbacks, for the limits panel.
    ///
    /// This is the only limit information Claude actually gives us: not how much room is
    /// left, but that we have already run out. Worth showing plainly for that reason.
    pub async fn rate_limited_providers(&self) -> Vec<String> {
        self.rate_limited.read().await.clone()
    }

    pub async fn is_rate_limited(&self, provider: &str) -> bool {
        self.rate_limited.read().await.iter().any(|p| p == provider)
    }

    /// Pick the role that will actually run, shedding to a fallback if the requested
    /// role's provider is rate-limited.
    ///
    /// Follows at most one hop: a fallback chain that is itself limited means the work
    /// runs anyway rather than searching, since the registry forbids cycles but not
    /// chains, and a degraded run beats no run.
    async fn resolve_role(&self, requested: &str) -> Result<String> {
        let registry = self.registry.read().await;
        let role = registry.get(requested)?;

        if !self.is_rate_limited(role.provider.as_str()).await {
            return Ok(requested.to_string());
        }

        match &role.fallback_role {
            Some(fallback) => {
                tracing::info!("{requested} is rate-limited; falling back to {fallback}");
                Ok(fallback.clone())
            }
            None => Ok(requested.to_string()),
        }
    }

    async fn upsert(&self, record: WorkerRecord) {
        self.workers.write().await.insert(record.id.clone(), record);
    }

    async fn set_status(&self, worker_id: &str, status: WorkerStatus) {
        if let Some(record) = self.workers.write().await.get_mut(worker_id) {
            record.status = status;
        }
        self.record(HarnessEvent::WorkerStatusChanged {
            worker_id: worker_id.to_string(),
            status,
        })
        .await;
    }

    /// Run one worker to completion.
    pub async fn delegate(
        self: &Arc<Self>,
        requested_role: &str,
        task: &str,
        context_files: Vec<String>,
    ) -> Result<WorkerRecord> {
        let role_name = self.resolve_role(requested_role).await?;
        // Cloned out of the lock: the worker runs for minutes and must not hold it.
        let role = self.registry.read().await.get(&role_name)?.clone();
        let worker_id = format!("w-{}", Uuid::new_v4().simple());

        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        self.cancels.write().await.insert(worker_id.clone(), stop);

        self.upsert(WorkerRecord {
            id: worker_id.clone(),
            role: role_name.clone(),
            status: WorkerStatus::Queued,
            task: task.to_string(),
            summary: String::new(),
            usage: Usage::default(),
            diff: None,
            branch: None,
            is_error: false,
        })
        .await;

        // Shared-mode workers queue behind each other; say so rather than looking hung.
        if role.isolation == crate::roles::Isolation::Shared {
            self.set_status(&worker_id, WorkerStatus::Blocked).await;
        }

        // Bootstrapping a worktree can take minutes on a cold `npm ci`; without a status
        // of its own the worker just looks hung.
        if matches!(
            role.isolation,
            crate::roles::Isolation::Worktree | crate::roles::Isolation::Readonly
        ) {
            self.set_status(&worker_id, WorkerStatus::Preparing).await;
        }

        let workspace = self
            .workspaces
            .prepare(&worker_id, role.isolation)
            .await
            .context("preparing the worker's workspace")?;

        self.record(HarnessEvent::WorkerSpawned {
            worker_id: worker_id.clone(),
            role: role_name.clone(),
            provider: role.provider.as_str().to_string(),
            model: role.model.clone(),
            isolation: role.isolation.as_str().to_string(),
            cwd: workspace.cwd.display().to_string(),
        })
        .await;

        self.store
            .lock()
            .await
            .create_run(
                &worker_id,
                &self.session_id,
                &role_name,
                role.provider.as_str(),
                role.model.as_deref(),
                role.isolation.as_str(),
            )
            .ok();

        self.set_status(&worker_id, WorkerStatus::Running).await;

        let spec = WorkerSpec {
            run_id: worker_id.clone(),
            role_name: role_name.clone(),
            role: role.clone(),
            task: task.to_string(),
            cwd: workspace.cwd.clone(),
            context_files,
        };

        // Dropping the worker's future kills its process: `claude` and `codex` are spawned
        // with `kill_on_drop`, and an HTTP request is abandoned when its future goes. So
        // one select here stops every backend the same way.
        let outcome = if *stopped.borrow() {
            // Stopped while its workspace was still being prepared: never start it.
            Ok(stopped_outcome())
        } else {
            tokio::select! {
                outcome = agents::run_worker(&spec, &self.events) => outcome,
                _ = stopped.wait_for(|stop| *stop) => Ok(stopped_outcome()),
            }
        };
        self.cancels.write().await.remove(&worker_id);
        // Announced before `WorkerFinished`, which carries only `is_error`: without this a
        // stopped worker would be indistinguishable from a failed one.
        if matches!(&outcome, Ok(o) if o.cancelled) {
            self.set_status(&worker_id, WorkerStatus::Cancelled).await;
        }

        // A backend that failed to launch is still a finished worker, not a lost one.
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!(worker_id = %worker_id, "worker failed: {err:#}");
                agents::RunOutcome {
                    text: format!("{err:#}"),
                    is_error: true,
                    ..Default::default()
                }
            }
        };

        // Commit before teardown so a reviewable diff outlives the worktree.
        let diff = workspace.diff().await.unwrap_or_default();
        let branch = workspace.mergeable_branch().map(str::to_string);
        if branch.is_some() && diff.files_changed > 0 {
            let message = if outcome.cancelled {
                format!("harness: {role_name} ({worker_id}) — stopped before finishing")
            } else {
                format!("harness: {role_name} ({worker_id})")
            };
            workspace.commit(&message).await.ok();
        }
        let workspace_cwd = workspace.cwd.clone();
        if let Err(err) = workspace.release().await {
            tracing::warn!(
                "failed to release workspace {}: {err:#}",
                workspace_cwd.display()
            );
        }

        {
            let store = self.store.lock().await;
            store
                .record_usage(
                    &self.session_id,
                    &worker_id,
                    role.provider.as_str(),
                    role.model.as_deref(),
                    &outcome.usage,
                    outcome.cost_usd,
                )
                .ok();
            store
                .finish_run(
                    &worker_id,
                    if outcome.cancelled {
                        "cancelled"
                    } else if outcome.is_error {
                        "failed"
                    } else {
                        "done"
                    },
                )
                .ok();
            if let Some(backend_session_id) = &outcome.backend_session_id {
                store
                    .set_backend_session_id(&worker_id, backend_session_id)
                    .ok();
            }
        }

        let record = WorkerRecord {
            id: worker_id.clone(),
            role: role_name,
            status: if outcome.cancelled {
                WorkerStatus::Cancelled
            } else if outcome.is_error {
                WorkerStatus::Failed
            } else {
                WorkerStatus::Done
            },
            task: task.to_string(),
            summary: outcome.text.clone(),
            usage: outcome.usage,
            diff: (diff.files_changed > 0).then(|| diff.clone()),
            branch,
            is_error: outcome.is_error,
        };
        self.upsert(record.clone()).await;

        self.record(HarnessEvent::WorkerFinished {
            worker_id,
            summary: outcome.text,
            usage: outcome.usage,
            diff: record.diff.clone(),
            is_error: outcome.is_error,
        })
        .await;

        Ok(record)
    }

    /// Start a worker and return immediately, so the orchestrator can fan out.
    pub async fn delegate_async(
        self: &Arc<Self>,
        role: &str,
        task: &str,
        context_files: Vec<String>,
    ) -> Result<String> {
        // Validate before detaching, so a bad role name is an error the orchestrator sees
        // now rather than a worker that mysteriously fails later.
        let resolved = self.resolve_role(role).await?;
        self.registry.read().await.get(&resolved)?;

        let worker_id = format!("w-{}", Uuid::new_v4().simple());
        self.upsert(WorkerRecord {
            id: worker_id.clone(),
            role: resolved.clone(),
            status: WorkerStatus::Queued,
            task: task.to_string(),
            summary: String::new(),
            usage: Usage::default(),
            diff: None,
            branch: None,
            is_error: false,
        })
        .await;

        let harness = Arc::clone(self);
        let placeholder = worker_id.clone();
        let task = task.to_string();

        tokio::spawn(async move {
            match harness.delegate(&resolved, &task, context_files).await {
                Ok(record) => {
                    // The real run allocated its own id; alias the placeholder to it so
                    // `check_workers` and `collect` can find the result.
                    let mut workers = harness.workers.write().await;
                    workers.insert(placeholder, record);
                }
                Err(err) => {
                    let mut workers = harness.workers.write().await;
                    if let Some(record) = workers.get_mut(&placeholder) {
                        record.status = WorkerStatus::Failed;
                        record.is_error = true;
                        record.summary = format!("{err:#}");
                    }
                }
            }
        });

        Ok(worker_id)
    }

    pub async fn worker(&self, worker_id: &str) -> Option<WorkerRecord> {
        self.workers.read().await.get(worker_id).cloned()
    }

    pub async fn workers(&self) -> Vec<WorkerRecord> {
        let mut all: Vec<_> = self.workers.read().await.values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }

    /// Propose landing a worker's branch. Records the request and surfaces it; it does
    /// not merge. Only [`Harness::approve_merge`], driven by a human click, does that.
    pub async fn request_merge(&self, worker_id: &str) -> Result<DiffStat> {
        let record = self
            .worker(worker_id)
            .await
            .with_context(|| format!("no worker `{worker_id}`"))?;

        let branch = record.branch.clone().with_context(|| {
            format!("worker `{worker_id}` ran with isolation that produces nothing mergeable")
        })?;

        let diff = record.diff.clone().unwrap_or_default();
        if diff.files_changed == 0 {
            bail!("worker `{worker_id}` changed nothing");
        }

        self.pending_merges
            .write()
            .await
            .insert(worker_id.to_string(), branch.clone());

        self.record(HarnessEvent::MergeRequested {
            worker_id: worker_id.to_string(),
            branch,
            diff: diff.clone(),
        })
        .await;

        Ok(diff)
    }

    pub async fn pending_merges(&self) -> Vec<(String, String)> {
        self.pending_merges
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Land a proposed merge. Host-side only — deliberately not exposed as an MCP tool,
    /// so no amount of model output can land code on its own.
    pub async fn approve_merge(&self, worker_id: &str) -> Result<String> {
        let branch = self
            .pending_merges
            .write()
            .await
            .remove(worker_id)
            .with_context(|| format!("no merge pending for `{worker_id}`"))?;

        self.workspaces.merge(&branch).await
    }

    /// Reject a proposed merge and drop its branch.
    pub async fn reject_merge(&self, worker_id: &str) -> Result<()> {
        let branch = self
            .pending_merges
            .write()
            .await
            .remove(worker_id)
            .with_context(|| format!("no merge pending for `{worker_id}`"))?;

        self.workspaces.discard(&branch).await
    }

    /// The diff a worker left on its branch, for review before it is landed.
    pub async fn worker_patch(
        &self,
        worker_id: &str,
        max_lines: usize,
    ) -> Result<crate::isolation::Patch> {
        let record = self
            .worker(worker_id)
            .await
            .with_context(|| format!("no worker `{worker_id}`"))?;

        let branch = record
            .branch
            .with_context(|| format!("worker `{worker_id}` left nothing on a branch"))?;

        self.workspaces.patch(&branch, max_lines).await
    }

    /// Token totals for the rolling window that actually governs a subscription.
    pub async fn usage_window(&self, seconds: i64) -> Result<Vec<crate::store::ProviderUsage>> {
        self.store.lock().await.usage_window(seconds)
    }

    pub async fn session_usage(&self) -> Result<crate::store::ProviderUsage> {
        self.store.lock().await.session_usage(&self.session_id)
    }

    /// Watch an event stream for rate-limit retries and shed load when one appears.
    ///
    /// Wired to the orchestrator's stream, this turns "am I near the limit?" from a guess
    /// into something the engine reacts to.
    pub async fn note_event(&self, event: &HarnessEvent) {
        let head = self.orchestrator_run.read().await.clone();
        let is_head = |run_id: &str| head.as_deref() == Some(run_id);

        // The head agent's side of the conversation is persisted here, because its events
        // never pass through `record` — they come straight off its stdout. Only settled
        // events: partial text deltas would bloat the store and replay as noise.
        let persist = match event {
            HarnessEvent::AssistantText {
                run_id,
                partial: false,
                ..
            }
            | HarnessEvent::ToolCall { run_id, .. }
            | HarnessEvent::RunFinished { run_id, .. }
            | HarnessEvent::Error { run_id, .. } => is_head(run_id),
            _ => false,
        };
        if persist {
            if let Err(err) = self.store.lock().await.append_event(&self.session_id, event) {
                tracing::warn!("failed to persist head-agent event: {err}");
            }
        }

        match event {
            HarnessEvent::SessionStarted {
                run_id,
                backend_session_id: Some(backend_session_id),
                ..
            } if is_head(run_id) => {
                self.store
                    .lock()
                    .await
                    .set_backend_session_id(run_id, backend_session_id)
                    .ok();
            }

            // Claude Code's own subagents, seen only through the head agent's stream. Turned
            // into ordinary workers so the rail, the diff drawer and the merge gate need
            // not know a worker ran natively rather than as a process of ours.
            HarnessEvent::SubagentStarted {
                task_id,
                subagent_type,
                description,
                ..
            } => {
                self.native_started(task_id, subagent_type, description).await;
            }

            HarnessEvent::SubagentProgress {
                task_id,
                description,
                last_tool,
                ..
            } => {
                let worker_id = crate::native::worker_id(task_id);
                if self.worker(&worker_id).await.is_some() {
                    // Emitted, not recorded: progress is for the card, not for history.
                    self.emit(HarnessEvent::ToolCall {
                        run_id: worker_id,
                        tool_use_id: String::new(),
                        name: last_tool.clone().unwrap_or_else(|| "Working".into()),
                        input: serde_json::json!({ "description": description }),
                    });
                }
            }

            HarnessEvent::SubagentFinished {
                task_id,
                status,
                summary,
                total_tokens,
                ..
            } => {
                self.native_finished(task_id, status, summary, *total_tokens).await;
            }

            HarnessEvent::QuotaReport {
                provider,
                status,
                windows,
                ..
            } if provider == "claude" => {
                if !windows.is_empty() {
                    let observed = time::OffsetDateTime::now_utc().unix_timestamp();
                    *self.claude_quota.write().await = Some((observed, windows.clone()));
                }
                // The server saying no is a firmer signal than a retry, and it arrives
                // before the turn has been spent waiting on one.
                if status == "rejected" {
                    self.mark_rate_limited(Provider::Claude.as_str()).await;
                }
            }

            HarnessEvent::ApiRetry { error, .. } if error == "rate_limit" => {
                self.mark_rate_limited(Provider::Claude.as_str()).await;
            }

            HarnessEvent::RunFinished {
                run_id,
                usage,
                cost_usd,
                is_error,
                ..
            } => {
                // Only a successful turn is evidence the limit has lifted.
                if !is_error {
                    self.clear_rate_limit(Provider::Claude.as_str()).await;
                }

                // Recorded whether or not the turn succeeded: a failed or stopped turn
                // still spent quota, and leaving it out would understate burn exactly
                // when something went wrong. Worker usage is recorded by `delegate` once
                // the run settles; recording it here too would double count it.
                if is_head(run_id) {
                    self.store
                        .lock()
                        .await
                        .record_usage(
                            &self.session_id,
                            run_id,
                            Provider::Claude.as_str(),
                            None,
                            usage,
                            *cost_usd,
                        )
                        .ok();
                }
            }

            _ => {}
        }
    }

    /// A native subagent started: register it as a worker.
    async fn native_started(&self, task_id: &str, subagent_type: &str, description: &str) {
        let worker_id = crate::native::worker_id(task_id);
        // A role from roles.toml gets its worktree from our hook. Claude Code's built-in
        // subagents (Explore, general-purpose) have no role here, run in the project with
        // the head's read-only permissions, and are shown without a worktree.
        let role = self.registry.read().await.roles.get(subagent_type).cloned();
        let native = role.as_ref().is_some_and(|role| {
            role.provider == Provider::Claude
                && matches!(role.isolation, crate::roles::Isolation::Worktree | crate::roles::Isolation::Readonly)
        });
        let isolation = if native {
            role.as_ref().map(|r| r.isolation).unwrap_or_default()
        } else {
            crate::roles::Isolation::None
        };
        let model = role.as_ref().and_then(|r| r.model.clone());
        let cwd = if native {
            self.workspaces
                .project_root()
                .join(crate::isolation::WORKTREE_DIR)
                .join(&worker_id)
        } else {
            self.workspaces.project_root().to_path_buf()
        };

        self.upsert(WorkerRecord {
            id: worker_id.clone(),
            role: subagent_type.to_string(),
            status: WorkerStatus::Running,
            task: description.to_string(),
            summary: String::new(),
            usage: Usage::default(),
            diff: None,
            branch: None,
            is_error: false,
        })
        .await;
        self.store
            .lock()
            .await
            .create_run(
                &worker_id,
                &self.session_id,
                subagent_type,
                Provider::Claude.as_str(),
                model.as_deref(),
                isolation.as_str(),
            )
            .ok();
        self.record(HarnessEvent::WorkerSpawned {
            worker_id,
            role: subagent_type.to_string(),
            provider: Provider::Claude.as_str().to_string(),
            model,
            isolation: isolation.as_str().to_string(),
            cwd: cwd.display().to_string(),
        })
        .await;
    }

    /// A native subagent finished: keep its work on its branch, account for what it spent,
    /// and put anything it changed in front of the person for review.
    ///
    /// Claude Code leaves a subagent's worktree on disk, uncommitted, when it made changes
    /// — so this is where the harness does what it does for its own workers: commit to the
    /// branch, then remove the worktree. The merge is proposed automatically, because the
    /// head agent cannot see the work in the checkout (by design) and would otherwise have
    /// to be told a worker id it never chose. Proposing lands nothing; only the person can.
    async fn native_finished(&self, task_id: &str, status: &str, summary: &str, total_tokens: u64) {
        let worker_id = crate::native::worker_id(task_id);
        let Some(record) = self.worker(&worker_id).await else {
            return;
        };
        let role = self.registry.read().await.roles.get(&record.role).cloned();
        let isolation = role.as_ref().map(|r| r.isolation).unwrap_or_default();

        let mut diff = None;
        let mut branch = None;
        if let Some(workspace) = self.workspaces.adopt(&worker_id, isolation) {
            let stat = workspace.diff().await.unwrap_or_default();
            if stat.files_changed > 0 && isolation.is_mergeable() {
                workspace
                    .commit(&format!("harness: {} ({worker_id})", record.role))
                    .await
                    .ok();
                branch = workspace.mergeable_branch().map(str::to_string);
                diff = Some(stat);
            }
            if let Err(err) = workspace.release().await {
                tracing::warn!("failed to release native worktree {worker_id}: {err:#}");
            }
        }

        let is_error = status != "completed";
        // The notification reports one total, not an input/output split. It is recorded as
        // input so the window's total is right; the split is not knowable from here.
        let usage = Usage {
            input_tokens: total_tokens,
            ..Default::default()
        };
        {
            let store = self.store.lock().await;
            store
                .record_usage(
                    &self.session_id,
                    &worker_id,
                    Provider::Claude.as_str(),
                    role.as_ref().and_then(|r| r.model.as_deref()),
                    &usage,
                    None,
                )
                .ok();
            store
                .finish_run(&worker_id, if is_error { "failed" } else { "done" })
                .ok();
        }

        let finished = WorkerRecord {
            status: if is_error { WorkerStatus::Failed } else { WorkerStatus::Done },
            summary: summary.to_string(),
            usage,
            diff: diff.clone(),
            branch: branch.clone(),
            is_error,
            ..record
        };
        self.upsert(finished).await;
        self.record(HarnessEvent::WorkerFinished {
            worker_id: worker_id.clone(),
            summary: summary.to_string(),
            usage,
            diff,
            is_error,
        })
        .await;

        if branch.is_some() {
            if let Err(err) = self.request_merge(&worker_id).await {
                tracing::warn!("could not propose {worker_id} for review: {err:#}");
            }
        }
    }

    /// Persist and broadcast an event on behalf of the head agent — what the person
    /// typed, or that they stopped a turn. These originate in the app rather than in the
    /// agent's output, so nothing else would record them.
    pub async fn record_head_event(&self, event: HarnessEvent) {
        self.record(event).await;
    }

    /// The head agent's conversation for a project, across every session it has had.
    ///
    /// Each app start is a new session, so a per-session replay would show nothing
    /// after a restart. Newest last, capped at `limit` events.
    pub async fn project_history(&self, limit: usize) -> Result<Vec<HarnessEvent>> {
        let root = self.workspaces.project_root();
        let key = root
            .canonicalize()
            .unwrap_or_else(|_| root.to_path_buf())
            .display()
            .to_string();
        self.store.lock().await.project_history(&key, limit)
    }
}

/// What a stopped worker reports. Worded for the head agent as much as the person: it
/// should not retry work someone deliberately stopped.
fn stopped_outcome() -> agents::RunOutcome {
    agents::RunOutcome {
        text: "Stopped by the user before finishing. Anything written so far was kept on \
               the branch; do not retry this task unless asked."
            .into(),
        is_error: true,
        cancelled: true,
        ..Default::default()
    }
}
