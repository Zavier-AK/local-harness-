import type { UsageRow } from "./types";

type Props = { usage: UsageRow[]; rateLimited: boolean };

/**
 * Burn over the rolling five-hour window, which is the constraint that actually binds a
 * Pro subscription. Dollar figures are shown only where they mean something: under
 * subscription auth the CLI's cost estimate is notional, and local inference is free.
 */
export default function BudgetMeter({ usage, rateLimited }: Props) {
  const subscription = usage.filter((row) => row.provider === "claude" || row.provider === "codex");
  const local = usage.filter((row) => row.provider !== "claude" && row.provider !== "codex");

  const subscriptionTokens = subscription.reduce(
    (sum, row) => sum + row.input_tokens + row.cache_creation_tokens + row.cache_read_tokens,
    0,
  );
  const localTokens = local.reduce(
    (sum, row) => sum + row.input_tokens + row.cache_read_tokens,
    0,
  );

  // Cache creation is the expensive part; a healthy session's ratio falls as it runs,
  // because a long-lived process reads context back instead of rebuilding it.
  const created = subscription.reduce((sum, row) => sum + row.cache_creation_tokens, 0);
  const read = subscription.reduce((sum, row) => sum + row.cache_read_tokens, 0);
  const reusePct = created + read > 0 ? Math.round((read / (created + read)) * 100) : null;

  return (
    <div className="meter" title="Rolling 5-hour window">
      {rateLimited && <span className="badge limited">rate limited</span>}

      <span className="meter-item">
        <span className="meter-label">subscription</span>
        <span className="mono">{subscriptionTokens.toLocaleString()}</span>
      </span>

      <span className="meter-item">
        <span className="meter-label">local</span>
        <span className="mono">{localTokens.toLocaleString()}</span>
      </span>

      {reusePct !== null && (
        <span className="meter-item" title="Share of context read from cache rather than rebuilt">
          <span className="meter-label">cache reuse</span>
          <span className="mono">{reusePct}%</span>
        </span>
      )}
    </div>
  );
}
