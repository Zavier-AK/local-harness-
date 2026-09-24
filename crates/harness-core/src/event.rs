//! The normalized event vocabulary.
//!
//! Every agent backend — the Claude CLI's stream-json, Codex's `exec --json`, a plain
//! OpenAI-compatible HTTP response — is translated into these events. Nothing downstream
//! (the store, the CLI renderer, the Tauri UI) knows which CLI produced what.

use serde::{Deserialize, Serialize};

/// Token accounting for a single run. Fields mirror the Anthropic usage block; backends
/// that report less simply leave the cache fields at zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Everything the model had to look at, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}

/// Lifecycle of a delegated worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Queued,
    /// Waiting on the shared-isolation advisory lock.
    Blocked,
    /// Building the worktree: copying files in and installing dependencies.
    Preparing,
    /// Proposed by the head agent, waiting for the person to approve it (autonomy `Ask`).
    AwaitingApproval,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl WorkerStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Summary of what a worktree-isolated worker changed.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Repo-relative paths, for the UI's file list.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// A backend session came up. `provider` is the Claude CLI's `firstParty` / `console`
    /// distinction where available — the signal that OAuth, not an API key, is in play.
    SessionStarted {
        run_id: String,
        backend_session_id: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        tools: Vec<String>,
        mcp_servers: Vec<String>,
        /// Servers from `mcp_servers` that did not connect (failed, or waiting on auth).
        #[serde(default)]
        mcp_failed: Vec<String>,
    },
    AssistantText {
        run_id: String,
        text: String,
        /// True for incremental deltas, false for a settled block.
        partial: bool,
    },
    Thinking {
        run_id: String,
        text: String,
    },
    ToolCall {
        run_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        run_id: String,
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    WorkerSpawned {
        worker_id: String,
        role: String,
        provider: String,
        model: Option<String>,
        isolation: String,
        cwd: String,
    },
    WorkerStatusChanged {
        worker_id: String,
        status: WorkerStatus,
    },
    WorkerFinished {
        worker_id: String,
        summary: String,
        usage: Usage,
        diff: Option<DiffStat>,
        is_error: bool,
    },
    /// A merge the orchestrator proposed. Never acted on without a human click.
    MergeRequested {
        worker_id: String,
        branch: String,
        diff: DiffStat,
    },
    /// A delegation the person must approve before it runs (autonomy `Ask`).
    DelegationRequested {
        worker_id: String,
        role: String,
        task: String,
    },
    DelegationApproved {
        worker_id: String,
    },
    DelegationDeclined {
        worker_id: String,
        reason: String,
    },
    /// A merge landed — by a person's click, or by itself under an autonomy level that
    /// allows it.
    MergeLanded {
        worker_id: String,
        branch: String,
        commit: String,
        automatic: bool,
        /// What verification made of it, when it was checked.
        #[serde(default)]
        risk: Option<crate::verify::Risk>,
    },
    /// A merge the autonomy level would have landed, that could not be (a conflict, most
    /// likely). It stays proposed for the person.
    MergeNotLanded {
        worker_id: String,
        reason: String,
    },
    /// A landed merge, undone.
    MergeReverted {
        worker_id: String,
        commit: String,
    },
    /// A plan was proposed, edited, or moved on — the whole plan, so the board redraws
    /// from one event.
    PlanUpdated {
        plan: crate::plan::Plan,
    },
    /// The night shift moved on: started, scored, tried something, or ended.
    NightUpdated {
        report: crate::night::NightReport,
    },
    /// Every step of a running plan reached a final state.
    PlanFinished {
        plan_id: String,
        title: String,
        outcome: String,
    },
    /// Checks began on a proposed merge's branch.
    VerificationStarted {
        worker_id: String,
    },
    /// One check finished; the card fills in as they do.
    VerificationCheck {
        worker_id: String,
        check: crate::verify::Check,
    },
    /// Every check is done and the change has a risk level.
    VerificationFinished {
        worker_id: String,
        report: crate::verify::VerificationReport,
    },
    /// The subscription's own account of how much of each limit window is used.
    ///
    /// Claude's stream-json output carries a `rate_limit_event` with every turn —
    /// server-reported utilization and reset times for the five-hour and weekly windows.
    /// A real figure, not an estimate from token counts.
    QuotaReport {
        run_id: String,
        provider: String,
        /// `allowed`, `allowed_warning`, or `rejected` once a limit is hit.
        status: String,
        windows: Vec<crate::quota::QuotaWindow>,
    },

    /// The head agent started one of Claude Code's own subagents (`system/task_started`).
    SubagentStarted {
        run_id: String,
        task_id: String,
        tool_use_id: String,
        /// The agent definition it runs — for a native role, the role's name.
        subagent_type: String,
        description: String,
    },

    /// A running subagent's progress (`system/task_progress`): the tool it last used and
    /// what it is doing, in the CLI's own words.
    SubagentProgress {
        run_id: String,
        task_id: String,
        description: String,
        last_tool: Option<String>,
        total_tokens: u64,
    },

    /// A subagent finished (`system/task_notification`).
    SubagentFinished {
        run_id: String,
        task_id: String,
        status: String,
        summary: String,
        /// Every token the subagent spent across all its requests. The head agent's own
        /// `result` usage does not include these.
        total_tokens: u64,
    },

    /// The person stopped the head agent's turn. The turn's own `RunFinished` still
    /// follows, with `is_error` set — this is what lets a UI show it as stopped rather
    /// than failed.
    TurnInterrupted { run_id: String },

    /// What the person typed to the head agent. Recorded so a conversation can be
    /// shown again after a restart; the CLI's own transcript is not ours to read.
    UserMessage { run_id: String, text: String },

    /// Emitted from the Claude CLI's `system/api_retry`. `error` carries the category,
    /// e.g. `rate_limit` — the hook for backpressure.
    ApiRetry {
        run_id: String,
        attempt: u32,
        max_retries: u32,
        retry_delay_ms: u64,
        error: String,
    },
    RunFinished {
        run_id: String,
        text: String,
        usage: Usage,
        /// Client-side estimate, and notional under subscription auth. A burn signal, not a bill.
        cost_usd: Option<f64>,
        is_error: bool,
    },
    Error {
        run_id: String,
        message: String,
    },
}

impl HarnessEvent {
    /// The run or worker this event belongs to, for routing in the UI.
    pub fn stream_key(&self) -> &str {
        match self {
            Self::SessionStarted { run_id, .. }
            | Self::AssistantText { run_id, .. }
            | Self::Thinking { run_id, .. }
            | Self::ToolCall { run_id, .. }
            | Self::ToolResult { run_id, .. }
            | Self::ApiRetry { run_id, .. }
            | Self::TurnInterrupted { run_id }
            | Self::QuotaReport { run_id, .. }
            | Self::SubagentStarted { run_id, .. }
            | Self::SubagentProgress { run_id, .. }
            | Self::SubagentFinished { run_id, .. }
            | Self::UserMessage { run_id, .. }
            | Self::RunFinished { run_id, .. }
            | Self::Error { run_id, .. } => run_id,
            Self::WorkerSpawned { worker_id, .. }
            | Self::WorkerStatusChanged { worker_id, .. }
            | Self::WorkerFinished { worker_id, .. }
            | Self::MergeRequested { worker_id, .. }
            | Self::DelegationRequested { worker_id, .. }
            | Self::DelegationApproved { worker_id }
            | Self::DelegationDeclined { worker_id, .. }
            | Self::MergeLanded { worker_id, .. }
            | Self::MergeNotLanded { worker_id, .. }
            | Self::MergeReverted { worker_id, .. }
            | Self::VerificationStarted { worker_id }
            | Self::VerificationCheck { worker_id, .. }
            | Self::VerificationFinished { worker_id, .. } => worker_id,
            Self::PlanUpdated { plan } => &plan.id,
            Self::PlanFinished { plan_id, .. } => plan_id,
            Self::NightUpdated { report } => &report.id,
        }
    }
}
