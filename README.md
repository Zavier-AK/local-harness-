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

### Claude roles delegate the way Claude already does

Opus is trained to hand work to subagents with Claude Code's own `Agent` tool, so the
harness does not make it go through ours for Claude roles. At session start every Claude
role with `worktree` or `readonly` isolation is passed to the head agent as a native
subagent (`--agents`): its brief becomes the subagent's prompt, and its model, tools,
turn limit and permission mode carry over. Codex and local-model roles, which the `Agent`
tool cannot run, stay on `delegate`. The brief tells the head agent which is which.

What stays the harness's job is everything around the subagent:

- **Worktrees.** Claude Code's `WorktreeCreate` / `WorktreeRemove` hooks call back into
  the harness binary, so a native subagent gets the same worktree as a delegated worker —
  under `.harness/worktrees/`, on a `harness/` branch, with the `[worktree]` copy and
  setup steps from `roles.toml` already run.
- **The rail.** `task_started` / `task_progress` / `task_notification` from the stream
  become the usual worker events, so native subagents show up as worker cards with a
  live current tool, their token use, and a diff.
- **The merge gate.** When a subagent finishes, its work is committed to its branch and
  a merge is proposed automatically. It still lands only on your click.

Two findings shaped this. The head agent must not have a session-level deny list —
subagents inherit it — so it is kept read-only by being approved for nothing but
`Read`, `Grep`, `Glob` and `Agent`, with permission prompts off. And in an early run a
head agent that had `Bash` copied a subagent's work straight into the checkout,
skipping review, because the work "wasn't there". The brief now says outright that
absence from the checkout is correct.

A role's subagent definition is fixed when the session starts. If you reassign a native
role mid-session, the head agent is told to reach it through `delegate` until the project
is reopened.

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

## Verification before merge

Every proposed merge is checked before it reaches you, in a fresh checkout of exactly
what would land. The idea comes from Kun Chen's
[No Mistakes](https://github.com/kunchenguid/no-mistakes), and from Boris Cherny's point
that giving an agent a way to check its work is what makes it good. The checks run
cheapest first:

1. **Change shape**, which is free and always runs. It flags:
   - size;
   - sensitive paths: migrations, auth, `.env`, CI, `roles.toml`;
   - dependency and build changes;
   - deleted files and binaries;
   - code changed without a test touched. A test added inline in the patch counts.
2. **Your commands** from `[verify] commands`, such as `cargo test`. A failure makes the
   change high risk, and its output is shown on the card.
3. **An independent review** by `[verify] reviewer`, a readonly role. It sees the diff
   and can read the checkout, and it must answer in a strict JSON format. A reply it
   can't parse is retried once, then reported as an error, never as a verdict. If the
   first reviewer says medium or high risk, `[verify] escalate_to` takes a second look,
   and its verdict wins. So a cheap local model can handle the easy majority, and Opus
   is only spent on changes that worried it.

The card leads with a **low / medium / high** badge and the reasons for it. **A change
nothing actually checked is marked *unverified*, never low risk.** The same summary
reaches the head agent through `check_workers` and `collect`. When a check actually
*fails*, the app sends the head agent a one-line note so it can delegate a fix while you
haven't looked yet. Checks run one at a time, since tests and local models are both
heavy. You can still merge before they finish; the button says "Merge anyway".

```toml
[verify]
commands    = ["cargo test"]
reviewer    = "tester"      # cheap first pass
escalate_to = "architect"   # only when the first pass is worried
```

## The plan board

For work with more than one step, the head agent calls `propose_plan` instead of
delegating each piece. The plan opens in the **Plan** tab as one card per step: its
title, its role, its task, and what it waits for. The idea comes from Kun Chen's
[Lavish](https://github.com/kunchenguid/lavish-axi): "visual plans, not walls of
markdown". The running view is borrowed from Vibe Kanban's lanes.

- **Before it runs**, you can:
  - edit any task, retitle a step, switch its role, or remove it (whatever depended on
    it is unhooked);
  - comment on individual steps, or on the plan as a whole;
  - **Send feedback**, which gives your comments and edits to the head agent to revise;
  - **Run plan**, which starts it.
- **Once running**, the harness drives the plan itself, with no head-agent turns:
  - steps with nothing to wait for start together, each in its own worktree;
  - a step that depends on another starts only after that one has **landed**, so it
    builds on real code on your branch, not on a branch you might still discard;
  - the same cards move across *Up next → Running → Checking → Your review → Landed*.
- **Every step is an ordinary worker.** Each is verified, goes through the merge gate,
  and obeys the autonomy dial. At **Land safe**, a plan can run start to finish without
  a click.
- **If a step is discarded or fails,** anything that depends on it is skipped.
- **Stop plan** keeps unstarted steps from starting. The head agent is told how the plan
  ended, step by step.

Headless, `harness-cli --run-plans` runs a proposed plan unedited.

## How much runs without you

A four-stop dial in the title bar, set per project. ⌘⇧A cycles through it. This is
Karpathy's "autonomy slider": you decide how much the fleet does alone, and the
**engine** enforces it; the model isn't just asked to behave.

| Stop | Delegation | Merging |
|---|---|---|
| **Ask** | each delegation waits for you: approve, edit the task, or decline with a reason | you merge |
| **Review** (default) | runs freely | you merge |
| **Land safe** | runs freely | verified *and* low-risk changes land by themselves |
| **Land most** | runs freely | verified changes land by themselves unless high risk |

- **Auto-landing needs something to trust.** It never lands a change that is unverified,
  has a failed or broken check, or doesn't merge cleanly; those wait for you. Every
  landing, automatic or yours, gets an **Undo** in the chat and the drawer. Undo adds a
  reverting commit and never rewrites history.
- **A failed merge no longer leaves a mess.** Your checkout is no longer left
  half-merged with conflict markers: the merge is aborted and the change stays proposed.
- **Ask covers every delegation path.** Under **Ask**, `delegate` returns immediately as
  awaiting approval. It can't wait for you, because Claude Code abandons an MCP tool call
  that goes silent for about five minutes. The head agent is told how each delegation
  ended once it has. Native subagents are covered too: a `PreToolUse` hook refuses the
  `Agent` tool for the project's own roles and points the head agent at `delegate`.
  Checked against the real CLI.
- **Where the level lives.** It's saved in `.harness/autonomy.json`, which is
  git-excluded because it's your setting, not the repository's. The hook reads it on
  every call, so moving the dial applies mid-session. `harness-cli --autonomy <level>`
  sets it for a headless run. Nobody is there to approve, so under `ask` the CLI declines
  delegations, and says why.

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

**After pulling, run `npm install` in `app/` again.** New features sometimes add frontend
packages, and Vite reports a missing one as `Failed to resolve import "…"` rather than
installing it. Cargo fetches new Rust crates on its own.

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
- **Claude reports real limits too — on its own stream.** Every turn of the head agent's
  `claude -p` process carries a `rate_limit_event` with server-reported utilization and
  reset times for the five-hour and weekly windows. The panel shows the latest one. Until
  the first turn of a session there is nothing to show, which reads as missing rather
  than as 0%. A `rejected` status also sheds Claude roles to their fallbacks immediately,
  instead of after a failed retry.

  An earlier version of this panel said Claude exposed no readable quota. That was wrong:
  `/usage` is interactive-only and the statusLine block never runs under `-p`, but the
  stream itself carries the same figures. It was found by capturing a real run rather
  than reading the docs.

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

**Agentic** — a real tool loop with sandboxing, via Codex, no code required. **Any role
that works in a worktree or read-only checkout needs this path**: `openai_compat` has no
tool loop, so a local model can only edit files with the Codex CLI driving it. Without
Codex installed (`npm i -g @openai/codex`, then `codex login`), such roles show as
unavailable in the rail, with the reason and a **Fix** link beside them.

```toml
[roles.local_builder]
provider      = "codex"
model         = "qwen/qwen3-coder-30b"     # as LM Studio lists it; Ollama tags look like qwen3.6:35b-a3b
isolation     = "worktree"
tools         = ["Read", "Edit", "Write", "Bash"]
provider_opts = { model_provider = "lmstudio" }
```

`lmstudio` and `ollama` are Codex built-ins pointing at `localhost:1234` and
`localhost:11434`. **A server on another machine needs its own provider id**, because Codex
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

The model has to exist on that server, too. A role naming a model the server doesn't
list shows as unavailable, with the missing model named, rather than failing on its first
task. Model ids differ between servers: LM Studio's look like `qwen/qwen3-coder-30b`,
Ollama's like `qwen3.6:35b-a3b`.

Confirm what the harness can actually reach:

```bash
cargo run -p harness-cli -- roles
```

`wire_api` is worth a try both ways — `"chat"` for `/v1/chat/completions`, `"responses"`
for `/v1/responses`. Which one a given server speaks varies, and the 2026 Codex docs
default to `responses` while most Ollama-compatible endpoints still want `chat`.

## Testing

```bash
cargo test                        # 205 engine tests, no network, no CLI login needed
cd app && npx tsc --noEmit        # frontend
```

The `mock` backend is a deterministic stand-in, so isolation, delegation, the merge gate
and rate-limit shedding are all tested without touching a subscription. `WRITE:<path>:<text>`
makes a mock worker write a file; `FAIL:<reason>` makes it fail.

The Tauri shell is a separate cargo workspace, so `cargo test` does not reach it. Its tests —
Preview discovery, URL safety, settings — run on their own:

```bash
cargo test --manifest-path app/src-tauri/Cargo.toml
```

That makes **205 engine tests, 216 including the Tauri shell** — worth stating explicitly,
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

## Tools & Skills, and Settings

Two views sit behind the icons at the foot of the project sidebar.

**Tools & Skills** holds what the fleet can use:

- **Skills.** A library shared by every project. Three are built in: `worktree`,
  `review` and `risky-changes`. You can add your own from a folder, from a git
  repository (every folder with a `SKILL.md` in it is imported, with its source kept
  for credit), or as a blank skill to write. Each one can be switched off.
- **Tools, by role.** Each role's allow-list for the open project. Rules can be
  scoped, like `Bash(git *)`, and `mcp__<server>` grants that server's tools. It is
  written back into `roles.toml` without disturbing its comments.
- **MCP servers.** A command or an HTTP URL. They are connected for the head agent and
  every Claude worker. The view shows whether each one actually connected, as reported
  when the head agent last started.

Skills reach Claude as a plugin passed with `--plugin-dir`, so they appear as
`harness:<name>`. **Nothing is written into your repository or `~/.claude`**. Earlier
versions copied skills into every worktree and kept them out of commits with a
pathspec; that is gone. The plugin directory is content-addressed, so changing a skill
builds a new one instead of rewriting a directory a running worker may be reading.
Workers pick up changes on their next task; head agents pick them up when a project is
next opened.

An MCP server's tools are available to a role only if that role lists them. The head
agent never lists them, so it stays a planner.

**Settings** (⌘,) holds:

- the head agent's default model and turn limit;
- whether to send notifications;
- the accent colour. It is one variable; hover, soft and text-on-accent colours are
  derived from it.

The CLI and the app share the extensions library in the app's data folder. Pass
`--extensions <dir>` to the CLI to use a different one.

## Credits

The skills in [`skills/`](skills/) are adapted from
[David Ondrej's agent skills](https://github.com/davidondrej/skills) (MIT). See
[`skills/NOTICE.md`](skills/NOTICE.md) for the licence and exactly what was changed.

That repository also earned its keep before any of it was vendored: the `git-worktree`
skill's "complete the setup" checklist — env files, dependencies, ports, generated
output — named a real gap here. `git worktree add` checks out tracked files only, so
builders were being told to run tests in a tree with no dependencies installed. That is
what the `[worktree]` block in `roles.toml` now fixes.
