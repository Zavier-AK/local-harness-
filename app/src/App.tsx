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
import ToolsView from "./ToolsView";
import AutonomyDial, { nextStop } from "./AutonomyDial";
import PlanBoard from "./PlanBoard";
import SettingsView from "./SettingsView";
import { notifyIfAway } from "./notify";
import { applySettings } from "./appSettings";
import type {
  AppSettings,
  Autonomy,
  Plan,
  StepInput,
  ChatItem,
  HarnessEvent,
  McpStatus,
  ProjectHarnessEvent,
  ProjectView,
  QuotaReport,
  Role,
  SessionInfo,
  UsageRow,
  Worker,
  WorkerActivity,
  Verification,
  VerificationReport,
} from "./types";

export default function App() {
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [chat, setChat] = useState<ChatItem[]>([]);
  const [workers, setWorkers] = useState<Record<string, Worker>>({});
  const [usage, setUsage] = useState<UsageRow[]>([]);
  const [rateLimited, setRateLimited] = useState(false);
  const [selectedWorker, setSelectedWorker] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [activePane, setActivePane] = useState<"chat" | "plan" | "preview">("chat");
  /** The plan on the board: the one under review or running, else the last one. */
  const [plan, setPlan] = useState<Plan | null>(null);
  const [fleetOpen, setFleetOpen] = useState(false);
  const [projects, setProjects] = useState<ProjectView[]>([]);
  const [switching, setSwitching] = useState(false);
  const [adding, setAdding] = useState(false);
  const [limitsOpen, setLimitsOpen] = useState(false);
  const [quotas, setQuotas] = useState<QuotaReport | null>(null);
  /** The session, or one of the two full-pane views that replace it. */
  const [view, setView] = useState<"session" | "tools" | "settings">("session");
  const [autonomy, setAutonomy] = useState<Autonomy>("review");
  /** Per project: which MCP servers its head agent's CLI managed to connect. */
  const [mcpStatus, setMcpStatus] = useState<Record<string, McpStatus>>({});

  // The project's plans, re-read when a different project comes to the front.
  useEffect(() => {
    if (!session?.project_root) return;
    invoke<Plan[]>("list_plans")
      .then((plans) => {
        const live = plans.find((p) => p.status === "draft" || p.status === "running");
        setPlan(live ?? plans.find((p) => p.status === "finished") ?? null);
      })
      .catch(() => setPlan(null));
  }, [session?.project_root]);

  async function runPlan(steps: StepInput[]) {
    if (!plan) return;
    try {
      await invoke("run_plan", { planId: plan.id, steps });
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function sendPlanFeedback(
    steps: StepInput[],
    comments: { step_id: string; text: string }[],
    note: string,
  ) {
    if (!plan) return;
    try {
      await invoke("plan_feedback", { planId: plan.id, steps, comments, note });
      setBusy(true);
      setActivePane("chat");
      setChat((prev) => [
        ...prev,
        { kind: "user", text: note.trim() || "Feedback on the plan" },
        { kind: "notice", tone: "info", text: `Sent ${comments.length} comment(s) on the plan; the head agent is revising it.` },
      ]);
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function discardPlan() {
    if (!plan) return;
    try {
      await invoke("discard_plan", { planId: plan.id });
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  // The project's autonomy level, read whenever a different project comes to the front.
  const activeRoot = session?.project_root;
  useEffect(() => {
    if (!activeRoot) return;
    invoke<Autonomy>("get_autonomy")
      .then(setAutonomy)
      .catch(() => setAutonomy("review"));
  }, [activeRoot]);

  async function changeAutonomy(level: Autonomy) {
    try {
      setAutonomy(await invoke<Autonomy>("set_autonomy", { level }));
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  // Settings that live in the page (accent, notifications) apply from the first frame.
  useEffect(() => {
    invoke<AppSettings>("get_settings")
      .then(applySettings)
      .catch(() => {
        // Defaults are already in the stylesheet; nothing to undo.
      });
  }, []);

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
      // Any project's head reports its MCP servers as it starts; kept per project so the
      // Tools view shows the truth for whichever is in front.
      if (payload.type === "session_started" && payload.run_id.startsWith("orchestrator-")) {
        const failed = payload.mcp_failed ?? [];
        setMcpStatus((prev) => ({
          ...prev,
          [payload.project]: {
            connected: payload.mcp_servers.filter((name) => !failed.includes(name)),
            failed,
          },
        }));
      }

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
      if (payload.type === "plan_updated") {
        const incoming = payload.plan;
        setPlan((current) => {
          // A discarded plan leaves the board only if it is the one on it.
          if (incoming.status === "discarded") return current?.id === incoming.id ? null : current;
          return incoming;
        });
        if (incoming.status === "draft") {
          setActivePane("plan");
          void notifyIfAway("The head agent proposed a plan", incoming.title);
        }
      }
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
      // Ready to review once it has been checked, not merely proposed — so the
      // notification can say how worried to be.
      if (payload.type === "verification_finished" && !landsByItself(autonomy, payload.report)) {
        const { report } = payload;
        void notifyIfAway(
          report.verified
            ? `A change is ready to review — ${report.risk} risk`
            : "A change is ready to review — unverified",
          report.reasons[0] ?? "Checks passed.",
        );
      }
      if (payload.type === "merge_landed" && payload.automatic) {
        void notifyIfAway(
          "A change landed by itself",
          `${payload.risk ?? "verified"} risk — open the app to undo it if you disagree.`,
        );
      }
      if (payload.type === "merge_not_landed") {
        void notifyIfAway("A change could not land by itself", payload.reason);
      }
      if (payload.type === "delegation_requested") {
        void notifyIfAway("The head agent wants to delegate", `${payload.role}: ${firstLine(payload.task)}`);
      }
      if (
        payload.type === "worker_spawned" ||
        payload.type === "merge_requested" ||
        payload.type === "merge_landed" ||
        payload.type === "delegation_requested"
      ) {
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
      if (mod && event.shiftKey && event.key.toLowerCase() === "a") {
        event.preventDefault();
        void changeAutonomy(nextStop(autonomy));
      } else if (mod && event.key === ",") {
        event.preventDefault();
        setView("settings");
      } else if (event.key === "Escape" && view !== "session" && !overlayOpen) {
        // Out of Tools or Settings first; a second Esc can then stop a turn.
        event.preventDefault();
        setView("session");
      } else if (event.key === "Escape" && busy && !overlayOpen) {
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

  async function decide(workerId: string, approve: boolean, text?: string) {
    try {
      if (approve) await invoke("approve_delegation", { workerId, task: text ?? null });
      else await invoke("decline_delegation", { workerId, reason: text ?? null });
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function undoMerge(workerId: string) {
    try {
      await invoke("undo_merge", { workerId });
    } catch (err) {
      setChat((prev) => [...prev, { kind: "notice", tone: "error", text: String(err) }]);
    }
  }

  async function resolveMerge(workerId: string, approve: boolean) {
    try {
      await invoke(approve ? "approve_merge" : "reject_merge", { workerId });
      // A merge announces itself (with Undo) through its `merge_landed` event.
      if (!approve) {
        setChat((prev) => [...prev, { kind: "notice", tone: "info", text: `Discarded ${workerId}.` }]);
      }
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
        <AutonomyDial level={autonomy} onChange={(level) => void changeAutonomy(level)} />
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
          view={view}
          onView={setView}
        />
        <main className={`main-pane ${switching ? "switching" : ""}`}>
          {view === "tools" && (
            <ToolsView
              projectRoot={session.project_root}
              mcpStatus={mcpStatus[session.project_root] ?? null}
              onClose={() => setView("session")}
            />
          )}
          {view === "settings" && (
            <SettingsView onSaved={applySettings} onClose={() => setView("session")} />
          )}
          {/* The session stays mounted underneath, so a half-typed message survives. */}
          <div className={`session-pane ${view === "session" ? "" : "hidden"}`}>
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
                aria-selected={activePane === "plan"}
                className={activePane === "plan" ? "active" : ""}
                onClick={() => setActivePane("plan")}
              >
                Plan
                {plan?.status === "draft" && <span className="tab-dot" aria-label="waiting for you" />}
                {plan?.status === "running" && (
                  <span className="tab-count">
                    {plan.steps.filter((s) => s.state === "landed").length}/{plan.steps.length}
                  </span>
                )}
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
                onUndo={(id) => void undoMerge(id)}
              />
            </div>
            <div
              className={`tab-panel plan-panel ${activePane === "plan" ? "" : "hidden"}`}
              role="tabpanel"
              aria-hidden={activePane !== "plan"}
            >
              <PlanBoard
                plan={plan}
                roles={session.roles}
                workers={workers}
                onRun={(steps) => void runPlan(steps)}
                onFeedback={(steps, comments, note) => void sendPlanFeedback(steps, comments, note)}
                onDiscard={() => void discardPlan()}
                onSelectWorker={setSelectedWorker}
              />
            </div>
            <div
              className={`tab-panel ${activePane === "preview" ? "" : "hidden"}`}
              role="tabpanel"
              aria-hidden={activePane !== "preview"}
            >
              <PreviewPanel
                active={activePane === "preview" && view === "session"}
                obscured={Boolean(selected)}
                projectRoot={session.project_root}
                workerRoots={workerList.map((worker) => worker.cwd)}
              />
            </div>
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
          onDecide={(id, approve, text) => void decide(id, approve, text)}
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
          onUndo={() => void undoMerge(selected.id)}
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
    case "merge_landed":
      return [
        ...prev,
        {
          kind: "landed",
          workerId: event.worker_id,
          text: event.automatic
            ? `Landed ${event.worker_id} automatically${event.risk ? ` — ${event.risk} risk` : ""}.`
            : `Merged ${event.worker_id} into your branch.`,
          undone: false,
        },
      ];

    case "merge_reverted":
      return prev.map((item) =>
        item.kind === "landed" && item.workerId === event.worker_id ? { ...item, undone: true } : item,
      );

    case "plan_updated":
      if (event.plan.status !== "draft") return prev;
      return [
        ...prev,
        {
          kind: "notice",
          tone: "info",
          text: `Proposed a plan — "${event.plan.title}", ${event.plan.steps.length} step(s). Review it on the Plan tab.`,
        },
      ];

    case "plan_finished":
      return [...prev, { kind: "notice", tone: "info", text: `Plan "${event.title}" finished.` }];

    case "merge_not_landed":
      return [
        ...prev,
        {
          kind: "notice",
          tone: "warn",
          text: `${event.worker_id} would have landed by itself but could not: ${event.reason}. It is waiting for you.`,
        },
      ];

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
          verification: null,
          // An approved delegation spawns under the id it waited with; keep its task.
          task: prev[event.worker_id]?.task,
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

    case "delegation_requested":
      return {
        ...prev,
        [event.worker_id]: {
          id: event.worker_id,
          role: event.role,
          provider: "",
          isolation: "",
          cwd: "",
          status: "awaiting_approval",
          summary: "",
          usage: { input_tokens: 0, output_tokens: 0, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
          diff: null,
          branch: null,
          is_error: false,
          startedAt: Date.now(),
          currentTool: null,
          activity: [],
          verification: null,
          task: event.task,
        },
      };

    case "delegation_declined": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.worker_id]: { ...existing, status: "cancelled", summary: `Declined: ${event.reason}` },
      };
    }

    case "merge_landed": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.worker_id]: {
          ...existing,
          branch: null,
          landed: { commit: event.commit, automatic: event.automatic },
        },
      };
    }

    case "merge_reverted": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return { ...prev, [event.worker_id]: { ...existing, landed: null } };
    }

    case "verification_started":
    case "verification_check":
    case "verification_finished": {
      const existing = prev[event.worker_id];
      if (!existing) return prev;
      return {
        ...prev,
        [event.worker_id]: { ...existing, verification: reduceVerification(existing.verification, event) },
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

/** The engine's landing rule (`Autonomy::lands`), so a change about to land by itself is
 * not first announced as waiting for review. */
function landsByItself(level: Autonomy, report: VerificationReport): boolean {
  if (level !== "land_safe" && level !== "land_most") return false;
  const ceiling = level === "land_safe" ? ["low"] : ["low", "medium"];
  return (
    report.verified &&
    ceiling.includes(report.risk) &&
    !report.checks.some((check) => check.status === "failed" || check.status === "error")
  );
}

function reduceVerification(prev: Verification | null, event: HarnessEvent): Verification | null {
  switch (event.type) {
    case "verification_started":
      return { state: "running", checks: [] };
    case "verification_check":
      // A check can arrive without its start when the window opened mid-run.
      return prev?.state === "done"
        ? prev
        : { state: "running", checks: [...(prev?.checks ?? []), event.check] };
    case "verification_finished":
      return { state: "done", report: event.report };
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
