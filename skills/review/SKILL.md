---
name: review
description: Review a change and report only what matters. Use when reviewing a diff, evaluating another worker's output, or combining several reviewers' findings.
---

# Reviewing without generating noise

The idea of counting what you discarded comes from David Ondrej's `total-review` skill
(MIT) — see `skills/NOTICE.md`.

A review that lists everything is as useless as one that lists nothing: the person has to
re-do the triage you were asked to do.

## Report

A numbered list, most serious first. For each finding:

- **What breaks**, concretely. Inputs or state, then the wrong result. "Could fail under
  concurrency" is not a finding; "two workers calling `prepare` at once corrupt
  `.git/worktrees/<id>/commondir`" is.
- **Where**, as `file:line`.
- **Why you believe it**, if it is not obvious from reading the code.

Then one line: **how many findings you discarded, and roughly why.** That number is the
point. It tells the reader you triaged rather than dumped, and a high count is a signal
worth seeing.

## Discard

- Style preferences the project has not adopted.
- Theoretical edge cases with no path to reaching them.
- Anything you cannot state as a concrete failure.
- Duplicates. If two reviewers found the same thing, it is one finding — tag it as agreed
  rather than listing it twice.

Agreement between reviewers is not evidence. Two models sharing a wrong assumption agree
just as readily as two models being right. Check the claim yourself before repeating it.

## Do not fix

Report findings; leave the change alone. The person decides what gets acted on — and in
this harness, a reviewer role is read-only anyway, so a fix you attempt will not land and
will only cost a turn.

If you found nothing worth reporting, say that in one line. It is a valid result and a
useful one.
