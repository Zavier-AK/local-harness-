# local-harness — project status

_Last updated: 22 September 2026_

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
| A stream-json `control_request` of subtype `interrupt` stops a turn and keeps the process | **Stop** costs neither the conversation nor its cached context. Verified: the turn ends within ~0.1 s and the same process answers the next one. |
| Every turn's stream carries a `rate_limit_event` with five-hour and weekly utilization and reset times | The limits panel shows **real Claude quota**. An earlier version wrongly said none was readable under `-p`; that was corrected. |
| Claude Code's native subagents (`--agents`, the `Agent` tool) take a `WorktreeCreate` hook that replaces worktree creation | Claude roles delegate the way Opus is trained to, while still working in the harness's own bootstrapped worktrees behind the merge gate. |
| A session-level `--disallowedTools` binds subagents too | The head agent is kept read-only by approving only `Read`/`Grep`/`Glob`/`Agent`, not by a deny list — otherwise builder subagents cannot write. |
| A head agent with `Bash` copied a subagent's work straight into the checkout, skipping review | The brief and subagent preamble now say outright that absence from the checkout is correct. |
| `--plugin-dir` loads skills as `plugin:name` in headless mode | Skills reach workers without writing anything into the user's repository. |
| `--mcp-config` takes several values | Placed last on a worker's argv, it swallows the prompt; extras go first. |
| `pi-herdr` drives terminal panes and scrapes them | Wrong substrate for a GUI, and bound to the `pi` agent. Not forked; its preset/spawn concept borrowed, engine written fresh. |

## Architecture

The engine is a **plain Rust library with no GUI dependency**. This is deliberate: the
Tauri app and the CLI are two front ends over identical code, so the whole orchestration
loop stays testable without a desktop.

| Crate / dir | Role |
|---|---|
| `crates/harness-core` | The engine: agents, isolation, MCP server, orchestrator, SQLite store |
| `crates/harness-cli` | Headless driver — `roles`, `run-worker`, `chat`, `mcp-serve`, `usage`, plus a hidden `hook` for Claude Code's worktree hooks |
| `app/` | Tauri v2 shell: project sidebar, head chat, worker rail, diff drawer, limits panel, Tools & Skills, Settings |
| `skills/` | The three built-in skills, adapted from David Ondrej's (credited in `skills/NOTICE.md`) |
| `roles.toml` | The fleet |

**Delegation is tool calls, not prompt parsing.** Claude roles with a worktree or
read-only isolation are native subagents, run through Claude Code's own `Agent` tool; the
harness answers its worktree hooks and maps its task events onto worker cards. Codex and
local roles go through `delegate`, served by a streamable-HTTP MCP server on an ephemeral
loopback port behind a per-session bearer token.

**Isolation is per-role.** `worktree` (own branch, parallel-safe), `readonly` (throwaway
worktree *and* edit tools denied), `shared` (project root, behind a lock admitting one
worker at a time), `none` (no filesystem).

**Landing code is a human action.** `request_merge` only queues a diff for review.
`approve_merge` exists solely on the host side and is **not** an MCP tool, so no model
output can merge anything.

## Where it stands

### Working and verified

- **161 engine tests** (172 including the separate Tauri shell workspace), no network or
  CLI login required.
- **Orchestrator** — live end-to-end against the real `claude` CLI: it called `list_roles`,
  then `delegate`, a worker wrote into its worktree, and it correctly reported the work as
  pending review rather than landed.
- **Native delegation** — live against the real CLI: the head agent used the `Agent` tool
  on a `builder` subagent, the `WorktreeCreate` hook built a bootstrapped worktree under
  `.harness/worktrees/`, the work was committed to `harness/agent-<id>`, the worktree was
  released, the checkout stayed untouched, and a merge was proposed for approval.
- **Skills via plugin** — a real `claude -p` worker started by the harness listed
  `harness:review, harness:risky-changes, harness:worktree` from the plugin directory.
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
- **Stop** — the head agent's interrupt verified against the real CLI; worker cancellation
  covered by tests (a stopped worker keeps what it wrote, on its branch).
- **Real Claude quota** — the parser is tested against a `rate_limit_event` captured from a
  live run.

### Not yet verified

- **Codex backend.** The CLI is not installed. Its event parsing is covered only by unit
  tests over recorded output shapes; its vocabulary has shifted between releases, so the
  first real run is where that gets confirmed.
- **A real multi-step task.** Everything so far has been one delegation deep.
- **Rate-limit shedding.** The code path exists and is unit-tested; it has never fired
  against an actual rate limit.
- **The vibrancy effect itself.** Verified only that the page is properly translucent — no
  `NSVisualEffectView` exists off macOS.
- **This round's UI on the Mac.** Stop, restored chat, live worker activity, markdown,
  notifications, shortcuts, the project sidebar, the limits panel, Tools & Skills and
  Settings were rendered in headless Chromium against a stubbed bridge, and their
  commands are tested — but not yet used in the real window. Notifications and the native
  folder picker in particular only exist there.
- **An MCP server added through Tools & Skills, used by a worker.** The config reaching
  Claude is tested; a real server has not been attached yet.
- **The embedded Preview webview on the target Mac.** URL validation, server discovery,
  compilation, and frontend integration are tested; the native child-webview behavior
  still needs its first target-platform GUI pass.

### Known gaps

- **No CI.** The repository has no workflows, so the 161 tests run only by hand. This
  matters more than usual here: both CLIs' JSON output is parsed leniently against
  fixtures rather than a stable contract, so upstream schema drift would go unnoticed
  until a live run misbehaved. Deferred by choice.
- **No auth-mode readout**, per the CLI finding above.
- **Native subagent definitions are fixed per session.** Reassigning a native role, or
  editing its tools, reaches it through `delegate` until the project is reopened; the head
  agent is told so.
- **Skill and MCP changes reach a running head agent only on reopen.** Workers get them on
  their next task.
- **Diff drawer shows the current diff, not a history** of earlier ones. The chat itself
  is restored on restart and on switching projects.

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
5. **Try this round's UI on the Mac** — Stop, Tools & Skills, Settings — and report what
   feels off.
6. **Push-to-talk voice assistant** (planned next): a menu-bar popover and global hotkey,
   on-device speech-to-text, fixed commands matched first, a *small* LM Studio model for
   the rest (not `qwen3-coder-30b`), and "ask the fleet" sent to the head agent.
7. **Add CI** when the schema-drift risk starts to bite.

## Risks worth tracking

- **Subscription policy is the load-bearing assumption.** The paused June 15 change and
  `--bare`'s slated promotion to the `-p` default both point one way. The agent layer sits
  behind an enum, so an API-key or OpenRouter backend is a config swap, not a rewrite.
- **Stream schema drift.** Both CLIs' JSON is parsed leniently and covered by fixture
  tests; unrecognized lines yield no events rather than failing a run.
- **Terms of service.** Solo developer on personal repositories is ordinary individual use.
  Worth reading Anthropic's compliance docs before pointing this at anything work-adjacent.
