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
  | "running"
  | "done"
  | "failed"
  | "cancelled";

export type HarnessEvent =
  | { type: "session_started"; run_id: string; backend_session_id: string | null; provider: string | null; model: string | null; tools: string[]; mcp_servers: string[] }
  | { type: "assistant_text"; run_id: string; text: string; partial: boolean }
  | { type: "thinking"; run_id: string; text: string }
  | { type: "tool_call"; run_id: string; tool_use_id: string; name: string; input: unknown }
  | { type: "tool_result"; run_id: string; tool_use_id: string; content: string; is_error: boolean }
  | { type: "worker_spawned"; worker_id: string; role: string; provider: string; model: string | null; isolation: string; cwd: string }
  | { type: "worker_status_changed"; worker_id: string; status: WorkerStatus }
  | { type: "worker_finished"; worker_id: string; summary: string; usage: Usage; diff: DiffStat | null; is_error: boolean }
  | { type: "merge_requested"; worker_id: string; branch: string; diff: DiffStat }
  | { type: "api_retry"; run_id: string; attempt: number; max_retries: number; retry_delay_ms: number; error: string }
  | { type: "run_finished"; run_id: string; text: string; usage: Usage; cost_usd: number | null; is_error: boolean }
  | { type: "error"; run_id: string; message: string };

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
};

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
  | { kind: "tool"; name: string; workerId?: string }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" };

export const totalInput = (u: Usage) =>
  u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
