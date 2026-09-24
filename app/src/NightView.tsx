import { useEffect, useState } from "react";
import type { NightConfig, NightReport, Role } from "./types";

type Props = {
  report: NightReport | null;
  roles: Role[];
  onStart: (config: NightConfig) => Promise<void>;
  onStop: () => void;
  onPropose: () => void;
  onSelectWorker: (id: string) => void;
};

const BLANK: NightConfig = {
  goal: "",
  metric: "",
  direction: "higher",
  guard: null,
  role: "",
  max_experiments: 20,
  max_hours: 8,
  timeout_secs: 900,
};

function formatScore(value: number | null): string {
  if (value === null) return "—";
  return Number.isInteger(value) ? String(value) : value.toFixed(3).replace(/0+$/, "");
}

function elapsed(from: number, to: number | null): string {
  const secs = Math.max(0, (to ?? Math.floor(Date.now() / 1000)) - from);
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** The change from the starting score, as the morning's one number. */
function gain(report: NightReport): string | null {
  const { baseline, best } = report;
  if (baseline === null || best === null || best === baseline) return null;
  const pct = baseline !== 0 ? ((best - baseline) / Math.abs(baseline)) * 100 : null;
  return pct === null ? `${formatScore(baseline)} → ${formatScore(best)}` : `${pct > 0 ? "+" : ""}${pct.toFixed(1)}%`;
}

/**
 * Every experiment's score against the best so far. Kept experiments are filled dots on
 * the stepped line; thrown-away ones are hollow, off it — so a night of near misses looks
 * different from a night of nothing.
 */
function ScoreChart({ report }: { report: NightReport }) {
  const scored = report.experiments.filter((e) => e.score !== null);
  if (report.baseline === null || scored.length === 0) return null;
  const values = [report.baseline, ...scored.map((e) => e.score as number)];
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const span = hi - lo || 1;
  const w = 900;
  const h = 150;
  const pad = 10;
  const n = Math.max(report.experiments.length, 1);
  const x = (i: number) => pad + (i / n) * (w - pad * 2);
  const y = (v: number) => h - pad - ((v - lo) / span) * (h - pad * 2);

  // The best-so-far line steps at every kept experiment.
  let best = report.baseline;
  const line = [`M ${x(0)} ${y(best)}`];
  for (const e of report.experiments) {
    if (e.kept && e.score !== null) {
      line.push(`H ${x(e.n)}`, `V ${y(e.score)}`);
      best = e.score;
    }
  }
  line.push(`H ${x(n)}`);

  return (
    <svg
      className="night-chart"
      viewBox={`0 0 ${w} ${h}`}
      role="img"
      aria-label={`Score from ${formatScore(report.baseline)} to ${formatScore(report.best)} over ${report.experiments.length} experiments`}
    >
      <path d={line.join(" ")} className="best-line" />
      <circle cx={x(0)} cy={y(report.baseline)} r={5} className="dot baseline" />
      {scored.map((e) => (
        <circle key={e.n} cx={x(e.n)} cy={y(e.score as number)} r={5} className={`dot ${e.kept ? "kept" : "thrown"}`}>
          <title>
            #{e.n}: {formatScore(e.score)} — {e.kept ? "kept" : "thrown away"}
          </title>
        </circle>
      ))}
    </svg>
  );
}

/**
 * Night shift: set a goal and a score, leave, and come back to a branch of kept
 * improvements and a report of everything tried.
 *
 * Three states on one tab. Nothing yet (or "start another"): the form. Running: live
 * progress and Stop. Ended: the morning report, with the one action that matters —
 * put the kept work up for review.
 */
export default function NightView({ report, roles, onStart, onStop, onPropose, onSelectWorker }: Props) {
  const editors = roles.filter((role) => role.available && role.isolation === "worktree");
  const [config, setConfig] = useState<NightConfig>(() => ({ ...BLANK, role: editors[0]?.name ?? "" }));
  const [composing, setComposing] = useState(report === null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [, tick] = useState(0);

  // A report arriving (another project, a restart) replaces the form; the last night's
  // settings become the next one's starting point.
  useEffect(() => {
    if (report) {
      setComposing(false);
      setConfig(report.config);
    } else {
      setComposing(true);
    }
  }, [report?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // Roles can arrive after the form; pick one as soon as there is one to pick.
  useEffect(() => {
    if (!config.role && editors[0]) setConfig((prev) => ({ ...prev, role: editors[0].name }));
  }, [editors.length]); // eslint-disable-line react-hooks/exhaustive-deps

  // The elapsed clock moves while it runs.
  useEffect(() => {
    if (report?.status !== "running") return;
    const timer = window.setInterval(() => tick((t) => t + 1), 30_000);
    return () => window.clearInterval(timer);
  }, [report?.status]);

  const set = (patch: Partial<NightConfig>) => setConfig((prev) => ({ ...prev, ...patch }));

  async function start() {
    setStarting(true);
    setError(null);
    try {
      await onStart({ ...config, guard: config.guard?.trim() ? config.guard.trim() : null });
      setComposing(false);
    } catch (err) {
      setError(String(err));
    } finally {
      setStarting(false);
    }
  }

  if (composing || !report) {
    const ready = config.goal.trim() && config.metric.trim() && config.role;
    return (
      <section className="night night-setup">
        <header className="plan-head">
          <div>
            <h2>Night shift</h2>
            <p className="muted">
              Give it a goal and a command that prints a score. It tries one small change at a
              time on its own branch, keeps what scores better, and throws the rest away. Your
              checkout is never touched; in the morning you review one branch.
            </p>
          </div>
        </header>

        <label className="field">
          <span>Goal</span>
          <textarea
            value={config.goal}
            onChange={(e) => set({ goal: e.target.value })}
            rows={3}
            placeholder="Make the test suite faster without dropping any tests."
          />
        </label>

        <div className="night-row">
          <label className="field grow">
            <span>Score command</span>
            <input
              className="mono"
              value={config.metric}
              onChange={(e) => set({ metric: e.target.value })}
              placeholder="./bench.sh"
            />
            <small className="muted">The last number it prints is the score.</small>
          </label>
          <label className="field">
            <span>Better is</span>
            <select
              value={config.direction}
              onChange={(e) => set({ direction: e.target.value as NightConfig["direction"] })}
            >
              <option value="higher">higher</option>
              <option value="lower">lower</option>
            </select>
          </label>
        </div>

        <label className="field">
          <span>Must keep passing (optional)</span>
          <input
            className="mono"
            value={config.guard ?? ""}
            onChange={(e) => set({ guard: e.target.value })}
            placeholder="cargo test"
          />
          <small className="muted">A change that breaks this is thrown away, however well it scores.</small>
        </label>

        <div className="night-row">
          <label className="field grow">
            <span>Role</span>
            <select value={config.role} onChange={(e) => set({ role: e.target.value })}>
              {editors.length === 0 && <option value="">No role can edit in its own worktree</option>}
              {editors.map((role) => (
                <option key={role.name} value={role.name}>
                  {role.name} · {role.provider}
                  {role.model ? ` · ${role.model}` : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Experiments</span>
            <input
              type="number"
              min={1}
              max={100}
              value={config.max_experiments}
              onChange={(e) => set({ max_experiments: Number(e.target.value) })}
            />
          </label>
          <label className="field">
            <span>Hours</span>
            <input
              type="number"
              min={0.25}
              max={24}
              step={0.25}
              value={config.max_hours}
              onChange={(e) => set({ max_hours: Number(e.target.value) })}
            />
          </label>
        </div>

        {error && <p className="error">{error}</p>}

        <div className="actions">
          {report && (
            <button className="ghost" onClick={() => setComposing(false)}>
              Back to the last report
            </button>
          )}
          <button className="primary" onClick={() => void start()} disabled={!ready || starting}>
            {starting ? "Starting…" : "Start night shift"}
          </button>
        </div>
      </section>
    );
  }

  const kept = report.experiments.filter((e) => e.kept).length;
  const change = gain(report);
  const running = report.status === "running";

  return (
    <section className="night">
      <header className="plan-head">
        <div>
          <h2>{running ? "Night shift running" : report.status === "stopped" ? "Night shift stopped" : "Morning report"}</h2>
          <p className="muted">{report.config.goal}</p>
          <p className="muted mono night-branch">
            {report.branch} · {elapsed(report.started_at, report.finished_at)}
          </p>
        </div>
        {running ? (
          <button className="ghost" onClick={onStop} title="What was kept stays kept">
            Stop
          </button>
        ) : (
          <button className="ghost" onClick={() => setComposing(true)}>
            Start another
          </button>
        )}
      </header>

      <div className="night-stats">
        <div>
          <span className="muted">Started at</span>
          <strong>{formatScore(report.baseline)}</strong>
        </div>
        <div>
          <span className="muted">Best</span>
          <strong>
            {formatScore(report.best)}
            {change && <em className={kept > 0 ? "up" : ""}> {change}</em>}
          </strong>
        </div>
        <div>
          <span className="muted">Kept</span>
          <strong>
            {kept} of {report.experiments.length}
            {running && <span className="muted"> / {report.config.max_experiments}</span>}
          </strong>
        </div>
      </div>

      <ScoreChart report={report} />

      {!running && (
        <div className="night-outcome">
          {report.ended_because && <p className="muted">Ended: {report.ended_because}.</p>}
          {report.proposed_as ? (
            <button className="ghost" onClick={() => onSelectWorker(report.proposed_as as string)}>
              Proposed for review — open it
            </button>
          ) : (
            <button
              className="primary"
              onClick={onPropose}
              disabled={kept === 0}
              title={kept === 0 ? "Nothing was kept" : "Verify the night's branch and put it up for merging"}
            >
              Propose for review
            </button>
          )}
        </div>
      )}

      <ol className="night-experiments" reversed>
        {running && (
          <li className="experiment pending">
            <span className="mono">#{report.experiments.length + 1}</span>
            <span className="muted">trying something…</span>
          </li>
        )}
        {[...report.experiments].reverse().map((e) => (
          <li key={e.n} className={`experiment ${e.kept ? "kept" : "thrown"}`}>
            <button className="ghost experiment-open" onClick={() => onSelectWorker(e.worker_id)} title="Open this worker">
              <span className="mono">#{e.n}</span>
              <span className={`badge ${e.kept ? "risk-low" : "risk-unverified"}`}>{e.kept ? "kept" : "thrown away"}</span>
              <span className="mono experiment-score">{formatScore(e.score)}</span>
              <span className="experiment-summary">{e.summary.split("\n")[0] || "(no summary)"}</span>
            </button>
            <span className="muted experiment-reason">{e.reason}</span>
          </li>
        ))}
      </ol>
    </section>
  );
}
