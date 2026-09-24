import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Heard, QuotaReport, VoicePhase } from "./types";
import { limitsLine, replyLine, speak } from "./voice";

const BARS = 28;

/**
 * The push-to-talk bar: a small window above everything, shown while listening and
 * answering. While the key is held it keeps the waveform up and shows the last thing
 * done underneath, because each command runs at the pause after it, not on letting go.
 * It asks yes-or-no when an action needs it. It decides nothing itself.
 */
export default function VoiceHud() {
  const [phase, setPhase] = useState<VoicePhase>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [levels, setLevels] = useState<number[]>(() => Array(BARS).fill(0));
  const [heard, setHeard] = useState<Heard | null>(null);
  const [line, setLine] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const phaseRef = useRef<VoicePhase>("idle");
  const heardThisTurn = useRef(false);
  const hideTimer = useRef<number | null>(null);

  function scheduleHide(ms: number) {
    if (hideTimer.current) window.clearTimeout(hideTimer.current);
    hideTimer.current = window.setTimeout(() => void invoke("voice_hide_hud"), ms);
  }
  function keep() {
    if (hideTimer.current) window.clearTimeout(hideTimer.current);
    hideTimer.current = null;
  }

  useEffect(() => {
    const offs = [
      listen<{ phase: VoicePhase; message: string | null }>("voice://state", ({ payload }) => {
        phaseRef.current = payload.phase;
        setPhase(payload.phase);
        setMessage(payload.message);
        if (payload.phase === "listening") {
          keep();
          heardThisTurn.current = false;
          setHeard(null);
          setLine(null);
          setLevels(Array(BARS).fill(0));
        } else if (payload.phase === "error") {
          setLine(payload.message);
          scheduleHide(5000);
        } else if (payload.phase === "idle" && !heardThisTurn.current) {
          // Let go without saying anything.
          scheduleHide(600);
        } else {
          keep();
        }
      }),
      listen<number>("voice://level", ({ payload }) => {
        setLevels((prev) => [...prev.slice(1), Math.min(1, payload * 6)]);
      }),
      listen<boolean>("voice://busy", ({ payload }) => setBusy(payload)),
      listen<Heard>("voice://heard", async ({ payload }) => {
        heardThisTurn.current = true;
        setHeard(payload);
        let text = replyLine(payload);
        const outcome = payload.interpretation.outcome;
        if (outcome.outcome === "act" && outcome.action.action === "status" && outcome.action.topic === "limits") {
          text = limitsLine(await invoke<QuotaReport>("quotas").catch(() => null));
        }
        setLine(text);
        // Not over the person while they are still talking.
        const stillTalking = phaseRef.current === "listening";
        if (payload.speak && text && !stillTalking) speak(text);
        if (stillTalking || payload.pending) {
          keep();
        } else if (outcome.outcome === "nothing") {
          scheduleHide(600);
        } else {
          scheduleHide(Math.max(3500, (text?.length ?? 0) * 70));
        }
      }),
    ];
    return () => offs.forEach((off) => void off.then((f) => f()));
  }, []);

  const interpretation = heard?.interpretation;
  const source =
    interpretation?.source === "laya"
      ? `Laya ${interpretation.confidence !== null ? interpretation.confidence.toFixed(2) : ""}${interpretation.laya_ms !== null ? ` · ${interpretation.laya_ms} ms` : ""}`
      : interpretation?.source === "matcher"
        ? "command"
        : interpretation
          ? "→ chat box"
          : null;
  const listening = phase === "listening";
  const status =
    phase === "transcribing"
      ? "Finishing…"
      : phase === "thinking"
        ? "Working out what you meant…"
        : null;

  return (
    <div className={`voice-hud phase-${phase} ${heard?.error || phase === "error" ? "failed" : ""}`} role="status" aria-live="polite">
      {listening && (
        <div className="hud-row">
          <span className="hud-mic listening" aria-hidden>
            ●
          </span>
          <div className="hud-wave" aria-label="Listening">
            {levels.map((level, i) => (
              <span key={i} style={{ height: `${Math.max(8, level * 100)}%` }} />
            ))}
          </div>
          <span className="hud-source">{busy ? "…" : message ? `${message}?` : "listening"}</span>
        </div>
      )}

      {(heard || status || (!listening && line)) && (
        <div className="hud-row">
          {!listening && (
            <span className={`hud-mic ${phase}`} aria-hidden>
              ●
            </span>
          )}
          <div className="hud-text">
            {interpretation?.transcript && <p className="hud-transcript">“{interpretation.transcript}”</p>}
            {status && !heard && <p className="hud-status muted">{status}</p>}
            {line && <p className="hud-line">{line}</p>}
          </div>
          {source && !listening && <span className="hud-source">{source}</span>}
        </div>
      )}

      {heard?.pending && (
        <div className="hud-actions">
          <span className="muted">{listening ? "Say yes — or" : "Hold the key and say yes — or"}</span>
          <button className="ghost" onClick={() => void invoke("voice_confirm", { yes: false })}>
            Cancel
          </button>
          <button className="primary" onClick={() => void invoke("voice_confirm", { yes: true })}>
            Yes
          </button>
        </div>
      )}
    </div>
  );
}
