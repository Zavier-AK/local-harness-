import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Patch, Worker } from "./types";

type Props = {
  worker: Worker;
  onClose: () => void;
  onApprove: () => void;
  onReject: () => void;
};

/** Classify a unified-diff line so it can be coloured. */
function lineKind(line: string): "add" | "del" | "meta" | "hunk" | "ctx" {
  if (line.startsWith("+++") || line.startsWith("---")) return "meta";
  if (line.startsWith("diff ") || line.startsWith("index ")) return "meta";
  if (line.startsWith("@@")) return "hunk";
  if (line.startsWith("+")) return "add";
  if (line.startsWith("-")) return "del";
  return "ctx";
}

/**
 * Review surface for a worker's changes.
 *
 * This is the only place work can land. The orchestrator can propose a merge; approving
 * it is a human action, and the engine exposes no path for a model to do it.
 */
export default function DiffDrawer({ worker, onClose, onApprove, onReject }: Props) {
  const diff = worker.diff;
  const awaitingReview = Boolean(worker.branch && diff && diff.files_changed > 0);

  const [patch, setPatch] = useState<Patch | null>(null);
  const [patchError, setPatchError] = useState<string | null>(null);
  const [loadingPatch, setLoadingPatch] = useState(false);

  useEffect(() => {
    if (!worker.branch) {
      setPatch(null);
      return;
    }

    let cancelled = false;
    setLoadingPatch(true);
    setPatchError(null);

    invoke<Patch>("worker_patch", { workerId: worker.id })
      .then((result) => {
        if (!cancelled) setPatch(result);
      })
      .catch((err) => {
        if (!cancelled) setPatchError(String(err));
      })
      .finally(() => {
        if (!cancelled) setLoadingPatch(false);
      });

    return () => {
      cancelled = true;
    };
  }, [worker.id, worker.branch]);

  // Escape closes, so reviewing never traps you in the drawer.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

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
          <section className="diff-section">
            <h3>
              Changes{" "}
              <span className="muted mono">
                {diff.files_changed} file{diff.files_changed === 1 ? "" : "s"} · +
                {diff.insertions} −{diff.deletions}
              </span>
            </h3>

            {loadingPatch && <p className="muted">Reading the diff…</p>}
            {patchError && <p className="error">{patchError}</p>}

            {patch && (
              <>
                <pre className="patch">
                  {patch.text.split("\n").map((line, i) => (
                    <div key={i} className={`dl ${lineKind(line)}`}>
                      {line || " "}
                    </div>
                  ))}
                </pre>
                {patch.truncated && (
                  <p className="muted">
                    Showing the first {patch.text.split("\n").length} of{" "}
                    {patch.total_lines} lines.
                    {worker.branch && (
                      <>
                        {" "}
                        Full diff: <code className="mono">git diff HEAD...{worker.branch}</code>
                      </>
                    )}
                  </p>
                )}
              </>
            )}

            {!patch && !loadingPatch && !patchError && (
              <ul className="file-list mono">
                {diff.files.map((file) => (
                  <li key={file}>{file}</li>
                ))}
              </ul>
            )}

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
