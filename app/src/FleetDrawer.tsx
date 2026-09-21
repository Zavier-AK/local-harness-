import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import FleetSetup from "./FleetSetup";
import type { FleetInspection, Role } from "./types";

type Props = {
  projectRoot: string;
  onClose: () => void;
  onRolesChanged: (roles: Role[]) => void;
};

/**
 * Reassigning a role while the session runs.
 *
 * Workers are spawned per delegation, so a swap costs nothing and needs no restart: the
 * next delegation resolves against the new fleet. In-flight workers finish on the backend
 * they started with, which is deliberate — killing them would throw away their worktree.
 */
export default function FleetDrawer({ projectRoot, onClose, onRolesChanged }: Props) {
  const [inspection, setInspection] = useState<FleetInspection | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<FleetInspection>("inspect_fleet", { projectRoot, rolesPath: null })
      .then((found) => !cancelled && setInspection(found))
      .catch((err) => !cancelled && setError(String(err)));
    return () => {
      cancelled = true;
    };
  }, [projectRoot]);

  async function saved(refreshed: FleetInspection) {
    setInspection(refreshed);
    // The engine already has the new fleet; re-probe so the rail agrees with it.
    try {
      onRolesChanged(await invoke<Role[]>("session_roles"));
    } catch (err) {
      setError(String(err));
    }
  }

  return (
    <>
      <div className="drawer-scrim" onClick={onClose} />
      <aside className="drawer fleet-drawer" role="dialog" aria-label="Change the fleet">
        <header className="drawer-head">
          <h2>Fleet</h2>
          <button onClick={onClose}>Close</button>
        </header>

        <p className="muted">
          Changes apply to the next delegation. Workers already running finish on the
          backend they started with.
        </p>

        {error && <p className="error">{error}</p>}
        {!inspection && !error && <p className="muted">Checking what is installed…</p>}
        {inspection && (
          <FleetSetup projectRoot={projectRoot} inspection={inspection} onSaved={saved} />
        )}
      </aside>
    </>
  );
}
