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
cargo run -p harness-cli -- roles          # fleet, and which backends are actually reachable
cargo run -p harness-cli -- run-worker mock "WRITE:demo.txt:hello" --patch
cargo run -p harness-cli -- chat "Plan the change, then delegate it."
cargo run -p harness-cli -- usage --hours 5

# Desktop app
cd app && npm install && npm run tauri dev
```

On Linux the desktop app additionally needs `libgtk-3-dev`, `libwebkit2gtk-4.1-dev`,
`libayatana-appindicator3-dev` and `librsvg2-dev`. macOS needs none of these — it uses
WKWebView.

## What the limits panel can and cannot tell you

Clicking the budget meter opens it. Two different kinds of number live there, kept
visibly apart because conflating them would be worse than showing nothing:

- **Codex reports real limits.** It writes a server-reported used-percentage and reset
  time for both its five-hour and weekly windows into its session rollout files, and the
  panel reads them. They are a snapshot from Codex's last turn rather than live, so each
  is shown with when it was observed and goes **stale** rather than quietly ageing.
- **Claude reports none.** There is no `claude usage` subcommand, `/usage` is
  interactive-only, and `--output-format stream-json` carries no limit or reset field.
  The documented `rate_limits` block reaches statusLine scripts only, and statusLine does
  not run under `-p`, which is how this harness drives the CLI. So the panel shows no
  Claude percentage. It shows what the harness itself has spent, labelled as spend.

The one piece of real Claude limit information available is the `api_retry` event, which
says you have *already* hit a wall. That is surfaced plainly, along with which providers
are currently shedding to their fallback roles.

Quota states are `available`, `stale` and `missing`, borrowed from Codex's own `/status`.
A meter that says unknown is more useful than one that says 0%.

## Working on several projects

The sidebar keeps more than one project open at once. Each gets its own engine, its own
worktrees, its own `.harness/sessions.db`, and its own MCP server on its own port — the
engine was already per-instance, so projects genuinely do not share state.

What they do share is your subscription, so the design protects it:

- **Only the project in front keeps a head agent running.** Switching away shuts the
  other one down, which is the expensive part — a live `claude -p` process holding the
  context floor. Switching back resumes the same conversation rather than starting over,
  so the floor is not paid twice.
- **A project will not suspend while its workers are running**, since their results are
  reported through that session. It suspends once they settle.
- **The budget meter sums every open project.** Each project's usage lives in its own
  database, but the five-hour window being measured belongs to the account, so reporting
  one project's burn would understate it by however many others are open.

The sidebar shows running workers and waiting diffs for *every* project, not just the
active one. That is deliberate: the most common way people lose work with tools like this
is forgetting something is still running somewhere they navigated away from.

## Attaching a local model

Two ways in, reaching the same server but differing in what the worker can do.

**Tool-free** — plain chat completions, for summarize/classify/draft work. This is the
shipped `local` role, pointed at LM Studio:

```toml
[roles.local]
provider  = "openai_compat"
base_url  = "http://localhost:1234/v1"    # LM Studio; Ollama serves :11434
model     = "qwen3.6-35b-a3b"
isolation = "none"
```

LM Studio does not start its server automatically — open the **Developer** tab and start
it, or nothing is listening. `model` must match what the server reports:

```bash
curl -s http://localhost:1234/v1/models | python3 -m json.tool
```

**Agentic** — a real tool loop with sandboxing, via Codex, no code required:

```toml
[roles.local_builder]
provider      = "codex"
model         = "qwen3.6:35b-a3b"
isolation     = "worktree"
tools         = ["Read", "Edit", "Write", "Bash"]
provider_opts = { model_provider = "lmstudio" }
```

`lmstudio` and `ollama` are Codex built-ins pointing at `localhost:11434` and
`localhost:1234`. **A server on another machine needs its own provider id**, because Codex
reserves those two names and refuses to override them. In `~/.codex/config.toml`:

```toml
[model_providers.bionic]
name = "bionic"
base_url = "http://bionic.local:11434/v1"
wire_api = "chat"
```

Then `provider_opts = { model_provider = "bionic" }`, plus `base_url` on the role so the
harness health-checks the right host rather than assuming localhost. `base_url` on a
`codex` role is used only for that check; it is never passed to the CLI.

Confirm what the harness can actually reach:

```bash
cargo run -p harness-cli -- roles
```

`wire_api` is worth a try both ways — `"chat"` for `/v1/chat/completions`, `"responses"`
for `/v1/responses`. Which one a given server speaks varies, and the 2026 Codex docs
default to `responses` while most Ollama-compatible endpoints still want `chat`.

## Testing

```bash
cargo test                        # 115 engine tests, no network, no CLI login needed
cd app && npx tsc --noEmit        # frontend
```

The `mock` backend is a deterministic stand-in, so isolation, delegation, the merge gate
and rate-limit shedding are all tested without touching a subscription. `WRITE:<path>:<text>`
makes a mock worker write a file; `FAIL:<reason>` makes it fail.

The Tauri shell is a separate cargo workspace, so `cargo test` does not reach it. Its five
Preview discovery and URL-safety tests run on their own:

```bash
cargo test --manifest-path app/src-tauri/Cargo.toml
```

That makes **115 engine tests, 123 including the Tauri shell** — worth stating explicitly,
because the two numbers measure different things and have drifted apart before.

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

## Worker skills

Every worktree-isolated worker gets a small set of skills materialized into
`.claude/skills/harness-*`, where its CLI picks them up. They tell a worker what it can
and cannot do here: that its worktree is disposable and harness-managed, that nothing it
writes lands without a human approving the diff, how to report a review without
generating noise, and when a change needs evidence rather than a passing test suite.

They are **held out of the worker's commits** by pathspec, so they are visible to the
agent and invisible to your diff. A project's own `.claude/skills/` is untouched by this
and remains the worker's to change.

## Credits

The skills in [`skills/`](skills/) are adapted from
[David Ondrej's agent skills](https://github.com/davidondrej/skills) (MIT). See
[`skills/NOTICE.md`](skills/NOTICE.md) for the licence and exactly what was changed.

That repository also earned its keep before any of it was vendored: the `git-worktree`
skill's "complete the setup" checklist — env files, dependencies, ports, generated
output — named a real gap here. `git worktree add` checks out tracked files only, so
builders were being told to run tests in a tree with no dependencies installed. That is
what the `[worktree]` block in `roles.toml` now fixes.
