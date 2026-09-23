import type { DetectedBackend, Role, Worker, WorkerActivity } from "./types";
import { totalInput } from "./types";

type Props = {
  workers: Worker[];
  roles: Role[];
  backends: DetectedBackend[];
  selected: string | null;
  onSelect: (id: string) => void;
  onChangeFleet: () => void;
  onStopWorker: (id: string) => void;
};

/** Statuses a stop can still act on. */
const STOPPABLE = new Set(["queued", "blocked", "preparing", "running"]);

/** One line for the card: the tool and what it touched, or the last thing it said. */
export function describeActivity(item: WorkerActivity): string {
  if (item.kind === "tool") return item.detail ? `${item.name} ${item.detail}` : item.name;
  const line = item.text.split("\n")[0];
  return line.length > 90 ? `${line.slice(0, 87)}…` : line;
}

const STATUS_LABEL: Record<Worker["status"], string> = {
  queued: "queued",
  blocked: "waiting for the shared lock",
  preparing: "preparing worktree",
  running: "running",
  done: "done",
  failed: "failed",
  cancelled: "stopped",
};

export default function WorkerRail({
  workers,
  roles,
  backends,
  selected,
  onSelect,
  onChangeFleet,
  onStopWorker,
}: Props) {
  return (
    <aside className="rail">
      <div className="rail-head">
        <h2>Workers</h2>
        <button className="link" onClick={onChangeFleet}>
          Change fleet
        </button>
      </div>

      <h3 className="first">Detected</h3>
      <ul className="backend-inventory">
        {backends.map((backend) => (
          <li key={backend.id} title={backend.message}>
            <span className={`backend-dot ${backend.available ? "ready" : "down"}`} />
            <span>
              <span className="backend-label">{backend.label}</span>
              <span className={`muted mono fleet-backend ${backend.available ? "" : "wrap"}`}>
                {backend.available
                  ? backend.models
                      .filter((model) => model.capability === "chat")
                      .map((model) => model.id)
                      .join(", ") || "connected"
                  : // A missing CLI says how to get it; a missing server just is not running.
                    backend.base_url
                    ? "not detected"
                    : backend.message}
              </span>
            </span>
          </li>
        ))}
      </ul>

      {workers.length === 0 && (
        <div className="rail-empty">
          <p className="muted">Nothing delegated yet.</p>
          <h3>Fleet</h3>
          <ul className="fleet">
            {roles.map((role) => (
              <li
                key={role.name}
                className={role.available ? "" : "unavailable"}
                title={role.unavailable_reason ?? role.brief ?? undefined}
              >
                <div className="fleet-row">
                  <span className="role-name">{role.name}</span>
                  <div className="fleet-badges">
                    <span className={`badge ${role.can_edit_files ? "write" : "read"}`}>
                      {role.isolation}
                    </span>
                    {!role.available && <span className="badge limited">unavailable</span>}
                  </div>
                </div>
                <span className="muted mono fleet-backend">
                  {role.provider}
                  {role.model ? `/${role.model}` : ""}
                </span>
                {!role.available && (
                  // The reason used to live only in a tooltip, which is where nobody looks.
                  <span className="fleet-reason">
                    {role.unavailable_reason ?? "backend not reachable"}{" "}
                    <button className="link" onClick={onChangeFleet}>
                      Fix
                    </button>
                  </span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}

      {workers.map((worker) => (
        // The stop control sits beside the card rather than inside it: the card is itself
        // a button, and a button nested in a button is not valid markup.
        <div key={worker.id} className="worker-card-wrap">
        {STOPPABLE.has(worker.status) && (
          <button
            className="worker-stop"
            onClick={() => onStopWorker(worker.id)}
            title="Stop this worker — anything it has written is kept"
            aria-label={`Stop ${worker.role}`}
          >
            Stop
          </button>
        )}
        <button
          className={`worker-card ${worker.status} ${selected === worker.id ? "selected" : ""}`}
          onClick={() => onSelect(worker.id)}
        >
          <div className="worker-head">
            <span className="role-name">{worker.role}</span>
            <span className={`status ${worker.status}`}>{STATUS_LABEL[worker.status]}</span>
          </div>

          <div className="worker-meta muted mono">
            {worker.provider} · {worker.isolation}
          </div>

          {/* What it is doing right now, while it runs — the rail used to show only that
              a worker was running, never what it was running. */}
          {!worker.summary && worker.activity.length > 0 && (
            <p className="worker-now mono">{describeActivity(worker.activity[worker.activity.length - 1])}</p>
          )}

          {worker.summary && <p className="worker-summary">{worker.summary}</p>}

          <div className="worker-foot muted mono">
            <span>
              {totalInput(worker.usage).toLocaleString()} in /{" "}
              {worker.usage.output_tokens.toLocaleString()} out
            </span>
            {worker.diff && worker.diff.files_changed > 0 && (
              <span className="diff-chip">
                {worker.diff.files_changed} file{worker.diff.files_changed === 1 ? "" : "s"}
                {worker.branch ? " · review" : ""}
              </span>
            )}
            {worker.verification &&
              (worker.verification.state === "running" ? (
                <span className="badge risk-pending">checking</span>
              ) : (
                <span
                  className={`badge risk-${worker.verification.report.verified ? worker.verification.report.risk : "unverified"}`}
                  title={worker.verification.report.reasons.join("\n")}
                >
                  {worker.verification.report.verified
                    ? `${worker.verification.report.risk} risk`
                    : "unverified"}
                </span>
              ))}
          </div>
        </button>
        </div>
      ))}
    </aside>
  );
}
