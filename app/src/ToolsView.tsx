import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type { ImportReport, McpServerConfig, McpStatus, RoleTools, SkillInfo } from "./types";

type Props = {
  projectRoot: string;
  mcpStatus: McpStatus | null;
  onClose: () => void;
};

/** Claude Code's built-in tools, offered as suggestions; any rule can still be typed. */
const BUILT_IN_TOOLS = ["Read", "Write", "Edit", "Bash", "Grep", "Glob", "WebFetch", "WebSearch"];

/**
 * Tools & Skills: what the fleet can use.
 *
 * Skills and MCP servers are app-wide — one library, whatever project is open. Tools are
 * per role and per project, because they live in that project's `roles.toml`.
 */
export default function ToolsView({ projectRoot, mcpStatus, onClose }: Props) {
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [servers, setServers] = useState<Record<string, McpServerConfig>>({});
  const [roles, setRoles] = useState<RoleTools[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [skills, servers, roles] = await Promise.all([
        invoke<SkillInfo[]>("list_skills"),
        invoke<Record<string, McpServerConfig>>("list_mcp_servers"),
        invoke<RoleTools[]>("role_tools", { projectRoot }),
      ]);
      setSkills(skills);
      setServers(servers);
      setRoles(roles);
    } catch (err) {
      setError(String(err));
    }
  }, [projectRoot]);

  useEffect(() => {
    void load();
  }, [load]);

  /** Run an action, surfacing its failure rather than swallowing it. */
  async function attempt<T>(action: () => Promise<T>): Promise<T | undefined> {
    setError(null);
    setNotice(null);
    try {
      return await action();
    } catch (err) {
      setError(String(err));
      return undefined;
    }
  }

  function reportImport(report: ImportReport | undefined) {
    if (!report) return;
    const parts = [];
    if (report.imported.length) parts.push(`Imported ${report.imported.join(", ")}.`);
    for (const [name, reason] of report.skipped) parts.push(`Skipped ${name}: ${reason}.`);
    setNotice(parts.join(" ") || "Nothing to import.");
    void load();
  }

  return (
    <section className="settings-view" aria-label="Tools and skills">
      <header className="settings-head">
        <h2>Tools &amp; Skills</h2>
        <button className="ghost" onClick={onClose} title="Back to the session (Esc)">
          Done
        </button>
      </header>

      {error && <p className="error">{error}</p>}
      {notice && <p className="notice info">{notice}</p>}

      <SkillsSection
        skills={skills}
        attempt={attempt}
        onSkills={setSkills}
        onImported={reportImport}
        onCreated={(path) => {
          setNotice(`Created. Write its instructions in ${path}`);
          void load();
        }}
      />

      <RoleToolsSection
        projectRoot={projectRoot}
        roles={roles}
        servers={Object.keys(servers)}
        attempt={attempt}
        onSaved={setRoles}
      />

      <McpSection servers={servers} status={mcpStatus} attempt={attempt} onServers={setServers} />
    </section>
  );
}

type Attempt = <T>(action: () => Promise<T>) => Promise<T | undefined>;

// ---------------------------------------------------------------- skills

function SkillsSection({
  skills,
  attempt,
  onSkills,
  onImported,
  onCreated,
}: {
  skills: SkillInfo[];
  attempt: Attempt;
  onSkills: (skills: SkillInfo[]) => void;
  onImported: (report: ImportReport | undefined) => void;
  onCreated: (path: string) => void;
}) {
  const [gitUrl, setGitUrl] = useState("");
  const [importing, setImporting] = useState(false);
  const [newName, setNewName] = useState("");
  const [newDescription, setNewDescription] = useState("");

  async function importFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Choose a skill folder" });
    if (typeof picked !== "string") return;
    onImported(await attempt(() => invoke<ImportReport>("import_skills_folder", { path: picked })));
  }

  async function importGit() {
    setImporting(true);
    onImported(await attempt(() => invoke<ImportReport>("import_skills_git", { url: gitUrl })));
    setImporting(false);
    setGitUrl("");
  }

  async function create() {
    const path = await attempt(() =>
      invoke<string>("create_skill", { name: newName, description: newDescription }),
    );
    if (path) {
      setNewName("");
      setNewDescription("");
      onCreated(path);
    }
  }

  return (
    <div className="settings-section">
      <h3>Skills</h3>
      <p className="muted">
        Instructions Claude loads when a task calls for them, shown to every Claude worker and
        the head agent as <code>harness:name</code>. Nothing is written into your projects.
        Workers pick up changes on their next task; the head agent when a project is next
        opened.
      </p>

      <ul className="settings-list">
        {skills.map((skill) => (
          <li key={skill.name} className={skill.enabled ? "" : "off"}>
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={skill.enabled}
                onChange={async (event) => {
                  const updated = await attempt(() =>
                    invoke<SkillInfo[]>("set_skill_enabled", {
                      name: skill.name,
                      enabled: event.target.checked,
                    }),
                  );
                  if (updated) onSkills(updated);
                }}
              />
              <span className="settings-name mono">harness:{skill.name}</span>
            </label>
            <span className="settings-desc">{skill.description}</span>
            <span className="settings-meta muted">
              {skill.source === "bundled"
                ? "Built in · adapted from David Ondrej's skills"
                : skill.origin
                  ? `From ${skill.origin}`
                  : "Yours"}
              {skill.path && <span className="mono"> · {skill.path}</span>}
            </span>
            {skill.source === "library" && (
              <button
                className="link"
                onClick={async () => {
                  const updated = await attempt(() =>
                    invoke<SkillInfo[]>("remove_skill", { name: skill.name }),
                  );
                  if (updated) onSkills(updated);
                }}
              >
                Remove
              </button>
            )}
          </li>
        ))}
      </ul>

      <div className="settings-actions">
        <button onClick={() => void importFolder()}>Import a folder…</button>
        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            void importGit();
          }}
        >
          <input
            value={gitUrl}
            onChange={(event) => setGitUrl(event.target.value)}
            placeholder="https://github.com/owner/skills"
            aria-label="Git repository to import skills from"
            spellCheck={false}
          />
          <button type="submit" disabled={!gitUrl.trim() || importing}>
            {importing ? "Importing…" : "Import from Git"}
          </button>
        </form>
      </div>

      <form
        className="inline-form"
        onSubmit={(event) => {
          event.preventDefault();
          void create();
        }}
      >
        <input
          value={newName}
          onChange={(event) => setNewName(event.target.value.toLowerCase())}
          placeholder="new-skill-name"
          aria-label="New skill name"
          spellCheck={false}
        />
        <input
          className="grow"
          value={newDescription}
          onChange={(event) => setNewDescription(event.target.value)}
          placeholder="When should an agent use it?"
          aria-label="New skill description"
        />
        <button type="submit" disabled={!newName.trim() || !newDescription.trim()}>
          New skill
        </button>
      </form>
    </div>
  );
}

// ----------------------------------------------------------------- tools

function RoleToolsSection({
  projectRoot,
  roles,
  servers,
  attempt,
  onSaved,
}: {
  projectRoot: string;
  roles: RoleTools[];
  servers: string[];
  attempt: Attempt;
  onSaved: (roles: RoleTools[]) => void;
}) {
  const suggestions = [...BUILT_IN_TOOLS, ...servers.map((name) => `mcp__${name}`)];
  return (
    <div className="settings-section">
      <h3>Tools, by role</h3>
      <p className="muted">
        What each role in this project may use, saved to its <code>roles.toml</code>. A rule
        can be scoped — <code>Bash(git *)</code> — and <code>mcp__name</code> grants a
        server's tools. Changes apply to the next delegation.
      </p>
      <datalist id="tool-suggestions">
        {suggestions.map((tool) => (
          <option key={tool} value={tool} />
        ))}
      </datalist>
      <ul className="settings-list">
        {roles.map((role) => (
          <RoleToolsRow
            key={role.name}
            projectRoot={projectRoot}
            role={role}
            attempt={attempt}
            onSaved={onSaved}
          />
        ))}
      </ul>
    </div>
  );
}

function RoleToolsRow({
  projectRoot,
  role,
  attempt,
  onSaved,
}: {
  projectRoot: string;
  role: RoleTools;
  attempt: Attempt;
  onSaved: (roles: RoleTools[]) => void;
}) {
  const [tools, setTools] = useState(role.tools);
  const [draft, setDraft] = useState("");
  useEffect(() => setTools(role.tools), [role.tools]);

  const dirty = tools.join("\n") !== role.tools.join("\n");
  const note =
    role.provider === "openai_compat"
      ? "No tool loop — tools are ignored for this backend."
      : role.isolation === "readonly"
        ? "Read-only: edit tools are always denied, whatever is listed."
        : role.native
          ? "Runs as Claude's own subagent; a change applies once the project is reopened."
          : null;

  function add() {
    const tool = draft.trim();
    if (tool && !tools.includes(tool)) setTools([...tools, tool]);
    setDraft("");
  }

  return (
    <li>
      <span className="settings-name">
        {role.name}
        <span className="muted"> · {role.provider}, {role.isolation}</span>
      </span>
      {note && <span className="settings-meta muted">{note}</span>}
      <div className="chip-row">
        {tools.map((tool) => (
          <span key={tool} className="tool-rule mono">
            {tool}
            <button
              className="chip-remove"
              aria-label={`Remove ${tool}`}
              onClick={() => setTools(tools.filter((t) => t !== tool))}
            >
              ×
            </button>
          </span>
        ))}
        <input
          className="chip-input mono"
          list="tool-suggestions"
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              add();
            }
          }}
          onBlur={add}
          placeholder="add a tool"
          aria-label={`Add a tool to ${role.name}`}
          spellCheck={false}
        />
      </div>
      {dirty && (
        <div className="actions">
          <button className="ghost" onClick={() => setTools(role.tools)}>
            Revert
          </button>
          <button
            className="primary"
            onClick={async () => {
              const updated = await attempt(() =>
                invoke<RoleTools[]>("save_role_tools", {
                  projectRoot,
                  role: role.name,
                  tools,
                }),
              );
              if (updated) onSaved(updated);
            }}
          >
            Save
          </button>
        </div>
      )}
    </li>
  );
}

// ------------------------------------------------------------ MCP servers

function McpSection({
  servers,
  status,
  attempt,
  onServers,
}: {
  servers: Record<string, McpServerConfig>;
  status: McpStatus | null;
  attempt: Attempt;
  onServers: (servers: Record<string, McpServerConfig>) => void;
}) {
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"command" | "url">("command");
  const [target, setTarget] = useState("");

  function describe(config: McpServerConfig): string {
    if (config.url) return config.url;
    return [config.command, ...(config.args ?? [])].join(" ");
  }

  function statusOf(server: string): { label: string; tone: string } {
    if (status?.failed.includes(server)) return { label: "failed to connect", tone: "error" };
    if (status?.connected.includes(server)) return { label: "connected", tone: "ok" };
    return { label: "connects when a project opens", tone: "muted" };
  }

  async function add() {
    // A command line splits on whitespace. Arguments that need spaces can be added by
    // editing the stored JSON; the common case (`npx -y some-server`) has none.
    const parts = target.trim().split(/\s+/);
    const config: McpServerConfig =
      kind === "url"
        ? { type: "http", url: target.trim() }
        : { command: parts[0], args: parts.slice(1) };
    const updated = await attempt(() =>
      invoke<Record<string, McpServerConfig>>("set_mcp_server", { name: name.trim(), config }),
    );
    if (updated) {
      onServers(updated);
      setName("");
      setTarget("");
    }
  }

  const names = Object.keys(servers);
  return (
    <div className="settings-section">
      <h3>MCP servers</h3>
      <p className="muted">
        Connected for the head agent and every Claude worker. A role only gets a server's
        tools when its tool list includes <code>mcp__name</code> — the head agent never
        does, so it stays a planner.
      </p>
      {names.length === 0 && <p className="muted">None yet.</p>}
      <ul className="settings-list">
        {names.map((server) => {
          const state = statusOf(server);
          return (
            <li key={server}>
              <span className="settings-name mono">{server}</span>
              <span className="settings-desc mono">{describe(servers[server])}</span>
              <span className={`settings-meta ${state.tone}`}>{state.label}</span>
              <button
                className="link"
                onClick={async () => {
                  const updated = await attempt(() =>
                    invoke<Record<string, McpServerConfig>>("remove_mcp_server", { name: server }),
                  );
                  if (updated) onServers(updated);
                }}
              >
                Remove
              </button>
            </li>
          );
        })}
      </ul>
      <form
        className="inline-form"
        onSubmit={(event) => {
          event.preventDefault();
          void add();
        }}
      >
        <input
          value={name}
          onChange={(event) => setName(event.target.value)}
          placeholder="name"
          aria-label="Server name"
          spellCheck={false}
        />
        <select
          value={kind}
          onChange={(event) => setKind(event.target.value as "command" | "url")}
          aria-label="Server kind"
        >
          <option value="command">Command</option>
          <option value="url">URL</option>
        </select>
        <input
          className="grow mono"
          value={target}
          onChange={(event) => setTarget(event.target.value)}
          placeholder={kind === "url" ? "https://example.com/mcp" : "npx -y @some/mcp-server"}
          aria-label={kind === "url" ? "Server URL" : "Command to run"}
          spellCheck={false}
        />
        <button type="submit" disabled={!name.trim() || !target.trim()}>
          Add server
        </button>
      </form>
    </div>
  );
}
