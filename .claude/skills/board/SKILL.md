---
name: board
description: >
  Consult the Review Board (.claude/agents/) on a design or a diff: work out which agents the
  change triggers from the files it touches, write one brief, run the consults in parallel, and
  record the verdicts in the spec so a later round continues from them instead of starting over.
  Trigger when the user says "consult the board", "board review", "run the board", or invokes
  /board [design|review] [<spec-url or diff range>].
---

# Board consult

The rules this applies are in CLAUDE.md, "Review Board". This skill makes them mechanical: which
agents, what they are told, and where their answers go.

## 1. Stage and subject

- **design**: the subject is the spec (artifact link or file). No code yet.
- **review**: the subject is a pinned range, `<base-sha>..<head-sha>`, never a branch name:
  - `head` is the commit under review (`git rev-parse HEAD`).
  - `base` is the lower layer's commit for a stacked PR; otherwise `git fetch origin master` and
    `git merge-base origin/master <head>`.
  - Never local `master`: in a worktree it is checked out elsewhere and cannot be updated, so it
    can be any number of commits stale (83, once — every reviewer first read a 21k-character
    diffstat of unrelated commits).

Design-stage checks, each a finding if missing (CLAUDE.md, "Phases and hand-offs"):

- every case the spec enumerates has a named test;
- every heuristic has its adversarial inputs listed;
- every **accepted exemption** gets an explicit verdict from each agent — not a nod in passing.
  An exemption that rode through design as a side note became a review BLOCKER.
- every lifetime and every new cross-component contract appears under **Invariants and trust**,
  with who checks it and the alternatives. `product-vision` judges whether the spec's choice is the
  one a user would make. A spec that checked provider labels on every resolved spec got no design
  board, and the user reversed it after the PR was open.

## 2. Who is consulted

Always `product-vision`, `feature-quality`, `code-quality` (design: `code-quality` only when the
shape of the code — traits, ownership, errors, concurrency — is being decided).

Then the triggered agents. For review, list the files and match them:

```bash
git diff --name-only <base-sha>..<head-sha>
```

| Files | Agent |
|---|---|
| `proto/**`, `gen/proto/**` | `compatibility` |
| `crates/plugin-abi/**`, `crates/plugin-stabby/**`, `crates/plugin-sdk/**` | `compatibility` |
| `crates/engine/src/engine/{local_cache*,remote_cache*,meta,scratch*}.rs` | `compatibility`, `hermeticity` |
| `crates/builtins/**`, Starlark rule or builtin signatures in `crates/plugin-buildfile/**` | `compatibility` |
| `src/commands/**` (a command, flag, exit code, or `--json` shape) | `compatibility` |
| `crates/plugin-*/**` driver or provider code, `crates/driver-*/**`, `crates/engine/src/engine/{spec,execute,deferred,credential*}.rs`, `crates/execrunner/**`, `crates/sandboxfuse/**` | `hermeticity` |
| `crates/engine/src/engine/{result,query,request_state,spec,meta,local_cache*}.rs`, `crates/walk/**`, `crates/core/**` hashing | `perf-measurement` (after implementation) |

For design, match the files the spec says it will touch. The table is the floor: add an agent the
change obviously concerns, never drop one whose row matched. Say which rows fired and why.

`perf-measurement` is the one exception to "never drop". Brief it with the scenario to time and the
command that times it: a `cargo test --release` instrument, a `heph` invocation on a corpus, or
`/perf-test`. If no such scenario exists, record `perf-measurement: NOT MEASURED — <why>` in Board
and do not consult it. On 2026-10-04 one consult came back with a verdict "from reading the code",
which is `feature-quality`'s job and costs a consult to prove nothing.

## 3. The brief

One brief, shared by every agent at this stage, pasted into each prompt. An agent told only
"review this" re-reads the codebase — one session spent 124M tokens on 36 such consults.

```
Stage: design | review
Subject: <spec URL or path> | Base: <sha> Head: <sha> (<N> files, +A/-B)
Read code: `git show <head>:<path>`, `git grep <pattern> <head>`, `git diff <base>..<head> -- <path>`.
  The working tree is not the subject: another session may be editing it or switching branches.
  If you must build, use a detached worktree at <head> and run `gen` there first.
Scratch: <scratchpad>/review-<agent>-<short head>/ — yours alone.
Already verified on <head>: <e.g. `lint` exit 0; `cargo test -p engine` 687 passed> — do not re-run.
What it does: <two or three sentences>
Files that matter: <paths, with line ranges when the change is local>
Decided already: <decisions the spec settles — not to be reopened>
Prior verdicts: <from the spec's board section, for round 2+>
Your lane: <per agent — what it owns, and what belongs to another agent at this stage>
Your question: <per agent — the specific thing this agent should judge>
Answer with: verdict, then findings ranked by severity, each with file:line. At most ~600 words.
```

Give the diffstat, not the diff: the agent reads the lines it needs.

**Pin, isolate, fence.** A reviewer reading the live tree while the session edits or switches
branches reviews a half-written file, or the wrong layer: 89 reads of a tree that changed under
them in one session, and one reviewer's greps returned the layer below. Reviewers that share a
scratch path overwrite each other's diffs. Lanes keep two agents from auditing the same thing:
test coverage is `feature-quality`'s, so `code-quality` judges correctness and idiom and leaves
coverage gaps to it; at review, four findings in one round came from both.

## 4. Run

All consults at one stage go in **one message**, in parallel, each with `isolation: "worktree"`
at review so a reviewer that builds does not touch the session's tree. Model:

- `product-vision` and `feature-quality`: `model: "sonnet"` at both stages. At review,
  `feature-quality` on Sonnet cost ~1.2M tokens a PR and still found the one BLOCKER.
- `code-quality`, `hermeticity`, `compatibility`: inherit the session model — depth is the point.

Fix while the board runs only what no pending reviewer covers; hold the rest for the last report
(CLAUDE.md, "Workflow").

For a second round on the same change, `SendMessage` the agent from round one with the delta
and the new head SHA; do not spawn a fresh one.

## 5. Record

Append to the spec's `## Board` section (create it if missing), one line per agent:

```
- code-quality (review, <short sha>): APPROVE — 2 nits fixed in <sha>
- hermeticity (review, <short sha>): NOT HERMETIC — env var X unhashed → fixed in <sha>
```

A BLOCKER, NOT HERMETIC or BREAKING is fixed or overruled with a stated reason before commit; a
RETHINK or DON'T BUILD goes to the user. Report the verdicts to the user as a table.
