---
name: pr-build-published
description: >
  Watch the current PR's "CI" GitHub Actions run and report the outcome of the
  `upload_artifacts` job (display name "Pre-release"), which publishes the build
  artifacts as a pre-release. Resolves the PR for the current branch, locates the
  CI workflow run for its head commit, and polls that job until it reaches a
  terminal state. Reports a positive outcome when the job completes successfully,
  or a failure with the reason when it is skipped or fails.
  Trigger when the user says "pr build published", "was the build published",
  "did upload_artifacts pass", "check the pre-release job", or invokes
  /pr-build-published.
---

# PR Build Published

Goal: report whether the current PR's `upload_artifacts` job (which uploads/publishes
the build artifacts as a pre-release) succeeded, was skipped, or failed.

How it works: the `upload_artifacts` job in `.github/workflows/heph.yml` carries the
display name **"Pre-release"** — that display name is what the GitHub Actions jobs API
returns, so the script matches on it. The job has `if: always() && needs.gen.result == 'success'`,
so it is **skipped** when the upstream `gen` codegen job does not succeed, and **fails**
when a step (artifact download, manifest generation, or the release creation) errors. The
script polls the job until it reaches a terminal state.

## Run

Execute this script. It resolves the PR, finds the `CI` run, and polls the `upload_artifacts`
("Pre-release") job until it completes (default cap 45 min; override with `TIMEOUT_SECONDS`).

```bash
bash .claude/skills/pr-build-published/watch-upload-artifacts.sh
```

## After running

- **Exit 0** (`ok`) → report a **positive outcome**: the `upload_artifacts` job completed and
  the build was published. Share the run URL and PR URL, all clickable.
- Non-zero → report a **failure with the reason** the script printed to stderr — do not retry
  blindly:
  - Exit 2 (`no PR`) → there is no open PR for the current branch; tell the user to open one.
  - Exit 3 (`no CI run`) → the `CI` workflow has not started for the head commit yet.
  - Exit 4 (`skipped`) → the job was skipped because an upstream dependency (the `gen` codegen
    job) did not succeed; relay that reason and the run URL.
  - Exit 5 (`failed`) → the job ran but concluded `failure`/`cancelled`/`timed_out`; relay the
    failing step name(s) the script reported and the run URL.
  - Exit 6 (`timeout`) → the job did not reach a terminal state within the deadline; share the
    run URL so the user can inspect it (or re-run with a larger `TIMEOUT_SECONDS`).
