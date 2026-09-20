# local-harness

A hierarchical multi-agent harness that runs on your **subscriptions** rather than API billing.

A head agent plans and delegates; a fleet of workers does the work. The head agent is a
long-lived `claude -p` process, workers are `claude`, `codex`, or local models, and every
one of them authenticates with the OAuth token its CLI already holds.

```
┌─ Tauri app ───────────────────────────────────────────────────┐
│  React UI          head chat │ worker rail │ diff drawer │ meter│
├───────────────────────────────────────────────────────────────┤
│  src-tauri         commands + event forwarding only            │
├───────────────────────────────────────────────────────────────┤
│  harness-core      the engine — no GUI dependency              │
│    orchestrator    one long-lived `claude -p` process          │
│    mcp             axum + rmcp, loopback, bearer-token gated   │
│    agents          claude │ codex │ openai_compat │ mock       │
│    isolation       worktree │ readonly │ shared │ none         │
│    store           SQLite: sessions, runs, events, usage       │
└───────────────────────────────────────────────────────────────┘
```

## Why this works

Anthropic blocked third-party tools from using Claude subscription credentials, so any
harness that speaks to the API directly needs its own billing. Driving the *official CLIs*
as subprocesses sidesteps that: the CLI reads its own OAuth credentials, and the work is
covered by the subscription.

Three details make it hold together:

**One process, many turns.** The orchestrator runs in streaming-input mode
(`--input-format stream-json`), so a single `claude` process serves the whole conversation.
A fresh `claude -p` call rebuilds its system prompt, tool definitions and `CLAUDE.md` every
time — tens of thousands of tokens before your prompt is even read. Measured against the
real CLI in one session:

| | cache creation | cache read |
|---|---|---|
| turn 1 | 39,306 | 77,922 |
| turn 2 | **906** | 118,666 |

The floor is paid once per session, not once per message. This is why the head chat is
cheap enough to leave running, and why `--resume` is the crash-recovery path here rather
than the normal one.

**`--bare` is never passed.** It skips OAuth and the keychain by design, so it requires an
API key — exactly what this project exists to avoid. API-key environment variables are also
stripped from every child process, because with `ANTHROPIC_API_KEY` set the CLI bills the
API instead of the subscription.

**Local models get a real agent loop for free.** Codex reserves `ollama` and `lmstudio` as
built-in provider ids, so a role with `provider_opts = { model_provider = "ollama" }` gets
tools and sandboxing with no code from us. The raw OpenAI-compatible backend is reserved
for work that needs no tools at all.

## Delegation

The orchestrator does not have its delegation parsed out of prose. The engine hosts an MCP
server on loopback, and Claude Code connects to it via `--mcp-config`, so `delegate(...)` is
a first-class tool call:

| Tool | Purpose |
|---|---|
| `list_roles` | The fleet: model, isolation, what each role is for |
| `delegate` | Run a worker, wait, return its result |
| `delegate_async` | Fan out; returns a worker id immediately |
| `check_workers` | Poll async workers |
| `collect` | Read a finished worker's result and diff |
| `request_merge` | **Propose** landing a branch — never lands it |

`approve_merge` is deliberately *not* an MCP tool. It exists only on the host side, driven
by a click in the app, so no amount of model output can land code on its own.

The MCP server binds to an ephemeral loopback port behind a per-session bearer token.
Anything that can reach that port can spend your subscription, so unauthenticated requests
are refused rather than logged.

## Roles and isolation

Isolation is a per-role property — builders, testers and reviewers need different things.

| Mode | Where it works | Use for |
|---|---|---|
| `worktree` | Own git worktree on its own branch | Builders. Parallel-safe, diff reviewable |
| `readonly` | Throwaway worktree **and** edit tools denied | Testers, reviewers, evaluators |
| `shared` | The project root, behind an advisory lock | Work that must happen in place |
| `none` | No filesystem | Summarize, classify, draft |

`readonly` is belt and braces on purpose: the tool denial states the intent, and the
throwaway worktree means a model that writes anyway touches nothing real. `shared` admits
**one worker at a time** — without that lock, two shared workers editing the same file
silently destroy each other's work.

See [`roles.toml`](roles.toml) for the default fleet.

## Getting started

Requirements: Rust, Node 18+, `git`. For the full fleet, `claude` and `codex` on `PATH` and
logged in (`claude /login`, `codex login`), plus Ollama or LM Studio for local roles.

```bash
# Headless — the whole loop, no desktop needed
cargo run -p harness-cli -- roles
cargo run -p harness-cli -- run-worker mock "WRITE:demo.txt:hello"
cargo run -p harness-cli -- chat "Plan the change, then delegate it."
cargo run -p harness-cli -- usage --hours 5

# Desktop app
cd app && npm install && npm run tauri dev
```

On Linux the desktop app additionally needs `libgtk-3-dev`, `libwebkit2gtk-4.1-dev`,
`libayatana-appindicator3-dev` and `librsvg2-dev`. macOS needs none of these — it uses
WKWebView.

## Testing

```bash
cargo test                        # engine: 66 tests, no network, no CLI login needed
cd app && npx tsc --noEmit        # frontend
```

The `mock` backend is a deterministic stand-in, so isolation, delegation, the merge gate
and rate-limit shedding are all tested without touching a subscription. `WRITE:<path>:<text>`
makes a mock worker write a file; `FAIL:<reason>` makes it fail.

## Notes and caveats

- **`total_cost_usd` is notional.** The CLI computes it regardless of auth mode. Under a
  subscription it is a *relative burn signal*, not a bill. Local inference reports nothing
  because it costs nothing.
- **There is no reliable auth-mode readout.** CLI 2.1.278 does not emit a `provider` field,
  so the app cannot display "this is running on your subscription". The parser treats the
  field as optional and shows `?` when absent. To confirm auth by hand, check that
  `ANTHROPIC_API_KEY` is unset and that the CLI is logged in — an expired OAuth session
  fails loudly.
- **Rate limits are the real constraint**, not dollars. The engine watches for
  `system/api_retry` events carrying `rate_limit` and sheds work to each role's
  `fallback_role` until a turn succeeds.
- **Subscription policy is the load-bearing assumption.** Anthropic paused a change that
  would have moved `claude -p` to a separate credit pool, and `--bare` is slated to become
  the `-p` default. The agent layer is behind an enum so an API-key or OpenRouter backend
  is a config swap, not a rewrite.
- **Stream schemas are not stable contracts.** Both CLIs' JSON output is parsed leniently:
  unrecognized lines yield no events rather than failing a run, and parsing is covered by
  fixture tests.
