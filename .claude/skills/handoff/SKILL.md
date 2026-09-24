---
name: handoff
description: >
  End the current phase (design, implementation, or PR follow-up): bring the spec or PR description
  up to date with what was decided, what shipped, and what is left, then print the one-line prompt
  that starts the next phase in a fresh session. Trigger when the user says "hand off", "wrap up
  this phase", "end the session", or invokes /handoff.
---

# Hand-off

The next session starts from the spec or the PR and nothing else. Anything that exists only in
this conversation is lost. Write it down now, in the place the next session will read.

## 1. Which phase is ending

- **Design** ends with a published spec. The hand-off target is the artifact.
- **Implementation** ends with an open PR. The hand-off target is the PR description.
- **Follow-up** ends with a PR that is green, approved or merged. The hand-off target is the PR
  description, plus the spec if the design moved.

## 2. Make the target complete

A **design spec** has every section listed in CLAUDE.md under "Phases and hand-offs": Goal, Non-goals, Decisions, Tests, Files,
Board, Open. Fill in or correct the ones this session touched. In particular:

- **Tests** names each test and its layer (unit in the crate, `crates/e2e`, or `crates/bin-e2e`),
  and says what it proves. These tests are the implementation's definition of done.
- **Decisions** lists the settled decisions, each with the reason it was made, so the next session
  does not reopen them.
- **Open** lists only real questions, each marked for the user or for implementation.

A **PR description** says:

- what changed;
- how it was tested, including exit codes and which tests ran;
- what the board said;
- what is deliberately left out of the PR and why;
- which spec it implements.

When a spec test was dropped or changed, the description says so.

Publish or `gh pr edit` once, after all the edits are made.

## 3. Save what is worth keeping

A fact about the repo that cost time to find goes in the repo, not in memory. "X looks like a
flake but is Y" belongs in the ci-triage skill. A tooling trap belongs in CLAUDE.md or the doc
next to the code. Memory is for the user's preferences.

## 4. Print the next step

End with exactly one line the user can paste into a new session:

```
/goal implement <spec-url>
/goal get PR #<n> green and through review
```

Do not keep working past the phase boundary in this session.
