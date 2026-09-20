import type { Worker } from "./types";

type Props = {
  worker: Worker;
  onClose: () => void;
  onApprove: () => void;
  onReject: () => void;
};

/**
 * Review surface for a worker's changes.
 *
 * This is the only place work can land. The orchestrator can propose a merge; approving
 * it is a human action, and the engine exposes no path for a model to do it.
 */
export default function DiffDrawer({ worker, onClose, onApprove, onReject }: Props) {
  const diff = worker.diff;
  const awaitingReview = Boolean(worker.branch && diff && diff.files_changed > 0);

  return (
    <div className="drawer-scrim" onClick={onClose}>
      <aside className="drawer" onClick={(e) => e.stopPropagation()}>
        <header>
          <div>
            <h2>{worker.role}</h2>
            <p className="muted mono">
              {worker.id} · {worker.provider} · {worker.isolation}
            </p>
          </div>
          <button className="ghost" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </header>

        {worker.summary && (
          <section>
            <h3>Result</h3>
            <p className={worker.is_error ? "error" : ""}>{worker.summary}</p>
          </section>
        )}

        {diff && diff.files_changed > 0 ? (
          <section>
            <h3>
              Changes <span className="muted mono">+{diff.insertions} −{diff.deletions}</span>
            </h3>
            <ul className="file-list mono">
              {diff.files.map((file) => (
                <li key={file}>{file}</li>
              ))}
            </ul>
            {worker.branch && <p className="muted mono">on {worker.branch}</p>}
          </section>
        ) : (
          <section>
            <p className="muted">
              {worker.isolation === "readonly"
                ? "Read-only worker — it produces findings, not changes."
                : "This worker changed nothing."}
            </p>
          </section>
        )}

        {awaitingReview && (
          <footer>
            <p className="muted">
              Nothing has landed yet. Approving merges this branch into your checkout.
            </p>
            <div className="actions">
              <button className="danger" onClick={onReject}>
                Discard
              </button>
              <button className="primary" onClick={onApprove}>
                Merge
              </button>
            </div>
          </footer>
        )}
      </aside>
    </div>
  );
}
