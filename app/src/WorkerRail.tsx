import type { Role, Worker } from "./types";
import { totalInput } from "./types";

type Props = {
  workers: Worker[];
  roles: Role[];
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

export default function WorkerRail({ workers, roles, selected, onSelect }: Props) {
  return (
    <aside className="rail">
      <h2>Workers</h2>

      {workers.length === 0 && (
        <div className="rail-empty">
          <p className="muted">Nothing delegated yet.</p>
          <h3>Fleet</h3>
          <ul className="fleet">
            {roles.map((role) => (
              <li key={role.name}>
                <span className="role-name">{role.name}</span>
                <span className="muted mono">
                  {role.provider}
                  {role.model ? `/${role.model}` : ""}
                </span>
                <span className={`badge ${role.can_edit_files ? "write" : "read"}`}>
                  {role.isolation}
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
