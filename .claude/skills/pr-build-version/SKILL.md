---
name: pr-build-version
description: >
  Find the "Build Version" annotation on the current PR's "CI" GitHub Actions run and print
  the version plus a link to the run. Resolves the PR for the current branch, locates the CI
  workflow run for its head commit, and reads the GHA annotations. The annotation is published
  late in CI, so this polls for up to 5 minutes until it appears.
  Trigger when user says "build version", "pr build version", "get the CI build version",
  or invokes /pr-build-version.
---

# PR Build Version

Goal: read the `Build Version` annotation from the current PR's `CI` Actions run, print version + run link.

How it works: GitHub annotations live on **check-runs**. An Actions workflow run's jobs *are* check-runs
(job id == check-run id), so the annotations endpoint is
`repos/{repo}/check-runs/{job_id}/annotations`. The `Build Version` annotation's `title` is
`Build Version`; its `message` carries the version string. The annotation appears only after the build
step runs, so poll until present (cap 5 min).

## Run

Execute this script. It resolves the PR, finds the `CI` run, polls annotations for 5 minutes, and
prints the version + run URL + PR URL (or a clear failure).

```bash
bash .claude/skills/pr-build-version/find-build-version.sh
```

## After running

- On success: report the version, the run URL, and the PR URL to the user, all clickable.
- If the script exits non-zero, relay the exact error it printed — do not retry blindly:
  - `no PR` → there is no open PR for the current branch; tell the user to open one.
  - `no CI run` → the `CI` workflow has not started for the head commit yet.
  - `timeout` → CI ran but the `Build Version` annotation never appeared within 5 min; share the run
    URL so the user can inspect it.
