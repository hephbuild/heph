# Outputs from failed targets

A failing target can leave behind what a human needs to fix it. Two knobs on
the `exec`/`bash` drivers cover the two cases:

- **`failure_out`** keeps diagnostics only a failed run produces, such as
  Playwright traces and screenshots or a junit report.
- **`codegen = "in_place"`** now writes a fixer's fixes back even when it
  exits non-zero, such as `eslint --fix` with unfixable errors left. The target
  still fails.

```python
target(
    name = "e2e",
    driver = "bash",
    run = "npx playwright test",
    failure_out = {"report": ["test-results/", "playwright-report/"]},
)

target(
    name = "lint_fix",
    driver = "bash",
    codegen = "in_place",  # fixes are written back even when eslint exits 1
    deps = {"src": ["//@heph/introspect:outputs"]},
    out = "*.ts",
    run = "eslint --fix $SRC_SRC",
)
```

Neither knob reaches any hash. See [Hashing](#hashing) below.

## The failure slot

Every failed target already has a slot: `<home>/failures/<addr-key>/`, which
is `.heph/failures/<addr-key>/` by default. `<addr-key>` is the address made
filename-safe, followed by a digest. A slot can hold:

| Part | What | Written when |
| --- | --- | --- |
| `log.txt` | The run's full process log | The run produced a log (never for credential material) |
| `out/` | The `failure_out` matches | The run exited non-zero and a declared path matched |
| `fixes.txt` | Every path an `in_place` write-back changed, one per line | Fixes were written back on failure |

There is one slot per address (per variant). It has no history:

- The next failure of the same address replaces the whole slot atomically.
- The slot is cleared when the target next resolves, on a cache hit or when
  its next run starts. A passing run therefore leaves no slot behind.
- `heph tool gc --failure-max-age-days N` drops old slots, 7 days by default.

The failure box prints where each part is:

```
× target failed: //web:e2e
╰─▶ process exited with status: exit status: 1
    ╭─[log]
    …
    ╰────
  full log: /ws/.heph/failures/web_e2e-…/log.txt
  outputs: /ws/.heph/failures/web_e2e-…/out
```

## `failure_out`

`failure_out` has the same shape as `out`: a string, a list, or a dict of group
to list. Paths are relative to the package and may be globs. Groups only name
the paths in the BUILD file; the slot does not split by group.

- **Collected only on a failed run.** The run's process must have exited
  non-zero, including on a signal. Nothing is collected after a heph-side
  failure that left no exit (sandbox preparation, credentials, the runner),
  after a cancellation (fail-fast, Ctrl-C), or under `--shell`. A passing run
  collects nothing and pays nothing: there is no stat of the failure paths.
- **Layout mirrors the workspace.** A match lands at
  `out/<workspace-relative path>`, so relative links inside a report still
  work:
  `npx playwright show-report .heph/failures/<addr-key>/out/web/playwright-report`.
- **A path that matches nothing is skipped**, not an error. Playwright writes
  `test-results/` only for some failures.
- **Symlinks do not leave the sandbox.** A symlink pointing outside the sandbox
  is not followed, as for the log.
- **Copied, never linked.** Files are copied with `std::fs::copy`, which uses a
  reflink on APFS and btrfs. They are never hardlinked or moved, so editing a
  file in the slot cannot change an input staged for another run.
- **Refused paths.** `..`, absolute paths and the package root itself are
  refused when the BUILD file is parsed. An entrypoint refuses `failure_out`
  altogether: what it wrote is already where it wrote it.
- **No size cap.** A video-recording test suite can fill the slot with
  gigabytes. Declare what you need.

### Uploading from CI

heph uploads nothing. A CI step uploads the slots itself, for example:

```yaml
- uses: actions/upload-artifact@v4
  if: failure()
  with:
    name: heph-failures
    path: .heph/failures/*/out/**
```

### Credential material

heph does not filter `failure_out`. Whatever a target lists there is kept,
including for a target whose outputs are credential material (`out =
{"credential": …}`). Such a target keeps no `log.txt`, but its `out/` is kept.
Keeping secrets out of `failure_out` is the target author's responsibility.

## `heph inspect failure`

`heph inspect failure <addr>` prints where the target's last failure was kept.
It reads the slot and builds nothing:

```
$ heph inspect failure //web:e2e
slot: /ws/.heph/failures/web_e2e-…
log: /ws/.heph/failures/web_e2e-…/log.txt
out: /ws/.heph/failures/web_e2e-…/out
```

`--json` prints one object. Paths are absolute, and an absent part is `null`:

```json
{
  "addr": "//web:e2e",
  "slot": "/ws/.heph/failures/web_e2e-…",
  "hashin": "…",
  "failed_at": 1762790400000,
  "log": "/ws/.heph/failures/web_e2e-…/log.txt",
  "out": "/ws/.heph/failures/web_e2e-…/out",
  "fixes": null
}
```

`failed_at` is in milliseconds since the Unix epoch. `hashin` is the input hash
of the run that failed, which may no longer be the target's. With no recorded
failure (the target never failed, or passed or hit the cache since), the
command exits 1 with `no recorded failure for <addr>`.

## `in_place` on failure

| Run ends with | `in_place` | `in_place_on_success` |
| --- | --- | --- |
| Exit 0 | Written back | Written back |
| Exit code 1–127 | Written back; the target still fails | Nothing written |
| Exit code 128 and above | Nothing written: counts as a signal | Nothing written |
| Killed by a signal (timeout, OOM) | Nothing written | Nothing written |
| Cancelled, or a heph-side failure | Nothing written | Nothing written |
| `--frozen` | Nothing written, and no diff: the tool's failure is the report | Nothing written |
| `--shell` | Nothing written | Nothing written |

A shell reports a killed child as 128 plus the signal number: 137 for an OOM
kill, 143 for a timeout. A fixer cut off mid-write can leave a half-written
file, so heph writes nothing back after any of these.

The write-back on failure goes through the same guard as a passing one. If an
`in_place` source changed while the target ran, nothing is written, and the
guard's error joins the tool's. A declared output the fixer did not produce is
skipped, so the fixes it did make are still written back.

The box names what was written, and how to undo it:

```
  fixes written, target still failed (re-run to verify; codegen = "in_place_on_success" opts out):
    web/a.ts
    web/b.ts
  restore with: git checkout -- web/a.ts web/b.ts
```

Past ten files, the box shows the first ten and `+N more (full list:
<slot>/fixes.txt)`. The restore hint then reads the full list:
`git checkout --pathspec-from-file=<slot>/fixes.txt`. Run the hint from the
workspace root, since the paths are workspace-relative.

When nothing was written, the box says why:

```
  fixes not written: killed by signal 9
  fixes not written: exit 137 (128+, treated as a signal)
  fixes not written: an in_place source changed while the target ran
  fixes not written: --frozen checks the tree, it never writes it
```

### A tool that truncates before it fails

A tool that rewrites a file through a temporary copy and fails in the middle,
such as `cmd < "$f" > "$f.out"; mv "$f.out" "$f"`, can leave an empty or partial
file. If it then exits 1, heph writes that file over the tracked source. The
moved-source guard does not catch this, because the source did not move.
Uncommitted edits in that file are lost.

The box names every file written, and `fixes.txt` keeps the full list. Restore
the files with the hint the box prints, or with `git checkout` on the paths in
`fixes.txt`. A tool that can do this should use
`codegen = "in_place_on_success"`.

### The idempotence contract

An `in_place` transform must be **idempotent**: running it over its own output
must change nothing. heph relies on this in two places:

- **Fixed-point caching.** After a write-back, heph files the result under the
  rewritten tree's key, so the next run over that tree is a cache hit.
- **Codegen is off the cache key.** A target switched from `in_place` to no
  codegen, or to `copy`, can be served the entry `in_place` filed over the
  rewritten tree. That entry is correct only for an idempotent transform.

A non-idempotent `in_place` tool was already served wrong results before this
change. It is not supported.

## Behaviour change

This changes what existing targets do.

- **`in_place` writes back on failure.** Every existing `codegen = "in_place"`
  target now writes its outputs back after an exit code from 1 to 127, and
  still fails. This includes the Go `go_format` driver and the Go lint-fix
  driver. Most formatters that fail changed nothing, and a write-back of
  unchanged bytes is a no-op (heph writes only files whose content differs).
- **`in_place_on_success` opts out.** It keeps the previous behaviour: a failed
  run writes nothing back.
- **One cache miss per codegen target.** The codegen mode left the exec/bash
  def hash, so every target with a `codegen` field gets a new def hash once and
  runs again on the first build after upgrading. Nothing else moves.

## Hashing

Nothing about failure outputs reaches any hash:

- `failure_out` is off the def hash, as `resources` is. It reaches no env var,
  argv, `LIST` file or sandbox directory, and is never an output of the target.
- `in_place`, `in_place_on_success`, `copy` and no codegen all hash the same.
  The mode decides what happens to the tree after a run, never what the run
  produces.
- The bytes a failure keeps never enter a cache entry, local or remote, and
  never feed a key. Nothing that hashes, selects content or stages a sandbox
  reads `failures/`.

Fixes written back on failure are the exception that is not one: they are
edits to the tree, and the next run hashes them as inputs by content, like any
other edit.
