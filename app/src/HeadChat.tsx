import { useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatItem, PermissionDecision } from "./types";

type Props = {
  items: ChatItem[];
  busy: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
  onSelectWorker: (id: string) => void;
  onUndo: (workerId: string) => void;
  /** Words said for the head agent: added to the box to edit and send, never sent. */
  dictated?: { text: string; at: number } | null;
  /** Changes when voice says "send it": the draft is handed to `onDraftForVoice`. */
  sendDraft?: number;
  onDraftForVoice?: (draft: string | null) => void;
  /** The person's answer to something the head agent asked to do. */
  onPermission: (requestId: string, decision: PermissionDecision) => void;
  /** The repository has no commits yet, so workers can't start. */
  needsFirstCommit?: boolean;
  onFirstCommit?: () => void;
};

/** Tool calls into the harness read as delegation, not as plumbing. */
function toolLabel(name: string): string {
  const stripped = name.replace(/^mcp__harness__/, "");
  switch (stripped) {
    case "delegate":
      return "Delegating to a worker";
    case "delegate_async":
      return "Fanning out a worker";
    case "check_workers":
      return "Checking on workers";
    case "collect":
      return "Collecting a result";
    case "list_roles":
      return "Reading the fleet";
    case "request_merge":
      return "Proposing a merge";
    default:
      return stripped;
  }
}

export default function HeadChat({
  items,
  busy,
  onSend,
  onStop,
  onSelectWorker,
  onUndo,
  dictated,
  sendDraft,
  onDraftForVoice,
  onPermission,
  needsFirstCommit,
  onFirstCommit,
}: Props) {
  const [draft, setDraft] = useState("");
  const endRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // What was said lands after anything already typed, ready to edit.
  useEffect(() => {
    if (!dictated?.text) return;
    setDraft((prev) => (prev.trim() ? `${prev.trimEnd()} ${dictated.text}` : dictated.text));
    requestAnimationFrame(() => {
      const input = inputRef.current;
      if (!input) return;
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);
    });
  }, [dictated?.at]); // eslint-disable-line react-hooks/exhaustive-deps

  // "Send it": hand the draft over to be sent, and clear the box.
  useEffect(() => {
    if (!sendDraft) return;
    const text = draft.trim();
    onDraftForVoice?.(text || null);
    if (text) setDraft("");
  }, [sendDraft]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [items]);

  function submit() {
    const text = draft.trim();
    if (!text || busy) return;
    onSend(text);
    setDraft("");
  }

  return (
    <section className="chat">
      <div className="transcript">
        {items.map((item, i) => {
          switch (item.kind) {
            case "user":
              return (
                <div key={i} className="bubble user">
                  {item.text}
                </div>
              );
            case "assistant":
              // Rendered as markdown: the head agent writes plans, lists and code, and
              // as plain text those arrive as a wall of asterisks and backticks. Raw HTML
              // stays off — model output is never trusted as markup.
              return (
                <div key={i} className="bubble assistant markdown">
                  <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
                </div>
              );
            case "tool":
              return (
                <button
                  key={i}
                  className="tool-chip"
                  onClick={() => item.workerId && onSelectWorker(item.workerId)}
                  disabled={!item.workerId}
                >
                  {toolLabel(item.name)}
                </button>
              );
            case "notice":
              return (
                <div key={i} className={`notice ${item.tone}`}>
                  {item.text}
                </div>
              );
            case "permission":
              return <PermissionCard key={i} item={item} onAnswer={onPermission} />;
            case "landed":
              // The way back sits right next to the news, so letting changes land by
              // themselves never means losing control of them.
              return (
                <div key={i} className={`notice landed ${item.undone ? "undone" : ""}`}>
                  <span>{item.undone ? `${item.text} Undone.` : item.text}</span>
                  {!item.undone && (
                    <button className="link" onClick={() => onUndo(item.workerId)}>
                      Undo
                    </button>
                  )}
                </div>
              );
          }
        })}
        {busy && (
          <div className="bubble assistant thinking">
            <span className="dot" />
            <span className="dot" />
            <span className="dot" />
          </div>
        )}
        <div ref={endRef} />
      </div>

      {needsFirstCommit && (
        <div className="first-commit">
          <span>
            This repository has no commits yet, so workers have nothing to branch from.
          </span>
          <button onClick={onFirstCommit} title="Adds a .gitignore if there is none, then commits everything else">
            Make the first commit
          </button>
        </div>
      )}

      <div className="composer">
        <textarea
          ref={inputRef}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            // Enter sends; Shift+Enter is a newline; Escape stops a running turn.
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
            } else if (e.key === "Escape" && busy) {
              e.preventDefault();
              onStop();
            }
          }}
          placeholder={busy ? "Working… (Esc to stop)" : "What should the fleet do?"}
          rows={3}
          spellCheck={false}
        />
        {busy ? (
          // While the head agent works, the send button is the way out. Stopping keeps
          // the conversation: the next message carries on from here.
          <button className="stop" onClick={onStop} title="Stop this turn (Esc)">
            Stop
          </button>
        ) : (
          <button onClick={submit} disabled={!draft.trim()}>
            Send
          </button>
        )}
      </div>
    </section>
  );
}

/** Something the head agent wants to do that it isn't approved for: a command, a file
 * change. It waits, mid-turn, for the person. */
function PermissionCard({
  item,
  onAnswer,
}: {
  item: Extract<ChatItem, { kind: "permission" }>;
  onAnswer: (requestId: string, decision: PermissionDecision) => void;
}) {
  const settled = item.state === "allowed" || item.state === "denied" || item.state === "lapsed";
  return (
    <div className={`permission ${item.state}`}>
      <div className="permission-head">
        <span className="permission-tool">{item.tool}</span>
        <span className="permission-what">{item.description}</span>
      </div>
      {item.detail && <pre className="permission-detail">{item.detail}</pre>}
      {settled ? (
        <div className="permission-outcome">
          {item.state === "allowed" ? "Allowed" : item.state === "denied" ? "Denied" : "No longer asked"}
        </div>
      ) : (
        <div className="permission-actions">
          <button
            className="primary"
            disabled={item.state === "sending"}
            onClick={() => onAnswer(item.requestId, "allow")}
          >
            Allow
          </button>
          {item.rules.length > 0 && (
            <button
              disabled={item.state === "sending"}
              onClick={() => onAnswer(item.requestId, "always")}
              title={`Always allow in this project: ${item.rules.join(", ")}`}
            >
              Always allow
            </button>
          )}
          <button
            className="deny"
            disabled={item.state === "sending"}
            onClick={() => onAnswer(item.requestId, "deny")}
          >
            Deny
          </button>
        </div>
      )}
    </div>
  );
}
