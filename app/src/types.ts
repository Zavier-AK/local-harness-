/** Mirrors `harness_core::event::HarnessEvent` — serde tags the variant as `type`. */
export type Usage = {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
};

export type DiffStat = {
  files_changed: number;
  insertions: number;
  deletions: number;
  files: string[];
};

export type WorkerStatus =
  | "queued"
  | "blocked"
  | "preparing"
  | "awaiting_approval"
  | "running"
  | "done"
  | "failed"
  | "cancelled";

export type HarnessEvent =
  | { type: "session_started"; run_id: string; backend_session_id: string | null; provider: string | null; model: string | null; tools: string[]; mcp_servers: string[]; mcp_failed: string[] }
  | { type: "assistant_text"; run_id: string; text: string; partial: boolean }
  | { type: "thinking"; run_id: string; text: string }
  | { type: "tool_call"; run_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_result"; run_id: string; tool_use_id: string; content: string; is_error: boolean }
  | { type: "worker_spawned"; worker_id: string; role: string; provider: string; model: string | null; isolation: string; cwd: string }
  | { type: "worker_status_changed"; worker_id: string; status: WorkerStatus }
  | { type: "worker_finished"; worker_id: string; summary: string; usage: Usage; diff: DiffStat | null; is_error: boolean }
  | { type: "merge_requested"; worker_id: string; branch: string; diff: DiffStat }
  | { type: "delegation_requested"; worker_id: string; role: string; task: string }
  | { type: "delegation_approved"; worker_id: string }
  | { type: "delegation_declined"; worker_id: string; reason: string }
  | { type: "merge_landed"; worker_id: string; branch: string; commit: string; automatic: boolean; risk: Risk | null }
  | { type: "merge_not_landed"; worker_id: string; reason: string }
  | { type: "merge_reverted"; worker_id: string; commit: string }
  | { type: "plan_updated"; plan: Plan }
  | { type: "plan_finished"; plan_id: string; title: string; outcome: string }
  | { type: "verification_started"; worker_id: string }
  | { type: "verification_check"; worker_id: string; check: VerifyCheck }
  | { type: "verification_finished"; worker_id: string; report: VerificationReport }
  | { type: "api_retry"; run_id: string; attempt: number; max_retries: number; retry_delay_ms: number; error: string }
  | { type: "run_finished"; run_id: string; text: string; usage: Usage; cost_usd: number | null; is_error: boolean }
  | { type: "error"; run_id: string; message: string }
  | { type: "turn_interrupted"; run_id: string }
  | {
      type: "quota_report";
      run_id: string;
      provider: string;
      status: string;
      windows: QuotaWindow[];
    }
  | { type: "user_message"; run_id: string; text: string };

/** Mirrors `harness_core::isolation::Patch`. */
export type Patch = {
  text: string;
  truncated: boolean;
  total_lines: number;
};

export type Role = {
  name: string;
  provider: string;
  model: string | null;
  isolation: string;
  can_edit_files: boolean;
  brief: string | null;
  available: boolean;
  unavailable_reason: string | null;
};

/** What a candidate project directory offers, checked before starting a session. */
export type ProjectStatus = {
  exists: boolean;
  is_git_repo: boolean;
  has_roles_file: boolean;
};

export type SessionInfo = {
  session_id: string;
  project_root: string;
  mcp_url: string;
  roles: Role[];
  backends: DetectedBackend[];
  resumed_head_session: boolean;
};

export type DetectedModel = {
  id: string;
  capability: "chat" | "embedding";
};

export type DetectedBackend = {
  id: string;
  label: string;
  available: boolean;
  message: string;
  base_url: string | null;
  models: DetectedModel[];
};

export type ModelOption = {
  /** Codex `-c key=value` overrides this option implies, e.g. model_provider. */
  provider_opts?: Record<string, string>;
  id: string;
  label: string;
  provider: string;
  model: string;
  base_url: string | null;
};

export type ConfigurableRole = {
  name: string;
  provider: string;
  model: string | null;
  base_url: string | null;
  isolation: string;
  options: ModelOption[];
  blocked_reason: string | null;
};

export type FleetInspection = {
  backends: DetectedBackend[];
  roles: ConfigurableRole[];
};

export type RoleModelPatch = {
  /** Move the role onto a different backend. Omitted keeps the current one. */
  provider?: string | null;
  provider_opts?: Record<string, string>;
  role_name: string;
  model: string;
  base_url: string | null;
};

export type Worker = {
  id: string;
  role: string;
  provider: string;
  isolation: string;
  cwd: string;
  status: WorkerStatus;
  summary: string;
  usage: Usage;
  diff: DiffStat | null;
  branch: string | null;
  is_error: boolean;
  startedAt: number;
  /** The tool the worker is using right now, while it runs. */
  currentTool: string | null;
  /** Recent tool calls and messages, newest last — what the worker is actually doing. */
  activity: WorkerActivity[];
  /** Checks on its proposed merge, once one is proposed. */
  verification: Verification | null;
  /** What it was asked to do — shown while it waits for approval. */
  task?: string;
  /** Set once its merge landed, so it can be undone. */
  landed?: { commit: string; automatic: boolean } | null;
};

/** How much runs without the person. Mirrors `Autonomy` in the engine. */
export type Autonomy = "ask" | "review" | "land_safe" | "land_most";

// ---------- Verification before merge ----------

export type Risk = "low" | "medium" | "high";

export type VerifyFinding = {
  severity: string;
  file?: string | null;
  line?: number | null;
  message: string;
};

export type VerifyCheck = {
  kind: "signals" | "command" | "review";
  name: string;
  status: "passed" | "failed" | "skipped" | "error";
  summary: string;
  output?: string;
  risk?: Risk;
  findings?: VerifyFinding[];
  reviewer?: string;
};

export type VerificationReport = {
  risk: Risk;
  /** False when nothing actually checked the change. */
  verified: boolean;
  reasons: string[];
  checks: VerifyCheck[];
};

export type Verification =
  | { state: "running"; checks: VerifyCheck[] }
  | { state: "done"; report: VerificationReport };

export type UsageRow = {
  provider: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  cost_usd: number;
  runs: number;
};

/** One rendered item in the head transcript. */
export type ChatItem =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "tool"; name: string; toolUseId?: string; workerId?: string }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" }
  /** A merge that landed, with the way back. */
  | { kind: "landed"; workerId: string; text: string; undone: boolean };

export const totalInput = (u: Usage) =>
  u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;

/** One open project, as the switcher sees it. Mirrors `ProjectView` in the Tauri shell. */
export type ProjectView = {
  project_root: string;
  name: string;
  active: boolean;
  /** Whether its head agent is up. A suspended project costs no subscription quota. */
  live: boolean;
  running_workers: number;
  pending_merges: number;
};

/**
 * Engine events are tagged with their project.
 *
 * Every open project emits on one Tauri channel, and `HarnessEvent` carries only run and
 * worker ids, so without this tag two projects' streams could not be told apart.
 */
export type ProjectHarnessEvent = HarnessEvent & { project: string };

export type QuotaState = "available" | "stale" | "missing";

export type QuotaWindow = {
  label: string;
  used_percent: number;
  /** Unix seconds when the window resets, when the vendor says. */
  resets_at: number | null;
};

/**
 * What a provider says is left.
 *
 * `missing` is rendered as unknown, never as zero — Claude exposes no readable quota, so
 * a percentage for it would be invented rather than measured.
 */
export type ProviderQuota = {
  provider: string;
  state: QuotaState;
  observed_at: number | null;
  windows: QuotaWindow[];
  note: string | null;
};

export type QuotaReport = {
  providers: ProviderQuota[];
  rate_limited: string[];
};

/** One line of a worker's live activity. */
export type WorkerActivity =
  | { kind: "tool"; name: string; detail: string | null }
  | { kind: "text"; text: string };

// ---------- Tools & Skills, Settings ----------

export type SkillInfo = {
  name: string;
  description: string;
  source: "bundled" | "library";
  enabled: boolean;
  origin: string | null;
  path: string | null;
};

export type ImportReport = {
  imported: string[];
  skipped: [string, string][];
};

/** Claude Code's own `mcpServers` entry shape. */
export type McpServerConfig = {
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  type?: string;
  url?: string;
  headers?: Record<string, string>;
};

export type RoleTools = {
  name: string;
  provider: string;
  isolation: string;
  tools: string[];
  native: boolean;
};

export type AppSettings = {
  default_model: string | null;
  max_turns: number;
  notifications: boolean;
  accent: string | null;
};

/** What the head agent's CLI reported about MCP servers when it last started. */
export type McpStatus = { connected: string[]; failed: string[] };

// ---------- Plan board ----------

export type StepInput = {
  id: string;
  title: string;
  role: string;
  task: string;
  context_files: string[];
  depends_on: string[];
};

export type StepState =
  | "planned"
  | "waiting"
  | "running"
  | "checking"
  | "review"
  | "landed"
  | "failed"
  | "skipped";

export type PlanStep = StepInput & {
  state: StepState;
  worker_id: string | null;
  note: string | null;
};

export type Plan = {
  id: string;
  title: string;
  summary: string;
  status: "draft" | "running" | "finished" | "discarded";
  steps: PlanStep[];
};
