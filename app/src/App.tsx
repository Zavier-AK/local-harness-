import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import StartGate from "./StartGate";
import HeadChat from "./HeadChat";
import WorkerRail from "./WorkerRail";
import DiffDrawer from "./DiffDrawer";
import BudgetMeter from "./BudgetMeter";
import type { ChatItem, HarnessEvent, Role, SessionInfo, UsageRow, Worker } from "./types";

export default function App() {
  const [session, setSession] = useState<SessionInfo | null>(null);
  const [chat, setChat] = useState<ChatItem[]>([]);
  const [workers, setWorkers] = useState<Record<string, Worker>>({});
  const [usage, setUsage] = useState<UsageRow[]>([]);
  const [rateLimited, setRateLimited] = useState(false);
  const [selectedWorker, setSelectedWorker] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  /** The orchestrator's run id, so its events are told apart from workers'. */
  const headRun = useRef<string | null>(null);

  const refreshUsage = useCallback(async () => {
    try {
      setUsage(await invoke<UsageRow[]>("usage", { hours: 5 }));
    } catch {
      // Usage is a readout, not a control path; a failure here must not break the app.
    }
  }, []);

  useEffect(() => {
    if (!session) return;

    const unlisten = listen<HarnessEvent>("harness://event", ({ payload }) => {
      setChat((prev) => reduceChat(prev, payload, headRun));
      setWorkers((prev) => reduceWorkers(prev, payload));

      if (payload.type === "api_retry") {
        setRateLimited(payload.error === "rate_limit");
      }
      if (payload.type === "run_finished") {
        if (payload.run_id === headRun.current) setBusy(false);
        setRateLimited(false);
        void refreshUsage();
      }
      if (payload.type === "worker_finished") void refreshUsage();
    });

    // The five-hour window may already have burn in it from an earlier session in this
    // project, so show it on open rather than only after the first run finishes.
    void refreshUsage();

    return () => {
      void unlisten.then((off) => off());
    };
  }, [session, refreshUsage]);

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
              text: `Session up. ${info.roles.length - blocked.length} of ${info.roles.length} roles ready.`,
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
        <BudgetMeter usage={usage} rateLimited={rateLimited} />
      </header>

      <div className="panes">
        <HeadChat items={chat} busy={busy} onSend={send} onSelectWorker={setSelectedWorker} />
        <WorkerRail
          workers={workerList}
          roles={session.roles}
          selected={selectedWorker}
          onSelect={setSelectedWorker}
        />
      </div>

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
      return [...prev, { kind: "tool", name: event.name }];

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
          status: event.is_error ? "failed" : "done",
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

    default:
      return prev;
  }
}
