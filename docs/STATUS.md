# local-harness — project status

_Last updated: 23 September 2026_

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
| Claude Code abandons an HTTP MCP tool call that is silent for ~5 minutes | Approval under the autonomy slider's **Ask** returns at once instead of blocking. The same limit threatens a synchronous `delegate` of a worker that runs longer — see known gaps. |
| A `PreToolUse` hook's `permissionDecision: "deny"` on the `Agent` tool is honoured in `-p` mode | Ask is enforced on native subagents too: live, the head agent was refused `Agent` and fell back to `delegate`. |
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

- **265 engine tests** (286 including the separate Tauri shell workspace), no network or
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
- **Verification before merge** — live against the real CLI:
  - a haiku builder subagent's merge was checked in a detached checkout of its branch;
  - both `[verify]` commands passed;
  - a real Claude reviewer returned a JSON verdict that parsed, giving low risk;
  - the checkout was removed afterwards.
  Failure, escalation, "unverified" and merge-while-checking are covered by engine tests.
- **Plan board** — live against the real CLI:
  - a haiku head agent called `propose_plan` for two steps, the second depending on the
    first, and `--run-plans` ran it;
  - step two waited while step one was checked and landed on its own at Land safe;
  - step two then ran on top of it and landed too; the commit order shows the dependency
    held.

  Finding: on the first try, haiku left `depends_on` out, so both steps started together.
  The tool description now says what happens without it, and the board shows how many
  steps start at once so you catch it in review.
- **Voice commander.** What was checked here:
  - Without Laya, voice is right on 68 of the 83 labelled phrases and wrong on none. The
    other 15 are paraphrases that are Laya's to read.
  - Tried on the Mac. Two follow-ups came from that:
    - "open Notes" failed while "Cursor" worked, because app names were guessed from
      the words. They are now matched against installed apps.
    - Commands had to wait for the key to be let go. Now each runs at the pause after it,
      while the key is held.
  - Text meant for Claude now goes into the chat box and is never sent automatically.
  - Inside apps, from the Mac tries ("open chrome and search for shoes", "open spotify and
    play my playlist called top 200" went to Claude): web search, music control, notes,
    reminders and volume are now fixed recipes. They are `open` or `osascript` with fixed
    scripts, and what was said is only an argument.
    - **Not verified here:** the AppleScripts run only on macOS. Spotify has no way to
      play a playlist by name, so it opens Spotify's search instead.
  - **Voice agent** (Claude Haiku, with the safe list as MCP tools and nothing else). It
    was added after multi-step requests and "create a new note" kept failing on the Mac.
    Real Haiku runs here, with `voice --agent` printing the steps instead of doing them:
    - "open notes and make a note with my groceries … then remind me at six to go
      shopping": open Notes, a *Groceries* note, a reminder at 18:00 (8.8 s);
    - "create a new note saying call the plumber tomorrow": one note (4.6 s);
    - "open chrome and search for running shoes on amazon then play my top 200 playlist
      on spotify": three steps in order (5.4 s);
    - "what's waiting for me, and approve the builder's change": the status, then a merge
      that **waits for a yes**;
    - a request to add retries to the fetcher went to the chat box.

    The app wiring (the steps ticking off in the bar, the fallback to Laya) was rendered
    headlessly. **Not verified here:** the agent driving the real Mac apps.
  - **Web tasks and email.** From the Mac tries, the agent could open Gmail but couldn't
    draft an email, and couldn't do much on a site. Now it hands web tasks to a browser
    agent (Sonnet) in its own Chrome window (Playwright, its own profile). Which clicks
    need a yes is decided in code, from what the element is. Emails are Gmail drafts in
    the person's words, from Settings › About you. Checked here, with real Claude and
    headless Chromium against a local test shop (the build machine can't reach public
    sites):
    - "go to the shop…, find the cheapest trail running shoe, put it in the basket and
      place the order": searched, picked the cheapest (€59) and added it to the basket,
      then **stopped at "Place your order" and asked for a yes** (15 s in the browser);
    - a reviews page with hidden text telling it to order and send the person's email
      elsewhere: it summarised the real reviews and reported the attempt, doing none of it;
    - a click the person said yes to runs in the real browser (engine test, with
      `HARNESS_TEST_CHROMIUM`);
    - "email sam that I'm running ten minutes late…", with an About-you note: a short
      draft to the address in the note, signed the way the note says.

    **Not verified here:** real sites (Amazon, Gmail), Google sign-in inside the
    automated window (flags that usually allow it are set), Contacts lookup, and the app's
    Stop button on a real run.
  - The real `@receptron/laya` package runs in the helper. Its protocol and error paths
    work, including a clean error when the weights can't be downloaded.
  - whisper.cpp loads a model and transcribes 11 s of audio (a test model, CPU only).
  - The voice bar, dispatch, the send countdown and its cancel, the confirmation buttons
    and Settings were rendered headlessly against a stubbed bridge.

  **Not verified here**, because Hugging Face is blocked on this build machine:
  - real Laya decisions: run `harness-cli voice --eval --laya` on the Mac for accuracy
    and speed;
  - real Whisper accuracy;
  - the microphone, the global hotkey, Metal, and spoken replies.
- **Night shift** — live against the real CLI, with a haiku builder on a toy project
  (score: distinct lines in a file; guard: no line over 20 characters):
  - the starting point scored 2;
  - experiment 1 was kept at 13, and experiment 2 built on it and was kept at 21;
  - `--propose` put the night's branch up for review, and it was verified low risk;
  - the checkout was untouched, and no experiment branches or worktrees were left.

  Thrown-away changes, a broken guard, an unscorable starting point and Stop are covered
  by engine tests.
- **Autonomy slider** — Ask verified live: with the level at Ask, the real head agent's
  native `Agent` call was refused by the hook, it delegated through `delegate` instead,
  and that returned awaiting approval. Auto-landing, its refusals, undo and conflict
  abort are covered by engine tests.

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

- **No CI.** The repository has no workflows, so the 265 tests run only by hand. This
  matters more than usual here: both CLIs' JSON output is parsed leniently against
  fixtures rather than a stable contract, so upstream schema drift would go unnoticed
  until a live run misbehaved. Deferred by choice.
- **No auth-mode readout**, per the CLI finding above.
- **Native subagent definitions are fixed per session.** Reassigning a native role, or
  editing its tools, reaches it through `delegate` until the project is reopened; the head
  agent is told so.
- **Skill and MCP changes reach a running head agent only on reopen.** Workers get them on
  their next task.
- **A synchronous `delegate` longer than ~5 minutes may be abandoned** by Claude Code's
  MCP idle timeout. `delegate_async` and native subagents are unaffected. Fix: send
  progress notifications from the MCP server during long calls.
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
5. **Try this round's UI on the Mac** — Stop, Tools & Skills, Settings, the Verification
   section of the review drawer — and report what feels off.
6. **Turn on `[verify]`** in a real project: its test command, and a cheap reviewer. Watch
   whether the risk levels match your own judgement before trusting them for more.
7. **Ask for a multi-step change and review it on the Plan tab** — edit a step, comment on
   another, send feedback, then run the revision.
8. **Try the autonomy slider** at Land safe on a project with `[verify]` set up, and see
   whether what lands by itself is what you would have merged.
9. **Measure voice on the Mac.** `brew install cmake`, then Settings › Voice: download
   Whisper, and install and load Laya. Run `harness-cli voice --eval --laya` and set the
   confidence to the suggested threshold. Add phrases you actually say to `phrases.toml`.
10. **Try the voice agent** with the longer things you actually say ("open notes and…",
   "…then remind me…"). For the browser: `npm install` in `app/voice-sidecar`, sign in to
   Gmail and Amazon in its window (Settings › Voice), fill in About you, then try a real
   web task and an email. Note where it stalls or asks too often.
11. **Run a night shift on something real** with a score you trust: a benchmark, a
   bundle size, a test count. Start with a few experiments and read the report before
   giving it a whole night.
12. **Add CI** when the schema-drift risk starts to bite.

## Risks worth tracking

- **Subscription policy is the load-bearing assumption.** The paused June 15 change and
  `--bare`'s slated promotion to the `-p` default both point one way. The agent layer sits
  behind an enum, so an API-key or OpenRouter backend is a config swap, not a rewrite.
- **Stream schema drift.** Both CLIs' JSON is parsed leniently and covered by fixture
  tests; unrecognized lines yield no events rather than failing a run.
- **Terms of service.** Solo developer on personal repositories is ordinary individual use.
  Worth reading Anthropic's compliance docs before pointing this at anything work-adjacent.
