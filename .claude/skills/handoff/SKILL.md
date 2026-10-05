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

A **design spec** has every section listed in CLAUDE.md under "Phases and hand-offs": Goal, Non-goals,
Invariants and trust, Decisions, Tests, Files, Accepted exemptions, Board, Open. Fill in or correct
the ones this session touched.

**A design hand-off stops here, unprinted, if either of these fails:**

- **Board**: list the agents the spec triggers (the always-consult three plus every row of the
  `/board` table that its Files match). Each must have a `(design, …)` verdict line in Board. If
  one is missing, run `/board design` first, or tell the user which agents are missing and ask
  whether to skip them. A list of agent names is not a verdict.
- **Invariants and trust**: each one must be marked as confirmed by the user. If one is not, ask
  now. A decision the spec made by itself is how a PR got half rebuilt after it was opened.

In particular:

- **Tests** names each test and its layer (unit in the crate, `crates/e2e`, or `crates/bin-e2e`),
  and says what it proves. These tests are the implementation's definition of done. Every case
  the spec enumerates has a row, and every heuristic has its adversarial inputs listed.
- **Accepted exemptions** lists each place the change does not hold its own rule, with each
  design agent's verdict on it.
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

**Export a Docs spec to Markdown.** When the spec is a Claude Docs artifact, export it
(`export`, `format: "markdown"`) to `~/.cache/heph-specs/<slug>.md` — not the session scratchpad,
which the next session cannot see, and not the repo — and put that path in the next step's prompt
next to the URL. The implementation session reads it
by heading with `rg -n '^#'` and `sed -n`, and goes back to Docs only to append its Board rounds.
Reading the doc itself costs a 34k-character guide plus a 57 KB XML node, and hand-converting
that XML to text dropped a code block the spec depended on.

## 3. Save what is worth keeping

A fact about the repo that cost time to find goes in the repo, not in memory. "X looks like a
flake but is Y" belongs in the ci-triage skill. A tooling trap belongs in CLAUDE.md or the doc
next to the code. Memory is for the user's preferences.

## 4. Print the next step

End with exactly one line the user can paste into a new session:

```
/goal implement <spec-url> (spec text: <exported .md path>)
/goal get PR #<n> green and through review
```

A spec with several PRs gets one line per PR, so each is built in its own context — stacked or
not. Building every layer of a stack in one context is what grows it past 400k (CLAUDE.md, "One
implementation context per stack layer").

Do not keep working past the phase boundary in this session.
