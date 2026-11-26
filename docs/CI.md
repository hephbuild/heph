# CI cache snapshots

A warm cache in CI with no bucket, no secrets and no new service, fork pull
requests included. heph writes a **snapshot** of the cache revisions a job
used into one directory. The CI's own directory cache (`actions/cache`,
GitLab `cache:`, CircleCI `save_cache`, a Buildkite cache volume) carries that
directory to the next job, which reads it before building.

## What a snapshot is, and what it isn't

A snapshot is a directory in the remote cache's `file://` format, holding only
the revisions some job used. Each revision is whole: every non-log blob is
kept, even one the job never pulled.

- **A read-only cache tier.** A build reads the snapshot first, before any
  remote, and never writes to it. A hit is a revision whose `hashin` already
  matched, so nothing about a snapshot reaches a def hash, a `hashin` or a
  cache key.
- **It can make a build faster or leave it cold, never fail it**, with one
  exception. A missing, empty or unreadable snapshot, a newer format, a
  damaged manifest or a missing blob is a miss, and the lookup moves on to
  the next tier. The exception: a blob found cut short only after the hit was
  decided, with no tier behind the snapshot to serve it, makes heph rebuild
  the target, and a target whose rebuild isn't byte-identical stops the build.
  The snapshot written after that job leaves the cut revision out, so a
  re-run builds it as a plain miss and succeeds.
- **One job's working set, not the lineage's history.** `heph tool cache
  snapshot` rewrites the directory to what this job read or wrote, so it
  doesn't grow with every job that came before.
- **Sequential, not sideways.** A snapshot moves from one job to the next run
  of the same job. Parallel jobs of one run don't share results through it.
- **Not a replacement for a bucket.** A team with S3, GCS or R2 keeps it; a
  snapshot can sit in front of it (configuration 2, with the trust caveat
  under [Trust](#trust)).

| Carried | Not carried |
| --- | --- |
| Revisions this job read from any tier, or wrote, each with all its non-log blobs | Targets with `cache = False`, revisions kept out of the remote cache, tmp revisions |
| On an incomplete run, the restored revisions the job didn't touch | A revision that can't be completed or fails an integrity check (counted in the report) |
| Scratch slots this job used, in its own scope ([`SCRATCH.md`](SCRATCH.md)) | Scratch slots with `remote = False`; failure outputs and logs |
| On an incomplete run, or one that used no scratch slot, the previous snapshot's slots the job didn't use | On a complete run that used some slot, the previous snapshot's slots it didn't use |

## A job, end to end

1. **Restore** the snapshot directory with the CI's cache step, under a key
   for this job, platform and branch, falling back to the same job and
   platform on the default branch. A miss leaves the directory absent and the
   build runs cold.
2. **Build.** Every heph process with a snapshot dir configured reads it as a
   cache tier and records the revisions it used in a journal under
   `<home>/cache/working-set/`.
3. **Snapshot.** `heph tool cache snapshot` writes exactly those revisions
   into a new generation of the directory and consumes the journals. Run it
   even when the build failed (`if: always()`, `when: always`,
   `after_script`): a failed or cancelled run is snapshotted as an
   *incomplete run*, which also keeps the restored revisions it didn't touch.
4. **Save** the directory under a new key, even when the build failed.

Run `heph tool cache snapshot` once every heph process of the job has exited.
It waits up to 10s for one still recording, then gives up with exit code 3.

## Configuration

`cacheSnapshot:` turns snapshots on, usually in a CI profile so a developer
machine never records a working set. There is no flag.

| Source | Effect |
| --- | --- |
| `HEPH_CACHE_SNAPSHOT=<dir>` | Wins over config. An empty value turns snapshots off, even when config sets one |
| `cacheSnapshot:` in the last profile named in `HEPH_PROFILES` that sets it | Wins over the base file |
| `cacheSnapshot:` in `.hephconfig` | The default |

A relative `dir` is relative to the workspace root, and `dir: ""` turns
snapshots off. The build and `heph tool cache snapshot` resolve the directory
the same way, so they can't disagree. The directory may live inside the
workspace: no glob ever sees it. The CI's cache `path` must name the same
directory, so if the heph workspace is not the repository root, prefix the
`path` in the recipes with it.

**1. A CI profile, no bucket.** CI sets `HEPH_PROFILES=ci`.

```yaml
# ci.hephconfig
cacheSnapshot:
  dir: .heph-snapshot
```

**2. In front of a bucket.** The snapshot is read first (local disk), the
bucket second, and only trusted CI writes the bucket. **This configuration
extends the bucket's trust to every workflow that can write the snapshot's
default-branch cache entries**: a build that hits a poisoned snapshot entry
uploads what it built from it to the bucket under its real key. Read
[Trust](#trust) before using it.

```yaml
# .hephconfig
caches:
  shared:
    uri: s3://heph-cache/v1
    endpoint: https://<account>.r2.cloudflarestorage.com
    region: auto
    write: false
```

```yaml
# ci.hephconfig
caches:
  shared:
    write: true
cacheSnapshot:
  dir: .heph-snapshot
```

**3. Fork pull requests.** No secrets reach a fork, so its profile turns the
bucket off and keeps the snapshot. CI sets `HEPH_PROFILES=ci,fork` for a fork
(each recipe shows the condition).

```yaml
# fork.hephconfig
caches:
  shared:
    read: false
    write: false
```

Why not `caches: {snapshot: {uri: file://…}}`? It gives the read tier and
nothing else: no working-set recording, no rewrite, a probed latency and
writes during the build. A `file://` URI also needs an absolute path, which
differs per runner.

### Pin the heph version

A workspace that uses `cacheSnapshot:` pins a heph `version` that has it. An
older binary reads only the pin and hands over to the pinned release; without
the pin it fails on the key it doesn't know.

```yaml
# .hephconfig
version: vX.Y.Z   # a release with cacheSnapshot
```

`vX.Y.Z` stays a placeholder until the release that ships `cacheSnapshot:`
is cut; this page then names it.

## Recipes

Each key carries the **job name, the runner's OS and arch, and the matrix
values**. Two jobs sharing a key would each replace the other's working set,
and scratch caches can differ across OS and arch. Put only scalar matrix
axes in the key. A literal `-ref-` before the branch and `-run-` after it
stop the prefix for `main` from also matching a branch named `main-foo`.

`heph run test //...` stands for the job's build: any number of heph commands
can run between the restore and the snapshot, and each one records.

### GitHub Actions

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      HEPH_PROFILES: ${{ github.event.pull_request.head.repo.fork && 'ci,fork' || 'ci' }}
    steps:
      - uses: actions/checkout@v4
      - id: snapshot
        run: echo "prefix=heph-${{ github.job }}-${{ runner.os }}-${{ runner.arch }}-${{ join(matrix.*, '-') }}-ref-" >> "$GITHUB_OUTPUT"
      - uses: actions/cache/restore@v4
        with:
          path: .heph-snapshot
          key: ${{ steps.snapshot.outputs.prefix }}${{ github.ref_name }}-run-${{ github.run_id }}-${{ github.run_attempt }}
          restore-keys: |
            ${{ steps.snapshot.outputs.prefix }}${{ github.ref_name }}-run-
            ${{ steps.snapshot.outputs.prefix }}${{ github.event.repository.default_branch }}-run-
      - run: heph run test //...
      - if: always()
        run: heph tool cache snapshot
      - if: always()
        uses: actions/cache/save@v4
        with:
          path: .heph-snapshot
          key: ${{ steps.snapshot.outputs.prefix }}${{ github.ref_name }}-run-${{ github.run_id }}-${{ github.run_attempt }}
```

- `restore` and `save` are separate steps so that a failed build still saves:
  the combined `actions/cache` saves only when the job succeeds.
- Keys are immutable, so the run id makes one saved entry per run. Each entry
  is one job's working set, and they count against the repository's cache
  limit (10 GB by default; least recently used entries go first, and any entry
  unused for 7 days). Size the limit for (jobs × platforms × matrix legs ×
  active branches) × snapshot size.
- A pull request writes only its own scope and reads its base's and the
  default branch's, so it can't write what `main` restores. See [Trust](#trust)
  for what *can*.

### GitLab CI

```yaml
build:
  variables:
    HEPH_PROFILES: ci
    CACHE_PLATFORM: linux-amd64   # per runner: GitLab cache keys can't contain '/'
  rules:
    - if: $CI_MERGE_REQUEST_SOURCE_PROJECT_ID && $CI_MERGE_REQUEST_SOURCE_PROJECT_ID != $CI_PROJECT_ID
      variables:
        HEPH_PROFILES: ci,fork
    - when: on_success
  cache:
    key: heph-$CI_JOB_NAME_SLUG-$CACHE_PLATFORM-ref-$CI_COMMIT_REF_SLUG-run
    fallback_keys:   # GitLab 16.0+
      - heph-$CI_JOB_NAME_SLUG-$CACHE_PLATFORM-ref-$CI_DEFAULT_BRANCH-run
    paths: [.heph-snapshot]
    when: always
  script:
    - heph run test //...
  after_script:
    - heph tool cache snapshot
```

- `$CI_JOB_NAME_SLUG` needs GitLab 15.4 or later, and `fallback_keys` 16.0.
- A GitLab cache key is overwritten in place, so the key needs no run id.
- `after_script` runs when `script` fails, and the cache is saved after it.
  It doesn't run when the job times out or fails for any reason other than
  its script. It has its own timeout, 5 minutes by default; a large snapshot
  on a slow disk can need more.
- **A failed `after_script` doesn't fail the job**: a snapshot that exits 2,
  3 or 4 there leaves the job green, and the cache saves the directory as it
  was. Only the job log shows it.
- **Write scoping:** protected and unprotected branches get separate caches
  only while the project's "Use separate caches for protected branches"
  setting is on (the default). Keep it on, and protect the default branch.
  Anyone who can push to a protected branch, or run a pipeline for one, can
  write what the default branch restores.

### CircleCI

CircleCI cache keys are project-wide, so the branch is in every key, and the
default branch falls back only to its own entries.

```yaml
jobs:
  build:
    docker:
      - image: cimg/base:current
    environment:
      HEPH_PROFILES: ci
    steps:
      - checkout
      - run: |
          if [ -n "${CIRCLE_PR_USERNAME:-}" ]; then echo 'export HEPH_PROFILES=ci,fork' >> "$BASH_ENV"; fi
      - restore_cache:
          keys:
            - heph-{{ .Environment.CIRCLE_JOB }}-{{ arch }}-ref-{{ .Branch }}-run-
            - heph-{{ .Environment.CIRCLE_JOB }}-{{ arch }}-ref-main-run-
      - run: heph run test //...
      - run:
          command: heph tool cache snapshot
          when: always
      - save_cache:
          key: heph-{{ .Environment.CIRCLE_JOB }}-{{ arch }}-ref-{{ .Branch }}-run-{{ .BuildNum }}
          paths: [.heph-snapshot]
          when: always
```

- Replace `main` with the default branch. `{{ arch }}` carries the OS and
  arch; matrix parameters are part of `CIRCLE_JOB`.
- `CIRCLE_PR_USERNAME` is set only on a build of a forked pull request.
- **Write scoping:** any job that runs in the project can save any key. The
  branch in the key keeps an ordinary branch build from writing `-ref-main-`,
  but not a job whose config a contributor controls. If the project builds
  forked pull requests, check that their caches are isolated from the
  project's, or give those builds no snapshot (`HEPH_CACHE_SNAPSHOT=`, empty,
  in the step above instead of the fork profile).

### Buildkite

On hosted agents, a cache volume keeps the directory between builds. A volume
has a name, not a key: no fallback, and every build of the pipeline mounts the
same one, so name one per step and platform.

```yaml
steps:
  - label: build
    key: build
    env:
      HEPH_PROFILES: ci
    cache:
      name: heph-build-linux-amd64
      paths: [.heph-snapshot]
    command: |
      status=0
      heph run test //... || status=$$?
      heph tool cache snapshot
      exit $$status
```

- The snapshot is rewritten in place on the volume, which is safe: a new
  generation is built beside the current one and replaces it by a rename.
- **Write scoping:** a cache volume has no access control, and every build of
  the pipeline, on any branch, reads and writes it. Use it only in a pipeline
  whose every build is trusted. A pull request build runs the steps it
  uploads, so don't build untrusted pull requests in a pipeline with this
  volume.
- On self-hosted agents with a persistent home, the local cache is already
  warm; a snapshot adds a tier, and a journal left by a crashed job makes the
  next snapshot bigger, never wrong.

## Trust

**A snapshot is trusted exactly like a remote cache you can write to.** A hit
is used without asking who produced it. heph checks nothing about who wrote a
snapshot; the CI's cache scoping carries the trust, through the keys in the
recipes. So the rule is: **a ref that's less trusted must never write a cache
entry that a ref that's more trusted restores.**

- GitHub scopes a pull request's writes to the pull request. But every
  workflow that runs on the default branch can write the default branch's
  scope, including `pull_request_target` and `issue_comment` workflows and
  every action they run. Treat each of those as a writer of what `main`
  restores.
- GitLab separates protected and unprotected caches by default.
- CircleCI keys are project-wide; the branch in every key is what keeps an
  ordinary branch from writing the default branch's entries.
- Buildkite cache volumes have no access control.

**A snapshot in front of a writable bucket** (configuration 2) is allowed,
and **extends the bucket's trust to every writer of the snapshot's
default-branch entries**. A build that hits a poisoned entry produces outputs
from it and uploads them to the bucket under their real keys, where every
consumer of the bucket, developer machines included, reads them. Scratch is
the stronger case: a slot has no output hash, so a poisoned slot is used by
every target that mounts it. Use configuration 2 only when every workflow
that can write the default branch's cache scope is as trusted as the
credentials that write the bucket.

## Fork pull requests

A fork gets no secrets, so without a snapshot it gets no cache at all. With
one, it restores the default branch's snapshot read-only (its writes go to
its own scope) and turns the bucket off with the `fork` profile, as each
recipe shows. The fork's own builds then reuse its earlier ones.

## Reading a run

In CI (stderr not a terminal), heph prints plain, stable lines. Each is also a
typed event in `heph run --json` and in the session record.

**First, read which key the CI's restore step matched.** heph can't see it.
GitHub prints `Cache restored from key: …` (and sets the restore step's
`cache-matched-key` output), GitLab prints the key it extracted, CircleCI
prints `Found a cache from build N at …`. A miss there explains every line
below.

At the start of a run, the tier's state:

```text
snapshot: .heph-snapshot, 1.2 GiB, 412 revisions, written by heph 0.4.1, incomplete run
snapshot: .heph-snapshot absent (restore missed)
snapshot: .heph-snapshot has no marker (skipped)
snapshot: .heph-snapshot unreadable format 2 (skipped): newer than this heph reads (1)
```

`written by` and `incomplete run` say where the snapshot came from: a small
snapshot marked `incomplete run` means an earlier job failed, not that the
restore missed. `absent` is a lineage's first job, or a restore that missed.

At the end of a run, hits and misses per tier, the misses by reason. The
line is left out when no snapshot or remote cache was consulted (every
target hit the local cache, or none was cacheable):

```text
cache tiers: local 0 hit 2 miss · snapshot 0 hit 2 miss (snapshot_other_hashin 1, addr_absent 1)
```

| Reason | Means |
| --- | --- |
| `snapshot_other_hashin` | The snapshot has the target under another `hashin`: an input changed (`heph inspect` says which) |
| `addr_absent` | The snapshot never had the target |
| `blob_missing_fell_through` | The snapshot had the revision but not a blob it needed; the lookup moved on |
| `revision_absent` | A remote cache has no revision under this `hashin` |
| `damaged` | The tier answered with a manifest it can't serve |
| `unreachable` | The read failed, or the cache's breaker is open |

All hits and `snapshot_other_hashin` misses: the snapshot is working, and the
inputs changed. All `addr_absent`: the restore brought back another job's
snapshot, or none.

When `CI` is set (to anything but empty, `0` or `false`) and a run recorded
revisions, heph notes once per process that the snapshot step still has to
run. Every correct job prints it, since that step comes after the build:

```text
snapshot: journal holds 12 revisions; 'heph tool cache snapshot' writes them
```

## `heph tool cache snapshot`

```text
snapshot: wrote gen-… in /work/.heph-snapshot
  kept              380 revisions  1.1 GiB
  new                32 revisions  96.0 MiB
  carried over        0 revisions  0 B
  dropped            41 revisions  210.0 MiB
journals: 3 consumed
```

- **kept / new / dropped**: against the previous generation. **carried
  over**: restored revisions kept because the run was incomplete. **left
  out**: revisions it couldn't write whole, by reason (trimmed by gc, a
  missing blob, a failed integrity check, …).
- **scratch kept / new / carried / dropped**, when the job or the previous
  generation had scratch slots, counted by slot. A slot the job used is
  written as it left it. A slot it didn't use is carried over when the run
  was incomplete or used no slot at all, and dropped when the run was
  complete and used some other slot. An entry this heph can't read is
  dropped.
- `incomplete: <reason> (<journal>)`, one line per journal, appears when a
  process ended without its trailer (killed, or a second Ctrl-C), a request
  failed or was cancelled, or a journal has a line this heph can't read.
  Then one `carried over: <bytes> (<n> revisions, <m> scratch slots)` line
  totals what the run kept that it didn't touch.
- `cleanup failed: …` means the new generation is written and current, but
  removing an old generation or deleting a consumed journal failed. The
  exit code is still 0. The line says what is left: an old generation goes
  with the next snapshot, and a journal that is still there is read again by
  it, which makes that snapshot bigger, never wrong.
- An empty working set leaves the directory as it was:
  `snapshot: unchanged, the working set is empty (<dir>)`.

Every zstd blob the new generation holds is decoded once per snapshot, kept
ones included, to check it against its manifest (a raw blob's length is
checked). That is the snapshot's CPU cost: it grows with the whole working
set, not with what the job built.

`--json` prints one document: `schema` (an integer), `dir`, `complete`, then
the report with integer bytes, the top revisions by bytes and the dropped
addresses, in a stable order with no timestamps. A failure prints
`{"schema": 1, "error": "…", "reason": "…"}`. Adding a field keeps `schema`;
removing or retyping one bumps it. Ignore fields you don't know.

- `complete` is the machine signal for an incomplete run; each
  `incomplete[].reason` is text for a human and may change.
- `error` is text for a human and may change. Branch on `reason`, whose set
  can grow: treat a value you don't know like `error`.
- The `left_out` keys (`trimmed`, `missing_blob`, `corrupt`, `key_mismatch`,
  `case_collision`, `unsupported`, `unknown_manifest`) are permanent: one may
  be added, none is renamed or reused.

| Exit | `reason` | Means |
| --- | --- | --- |
| 0 | | Written or unchanged, including an incomplete run and revisions left out (both in the report) |
| 1 | `not_configured` | No snapshot dir is configured |
| 2 | `error` | An I/O error, a full disk, an unwritable dir, or a usage error |
| 3 | `recorder_alive` | A heph process is still recording after 10s; the message names it |
| 4 | `newer_journal` | A journal from a newer heph; the message names the file, and deleting it is safe |
| 130 | `cancelled` | Interrupted |

## Troubleshooting

- **Every job is cold.** Check the restore step's matched key first. Then the
  start-of-run `snapshot:` line: `absent` means nothing was restored to the
  path heph reads, so check that the cache `path` and `cacheSnapshot.dir`
  name the same directory relative to the workspace root, and that
  `HEPH_PROFILES` is set in the step that builds.
- **The snapshot shrinks after a red job.** It doesn't: a failed or cancelled
  run is incomplete, and keeps what it didn't touch. If it shrank, the snapshot
  step didn't run (missing `always`), or the save step didn't.
- **The snapshot keeps growing.** A target that fails on every run keeps the
  run incomplete, so restored revisions are carried over each time. The
  report's `incomplete:` line says why and how many bytes were carried.
- **Exit 3.** A heph process of the job is still alive: a background
  `heph run`, or a step that didn't wait for one. The message names its pid;
  pid 1 is a container's main process, so look for it in the container that
  recorded the journal.
- **Exit 4 on a persistent runner.** A newer heph ran on this machine and
  left a journal this heph can't read. The message names the file: delete
  it. Its revisions are only missing from this snapshot; the next job that
  uses them records them again.
- **`unreadable format N (skipped)`.** The snapshot was written by a newer
  heph. The run is cold for the tier, and the next `heph tool cache snapshot`
  replaces it.
- **Two jobs' snapshots overwrite each other.** Their keys are equal: add the
  job name, platform or matrix value that tells them apart.
