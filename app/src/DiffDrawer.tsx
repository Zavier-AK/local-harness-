import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Patch, Verification, VerifyCheck, Worker } from "./types";
import { describeActivity } from "./WorkerRail";

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

  // Events fill this in live; asking covers a drawer opened after they went by.
  const [fetched, setFetched] = useState<Verification | null>(null);
  useEffect(() => {
    if (worker.verification || !worker.branch) return;
    let cancelled = false;
    invoke<Verification | null>("worker_verification", { workerId: worker.id })
      .then((found) => !cancelled && setFetched(found))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [worker.id, worker.branch, worker.verification]);
  const verification = worker.verification ?? fetched;
  const stillChecking = verification?.state === "running";

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
            <p className={worker.status === "cancelled" ? "muted" : worker.is_error ? "error" : ""}>
              {worker.summary}
            </p>
          </section>
        )}

        {/* How it got there, not just where it ended up. The live trace is only in memory:
            it covers workers started since this window opened. */}
        {worker.activity.length > 0 && (
          <section>
            <h3>Activity</h3>
            <ol className="activity-log mono">
              {worker.activity.map((item, i) => (
                <li key={i} className={item.kind}>
                  {describeActivity(item)}
                </li>
              ))}
            </ol>
          </section>
        )}

        {verification && <VerificationSection verification={verification} />}

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
              {["queued", "blocked", "preparing", "running"].includes(worker.status)
                ? "Still working — its changes appear here when it finishes."
                : worker.isolation === "readonly"
                  ? "Read-only worker — it produces findings, not changes."
                  : "This worker changed nothing."}
            </p>
          </section>
        )}

        {awaitingReview && (
          <footer>
            <p className="muted">
              Nothing has landed yet. Approving merges this branch into your checkout.
              {stillChecking && " Checks are still running — you can merge now, but they have not finished."}
            </p>
            <div className="actions">
              <button className="danger" onClick={onReject}>
                Discard
              </button>
              <button className="primary" onClick={onApprove}>
                {stillChecking ? "Merge anyway" : "Merge"}
              </button>
            </div>
          </footer>
        )}
      </aside>
    </div>
  );
}

type Shown = VerifyCheck["status"] | "flagged";

const CHECK_MARK: Record<Shown, string> = {
  passed: "✓",
  flagged: "!",
  failed: "✕",
  skipped: "–",
  error: "?",
};

/** A check that ran fine but found something worth a look is not a green tick. */
function shown(check: VerifyCheck): Shown {
  return check.status === "passed" && check.risk && check.risk !== "low" ? "flagged" : check.status;
}

/**
 * What checked this change before you did: a risk level that says why, then each check.
 *
 * The level is the first thing read, so it leads — but "unverified" is never dressed up
 * as low risk: a change nothing checked says so.
 */
function VerificationSection({ verification }: { verification: Verification }) {
  const checks = verification.state === "done" ? verification.report.checks : verification.checks;
  return (
    <section className="verification">
      <h3>
        Verification{" "}
        {verification.state === "running" ? (
          <span className="badge risk-pending">checking…</span>
        ) : (
          <>
            <span className={`badge risk-${verification.report.risk}`}>
              {verification.report.risk} risk
            </span>
            {!verification.report.verified && <span className="badge risk-unverified">unverified</span>}
          </>
        )}
      </h3>

      {verification.state === "done" && verification.report.reasons.length > 0 && (
        <ul className="risk-reasons">
          {verification.report.reasons.map((reason, i) => (
            <li key={i}>{reason}</li>
          ))}
        </ul>
      )}

      <ul className="checks">
        {checks.map((check, i) => (
          <li key={i} className={`check ${shown(check)}`}>
            <span className="check-mark" aria-label={shown(check)}>
              {CHECK_MARK[shown(check)]}
            </span>
            <div className="check-body">
              <div>
                <span className={check.kind === "command" ? "mono" : ""}>{check.name}</span>
                <span className="muted"> — {check.summary}</span>
              </div>
              {check.reviewer && <div className="muted mono check-by">{check.reviewer}</div>}
              {check.findings && check.findings.length > 0 && (
                <ul className="findings">
                  {check.findings.map((finding, j) => (
                    <li key={j} className={`finding ${finding.severity}`}>
                      {finding.file && (
                        <span className="mono">
                          {finding.file}
                          {finding.line ? `:${finding.line}` : ""}
                        </span>
                      )}{" "}
                      {finding.message}
                    </li>
                  ))}
                </ul>
              )}
              {check.output && (
                <details open={check.status === "failed"}>
                  <summary className="muted">output</summary>
                  <pre className="check-output mono">{check.output}</pre>
                </details>
              )}
            </div>
          </li>
        ))}
        {verification.state === "running" && <li className="check pending muted">Running the next check…</li>}
      </ul>
    </section>
  );
}
