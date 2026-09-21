---
name: harness-risky-changes
description: Verify assumptions before shipping a change where being wrong is expensive — APIs, data shaping, billing, quotas, defaults, thresholds. Use when a mistake would be customer-visible or hard to reverse, or when asked whether something is safe to ship.
---

# Risky changes

Adapted from David Ondrej's `risky-changes` skill (MIT) — see `skills/NOTICE.md`.

A filter can pass every test and still disable most of the feature on live data. Tests
check that the code does what it says; they do not check that what it says is right.

## When this applies

Changes where being wrong is expensive or hard to walk back:

- Public API fields, filters, or response shaping
- Dropping, transforming, or reordering upstream data
- Billing, pricing, caps, or quotas
- Defaults, thresholds, or request parameters sent to a provider
- Any assumption about external data or user behaviour that nobody has checked

If you cannot tell whether a change qualifies, treat it as though it does.

## 1. Name the assumptions

List what the change depends on being true. Mark each as evidenced or unverified. This
step is cheap and is usually where the problem surfaces.

## 2. Get evidence before writing the code

Look at the real thing: the actual data, the actual endpoint, the actual current
behaviour. Prefer reading production-shaped data over reasoning about what it probably
looks like.

If the evidence contradicts an assumption, change the design before implementing. If you
cannot get evidence, **say so plainly and name the gap** — an unverified assumption
reported as verified is worse than no work at all.

## 3. Measure, don't assert

Run enough realistic cases to see the distribution, not one happy path. Vary the inputs
that matter. Where you can, compare before against after with numbers rather than
impressions.

Write the cases and results down in your report. Unit tests do not substitute for this.

## 4. Surface the decision, don't bury it

Anything affecting what a user sees or pays is a decision for the person, not a default
you pick and mention in passing. Put it in your summary where they will read it, with
the evidence beside it.

## In this harness specifically

You cannot deploy and you cannot merge — a human reads your diff first. So the useful
output of this skill is not a safe deploy, it is **a diff that arrives with its
assumptions and evidence attached**, so the person approving it can see what you checked
and what you could not.
