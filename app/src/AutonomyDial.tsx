import type { Autonomy } from "./types";

/** The four stops, most supervised first. Mirrors `Autonomy` in the engine. */
export const STOPS: { level: Autonomy; label: string; description: string }[] = [
  {
    level: "ask",
    label: "Ask",
    description: "Every delegation waits for your approval, and every merge.",
  },
  {
    level: "review",
    label: "Review",
    description: "Delegations run freely; every merge waits for you.",
  },
  {
    level: "land_safe",
    label: "Land safe",
    description:
      "Delegations run freely. Changes that were verified and are low risk land by themselves — with Undo. The rest wait for you.",
  },
  {
    level: "land_most",
    label: "Land most",
    description:
      "Delegations run freely. Verified changes land by themselves unless they are high risk — with Undo. High risk and unverified wait for you.",
  },
];

type Props = {
  level: Autonomy;
  onChange: (level: Autonomy) => void;
};

/**
 * How much runs without you — Karpathy's autonomy slider.
 *
 * The engine enforces the level; this only chooses it. The current stop is written out,
 * not just highlighted, so it reads at a glance and without colour.
 */
export default function AutonomyDial({ level, onChange }: Props) {
  return (
    <div className="autonomy" role="radiogroup" aria-label="How much runs without you">
      {STOPS.map((stop) => (
        <button
          key={stop.level}
          role="radio"
          aria-checked={stop.level === level}
          className={stop.level === level ? "active" : ""}
          title={`${stop.label} — ${stop.description} (⌘⇧A cycles)`}
          onClick={() => onChange(stop.level)}
        >
          {stop.label}
        </button>
      ))}
    </div>
  );
}

/** The next stop round the dial, for the keyboard shortcut. */
export function nextStop(level: Autonomy): Autonomy {
  const index = STOPS.findIndex((stop) => stop.level === level);
  return STOPS[(index + 1) % STOPS.length].level;
}
