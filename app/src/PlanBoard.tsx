import { useEffect, useState } from "react";
import type { Plan, PlanStep, Role, StepInput, StepState, Worker } from "./types";

type Props = {
  plan: Plan | null;
  roles: Role[];
  workers: Record<string, Worker>;
  onRun: (steps: StepInput[]) => void;
  onFeedback: (steps: StepInput[], comments: { step_id: string; text: string }[], note: string) => void;
  onDiscard: () => void;
  onSelectWorker: (id: string) => void;
};

/** The lanes a running plan moves through, left to right. */
const LANES: { title: string; states: StepState[] }[] = [
  { title: "Up next", states: ["planned", "waiting"] },
  { title: "Running", states: ["running"] },
  { title: "Checking", states: ["checking"] },
  { title: "Your review", states: ["review"] },
  { title: "Landed", states: ["landed"] },
  { title: "Stopped", states: ["failed", "skipped"] },
];

const STATE_LABEL: Record<StepState, string> = {
  planned: "ready",
  waiting: "waiting",
  running: "running",
  checking: "checking",
  review: "your review",
  landed: "landed",
  failed: "failed",
  skipped: "skipped",
};

function inputOf(step: PlanStep): StepInput {
  return {
    id: step.id,
    title: step.title,
    role: step.role,
    task: step.task,
    context_files: step.context_files,
    depends_on: step.depends_on,
  };
}

/**
 * The head agent's plan, as something to shape rather than a wall of text to read.
 *
 * While it is a draft, every step is an editable card: its task, its role, what it waits
 * for, and a comment for the head agent. Once running, the same cards move across lanes
 * as their workers run, get checked, and land — so the plan is the progress view too.
 */
export default function PlanBoard({ plan, roles, workers, onRun, onFeedback, onDiscard, onSelectWorker }: Props) {
  const [steps, setSteps] = useState<StepInput[]>([]);
  const [comments, setComments] = useState<Record<string, string>>({});
  const [note, setNote] = useState("");

  // A new plan, or a revision of one, starts the review over.
  useEffect(() => {
    setSteps(plan ? plan.steps.map(inputOf) : []);
    setComments({});
    setNote("");
  }, [plan?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!plan) {
    return (
      <div className="plan-empty muted">
        <p>No plan yet.</p>
        <p>
          When the work has more than one step, the head agent lays it out here first — you can
          edit it, comment on it, and run it. Ask for a plan in the chat.
        </p>
      </div>
    );
  }

  const update = (id: string, patch: Partial<StepInput>) =>
    setSteps((prev) => prev.map((step) => (step.id === id ? { ...step, ...patch } : step)));

  const remove = (id: string) =>
    // Whatever waited on the removed step no longer can.
    setSteps((prev) =>
      prev
        .filter((step) => step.id !== id)
        .map((step) => ({ ...step, depends_on: step.depends_on.filter((dep) => dep !== id) })),
    );

  const commentList = Object.entries(comments)
    .filter(([, text]) => text.trim())
    .map(([step_id, text]) => ({ step_id, text }));
  const parallel = steps.filter((step) => step.depends_on.length === 0).length;

  if (plan.status === "draft") {
    return (
      <section className="plan-board draft">
        <header className="plan-head">
          <div>
            <h2>{plan.title}</h2>
            {plan.summary && <p className="muted">{plan.summary}</p>}
            <p className="muted plan-shape">
              {steps.length} step{steps.length === 1 ? "" : "s"}
              {parallel > 1 && ` · ${parallel} start at once`}
            </p>
          </div>
        </header>

        <ol className="plan-steps">
          {steps.map((step) => (
            <li key={step.id} className="plan-card">
              <div className="plan-card-head">
                <span className="mono step-id">{step.id}</span>
                <input
                  className="step-title"
                  value={step.title}
                  onChange={(e) => update(step.id, { title: e.target.value })}
                  aria-label={`Title of step ${step.id}`}
                />
                <select
                  value={step.role}
                  onChange={(e) => update(step.id, { role: e.target.value })}
                  aria-label={`Role for step ${step.id}`}
                >
                  {roles.map((role) => (
                    <option key={role.name} value={role.name} disabled={!role.available}>
                      {role.name}
                      {role.available ? "" : " (unavailable)"}
                    </option>
                  ))}
                </select>
                <button className="ghost" onClick={() => remove(step.id)} aria-label={`Remove step ${step.id}`}>
                  ✕
                </button>
              </div>
              <textarea
                className="step-task"
                value={step.task}
                onChange={(e) => update(step.id, { task: e.target.value })}
                rows={Math.min(8, Math.max(2, Math.ceil(step.task.length / 70)))}
                aria-label={`Task for step ${step.id}`}
              />
              <div className="plan-card-foot">
                <span className="muted">
                  {step.depends_on.length > 0
                    ? `after ${step.depends_on.join(", ")} has landed`
                    : "starts right away"}
                </span>
                <input
                  className="step-comment"
                  value={comments[step.id] ?? ""}
                  onChange={(e) => setComments((prev) => ({ ...prev, [step.id]: e.target.value }))}
                  placeholder="Comment for the head agent…"
                  aria-label={`Comment on step ${step.id}`}
                />
              </div>
            </li>
          ))}
        </ol>

        <footer className="plan-foot">
          <textarea
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="Anything about the plan as a whole…"
            rows={2}
          />
          <div className="actions">
            <button className="ghost" onClick={onDiscard}>
              Discard
            </button>
            <button
              onClick={() => onFeedback(steps, commentList, note)}
              disabled={commentList.length === 0 && !note.trim()}
              title="Send your comments and edits back; the head agent revises the plan"
            >
              Send feedback
            </button>
            <button className="primary" onClick={() => onRun(steps)} disabled={steps.length === 0}>
              Run plan
            </button>
          </div>
        </footer>
      </section>
    );
  }

  return (
    <section className="plan-board">
      <header className="plan-head">
        <div>
          <h2>{plan.title}</h2>
          <p className="muted">
            {plan.status === "running" ? "Running" : plan.status === "finished" ? "Finished" : "Stopped"} ·{" "}
            {plan.steps.filter((s) => s.state === "landed").length} of {plan.steps.length} landed
          </p>
        </div>
        {plan.status === "running" && (
          <button className="ghost" onClick={onDiscard} title="Steps not yet started will not start">
            Stop plan
          </button>
        )}
      </header>

      <div className="lanes">
        {LANES.map((lane) => {
          const cards = plan.steps.filter((step) => lane.states.includes(step.state));
          return (
            <div key={lane.title} className="lane">
              <h3>
                {lane.title} <span className="muted">{cards.length || ""}</span>
              </h3>
              {cards.map((step) => {
                const worker = step.worker_id ? workers[step.worker_id] : undefined;
                const verification = worker?.verification;
                return (
                  <button
                    key={step.id}
                    className={`lane-card ${step.state}`}
                    onClick={() => step.worker_id && onSelectWorker(step.worker_id)}
                    disabled={!step.worker_id}
                  >
                    <span className="lane-card-title">{step.title}</span>
                    <span className="muted mono lane-card-meta">
                      {step.role} · {STATE_LABEL[step.state]}
                    </span>
                    {step.state === "waiting" && (
                      <span className="muted lane-card-note">after {step.depends_on.join(", ")}</span>
                    )}
                    {verification?.state === "done" && (
                      <span
                        className={`badge risk-${verification.report.verified ? verification.report.risk : "unverified"}`}
                      >
                        {verification.report.verified ? `${verification.report.risk} risk` : "unverified"}
                      </span>
                    )}
                    {step.note && <span className="lane-card-note">{step.note}</span>}
                  </button>
                );
              })}
            </div>
          );
        })}
      </div>
    </section>
  );
}
