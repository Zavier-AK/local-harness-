import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import type { Heard, QuotaReport, VoicePhase } from "./types";
import { limitsLine, replyLine, speak } from "./voice";

/** How long a message for the head agent waits for a Cancel before it is sent. */
export const SEND_DELAY_MS = 2000;
const BARS = 28;

/**
 * The push-to-talk bar: a small window above everything, shown while listening and
 * answering. It shows what was heard and what it came to, asks yes-or-no when an action
 * needs it, and gives a moment to cancel a message before it reaches the head agent.
 * It decides nothing; the shell and the main window do.
 */
export default function VoiceHud() {
  const [phase, setPhase] = useState<VoicePhase>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [levels, setLevels] = useState<number[]>(() => Array(BARS).fill(0));
  const [heard, setHeard] = useState<Heard | null>(null);
  const [line, setLine] = useState<string | null>(null);
  const [sendAt, setSendAt] = useState<number | null>(null);
  const [, tick] = useState(0);
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
        setPhase(payload.phase);
        setMessage(payload.message);
        if (payload.phase === "listening") {
          keep();
          setHeard(null);
          setLine(null);
          setSendAt(null);
          setLevels(Array(BARS).fill(0));
        } else if (payload.phase === "error") {
          setLine(payload.message);
          scheduleHide(5000);
        } else if (payload.phase === "idle") {
          scheduleHide(600);
        } else {
          keep();
        }
      }),
      listen<number>("voice://level", ({ payload }) => {
        setLevels((prev) => [...prev.slice(1), Math.min(1, payload * 6)]);
      }),
      listen<Heard>("voice://heard", async ({ payload }) => {
        setPhase("idle");
        setHeard(payload);
        let text = replyLine(payload);
        const outcome = payload.interpretation.outcome;
        if (outcome.outcome === "act" && outcome.action.action === "status" && outcome.action.topic === "limits") {
          text = limitsLine(await invoke<QuotaReport>("quotas").catch(() => null));
        }
        setLine(text);
        if (payload.speak && text) speak(text);
        if (payload.pending) {
          keep();
        } else if (outcome.outcome === "to_head") {
          setSendAt(Date.now() + SEND_DELAY_MS);
          scheduleHide(SEND_DELAY_MS + 1500);
        } else if (outcome.outcome === "nothing") {
          scheduleHide(600);
        } else {
          scheduleHide(Math.max(3500, (text?.length ?? 0) * 70));
        }
      }),
    ];
    return () => offs.forEach((off) => void off.then((f) => f()));
  }, []);

  // The countdown ticks while a message waits to be sent.
  useEffect(() => {
    if (sendAt === null) return;
    const timer = window.setInterval(() => {
      tick((t) => t + 1);
      if (Date.now() >= sendAt) setSendAt(null);
    }, 100);
    return () => window.clearInterval(timer);
  }, [sendAt]);

  const interpretation = heard?.interpretation;
  const source =
    interpretation?.source === "laya"
      ? `Laya ${interpretation.confidence !== null ? interpretation.confidence.toFixed(2) : ""}${interpretation.laya_ms !== null ? ` · ${interpretation.laya_ms} ms` : ""}`
      : interpretation?.source === "matcher"
        ? "command"
        : interpretation
          ? "→ Claude"
          : null;
  const transcript = interpretation?.transcript ?? (phase === "thinking" ? message : null);
  const status =
    phase === "listening"
      ? message
        ? `Listening — ${message}?`
        : "Listening…"
      : phase === "transcribing"
        ? "Hearing you…"
        : phase === "thinking"
          ? "Working out what you meant…"
          : null;
  const remaining = sendAt ? Math.max(0, Math.ceil((sendAt - Date.now()) / 100) / 10) : null;

  return (
    <div className={`voice-hud phase-${phase} ${heard?.error || phase === "error" ? "failed" : ""}`} role="status" aria-live="polite">
      <div className="hud-row">
        <span className={`hud-mic ${phase}`} aria-hidden>
          ●
        </span>
        {phase === "listening" ? (
          <div className="hud-wave" aria-label="Listening">
            {levels.map((level, i) => (
              <span key={i} style={{ height: `${Math.max(8, level * 100)}%` }} />
            ))}
          </div>
        ) : (
          <div className="hud-text">
            {transcript && <p className="hud-transcript">“{transcript}”</p>}
            {status && <p className="hud-status muted">{status}</p>}
            {!status && line && <p className="hud-line">{line}</p>}
          </div>
        )}
        {source && phase !== "listening" && <span className="hud-source">{source}</span>}
      </div>

      {heard?.pending && (
        <div className="hud-actions">
          <span className="muted">Hold the key and say yes — or</span>
          <button className="ghost" onClick={() => void invoke("voice_confirm", { yes: false })}>
            Cancel
          </button>
          <button className="primary" onClick={() => void invoke("voice_confirm", { yes: true })}>
            Yes
          </button>
        </div>
      )}
      {remaining !== null && (
        <div className="hud-actions">
          <span className="muted">Sending in {remaining.toFixed(1)}s</span>
          <button
            className="ghost"
            onClick={() => {
              setSendAt(null);
              setLine("Not sent.");
              void emit("voice://cancel-send");
              scheduleHide(1500);
            }}
          >
            Don't send
          </button>
        </div>
      )}
    </div>
  );
}
