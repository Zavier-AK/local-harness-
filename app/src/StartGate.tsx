import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { ProjectStatus, SessionInfo } from "./types";

type Props = { onStarted: (info: SessionInfo) => void };

export default function StartGate({ onStarted }: Props) {
  const [projectRoot, setProjectRoot] = useState("");
  const [status, setStatus] = useState<ProjectStatus | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const inspect = useCallback(async (path: string) => {
    if (!path.trim()) {
      setStatus(null);
      return;
    }
    try {
      setStatus(await invoke<ProjectStatus>("inspect_project", { projectRoot: path.trim() }));
    } catch {
      setStatus(null);
    }
  }, []);

  // Re-check as the path is typed, so problems surface before committing to a start.
  useEffect(() => {
    const timer = setTimeout(() => void inspect(projectRoot), 250);
    return () => clearTimeout(timer);
  }, [projectRoot, inspect]);

  async function choose() {
    const picked = await open({ directory: true, multiple: false, title: "Choose a project" });
    if (typeof picked === "string") {
      setProjectRoot(picked);
      setError(null);
    }
  }

  async function createRoles() {
    setError(null);
    try {
      await invoke<string>("write_default_roles", { projectRoot: projectRoot.trim() });
      await inspect(projectRoot);
    } catch (err) {
      setError(String(err));
    }
  }

  async function start() {
    setStarting(true);
    setError(null);
    try {
      onStarted(
        await invoke<SessionInfo>("start_session", {
          projectRoot: projectRoot.trim(),
          rolesPath: null,
          model: null,
        }),
      );
    } catch (err) {
      setError(String(err));
    } finally {
      setStarting(false);
    }
  }

  const ready = Boolean(status?.exists && status.has_roles_file);

  return (
    <main className="gate">
      <div className="gate-card">
        <h1>Harness</h1>
        <p className="muted">
          A head agent that plans, and a fleet of workers that do the work — on your
          subscriptions, not API billing.
        </p>

        <label htmlFor="project">Project directory</label>
        <div className="picker">
          <input
            id="project"
            value={projectRoot}
            onChange={(e) => setProjectRoot(e.target.value)}
            placeholder="Choose a folder, or paste a path"
            spellCheck={false}
          />
          <button onClick={choose}>Browse…</button>
        </div>

        {status && (
          <ul className="checks">
            <Check ok={status.exists} label="Directory exists" />
            <Check
              ok={status.is_git_repo}
              label="Git repository"
              hint="Workers need a repo to get their own worktrees."
            />
            <Check
              ok={status.has_roles_file}
              label="roles.toml"
              hint="Defines the fleet."
              action={
                status.exists && !status.has_roles_file ? (
                  <button className="link" onClick={createRoles}>
                    Create the default
                  </button>
                ) : undefined
              }
            />
          </ul>
        )}

        <button className="primary wide" onClick={start} disabled={starting || !ready}>
          {starting ? "Starting…" : "Start session"}
        </button>

        {error && <p className="error">{error}</p>}
      </div>
    </main>
  );
}

function Check({
  ok,
  label,
  hint,
  action,
}: {
  ok: boolean;
  label: string;
  hint?: string;
  action?: React.ReactNode;
}) {
  return (
    <li className={ok ? "ok" : "missing"}>
      <span className="tick">{ok ? "✓" : "○"}</span>
      <span>
        {label}
        {!ok && hint && <span className="muted"> — {hint}</span>}
      </span>
      {action}
    </li>
  );
}
