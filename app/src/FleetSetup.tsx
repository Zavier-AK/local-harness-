import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  FleetInspection,
  ModelOption,
  RoleModelPatch,
} from "./types";

type Props = {
  projectRoot: string;
  inspection: FleetInspection;
  onSaved: (inspection: FleetInspection) => void;
};

export default function FleetSetup({ projectRoot, inspection, onSaved }: Props) {
  const [customizing, setCustomizing] = useState(false);
  const [selected, setSelected] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const optionById = useMemo(() => {
    const entries = inspection.roles.flatMap((role) =>
      role.options.map((option) => [`${role.name}\u0000${option.id}`, option] as const),
    );
    return new Map(entries);
  }, [inspection]);

  useEffect(() => {
    setSelected({});
  }, [inspection]);

  async function save() {
    const patches: RoleModelPatch[] = Object.entries(selected).flatMap(
      ([roleName, optionId]) => {
        const option = optionById.get(`${roleName}\u0000${optionId}`);
        if (!option) return [];
        return [
          {
            role_name: roleName,
            model: option.model,
            base_url: option.base_url,
          },
        ];
      },
    );
    if (patches.length === 0) return;

    setSaving(true);
    setError(null);
    try {
      const refreshed = await invoke<FleetInspection>("save_role_assignments", {
        projectRoot,
        rolesPath: null,
        patches,
      });
      onSaved(refreshed);
      setCustomizing(false);
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className="fleet-setup">
      <div className="fleet-setup-head">
        <div>
          <h2>Detected backends</h2>
          <p className="muted">Live inventory, independent of the current role settings.</p>
        </div>
        <button onClick={() => setCustomizing((open) => !open)}>
          {customizing ? "Done" : "Customize roles"}
        </button>
      </div>

      <ul className="backend-checks">
        {inspection.backends.map((backend) => (
          <li key={backend.id} className={backend.available ? "available" : "unavailable"}>
            <span className="tick">{backend.available ? "✓" : "○"}</span>
            <span>
              {backend.message}
              {backend.available && backend.models.length > 0 && (
                <span className="backend-models muted">
                  {backend.models
                    .filter((model) => model.capability === "chat")
                    .map((model) => model.id)
                    .join(", ")}
                </span>
              )}
            </span>
          </li>
        ))}
      </ul>

      {customizing && (
        <div className="role-editor">
          <p className="muted role-editor-note">
            Assignments update this project’s roles.toml. Local HTTP models remain limited
            to text-only roles until Codex provides their tool loop.
          </p>
          {inspection.roles.map((role) => (
            <label className="role-assignment" key={role.name}>
              <span>
                <strong>{role.name}</strong>
                <span className="muted mono">
                  {role.isolation} · {role.provider}/{role.model ?? "default"}
                </span>
              </span>
              <RoleSelect
                roleName={role.name}
                currentModel={role.model}
                currentBaseUrl={role.base_url}
                options={role.options}
                blockedReason={role.blocked_reason}
                selected={selected[role.name] ?? ""}
                onChange={(id) =>
                  setSelected((current) => ({ ...current, [role.name]: id }))
                }
              />
            </label>
          ))}
          <div className="role-editor-actions">
            <button
              className="primary"
              disabled={saving || Object.keys(selected).length === 0}
              onClick={save}
            >
              {saving ? "Saving…" : "Save assignments"}
            </button>
          </div>
        </div>
      )}

      {error && <p className="error">{error}</p>}
    </section>
  );
}

function RoleSelect({
  roleName,
  currentModel,
  currentBaseUrl,
  options,
  blockedReason,
  selected,
  onChange,
}: {
  roleName: string;
  currentModel: string | null;
  currentBaseUrl: string | null;
  options: ModelOption[];
  blockedReason: string | null;
  selected: string;
  onChange: (id: string) => void;
}) {
  if (options.length === 0) {
    return (
      <span className="assignment-blocked" title={blockedReason ?? undefined}>
        {blockedReason ?? "No compatible detected models"}
      </span>
    );
  }

  const current = options.find(
    (option) => option.model === currentModel && option.base_url === currentBaseUrl,
  );

  return (
    <select
      aria-label={`Model for ${roleName}`}
      value={selected || current?.id || ""}
      onChange={(event) => onChange(event.target.value)}
    >
      {!current && (
        <option value="" disabled>
          Current model not detected
        </option>
      )}
      {options.map((option) => (
        <option key={option.id} value={option.id}>
          {option.label}
        </option>
      ))}
    </select>
  );
}
