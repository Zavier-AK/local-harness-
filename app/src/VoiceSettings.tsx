import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Interpretation, VoiceProgress, VoiceSettings, VoiceStatus, WhisperSize } from "./types";

type Props = {
  value: VoiceSettings;
  onChange: (voice: VoiceSettings) => void;
};

const SIZES: { size: WhisperSize; label: string }[] = [
  { size: "tiny.en", label: "Tiny — fastest, 75 MB" },
  { size: "base.en", label: "Base — the default, 142 MB" },
  { size: "small.en", label: "Small — most accurate, 466 MB" },
];

function megabytes(bytes: number): string {
  return `${Math.round(bytes / 1_000_000)} MB`;
}

/** What an interpretation comes to, in a few words, for "Try a phrase". */
function summarize(heard: Interpretation): string {
  // (to_head: put in the chat box, never sent.)
  const o = heard.outcome;
  const how =
    heard.source === "laya"
      ? ` (Laya ${heard.confidence?.toFixed(2) ?? ""}${heard.laya_ms !== null ? `, ${heard.laya_ms} ms` : ""})`
      : heard.source === "matcher"
        ? " (exact command)"
        : "";
  switch (o.outcome) {
    case "act":
      return `${o.describe}${o.confirm ? " — asks you first" : ""}${how}`;
    case "clarify":
      return `Would ask: ${o.question}${how}`;
    case "reply":
      return `Would say: ${o.text}${how}`;
    case "to_head":
      return `Would put in the chat box for Claude: “${o.text}”`;
    case "nothing":
      return "Nothing to do.";
  }
}

/**
 * Settings › Voice: turn push-to-talk on, get the two models it needs, and try phrases
 * against the harness as it is, without doing anything.
 */
export default function VoiceSettingsPanel({ value, onChange }: Props) {
  const [status, setStatus] = useState<VoiceStatus | null>(null);
  const [progress, setProgress] = useState<Record<string, VoiceProgress>>({});
  const [busy, setBusy] = useState<"whisper" | "laya" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [phrase, setPhrase] = useState("");
  const [tried, setTried] = useState<string | null>(null);

  const refresh = () =>
    invoke<VoiceStatus>("voice_status")
      .then(setStatus)
      .catch((err) => setError(String(err)));

  useEffect(() => {
    void refresh();
    const off = listen<VoiceProgress>("voice://progress", ({ payload }) =>
      setProgress((prev) => ({ ...prev, [payload.what]: payload })),
    );
    return () => void off.then((f) => f());
  }, []);

  async function prepare(what: "whisper" | "laya") {
    setBusy(what);
    setError(null);
    try {
      setStatus(await invoke<VoiceStatus>("voice_prepare", { what }));
    } catch (err) {
      setError(String(err));
      void refresh();
    } finally {
      setBusy(null);
      setProgress((prev) => ({ ...prev, [what]: undefined as unknown as VoiceProgress }));
    }
  }

  async function tryPhrase() {
    if (!phrase.trim()) return;
    try {
      setTried(summarize(await invoke<Interpretation>("voice_try", { text: phrase })));
    } catch (err) {
      setTried(String(err));
    }
  }

  const set = (patch: Partial<VoiceSettings>) => onChange({ ...value, ...patch });
  const laya = status?.laya;
  const bar = (what: "whisper" | "laya") => {
    const p = progress[what];
    if (!p || !p.total) return null;
    return (
      <span className="voice-progress" role="progressbar" aria-valuenow={Math.round((p.received / p.total) * 100)}>
        <span style={{ width: `${(p.received / p.total) * 100}%` }} />
        <em>
          {p.file ? `${p.file} · ` : ""}
          {megabytes(p.received)} of {megabytes(p.total)}
        </em>
      </span>
    );
  };

  return (
    <div className="settings-section voice-settings">
      <h3>Voice</h3>
      <p className="muted">
        Hold <kbd>{value.hotkey.replace("Alt", "⌥").replace("Cmd", "⌘").replace("Shift", "⇧").replace(/\+/g, "")}</kbd>{" "}
        anywhere and speak. Each command runs at the pause after it, so you can keep talking. Everything runs on this Mac: Whisper writes down what you said, exact commands are matched instantly, and Laya works out the rest. Anything else goes into the chat box for you to edit and send. Merges, plans and stopping things always ask
        you first.
      </p>
      {status && !status.built && (
        <p className="error">This build has no voice support. Build with the default features (needs cmake).</p>
      )}
      {error && <p className="error">{error}</p>}

      <label className="toggle-row">
        <input type="checkbox" checked={value.enabled} onChange={(e) => set({ enabled: e.target.checked })} />
        <span>Push-to-talk (asks for the microphone the first time)</span>
      </label>

      <label className="field">
        <span>Hotkey</span>
        <input value={value.hotkey} onChange={(e) => set({ hotkey: e.target.value })} spellCheck={false} />
      </label>

      <div className="voice-model">
        <label className="field">
          <span>Speech model</span>
          <select value={value.stt_model} onChange={(e) => set({ stt_model: e.target.value as WhisperSize })}>
            {SIZES.map((s) => (
              <option key={s.size} value={s.size}>
                {s.label}
              </option>
            ))}
          </select>
        </label>
        <div className="voice-model-state">
          {status?.whisper_downloaded && status.settings.stt_model === value.stt_model ? (
            <span className="badge risk-low">downloaded</span>
          ) : (
            <button onClick={() => void prepare("whisper")} disabled={busy !== null || status?.settings.stt_model !== value.stt_model} title={status?.settings.stt_model !== value.stt_model ? "Save first, then download" : undefined}>
              {busy === "whisper" ? "Downloading…" : `Download (${status?.whisper_megabytes ?? "…"} MB)`}
            </button>
          )}
          {bar("whisper")}
        </div>
      </div>

      <div className="voice-model">
        <div className="field">
          <span>Laya</span>
        </div>
        <div className="voice-model-state">
          {laya?.state === "not_installed" && <span className="error">{laya.hint}</span>}
          {laya?.state === "ready" && <span className="badge risk-low">loaded</span>}
          {laya?.state === "failed" && <span className="error">{laya.error}</span>}
          {laya && laya.state !== "not_installed" && laya.state !== "ready" && (
            <button onClick={() => void prepare("laya")} disabled={busy !== null}>
              {busy === "laya"
                ? "Loading…"
                : laya.state === "stopped" && laya.downloaded
                  ? "Load"
                  : "Download and load"}
            </button>
          )}
          {bar("laya")}
        </div>
      </div>
      <p className="muted hint">
        An open decision model that reads the phrasing exact commands miss. About 1.7 GB to
        download, and about 2 GB of memory while loaded.
      </p>

      <label className="toggle-row">
        <input type="checkbox" checked={value.agent} onChange={(e) => set({ agent: e.target.checked })} />
        <span>Voice agent for anything that isn't an exact command</span>
      </label>
      <p className="muted hint">
        A small Claude that works out the steps and does them: "open Notes and jot down milk, eggs and bread, then
        remind me at six to go shopping". It can only use the same safe actions as voice, asks before anything
        risky, and puts requests about code in the chat box. It runs on your Claude subscription (a few seconds and
        a little usage per request); plain commands like "pause" never use it.
      </p>
      {value.agent && (
        <label className="field">
          <span>Agent model</span>
          <input value={value.agent_model} onChange={(e) => set({ agent_model: e.target.value })} spellCheck={false} />
          <span className="muted">haiku is fast and cheap</span>
        </label>
      )}

      {value.agent && (
        <>
          <label className="toggle-row">
            <input type="checkbox" checked={value.browser} onChange={(e) => set({ browser: e.target.checked })} />
            <span>Let it use a browser for longer web tasks</span>
          </label>
          <p className="muted hint">
            "Find the cheapest trail runners on Amazon and add them to the basket": a stronger model works through it
            in its own Chrome window, in the background, and tells you what it found. It reads, scrolls, types and
            clicks freely, but asks for your yes before anything that sends, buys, posts, deletes or submits a form,
            and never types passwords or card details. Sign in to the sites you want it to use once, in its window.
            Needs Google Chrome, and <code>npm install</code> in <code>app/voice-sidecar</code>.
          </p>
          {value.browser && (
            <>
              <div className="field">
                <span>Its window</span>
                <button
                  className="ghost"
                  onClick={() => void invoke("voice_open_browser").catch((err) => setError(String(err)))}
                >
                  Open it to sign in to sites
                </button>
              </div>
              <label className="field">
                <span>Browser model</span>
                <input
                  value={value.browser_model}
                  onChange={(e) => set({ browser_model: e.target.value })}
                  spellCheck={false}
                />
                <span className="muted">sonnet keeps track of long tasks; haiku tends to get lost</span>
              </label>
            </>
          )}
          <label className="field">
            <span>About you</span>
            <textarea
              rows={5}
              value={value.about_me}
              maxLength={4000}
              placeholder={"How you write and who people are, e.g.\nI keep emails short and friendly, sign off with “Cheers, Z”.\nSam is my co-founder (sam@example.com). My manager is Priya."}
              onChange={(e) => set({ about_me: e.target.value })}
            />
            <span className="muted">
              Both agents read this. Emails are drafted in your words and open in Gmail for you to send.
            </span>
          </label>
        </>
      )}

      <label className="field">
        <span>Confidence</span>
        <input
          type="range"
          min={0.5}
          max={0.95}
          step={0.05}
          value={value.confidence}
          onChange={(e) => set({ confidence: Number(e.target.value) })}
          aria-valuetext={value.confidence.toFixed(2)}
        />
        <span className="mono">{value.confidence.toFixed(2)}</span>
      </label>
      <p className="muted hint">
        How sure Laya must be before it acts. Lower acts on more; higher leaves more for you in the chat box.{" "}
        <code>harness-cli voice --eval --laya</code> measures it on real phrases.
      </p>

      <label className="field">
        <span>Pause before acting</span>
        <input
          type="range"
          min={300}
          max={2000}
          step={100}
          value={value.pause_ms}
          onChange={(e) => set({ pause_ms: Number(e.target.value) })}
          aria-valuetext={`${(value.pause_ms / 1000).toFixed(1)} seconds`}
        />
        <span className="mono">{(value.pause_ms / 1000).toFixed(1)}s</span>
      </label>
      <p className="muted hint">
        How long you pause before what you just said runs, while you keep holding the key.
        Shorter feels snappier; longer lets you think mid-sentence.
      </p>

      <label className="field">
        <span>Unload Laya after</span>
        <input
          type="number"
          min={1}
          max={240}
          value={value.laya_idle_minutes}
          onChange={(e) => set({ laya_idle_minutes: Number(e.target.value) })}
        />
        <span className="muted">minutes idle</span>
      </label>

      <label className="toggle-row">
        <input type="checkbox" checked={value.speak_replies} onChange={(e) => set({ speak_replies: e.target.checked })} />
        <span>Say answers aloud</span>
      </label>

      <div className="voice-try">
        <input
          value={phrase}
          onChange={(e) => setPhrase(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void tryPhrase();
          }}
          placeholder="Try a phrase: approve the builder's merge"
        />
        <button onClick={() => void tryPhrase()} disabled={!phrase.trim()}>
          Try
        </button>
      </div>
      {tried && <p className="muted voice-tried">{tried}</p>}
    </div>
  );
}
