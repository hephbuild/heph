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
- **review**: the subject is the diff, `master...HEAD` unless a range was given.

## 2. Who is consulted

Always `product-vision`, `feature-quality`, `code-quality` (design: `code-quality` only when the
shape of the code — traits, ownership, errors, concurrency — is being decided).

Then the triggered agents. For review, list the files and match them:

```bash
git diff --name-only master...HEAD
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

## 3. The brief

One brief, shared by every agent at this stage, pasted into each prompt. An agent told only
"review this" re-reads the codebase — one session spent 124M tokens on 36 such consults.

```
Stage: design | review
Subject: <spec URL or path> | diff master...HEAD (<N> files, +A/-B)
What it does: <two or three sentences>
Files that matter: <paths, with line ranges when the change is local>
Decided already: <decisions the spec settles — not to be reopened>
Prior verdicts: <from the spec's board section, for round 2+>
Your question: <per agent — the specific thing this agent should judge>
Answer with: verdict, then findings ranked by severity, each with file:line.
```

Give the diffstat, not the diff: the agent reads the lines it needs.

## 4. Run

All consults at one stage go in **one message**, in parallel. Model:

- design stage: `product-vision` and `feature-quality` with `model: "sonnet"`; the rest inherit.
- review stage: every agent inherits the session model — review depth is the point.

For a second round on the same change, `SendMessage` the agent from round one with the delta;
do not spawn a fresh one.

## 5. Record

Append to the spec's `## Board` section (create it if missing), one line per agent:

```
- code-quality (review, <short sha>): APPROVE — 2 nits fixed in <sha>
- hermeticity (review, <short sha>): NOT HERMETIC — env var X unhashed → fixed in <sha>
```

A BLOCKER, NOT HERMETIC or BREAKING is fixed or overruled with a stated reason before commit; a
RETHINK or DON'T BUILD goes to the user. Report the verdicts to the user as a table.
