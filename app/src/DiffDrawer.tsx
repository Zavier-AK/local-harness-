import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  Patch,
  PrDraft,
  PrPlan,
  PrStatus,
  PullRequest,
  ReviewComment,
  Verification,
  VerifyCheck,
  Worker,
} from "./types";
import { describeActivity } from "./WorkerRail";

type Props = {
  worker: Worker;
  onClose: () => void;
  onApprove: () => void;
  onReject: () => void;
  onUndo: () => void;
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

/** A diff line, with where it sits in which file, so a comment can say where it is. */
type Row = {
  text: string;
  kind: ReturnType<typeof lineKind>;
  file: string | null;
  /** In the new file, or the old one for a removed line. */
  line: number | null;
  removed: boolean;
};

/** Walk a unified diff, numbering each line the way an editor would show it. */
export function parsePatch(text: string): Row[] {
  const rows: Row[] = [];
  let file: string | null = null;
  let oldLine = 0;
  let newLine = 0;
  for (const line of text.split("\n")) {
    const kind = lineKind(line);
    if (line.startsWith("diff --git ")) {
      const match = line.match(/ b\/(.+)$/);
      file = match ? match[1] : null;
    } else if (line.startsWith("+++ ")) {
      const path = line.slice(4).trim();
      if (path !== "/dev/null") file = path.replace(/^b\//, "");
    } else if (kind === "hunk") {
      const match = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)/);
      if (match) {
        oldLine = Number(match[1]);
        newLine = Number(match[2]);
      }
    }
    if (kind === "add") {
      rows.push({ text: line, kind, file, line: newLine, removed: false });
      newLine += 1;
    } else if (kind === "del") {
      rows.push({ text: line, kind, file, line: oldLine, removed: true });
      oldLine += 1;
    } else if (kind === "ctx" && !line.startsWith("\\") && line !== "") {
      rows.push({ text: line, kind, file, line: newLine, removed: false });
      oldLine += 1;
      newLine += 1;
    } else {
      rows.push({ text: line, kind, file: null, line: null, removed: false });
    }
  }
  return rows;
}

const rowKey = (row: Row) => `${row.file}|${row.removed ? "old" : "new"}|${row.line}`;

/**
 * Review surface for a worker's changes.
 *
 * This is the only place work can land. The orchestrator can propose a merge; approving
 * it is a human action, and the engine exposes no path for a model to do it.
 */
export default function DiffDrawer({ worker, onClose, onApprove, onReject, onUndo }: Props) {
  const diff = worker.diff;
  const working = ["queued", "blocked", "preparing", "running"].includes(worker.status);
  const hasChanges = Boolean(worker.branch && diff && diff.files_changed > 0);
  const awaitingReview = hasChanges && !working && !worker.landed;

  // Line comments, by where they sit; `editing` is the one whose box is open.
  const [comments, setComments] = useState<Record<string, ReviewComment>>({});
  const [editing, setEditing] = useState<string | null>(null);
  const [note, setNote] = useState("");
  const [sending, setSending] = useState(false);
  const [reviseError, setReviseError] = useState<string | null>(null);
  const [prOpen, setPrOpen] = useState(false);
  const noteRef = useRef<HTMLTextAreaElement>(null);
  const commentList = Object.values(comments).filter((c) => c.text.trim());

  // A different worker is a different review.
  useEffect(() => {
    setComments({});
    setEditing(null);
    setNote("");
    setReviseError(null);
    setPrOpen(false);
  }, [worker.id]);

  async function sendBack() {
    setSending(true);
    setReviseError(null);
    try {
      await invoke<number>("revise_worker", {
        workerId: worker.id,
        note,
        comments: commentList,
      });
      setComments({});
      setEditing(null);
      setNote("");
    } catch (err) {
      setReviseError(String(err));
    } finally {
      setSending(false);
    }
  }

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
  }, [worker.id, worker.branch, worker.diff?.files_changed, worker.diff?.insertions, worker.status]);

  // Escape closes, so reviewing never traps you in the drawer — after closing an open
  // comment box first, so a half-written comment isn't lost with the drawer.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (editing) setEditing(null);
      else if (prOpen) setPrOpen(false);
      else onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, editing, prOpen]);

  return (
    <div className="drawer-scrim" onClick={onClose}>
      <aside className="drawer" onClick={(e) => e.stopPropagation()}>
        <header>
          <div>
            <h2>
              {worker.role}
              {worker.revision ? <span className="badge revision">revision {worker.revision}</span> : null}
            </h2>
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

            {working && (
              <p className="muted">
                Making another pass on this branch — below is the version you sent back.
              </p>
            )}
            {loadingPatch && <p className="muted">Reading the diff…</p>}
            {patchError && <p className="error">{patchError}</p>}

            {patch && (
              <>
                {awaitingReview && (
                  <p className="muted review-hint">Click a line to comment on it.</p>
                )}
                <div className="patch">
                  {parsePatch(patch.text).map((row, i) => {
                    const commentable = awaitingReview && row.file !== null && row.line !== null;
                    const key = commentable ? rowKey(row) : null;
                    const comment = key ? comments[key] : undefined;
                    return (
                      <div key={i}>
                        <div
                          className={`dl ${row.kind} ${commentable ? "commentable" : ""} ${comment?.text ? "has-comment" : ""}`}
                          onClick={
                            key
                              ? () => {
                                  setComments((prev) =>
                                    prev[key]
                                      ? prev
                                      : {
                                          ...prev,
                                          [key]: {
                                            file: row.file!,
                                            line: row.line,
                                            removed: row.removed,
                                            excerpt: row.text.slice(1).trim() || null,
                                            text: "",
                                          },
                                        },
                                  );
                                  setEditing(key);
                                }
                              : undefined
                          }
                        >
                          {commentable && (
                            <span className="ln">{row.line}</span>
                          )}
                          <span className="code">{row.text || " "}</span>
                        </div>
                        {key && comment && (editing === key || comment.text) && (
                          <LineComment
                            comment={comment}
                            editing={editing === key}
                            onEdit={() => setEditing(key)}
                            onChange={(text) =>
                              setComments((prev) => ({ ...prev, [key]: { ...prev[key], text } }))
                            }
                            onDone={() => {
                              setEditing(null);
                              if (!comment.text.trim()) {
                                setComments(({ [key]: _gone, ...rest }) => rest);
                              }
                            }}
                            onRemove={() => {
                              setEditing(null);
                              setComments(({ [key]: _gone, ...rest }) => rest);
                            }}
                          />
                        )}
                      </div>
                    );
                  })}
                </div>
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
              {working
                ? "Still working — its changes appear here when it finishes."
                : worker.isolation === "readonly"
                  ? "Read-only worker — it produces findings, not changes."
                  : "This worker changed nothing."}
            </p>
          </section>
        )}

        {worker.landed && (
          <footer>
            <p className="muted">
              {worker.landed.automatic ? "Landed by itself" : "Merged"} as{" "}
              <code className="mono">{worker.landed.commit.slice(0, 10)}</code>. Undo adds a commit
              that reverses it; history is kept.
            </p>
            <div className="actions">
              <button onClick={onUndo}>Undo</button>
            </div>
          </footer>
        )}

        {hasChanges && !working && (worker.pullRequest || prOpen) && (
          <PullRequestSection
            worker={worker}
            composing={prOpen && !worker.pullRequest}
            onCancel={() => setPrOpen(false)}
          />
        )}

        {awaitingReview && (
          <section className="send-back">
            <h3>
              Send back for changes
              {commentList.length > 0 && (
                <span className="muted">
                  {" "}
                  · {commentList.length} comment{commentList.length === 1 ? "" : "s"}
                </span>
              )}
            </h3>
            <textarea
              ref={noteRef}
              value={note}
              onChange={(e) => setNote(e.target.value)}
              placeholder={
                commentList.length
                  ? "Anything else? (optional)"
                  : "What should change? Or click lines above to comment on them."
              }
              rows={2}
            />
            <div className="send-back-row">
              <span className="muted">
                {stillChecking
                  ? "Checks are still running — send it back once they finish."
                  : "It makes another pass on the same branch, then comes back for review."}
              </span>
              <button
                onClick={() => void sendBack()}
                disabled={sending || stillChecking || (!note.trim() && commentList.length === 0)}
              >
                {sending ? "Sending…" : "Send back"}
              </button>
            </div>
            {reviseError && <p className="error">{reviseError}</p>}
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
              {!worker.pullRequest && (
                <button onClick={() => setPrOpen(true)} disabled={prOpen}>
                  Open pull request
                </button>
              )}
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

/** A comment under the line it is about: a box while it's written, then the words. */
function LineComment({
  comment,
  editing,
  onEdit,
  onChange,
  onDone,
  onRemove,
}: {
  comment: ReviewComment;
  editing: boolean;
  onEdit: () => void;
  onChange: (text: string) => void;
  onDone: () => void;
  onRemove: () => void;
}) {
  if (!editing) {
    return (
      <button className="line-comment saved" onClick={onEdit} title="Edit this comment">
        {comment.text}
      </button>
    );
  }
  return (
    <div className="line-comment">
      <textarea
        autoFocus
        value={comment.text}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            onDone();
          }
        }}
        placeholder={`What should change on line ${comment.line}?`}
        rows={2}
      />
      <div className="line-comment-actions">
        <button className="ghost" onClick={onRemove}>
          Remove
        </button>
        <button onClick={onDone}>Done</button>
      </div>
    </div>
  );
}

const GH_NOTE: Record<PrPlan["gh"], string> = {
  ready: "Opens it on GitHub with the GitHub CLI.",
  signed_out:
    "The GitHub CLI isn't signed in (`gh auth login`), so Harness pushes the branch and opens GitHub's page to finish it.",
  missing:
    "Harness pushes the branch and opens GitHub's page to finish it. Install the GitHub CLI (`brew install gh`) to open it directly.",
};

/** Push the branch and open a pull request — or, once open, where it stands. */
function PullRequestSection({
  worker,
  composing,
  onCancel,
}: {
  worker: Worker;
  composing: boolean;
  onCancel: () => void;
}) {
  const [plan, setPlan] = useState<PrPlan | null>(null);
  const [draft, setDraft] = useState<PrDraft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [status, setStatus] = useState<PrStatus | null>(null);
  const [checking, setChecking] = useState(false);
  const pr = worker.pullRequest;
  const sectionRef = useRef<HTMLElement>(null);

  useEffect(() => {
    if (!composing) return;
    sectionRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
    let cancelled = false;
    invoke<PrPlan>("pull_request_plan", { workerId: worker.id })
      .then((found) => {
        if (cancelled) return;
        setPlan(found);
        setDraft(found.suggested);
      })
      .catch((err) => !cancelled && setError(String(err)));
    return () => {
      cancelled = true;
    };
  }, [composing, worker.id]);

  async function refresh() {
    setChecking(true);
    try {
      const view = await invoke<(PullRequest & { status: PrStatus | null }) | null>(
        "pull_request_status",
        { workerId: worker.id },
      );
      setStatus(view?.status ?? null);
    } catch {
      // A readout; the link still works.
    } finally {
      setChecking(false);
    }
  }
  useEffect(() => {
    if (pr?.via === "gh") void refresh();
  }, [pr?.url]); // eslint-disable-line react-hooks/exhaustive-deps

  async function send() {
    if (!draft) return;
    setSending(true);
    setError(null);
    try {
      await invoke<PullRequest>("open_pull_request", { workerId: worker.id, draft });
    } catch (err) {
      setError(String(err));
    } finally {
      setSending(false);
    }
  }

  if (pr) {
    return (
      <section className="pull-request">
        <h3>Pull request</h3>
        <div className="pr-row">
          {pr.via === "pushed_only" ? (
            <span>
              Pushed as <code className="mono">{pr.remote_branch}</code>.
            </span>
          ) : (
            <button className="link" onClick={() => void invoke("open_link", { url: pr.url })}>
              {pr.number ? `#${pr.number}` : "Finish it on GitHub"} · {pr.remote_branch}
            </button>
          )}
          {pr.via === "gh" && (
            <button className="ghost" onClick={() => void refresh()} disabled={checking}>
              {checking ? "Checking…" : "Refresh"}
            </button>
          )}
        </div>
        {status && (
          <p className="pr-status">
            <span className={`badge pr-${status.state.toLowerCase()}`}>
              {status.draft ? "draft" : status.state.toLowerCase()}
            </span>{" "}
            {status.passed + status.failed + status.pending === 0 ? (
              <span className="muted">no checks</span>
            ) : (
              <span className="muted">
                {status.passed} passed
                {status.failed > 0 && <span className="error"> · {status.failed} failed ({status.failing.join(", ")})</span>}
                {status.pending > 0 && ` · ${status.pending} running`}
              </span>
            )}
          </p>
        )}
      </section>
    );
  }

  const noRemote = plan !== null && plan.remote === null;
  return (
    <section className="pull-request" ref={sectionRef}>
      <h3>Open a pull request</h3>
      {!plan && !error && <p className="muted">Looking at the remote…</p>}
      {noRemote && (
        <p className="error">
          This project has no remote to push to. Add one with{" "}
          <code className="mono">git remote add origin &lt;url&gt;</code>.
        </p>
      )}
      {plan && draft && !noRemote && (
        <div className="pr-form">
          <label>
            Title
            <input value={draft.title} onChange={(e) => setDraft({ ...draft, title: e.target.value })} />
          </label>
          <div className="pr-branches">
            <label>
              Branch
              <input
                className="mono"
                value={draft.remote_branch}
                onChange={(e) => setDraft({ ...draft, remote_branch: e.target.value })}
              />
            </label>
            <span className="muted pr-into">into</span>
            <label>
              Base
              <input
                className="mono"
                value={draft.base}
                onChange={(e) => setDraft({ ...draft, base: e.target.value })}
              />
            </label>
          </div>
          <label>
            Description
            <textarea
              value={draft.body}
              onChange={(e) => setDraft({ ...draft, body: e.target.value })}
              rows={8}
            />
          </label>
          <label className="pr-draft">
            <input
              type="checkbox"
              checked={draft.draft}
              onChange={(e) => setDraft({ ...draft, draft: e.target.checked })}
            />
            Open as a draft
          </label>
          <p className="muted">
            {plan.remote?.github
              ? GH_NOTE[plan.gh]
              : `Pushes the branch to ${plan.remote?.name}; it isn't GitHub, so open the pull request there.`}
          </p>
          <div className="actions">
            <button className="ghost" onClick={onCancel}>
              Cancel
            </button>
            <button className="primary" onClick={() => void send()} disabled={sending || !draft.title.trim()}>
              {sending
                ? "Pushing…"
                : plan.remote?.github && plan.gh === "ready"
                  ? "Push and open"
                  : "Push"}
            </button>
          </div>
        </div>
      )}
      {error && <p className="error">{error}</p>}
    </section>
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
