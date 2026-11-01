---
name: pr-build-published
description: >
  Watch the current PR's "CI" GitHub Actions run and report the outcome of the
  `manifests` job (display name "Plugin manifests"), the last step of publishing
  the build artifacts as a pre-release. Resolves the PR for the current branch,
  locates the CI workflow run for its head commit, and polls that job until it
  reaches a terminal state. Reports a positive outcome when the job completes
  successfully, or a failure with the reason when it is skipped or fails.
  Trigger when the user says "pr build published", "was the build published",
  "did the pre-release publish", "check the pre-release job", or invokes
  /pr-build-published.
---

# PR Build Published

Goal: report whether the current PR's build artifacts were published as a
complete pre-release, or why not.

How it works: publishing is spread over three jobs in `.github/workflows/heph.yml`.
`prerelease_create` ("Create pre-release") creates the empty pre-release, each
`build` leg ("Build <os>/<arch>") uploads its own files as soon as it is built,
and `manifests` (display name **"Plugin manifests"**) uploads the plugin
manifests and `heph-govet` once every build has succeeded, then marks the
release body "complete". The script watches that last job, matching on its
display name (what the GitHub Actions jobs API returns). It is **skipped** when
any build leg failed — the pre-release then stays partial — and **fails** when
a step (artifact download, manifest generation, the upload) errors.

## Run

Execute this script. It resolves the PR, finds the `CI` run, and polls the
"Plugin manifests" job until it completes (default cap 45 min; override with
`TIMEOUT_SECONDS`).

```bash
bash .claude/skills/pr-build-published/watch-upload-artifacts.sh
```

## After running

- **Exit 0** (`ok`) → report a **positive outcome**: every platform's artifacts and
  the manifests were published. Share the run URL and PR URL, all clickable. (On a
  fork PR there is no upload secret: the job succeeds with its upload step skipped,
  and nothing is published. The `Summary` job's step summary says so.)
- Non-zero → report a **failure with the reason** the script printed to stderr — do not retry
  blindly:
  - Exit 2 (`no PR`) → there is no open PR for the current branch; tell the user to open one.
  - Exit 3 (`no CI run`) → the `CI` workflow has not started for the head commit yet.
  - Exit 4 (`skipped`) → the job was skipped because a build leg (or `gen`/`govet`) did not
    succeed; the platforms that did build are already on the pre-release. Relay that and the
    run URL.
  - Exit 5 (`failed`) → the job ran but concluded `failure`/`cancelled`/`timed_out`; relay the
    failing step name(s) the script reported and the run URL.
  - Exit 6 (`timeout`) → the job did not reach a terminal state within the deadline; share the
    run URL so the user can inspect it (or re-run with a larger `TIMEOUT_SECONDS`).
