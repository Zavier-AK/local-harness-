import type { ProviderQuota, QuotaReport, UsageRow } from "./types";
import { totalInput } from "./types";

type Props = {
  quotas: QuotaReport | null;
  usage: UsageRow[];
  onClose: () => void;
};

const LABEL: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
};

/**
 * Subscription headroom.
 *
 * Two different kinds of number live here and they are kept visibly apart. What the
 * vendor reports as remaining is a real limit; what this harness has spent is a real
 * measurement but says nothing about how much room is left. Only Codex supplies the
 * former, so Claude shows tokens and an explanation rather than a fabricated percentage.
 */
export default function LimitsPanel({ quotas, usage, onClose }: Props) {
  const spent = new Map(usage.map((row) => [row.provider, row]));

  return (
    <div className="drawer-scrim" onClick={onClose}>
      <aside
        className="drawer limits-panel"
        role="dialog"
        aria-label="Subscription limits"
        onClick={(event) => event.stopPropagation()}
      >
        <header className="drawer-head">
          <h2>Limits</h2>
          <button onClick={onClose}>Close</button>
        </header>

        {!quotas && <p className="muted">Checking…</p>}

        {quotas?.rate_limited.map((provider) => (
          <p className="limit-hit" key={provider}>
            {LABEL[provider] ?? provider} is rate-limited right now — work is shedding to
            fallback roles.
          </p>
        ))}

        {quotas?.providers.map((quota) => (
          <Provider key={quota.provider} quota={quota} spent={spent.get(quota.provider)} />
        ))}

        <p className="muted limits-footnote">
          Token counts are what this harness has spent in the last 5 hours, summed across
          open projects. They are a real measurement, but not a share of any limit.
        </p>
      </aside>
    </div>
  );
}

function Provider({ quota, spent }: { quota: ProviderQuota; spent?: UsageRow }) {
  return (
    <section className="quota">
      <h3>
        {LABEL[quota.provider] ?? quota.provider}
        {quota.state === "stale" && <span className="quota-age"> · last known</span>}
      </h3>

      {quota.windows.map((window) => (
        <div className="quota-window" key={window.label}>
          <div className="quota-row">
            <span>{window.label}</span>
            <span className="mono">{window.used_percent.toFixed(0)}% used</span>
          </div>
          <div className="quota-bar">
            <div
              className={`quota-fill ${window.used_percent >= 80 ? "high" : ""}`}
              style={{ width: `${Math.min(100, Math.max(0, window.used_percent))}%` }}
            />
          </div>
          {window.resets_at && (
            <p className="muted quota-reset">resets {relative(window.resets_at)}</p>
          )}
        </div>
      ))}

      {quota.state === "missing" && <p className="muted quota-note">{quota.note}</p>}

      <p className="quota-spent muted">
        {spent
          ? `${totalInput(spentUsage(spent)).toLocaleString()} in · ${spent.output_tokens.toLocaleString()} out · ${spent.runs} run(s)`
          : "nothing spent yet in this window"}
      </p>
    </section>
  );
}

/** `UsageRow` names its cache fields differently from `Usage`; bridge the two. */
function spentUsage(row: UsageRow) {
  return {
    input_tokens: row.input_tokens,
    output_tokens: row.output_tokens,
    cache_creation_input_tokens: row.cache_creation_tokens,
    cache_read_input_tokens: row.cache_read_tokens,
  };
}

function relative(unixSeconds: number): string {
  const seconds = unixSeconds - Date.now() / 1000;
  if (seconds <= 0) return "now";
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.round((seconds % 3600) / 60);
  if (hours >= 24) return `in ${Math.round(hours / 24)}d`;
  if (hours > 0) return `in ${hours}h ${minutes}m`;
  return `in ${minutes}m`;
}
