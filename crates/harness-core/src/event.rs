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
            | Self::RunFinished { run_id, .. }
            | Self::Error { run_id, .. } => run_id,
            Self::WorkerSpawned { worker_id, .. }
            | Self::WorkerStatusChanged { worker_id, .. }
            | Self::WorkerFinished { worker_id, .. }
            | Self::MergeRequested { worker_id, .. } => worker_id,
        }
    }
}
