---
name: worktree
description: How work is isolated and landed in this harness. Use when about to write files, run the project's tests, commit, merge, or push from inside a worker.
---

# You are working in a disposable worktree

Adapted from David Ondrej's `git-worktree` skill (MIT) — see `skills/NOTICE.md`.

The harness put you in your own git worktree on your own branch. It is yours alone: no
other worker shares this directory, so you can edit freely without coordinating.

## What the harness already did

- Created the worktree and its branch.
- Copied in whatever the project declared under `[worktree] copy` in `roles.toml`
  (`.env` and friends), and ran its `setup` commands, so dependencies are installed.
- Will commit your changes to your branch and remove the worktree when you finish.

## What you must not do

**Do not manage worktrees yourself.** No `git worktree add`, `remove`, or `prune`. The
harness serializes that bookkeeping; a worker running it concurrently corrupts the
administrative files for every other worker in flight.

**Do not merge, push, or switch branches.** Nothing you write reaches the user's
checkout until a human reads your diff and approves it. Merging or pushing yourself
bypasses the one gate this design exists to provide — and the head agent cannot approve
it either; only the person can.

**Do not edit outside this directory.** Paths above the worktree root belong to the real
project or to other workers.

## What to do instead

- Work normally: read, edit, write, run the tests.
- Commit early if you want checkpoints; commits on your branch survive teardown, and
  uncommitted work does not.
- If dependencies are missing or a setup step clearly failed, say so and stop rather
  than working around it — the project's `[worktree] setup` needs fixing, and a
  workaround here would hide that from everyone.
- When done, report what you changed and what you verified. Be specific about what you
  ran and what it printed; "tests pass" with nothing behind it is not a result.
