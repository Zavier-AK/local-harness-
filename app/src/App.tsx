import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import StartGate from "./StartGate";
import HeadChat from "./HeadChat";
import WorkerRail from "./WorkerRail";
import DiffDrawer from "./DiffDrawer";
import FleetDrawer from "./FleetDrawer";
import ProjectSidebar from "./ProjectSidebar";
import LimitsPanel from "./LimitsPanel";
import BudgetMeter from "./BudgetMeter";
import PreviewPanel from "./PreviewPanel";
import { notifyIfAway } from "./notify";
import type {
  ChatItem,
  HarnessEvent,
  ProjectHarnessEvent,
  ProjectView,
  QuotaReport,
  Role,
  SessionInfo,
  UsageRow,
  Worker,
  WorkerActivity,
} from "./types";

export default function App() {
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [chat, setChat] = useState<ChatItem[]>([]);
  const [workers, setWorkers] = useState<Record<string, Worker>>({});
  const [usage, setUsage] = useState<UsageRow[]>([]);
  const [rateLimited, setRateLimited] = useState(false);
  const [selectedWorker, setSelectedWorker] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [activePane, setActivePane] = useState<"chat" | "preview">("chat");
  const [fleetOpen, setFleetOpen] = useState(false);
  const [projects, setProjects] = useState<ProjectView[]>([]);
  const [switching, setSwitching] = useState(false);
  const [adding, setAdding] = useState(false);
  const [limitsOpen, setLimitsOpen] = useState(false);
  const [quotas, setQuotas] = useState<QuotaReport | null>(null);

  /** The orchestrator's run id, so its events are told apart from workers'. */
  const headRun = useRef<string | null>(null);

  const refreshProjects = useCallback(async () => {
    try {
      setProjects(await invoke<ProjectView[]>("list_projects"));
    } catch {
      // The switcher is a readout; a failure here must not break the app.
    }
  }, []);

  /** Switch projects, resetting the panes that belong to the one being left. */
  const focusProject = useCallback(
    async (projectRoot: string) => {
      if (projectRoot === session?.project_root) return;
      setSwitching(true);
      try {
        const info = await invoke<SessionInfo>("focus_project", { project: projectRoot });
        headRun.current = null;
        setChat([]);
        setWorkers({});
        setSelectedWorker(null);
        setBusy(false);
        setFleetOpen(false);
        setSession(info);
        await refreshProjects();
      } catch (err) {
        setChat((prev) => [...prev, { kind: "notice", text: String(err), tone: "error" }]);
      } finally {
        setSwitching(false);
      }
    },
    [refreshProjects, session],
  );

  const closeProject = useCallback(
    async (projectRoot: string) => {
      await invoke("close_project", { project: projectRoot }).catch(() => {});
      const remaining = await invoke<ProjectView[]>("list_projects").catch(() => []);
      setProjects(remaining);
      // Closing the project in front leaves nothing to show; fall back to another open
      // one, or all the way to the start gate.
      if (session?.project_root === projectRoot) {
        if (remaining.length > 0) await focusProject(remaining[0].project_root);
        else setSession(null);
      }
    },
    [session, focusProject],
  );

  const refreshUsage = useCallback(async () => {
    try {
      setUsage(await invoke<UsageRow[]>("usage", { hours: 5 }));
    } catch {
      // Usage is a readout, not a control path; a failure here must not break the app.
    }
  }, []);

  // A project's earlier conversation, restored when it comes to the front — after an app
  // restart or a switch, the head agent resumes its conversation, and now so does the chat.
  const projectRoot = session?.project_root;
  useEffect(() => {
    if (!projectRoot) return;
    let cancelled = false;
    invoke<HarnessEvent[]>("chat_history", { project: projectRoot })
      .then((events) => {
        if (cancelled || events.length === 0) return;
        const restored = historyToChat(events);
        setChat((prev) => [
          ...restored,
          { kind: "notice", tone: "info", text: "Earlier conversation above." },
          ...prev,
        ]);
      })
      .catch(() => {
        // History is a convenience; a project must still open without it.
      });
    return () => {
      cancelled = true;
    };
  }, [projectRoot]);

  useEffect(() => {
    if (!session) return;

    const unlisten = listen<ProjectHarnessEvent>("harness://event", ({ payload }) => {
      // Every open project emits on this one channel. Another project's events must not
      // land in this project's chat or worker rail — but they do change what the sidebar
      // should say, which is the whole point of showing work happening elsewhere.
      if (payload.project !== session.project_root) {
        void refreshProjects();
        return;
      }

      setChat((prev) => reduceChat(prev, payload, headRun));
      setWorkers((prev) => reduceWorkers(prev, payload));

      if (payload.type === "api_retry") {
        setRateLimited(payload.error === "rate_limit");
      }
      if (payload.type === "turn_interrupted") setBusy(false);
      if (payload.type === "run_finished") {
        if (payload.run_id === headRun.current) setBusy(false);
        setRateLimited(false);
        void refreshUsage();
      }
      if (payload.type === "worker_finished") {
        void refreshUsage();
        void refreshProjects();
        void notifyIfAway(
          payload.is_error ? "A worker stopped without finishing" : "A worker finished",
          firstLine(payload.summary),
        );
      }
      if (payload.type === "merge_requested") {
        void notifyIfAway(
          "A change is ready to review",
          `${payload.diff.files_changed} file(s) on ${payload.branch}`,
        );
      }
      if (payload.type === "worker_spawned" || payload.type === "merge_requested") {
        void refreshProjects();
      }
    });

    // The five-hour window may already have burn in it from an earlier session in this
    // project, so show it on open rather than only after the first run finishes.
    void refreshUsage();
    void refreshProjects();

    return () => {
      void unlisten.then((off) => off());
    };
  }, [session, refreshUsage, refreshProjects]);

  async function send(text: string) {
    setChat((prev) => [...prev, { kind: "user", text }]);
    setBusy(true);
    try {
      await invoke("send_turn", { text });
    } catch (err) {
      setBusy(false);
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  // App-wide shortcuts. Esc in the composer is handled there, where focus usually is;
  // this covers Esc from anywhere else, and project navigation.
  useEffect(() => {
    if (!session) return;
    const overlayOpen = Boolean(selectedWorker) || fleetOpen || adding || limitsOpen;

    function onKey(event: KeyboardEvent) {
      const mod = event.metaKey || event.ctrlKey;
      if (event.key === "Escape" && busy && !overlayOpen) {
        event.preventDefault();
        void stopTurn();
      } else if (mod && event.key.toLowerCase() === "n") {
        event.preventDefault();
        setAdding(true);
      } else if (mod && /^[1-9]$/.test(event.key)) {
        const target = projects[Number(event.key) - 1];
        if (target) {
          event.preventDefault();
          void focusProject(target.project_root);
        }
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  async function stopTurn() {
    try {
      await invoke("stop_turn");
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function stopWorker(workerId: string) {
    try {
      await invoke("stop_worker", { workerId });
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function resolveMerge(workerId: string, approve: boolean) {
    try {
      await invoke(approve ? "approve_merge" : "reject_merge", { workerId });
      setChat((prev) => [
        ...prev,
        {
          kind: "notice",
          tone: "info",
          text: approve ? `Merged ${workerId} into your branch.` : `Discarded ${workerId}.`,
        },
      ]);
      setWorkers((prev) => {
        const next = { ...prev };
        if (next[workerId]) next[workerId] = { ...next[workerId], branch: null };
        return next;
      });
      setSelectedWorker(null);
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  if (!session) {
    return (
      <StartGate
        onStarted={(info) => {
          setSession(info);
          const blocked = info.roles.filter((r) => !r.available);
          setChat([
            {
              kind: "notice",
              tone: "info",
              text: info.resumed_head_session
                ? `Session up. Resumed the previous Harness-owned Claude conversation. ${info.roles.length - blocked.length} of ${info.roles.length} roles ready.`
                : `Session up with a fresh Claude conversation. ${info.roles.length - blocked.length} of ${info.roles.length} roles ready.`,
            },
            // Surfaced here rather than discovered mid-delegation, which costs a turn.
            // Grouped by reason: four roles blocked on one missing CLI is one problem
            // to fix, not four, and repeating the same sentence per role buries that.
            ...groupByReason(blocked),
          ]);
        }}
      />
    );
  }

  const workerList = Object.values(workers).sort((a, b) => b.startedAt - a.startedAt);
  const selected = selectedWorker ? workers[selectedWorker] : null;

  return (
    <div className="app">
      <header className="titlebar">
        <span className="title">Harness</span>
        <span className="muted mono">{session.project_root}</span>
        <BudgetMeter
          usage={usage}
          rateLimited={rateLimited}
          onOpenLimits={() => {
            setLimitsOpen(true);
            void invoke<QuotaReport>("quotas").then(setQuotas).catch(() => setQuotas(null));
          }}
        />
      </header>

      <div className="panes">
        <ProjectSidebar
          projects={projects}
          onFocus={focusProject}
          onClose={closeProject}
          onAdd={() => setAdding(true)}
        />
        <main className={`main-pane ${switching ? "switching" : ""}`}>
          <nav className="pane-tabs" role="tablist" aria-label="Main pane">
            <button
              role="tab"
              aria-selected={activePane === "chat"}
              className={activePane === "chat" ? "active" : ""}
              onClick={() => setActivePane("chat")}
            >
              Chat
            </button>
            <button
              role="tab"
              aria-selected={activePane === "preview"}
              className={activePane === "preview" ? "active" : ""}
              onClick={() => setActivePane("preview")}
            >
              Preview
            </button>
          </nav>
          <div
            className={`tab-panel chat-panel ${activePane === "chat" ? "" : "hidden"}`}
            role="tabpanel"
            aria-hidden={activePane !== "chat"}
          >
            <HeadChat
              items={chat}
              onStop={() => void stopTurn()}
              busy={busy}
              onSend={send}
              onSelectWorker={setSelectedWorker}
            />
          </div>
          <div
            className={`tab-panel ${activePane === "preview" ? "" : "hidden"}`}
            role="tabpanel"
            aria-hidden={activePane !== "preview"}
          >
            <PreviewPanel
              active={activePane === "preview"}
              obscured={Boolean(selected)}
              projectRoot={session.project_root}
              workerRoots={workerList.map((worker) => worker.cwd)}
            />
          </div>
        </main>
        <WorkerRail
          workers={workerList}
          roles={session.roles}
          backends={session.backends}
          selected={selectedWorker}
          onSelect={setSelectedWorker}
          onChangeFleet={() => setFleetOpen(true)}
          onStopWorker={(id) => void stopWorker(id)}
        />
      </div>

      {limitsOpen && (
        <LimitsPanel quotas={quotas} usage={usage} onClose={() => setLimitsOpen(false)} />
      )}

      {adding && (
        <div className="drawer-scrim" onClick={() => setAdding(false)}>
          <div className="add-project" onClick={(event) => event.stopPropagation()}>
            <StartGate
              onStarted={(info) => {
                setAdding(false);
                void focusProject(info.project_root);
              }}
            />
          </div>
        </div>
      )}

      {fleetOpen && (
        <FleetDrawer
          projectRoot={session.project_root}
          onClose={() => setFleetOpen(false)}
          onRolesChanged={(roles) =>
            setSession((current) => (current ? { ...current, roles } : current))
          }
        />
      )}

      {selected && (
        <DiffDrawer
          worker={selected}
          onClose={() => setSelectedWorker(null)}
          onApprove={() => resolveMerge(selected.id, true)}
          onReject={() => resolveMerge(selected.id, false)}
        />
      )}
    </div>
  );
}

/**
 * One notice per distinct cause, naming the roles it blocks.
 *
 * Several roles usually share a backend, so listing a reason per role repeats the same
 * remedy and hides how few things actually need fixing.
 */
function groupByReason(blocked: Role[]): ChatItem[] {
  const byReason = new Map<string, string[]>();

  for (const role of blocked) {
    const reason = role.unavailable_reason ?? "backend not reachable";
    byReason.set(reason, [...(byReason.get(reason) ?? []), role.name]);
  }

  return [...byReason].map(([reason, names]) => ({
    kind: "notice" as const,
    tone: "warn" as const,
    text: `${names.join(", ")} unavailable — ${reason}`,
  }));
}

/** Fold an event into the head transcript. */
function reduceChat(
  prev: ChatItem[],
  event: HarnessEvent,
  headRun: React.MutableRefObject<string | null>,
): ChatItem[] {
  switch (event.type) {
    case "session_started":
      // The first session to appear is the head agent; workers come later.
      if (headRun.current === null) headRun.current = event.run_id;
      return prev;

    case "assistant_text": {
      if (event.run_id !== headRun.current) return prev;

      // Deltas append to the open assistant bubble; a settled block replaces it only if
      // nothing was streamed, so text is never shown twice.
      const last = prev[prev.length - 1];
      if (last?.kind === "assistant") {
        if (!event.partial) return prev;
        return [...prev.slice(0, -1), { kind: "assistant", text: last.text + event.text }];
      }
      return [...prev, { kind: "assistant", text: event.text }];
    }

    case "tool_call":
      if (event.run_id !== headRun.current) return prev;
      return [...prev, { kind: "tool", name: event.name, toolUseId: event.tool_use_id }];

    case "tool_result": {
      // A delegation's result names the worker it started; linking the chip to it is what
      // makes "Delegating to a worker" clickable instead of a dead label.
      if (event.run_id !== headRun.current) return prev;
      const workerId = event.content.match(/w-[0-9a-f]{32}/)?.[0];
      if (!workerId) return prev;
      return prev.map((item) =>
        item.kind === "tool" && item.toolUseId === event.tool_use_id
          ? { ...item, workerId }
          : item,
      );
    }

    case "turn_interrupted":
      if (event.run_id !== headRun.current) return prev;
      return [
        ...prev,
        { kind: "notice", tone: "info", text: "Stopped. Send a message to carry on from here." },
      ];

    case "merge_requested":
      return [
        ...prev,
        {
          kind: "notice",
          tone: "warn",
          text: `${event.worker_id} proposes ${event.diff.files_changed} file(s) on ${event.branch}. Review before it lands.`,
        },
      ];

    case "api_retry":
      return [
        ...prev,
        {
          kind: "notice",
          tone: "warn",
          text: `${event.error} — retrying (${event.attempt}/${event.max_retries}).`,
        },
      ];

    case "error":
      return [...prev, { kind: "notice", tone: "error", text: event.message }];

    default:
      return prev;
  }
}

function reduceWorkers(
  prev: Record<string, Worker>,
  event: HarnessEvent,
): Record<string, Worker> {
  switch (event.type) {
    case "worker_spawned":
      return {
        ...prev,
        [event.worker_id]: {
          id: event.worker_id,
          role: event.role,
          provider: event.provider,
          isolation: event.isolation,
          cwd: event.cwd,
          status: "running",
          summary: "",
          usage: {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
          },
          diff: null,
          branch: null,
          is_error: false,
          startedAt: Date.now(),
          currentTool: null,
          activity: [],
        },
      };

    case "worker_status_changed": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return { ...prev, [event.worker_id]: { ...existing, status: event.status } };
    }

    case "worker_finished": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.worker_id]: {
          ...existing,
          // A stop arrives as its own status change first; `is_error` alone would call it
          // a failure.
          status: existing.status === "cancelled" ? "cancelled" : event.is_error ? "failed" : "done",
          currentTool: null,
          summary: event.summary,
          usage: event.usage,
          diff: event.diff,
          is_error: event.is_error,
        },
      };
    }

    case "merge_requested": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.worker_id]: { ...existing, branch: event.branch, diff: event.diff },
      };
    }

    // A worker's own stream: its run id is its worker id. Kept, rather than dropped, so
    // the rail shows what a worker is doing instead of only that it is running.
    case "tool_call": {
      const existing = prev[event.run_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.run_id]: {
          ...existing,
          currentTool: event.name,
          activity: pushActivity(existing.activity, {
            kind: "tool",
            name: event.name,
            detail: toolDetail(event.input),
          }),
        },
      };
    }

    case "assistant_text": {
      const existing = prev[event.run_id];
      if (!existing || event.partial || !event.text.trim()) return prev;
      return {
        ...prev,
        [event.run_id]: {
          ...existing,
          activity: pushActivity(existing.activity, { kind: "text", text: event.text.trim() }),
        },
      };
    }

    default:
      return prev;
  }
}

/** Recent activity only: a long-running worker would otherwise grow without bound. */
const ACTIVITY_LIMIT = 40;

function pushActivity(list: WorkerActivity[], item: WorkerActivity): WorkerActivity[] {
  const next = [...list, item];
  return next.length > ACTIVITY_LIMIT ? next.slice(next.length - ACTIVITY_LIMIT) : next;
}

/** The one argument that says what a tool call is about — a path, a command, a query. */
function toolDetail(input: unknown): string | null {
  if (!input || typeof input !== "object") return null;
  const fields = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "command", "pattern", "url", "description"]) {
    const value = fields[key];
    if (typeof value === "string" && value.trim()) {
      const line = value.trim().split("\n")[0];
      return line.length > 120 ? `${line.slice(0, 117)}…` : line;
    }
  }
  return null;
}

/** The head agent's earlier conversation, rebuilt from its recorded events. */
function historyToChat(events: HarnessEvent[]): ChatItem[] {
  const items: ChatItem[] = [];
  for (const event of events) {
    switch (event.type) {
      case "user_message":
        items.push({ kind: "user", text: event.text });
        break;
      case "assistant_text":
        if (!event.partial) items.push({ kind: "assistant", text: event.text });
        break;
      case "tool_call":
        items.push({ kind: "tool", name: event.name, toolUseId: event.tool_use_id });
        break;
      case "turn_interrupted":
        items.push({ kind: "notice", tone: "info", text: "Stopped." });
        break;
      case "error":
        items.push({ kind: "notice", tone: "error", text: event.message });
        break;
      default:
        break;
    }
  }
  return items;
}

function firstLine(text: string): string {
  const line = text.trim().split("\n")[0] ?? "";
  return line.length > 140 ? `${line.slice(0, 137)}…` : line;
}
