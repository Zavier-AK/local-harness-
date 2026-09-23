import { useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatItem } from "./types";

type Props = {
  items: ChatItem[];
  busy: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
  onSelectWorker: (id: string) => void;
  onUndo: (workerId: string) => void;
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

export default function HeadChat({ items, busy, onSend, onStop, onSelectWorker, onUndo }: Props) {
  const [draft, setDraft] = useState("");
  const endRef = useRef<HTMLDivElement>(null);

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

      <div className="composer">
        <textarea
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
