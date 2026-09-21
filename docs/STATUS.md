# local-harness — project status

_Last updated: 20 September 2026_

## What this is

A hierarchical multi-agent harness that runs on **subscriptions rather than API billing**.

Anthropic blocked third-party tools from using Claude subscription credentials in January
2026, so every off-the-shelf multi-agent harness wants its own API key. This project
sidesteps that by driving the vendors' **own CLIs as subprocesses**: `claude` and `codex`
each read the OAuth token they already hold, so the work is covered by Claude Pro and
ChatGPT Plus. Local models cost nothing at all.

A long-lived head agent plans and delegates. A fleet of workers does the work, each with a
role, a backing model, and an isolation policy.

```
head agent (claude -p, one long-lived process)
├── MCP delegation tools over loopback HTTP
├── workers → claude / codex / local model / mock
└── each worker: own git worktree, read-only sandbox, or no filesystem
```

## Findings that shaped the design

Verified against live docs and against the real CLI, not assumed.

| Finding | Consequence |
|---|---|
| `--input-format stream-json` keeps **one** `claude` process alive across all turns | The ~34k-token context floor is paid once per session, not per message. Measured: turn 1 cost 39,306 cache-creation tokens, turn 2 cost **906**. This is why the head chat is cheap enough to leave running. |
| `claude -p` still bills to the subscription | The June 15 2026 change that would have moved it to a separate credit pool was **paused**. Rate limits, not dollars, are the binding constraint. |
| `--bare` skips OAuth and the keychain by design | Unusable here — it requires an API key, which defeats the premise. Never passed. |
| Codex reserves `ollama` and `lmstudio` as built-in provider ids | A local model gets a **real agent loop with tools and sandboxing for free**, no code written. A server on another machine needs its own provider id, since those two cannot be overridden. |
| CLI 2.1.278 emits no `provider` field | There is no programmatic readout of which auth mode is in use. The app cannot display it; confirm by hand instead. |
| `pi-herdr` drives terminal panes and scrapes them | Wrong substrate for a GUI, and bound to the `pi` agent. Not forked; its preset/spawn concept borrowed, engine written fresh. |

## Architecture

The engine is a **plain Rust library with no GUI dependency**. This is deliberate: the
Tauri app and the CLI are two front ends over identical code, so the whole orchestration
loop stays testable without a desktop.

| Crate / dir | Role |
|---|---|
| `crates/harness-core` | The engine: agents, isolation, MCP server, orchestrator, SQLite store |
| `crates/harness-cli` | Headless driver — `roles`, `run-worker`, `chat`, `mcp-serve`, `usage` |
| `app/` | Tauri v2 shell: head chat, worker rail, diff drawer, budget meter |
| `roles.toml` | The fleet |

**Delegation is MCP tools, not prompt parsing.** The engine hosts a streamable-HTTP MCP
server on an ephemeral loopback port behind a per-session bearer token; Claude Code
connects via `--mcp-config`, so `delegate(...)` is a first-class tool call.

**Isolation is per-role.** `worktree` (own branch, parallel-safe), `readonly` (throwaway
worktree *and* edit tools denied), `shared` (project root, behind a lock admitting one
worker at a time), `none` (no filesystem).

**Landing code is a human action.** `request_merge` only queues a diff for review.
`approve_merge` exists solely on the host side and is **not** an MCP tool, so no model
output can merge anything.

## Where it stands

### Working and verified

- **108 engine tests** (116 including the separate Tauri shell workspace), no network or
  CLI login required.
- **Orchestrator** — live end-to-end against the real `claude` CLI: it called `list_roles`,
  then `delegate`, a worker wrote into its worktree, and it correctly reported the work as
  pending review rather than landed.
- **MCP surface** — exercised over the wire with `curl`. Unauthenticated and bad-token
  requests both refused with 401; all six tools listed with correct schemas.
- **Merge gate** — `request_merge` left `HEAD` unmoved; calling `approve_merge` through MCP
  returns `tool not found`.
- **Isolation** — worker writes land in the worktree and on a branch, never in the project
  root. Confirmed on the target machine, not just in CI.
- **GUI** — launches, starts a session, head chat responds, fleet shows live availability.
  Dark cherry-blossom theme over a translucent vibrancy layer.
- **Local model backend.** Verified against the running LM Studio model. Backend discovery
  now enumerates the exact model ids independently from whichever endpoint a role uses.
- **CLI** — all five subcommands working on the target machine.

### Not yet verified

- **Codex backend.** The CLI is not installed. Its event parsing is covered only by unit
  tests over recorded output shapes; its vocabulary has shifted between releases, so the
  first real run is where that gets confirmed.
- **A real multi-step task.** Everything so far has been one delegation deep.
- **Rate-limit shedding.** The code path exists and is unit-tested; it has never fired
  against an actual rate limit.
- **The vibrancy effect itself.** Verified only that the page is properly translucent — no
  `NSVisualEffectView` exists off macOS.
- **The embedded Preview webview on the target Mac.** URL validation, server discovery,
  compilation, and frontend integration are tested; the native child-webview behavior
  still needs its first target-platform GUI pass.

### Known gaps

- **No CI.** The repository has no workflows, so the 108 tests run only by hand. This
  matters more than usual here: both CLIs' JSON output is parsed leniently against
  fixtures rather than a stable contract, so upstream schema drift would go unnoticed
  until a live run misbehaved. Deferred by choice.
- **No auth-mode readout**, per the CLI finding above.
- **Diff drawer shows changes, not history.** Sessions persist to SQLite but the UI does
  not reload them yet.

## Environment

| Piece | State |
|---|---|
| MacBook Pro M5 Pro | Primary machine |
| Claude Pro | Working — `architect` and `builder` roles are live |
| ChatGPT Plus | Codex CLI **not yet installed** — 3 roles blocked on it |
| LM Studio | Running and verified with `qwen/qwen3-coder-30b` on `:1234`. Heavy on the host; turn limits kept deliberately low |
| Rust, Node | Installed and working |

## Next steps

In the order that unblocks the most.

1. **Install Codex when ready** (`codex login`). This unlocks three roles at once: `reviewer` for
   second opinions on ChatGPT Plus, and `local_builder` / `tester` which turn the local
   Qwen into a *real* agent with tools and its own worktree. Biggest single unlock
   available.
2. **Run a real task through `chat`.** The untested question is behavioural, not
   mechanical: does the head agent actually delegate sensibly, or try to do the work
   itself? Nothing in the test suite can answer that.
3. **Watch the burn.** `harness-cli usage --hours 5` through a working session answers the
   original open question — how hard does this hit the Pro window versus interactive use?
   If it is tight, that argues for Max rather than for API keys.
4. **Tune the fleet from evidence.** Turn limits, which roles exist, which model backs
   each. The current values are educated guesses.
5. **Add CI** when the schema-drift risk starts to bite.
6. **Then the deferred UI work**: session reload, richer diff review, multi-project
   workspaces.

## Risks worth tracking

- **Subscription policy is the load-bearing assumption.** The paused June 15 change and
  `--bare`'s slated promotion to the `-p` default both point one way. The agent layer sits
  behind an enum, so an API-key or OpenRouter backend is a config swap, not a rewrite.
- **Stream schema drift.** Both CLIs' JSON is parsed leniently and covered by fixture
  tests; unrecognized lines yield no events rather than failing a run.
- **Terms of service.** Solo developer on personal repositories is ordinary individual use.
  Worth reading Anthropic's compliance docs before pointing this at anything work-adjacent.
