import type { DetectedBackend, Role, Worker } from "./types";
import { totalInput } from "./types";

type Props = {
  workers: Worker[];
  roles: Role[];
  backends: DetectedBackend[];
  selected: string | null;
  onSelect: (id: string) => void;
};

const STATUS_LABEL: Record<Worker["status"], string> = {
  queued: "queued",
  blocked: "waiting for the shared lock",
  running: "running",
  done: "done",
  failed: "failed",
  cancelled: "cancelled",
};

export default function WorkerRail({ workers, roles, backends, selected, onSelect }: Props) {
  return (
    <aside className="rail">
      <h2>Workers</h2>

      <h3 className="first">Detected</h3>
      <ul className="backend-inventory">
        {backends.map((backend) => (
          <li key={backend.id} title={backend.message}>
            <span className={`backend-dot ${backend.available ? "ready" : "down"}`} />
            <span>
              <span className="backend-label">{backend.label}</span>
              <span className="muted mono fleet-backend">
                {backend.available
                  ? backend.models
                      .filter((model) => model.capability === "chat")
                      .map((model) => model.id)
                      .join(", ") || "connected"
                  : "not detected"}
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
              </li>
            ))}
          </ul>
        </div>
      )}

      {workers.map((worker) => (
        <button
          key={worker.id}
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
          </div>
        </button>
      ))}
    </aside>
  );
}
