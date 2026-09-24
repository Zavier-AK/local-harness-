import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AppSettings, VoiceSettings } from "./types";
import VoiceSettingsPanel from "./VoiceSettings";

type Props = {
  onSaved: (settings: AppSettings) => void;
  onClose: () => void;
};

/** A few from the theme's own family, plus whatever the colour picker gives. */
const SWATCHES = ["#ff9ebb", "#b69cff", "#7cc4ff", "#7ee0b5", "#f3c67c", "#ff8a6b"];

/** Aliases the Claude CLI resolves itself, so they never go stale with a model release. */
const MODELS = ["opus", "sonnet", "haiku"];

export default function SettingsView({ onSaved, onClose }: Props) {
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [saved, setSaved] = useState<AppSettings | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<AppSettings>("get_settings")
      .then((loaded) => {
        setSettings(loaded);
        setSaved(loaded);
      })
      .catch((err) => setError(String(err)));
  }, []);

  if (!settings) {
    return (
      <section className="settings-view">
        {error ? <p className="error">{error}</p> : <p className="muted">Loading…</p>}
      </section>
    );
  }

  const dirty = JSON.stringify(settings) !== JSON.stringify(saved);

  async function save() {
    if (!settings) return;
    setError(null);
    try {
      const stored = await invoke<AppSettings>("save_settings", { settings });
      setSettings(stored);
      setSaved(stored);
      onSaved(stored);
    } catch (err) {
      setError(String(err));
    }
  }

  const update = (patch: Partial<AppSettings>) => setSettings({ ...settings, ...patch });

  return (
    <section className="settings-view" aria-label="Settings">
      <header className="settings-head">
        <h2>Settings</h2>
        <button className="ghost" onClick={onClose} title="Back to the session (Esc)">
          Done
        </button>
      </header>

      {error && <p className="error">{error}</p>}

      <div className="settings-section">
        <h3>Head agent</h3>
        <p className="muted">
          Used when a project opens. A head agent already running keeps what it started with.
        </p>
        <label className="field">
          <span>Model</span>
          <input
            list="model-suggestions"
            value={settings.default_model ?? ""}
            onChange={(event) => update({ default_model: event.target.value || null })}
            placeholder="Claude CLI default"
            spellCheck={false}
          />
          <datalist id="model-suggestions">
            {MODELS.map((model) => (
              <option key={model} value={model} />
            ))}
          </datalist>
        </label>
        <label className="field">
          <span>Turn limit</span>
          <input
            type="number"
            min={1}
            max={1000}
            value={settings.max_turns}
            onChange={(event) => update({ max_turns: Number(event.target.value) })}
          />
        </label>
      </div>

      <div className="settings-section">
        <h3>Notifications</h3>
        <label className="toggle-row">
          <input
            type="checkbox"
            checked={settings.notifications}
            onChange={(event) => update({ notifications: event.target.checked })}
          />
          <span>Tell me when a worker finishes or a change waits for review, while I'm away</span>
        </label>
      </div>

      <VoiceSettingsPanel
        value={settings.voice}
        onChange={(voice: VoiceSettings) => update({ voice })}
      />

      <div className="settings-section">
        <h3>Accent</h3>
        <div className="swatches" role="radiogroup" aria-label="Accent colour">
          <button
            role="radio"
            aria-checked={settings.accent === null}
            className={`swatch theme ${settings.accent === null ? "chosen" : ""}`}
            onClick={() => update({ accent: null })}
            title="The theme's own"
          >
            Theme
          </button>
          {SWATCHES.map((colour) => (
            <button
              key={colour}
              role="radio"
              aria-checked={settings.accent === colour}
              aria-label={colour}
              className={`swatch ${settings.accent === colour ? "chosen" : ""}`}
              style={{ background: colour }}
              onClick={() => update({ accent: colour })}
            />
          ))}
          <input
            type="color"
            value={settings.accent ?? "#ff9ebb"}
            onChange={(event) => update({ accent: event.target.value })}
            aria-label="Pick any colour"
          />
        </div>
      </div>

      <div className="actions">
        <button className="ghost" disabled={!dirty} onClick={() => setSettings(saved)}>
          Revert
        </button>
        <button className="primary" disabled={!dirty} onClick={() => void save()}>
          Save
        </button>
      </div>
    </section>
  );
}
