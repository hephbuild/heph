# Outputs from failed targets

> **Where did my traces go?** `heph inspect failure <addr>` prints where the
> target's last failure kept its log, its `failure_out` files and its fixes.
> An agent or a script does it in two steps: after `heph run` exits non-zero,
> call `heph inspect failure --json <addr>` and read the paths from the JSON.

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
| `manifest.json` | The address, the revision, when it failed, the fill's id | Always, last: a slot is complete exactly when it exists |
| `log.txt` | The run's full process log | The run had a log, an empty one included (never for credential material) |
| `out/` | The `failure_out` matches | The run exited non-zero and a declared path matched |
| `fixes.txt` | Every path an `in_place` write-back changed, one per line | Fixes were written back on failure |
| `diagnostic.json` | The failure's record: its diagnostic, causes, exit, path | Every recorded failure (never for credential material) |

There is one slot per address (per variant). It has no history:

- The next failure of the same address replaces the whole slot atomically.
- The slot is cleared when the target next resolves, on a cache hit or when
  its next run starts. A passing run therefore leaves no slot behind.
- `heph tool gc --failure-max-age-days N` drops old slots, 7 days by default.

### The manifest and the diagnostic

Each fill of a slot mints a **fill id** (a uuid) and writes it into the
manifest. A failure's record names its log by that id, so a reader holding an
older record can tell the slot was since filled by a newer failed run
(`log replaced by a newer failed run`) or cleared (`log cleared`), and never
shows the newer run's log as the older one's. A slot from an older heph has no
fill id: its log is shown with no claim about whose it is.

`hashin`, the run's revision, is `null` for a failure before the run (which has
none) and for an invocation (never hashed).

Every failure the request records (each culprit's, once) writes its record
into the culprit's slot as `diagnostic.json`. It is versioned (`"version": 1`)
and names the fill it belongs to; a reader ignores one of a version it does
not know or of another fill. It is the record as compacted for the report:
scrubbed of credential values and within the same caps, without the log
reference or what the failure left behind, which are the slot's own files.
It is written in the background once the failure is recorded, and `heph run`
does not exit before it lands. A fill or a clear of the same slot waits for
it.

**A failure before the run** keeps a slot too: no log, no `out/`, a manifest
with no revision and `diagnostic.json`. That covers a def that does not
parse, a missing dependency, a run that failed and kept nothing, and a BUILD
file that did not evaluate. A BUILD file's slot is keyed by the file, not by
a target: `<home>/failures/file_<path>-<digest>/`, its manifest's `addr` is
`file:<path>` (workspace-relative), and it lists the packages whose targets
were reached through the file. Nothing clears it when the file evaluates
again; `heph tool gc` keeps it while the file exists and drops it once the
file is gone or the slot is older than the max age.

**Credential material gets no `diagnostic.json`.** A target whose outputs are
material already keeps no log on disk, and its slot gets no record either.
The one exception: a material target that fails before it runs carries no
slot to say it is material, so it gets a slot holding only
`diagnostic.json`. That record went through the same scrub as the report.

A run that printed nothing keeps an empty `log.txt`: the slot says the run
ran and printed nothing, rather than reading as a failure that kept no log.

`log.txt` is the engine's own copy of the run's output. A plugin streams its
output into the engine's live run log (`<home>/runs/<id>/<n>.log`, see
`docs/SESSIONS.md`), scrubbed of credential values as it arrives; when the run
fails, that file is copied into the slot through its open descriptor and then
unlinked. A plugin built before log streaming returns a log file instead: the
engine imports it, scrubbed the same way, provided it is a regular file inside
the run's sandbox, and the returned file wins over anything streamed.

A non-interactive run (CI, a pipe) prints a root failure's block the moment
it fails, with the last `--log-lines` lines of its log, then where each part
is:

```
ERROR ✗ //web:e2e · exit status: 1
  │ …
  │ 3 failed, 41 passed
  full log: /ws/.heph/failures/web_e2e-…/log.txt
  outputs: /ws/.heph/failures/web_e2e-…/out
  log: heph log //web:e2e
```

The end of the run does not say it again. A failure that printed no block as
it failed — one that never reached its run, such as a dependency that does
not exist — gets the failure box at the end instead, with the same rows:

```
× target failed: //web:e2e
╰─▶ get_def: //web:e2e: get spec: //web:nope: target not found: //web:nope
  log: none kept
```

A box also frames the log's tail, when there is one. Either way the last row
is the
command that prints the log without naming where it lives, printed whenever
heph can open a log for the target — on disk or not, a remote hit's log it
would fetch included — and `log: none kept` when it cannot. An interactive run
prints the failure report instead: one block per root failure, naming no path
into heph's home, each ending with the same answer as `full log: heph log
<addr>` or `full log: none kept`.

A script reading either shape can rely on the order of the rows and on their
labels (`full log:`, `outputs:`, `took down:`, `fixes written:`, …): those
are stable. The whitespace is not. A report's labels are padded to line up
with each other, so the indentation and the gap after a label change with
which rows a failure prints. Match a label with its colon and trim what
follows it.

### The failed sandbox

The slot is what outlives a failure; the sandbox the run failed in does not.
Once the slot is filled (and an `in_place` write-back is done), the sandbox is
removed, like a passing run's, and the box says nothing of it. Rerun with
`--keep-failed-sandboxes` to keep it on disk for inspection instead: it then
stays until the target's next run, and it is outside the `sandbox` pool's cap
(see [RESOURCES.md](RESOURCES.md)). A target whose outputs are credential
material has its failed sandbox removed even under the flag.

The box's `sandbox:` line, after `full log:`, appears only when the sandbox's
fate is out of the ordinary:

| Line | When |
|---|---|
| `sandbox: <path> (kept)` | under `--keep-failed-sandboxes` |
| `sandbox: removed (credential material)` | material, under the flag |
| `sandbox: left (run abandoned after timeout)` | the run outlived its timeout's grace (see [TIMEOUTS.md](TIMEOUTS.md)) |

There is no line for the default removal, nor when the run had no sandbox (a
plain driver such as `template` or `fs`, or a failure before the driver ran),
nor for material without the flag. `heph inspect failure` (and its `--json`) never reports the
sandbox: it reads the slot, and the sandbox is not part of it.

When the slot's fill itself fails (an unwritable failures directory, a log
outside the sandbox), the tail shown falls back to the log's tail in memory, and the
full log is gone with the sandbox unless `--keep-failed-sandboxes` kept it.

## `failure_out`

`failure_out` has the same shape as `out`: a string, a list, or a dict of group
to list. Paths are relative to the package and may be globs. Groups only name
the paths in the BUILD file; the slot does not split by group.

- **Collected only on a failed run.** The run's process must have exited
  non-zero, including on a signal or a `timeout`. Nothing is collected after a heph-side
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
  is not followed, as for the log. One inside it is kept as the file it points
  to, or as the whole directory when the link is the declared path or a glob
  match itself.
- **Globs are bounded.** A `failure_out` glob walks from its literal prefix,
  never through a linked directory, and only as deep as the pattern can match:
  `*.txt` reads the package directory, not every input staged below it. This is
  the `failure_out` walk only. The `in_place` fixes written back on failure are
  walked as a passing run's outputs are, by the output glob that refuses a
  linked directory outside the stage (#566).
- **Packed, then unpacked: never linked.** The matches are packed into a tar in
  the sandbox (`heph-collect-artifacts/failureout-*.tar`) by the same packer
  as outputs, and the engine unpacks it into the slot and deletes it. Only
  file members are unpacked, never a link, and only at a plain relative path.
  Every file in the slot is a new inode, never a hardlink or a move, so
  editing a file in the slot cannot change an input staged for another run.
  A tar holds files: an empty directory is not kept, and neither is a file
  whose name is not UTF-8.
- **Refused paths.** These are refused when the BUILD file is parsed, and the
  error names the path:
  - a path with a `..` segment;
  - an absolute path;
  - the package root itself: `.`, an empty path, or a pattern of nothing but
    `*` and `**` (such as `**` or `./**/*`). Name what to keep instead, such
    as `test-results/`.

  An entrypoint accepts `failure_out` only with `workdir = "sandbox"`
  ([Entrypoints](#entrypoints)).
- **Never written into the tree.** A `failure_out` path that falls inside an
  `in_place` output is left out of the write-back on failure: it lands only in
  the slot's `out/`.
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

The glob assumes the default `<home>`, `.heph`. To upload one target's
failure wherever `<home>` is, read the path from `heph inspect failure`'s
JSON. `--jq` filters it in heph, so the runner needs no `jq`; a string result
prints unquoted, and `empty` prints nothing when the part (or the whole
failure) is `null`:

```yaml
- id: e2e_failure
  if: failure()
  run: echo "out=$(heph inspect failure //web:e2e --jq '.out // empty')" >> "$GITHUB_OUTPUT"
- uses: actions/upload-artifact@v4
  if: failure() && steps.e2e_failure.outputs.out != ''
  with:
    name: e2e-failure
    path: ${{ steps.e2e_failure.outputs.out }}
```

There is no command that lists every recorded failure in v1. Ask per target,
or glob the slots.

### Credential material

heph does not filter `failure_out`. Whatever a target lists there is kept,
including for a target whose outputs are credential material (`out =
{"credential": …}`). Such a target keeps no `log.txt`, but its `out/` is kept.
Keeping secrets out of `failure_out` is the target author's responsibility.

### Entrypoints

An entrypoint (`entrypoint = True`, see [`ENTRYPOINTS.md`](ENTRYPOINTS.md))
takes `failure_out` only with `workdir = "sandbox"`. That is the only mode
whose writes are lost: the invocation's sandbox is removed when the tool
exits. In the other modes the tool runs in your tree or your cwd, and whatever
it wrote is already where it wrote it, so parse refuses `failure_out` there.

```python
target(
    name = "e2e",
    driver = "bash",
    entrypoint = True,
    workdir = "sandbox",
    failure_out = ["test-results/"],
    run = "playwright test",
)
```

It keeps what a build keeps, read the same way, with these differences:

- **`out/` only, never a log.** An invocation's output goes to your terminal
  and is never logged, so its slot has no `log.txt`.
- **Copied before the teardown.** The matches go into the slot before the
  invocation's sandbox is removed, so the slot is there by the time
  `heph run` exits with the tool's own status. Nothing is printed;
  `heph inspect failure <addr>` says where they are.
- **Cleared when the next invocation starts**, once its closure is built.
  Two invocations of the same target running at once share one slot: the
  last failure to finish is the one kept.
- **No revision.** An invocation is never hashed, so the manifest's `hashin`
  is `null`.

## `heph inspect failure`

`heph inspect failure <addr>` prints where the target's last failure was kept.
It reads the slot and builds nothing:

```
$ heph inspect failure //web:e2e
slot: /ws/.heph/failures/web_e2e-…
log: /ws/.heph/failures/web_e2e-…/log.txt
out: /ws/.heph/failures/web_e2e-…/out
```

`--json` prints one object (pretty on a terminal, as here; one line when
piped). Paths are absolute, and an absent part is `null`. `--fields` picks
keys and `--jq` filters it, as on every command (`docs/JSON_OUTPUT.md`):

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
of the run that failed, which may no longer be the target's, and `null` for a
failure before the run. Both are in the JSON only.

With no recorded failure (the target never failed, or passed or hit the cache
since):

- the text form prints `no recorded failure for <addr>` on stderr and exits 1;
- `--json` prints `null` on stdout and exits 0 (with `--jq`, the filter runs
  over `null`).

So a script reads "no failure" as `null`, and a non-zero exit only means the
command itself failed.

### A failed package probe

A failure is recorded under its culprit: a target, or a BUILD file that did
not evaluate. No culprit is a package. When the provider's probe of a package
fails (it cannot list it, for a reason other than a BUILD file that does not
evaluate), the failure is recorded under the target that was being resolved
in that package. The failure report, `run --json` and the event stream name
that target, not the package. The targets that depend on it are not blamed.
This is a known limit: an error about the whole package reads as one
target's failure. `deep_probe_error_blames_the_package` in
`crates/e2e/tests/e2e/blame.rs` pins it.

## `in_place` on failure

| Run ends with | `in_place` | `in_place_on_success` |
| --- | --- | --- |
| Exit 0 | Written back | Written back |
| Exit code 1–127 | Written back; the target still fails | Nothing written |
| Exit code 128 and above | Nothing written: counts as a signal | Nothing written |
| Killed by a signal (OOM), or a `timeout` | Nothing written | Nothing written |
| Cancelled, or a heph-side failure | Nothing written | Nothing written |
| `--frozen` | Nothing written, and no diff: the tool's failure is the report | Nothing written |
| `--shell` | Nothing written | Nothing written |

A shell reports a killed child as 128 plus the signal number: 137 for an OOM
kill, 143 for a SIGTERM. A fixer cut off mid-write can leave a half-written
file, so heph writes nothing back after any of these. A run stopped by its
`timeout` writes nothing back either, but keeps its `failure_out`, as a run
killed by a signal does.

All of this, the 128-and-above rule included, behaves the same on Linux and
macOS.

The write-back on failure goes through the same guard as a passing one. If an
`in_place` source changed while the target ran, nothing is written, and the
guard's error joins the tool's. A declared output the fixer did not produce is
skipped, so the fixes it did make are still written back.

The box names what was written, and how to undo it:

```
  fixes written, target still failed (re-run to verify; codegen = "in_place_on_success" opts out):
    web/a.ts
    web/b.ts
  restore with (takes the index version; uncommitted edits to these files are lost):
    git -C /ws --literal-pathspecs checkout -- web/a.ts web/b.ts
```

**The restore hint loses uncommitted edits.** It puts back the index version
of each file, the committed one unless you staged a change. Any edit you had
not staged in those files, made before the run or by the fixer, is gone.

The hint runs git in the workspace root (`-C`), so it works from any
directory. It reads each path literally (`--literal-pathspecs`): otherwise git
would read `web/[id].ts` as a glob and also revert `web/i.ts`. Past ten files,
the box shows the first ten and `+N more (full list: <slot>/fixes.txt)`, and
the hint reads the full list:
`git -C <root> --literal-pathspecs checkout --pathspec-from-file=<slot>/fixes.txt`.

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
the files with the hint the box prints. A tool that can do this should use
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
- **One cache miss per exec/bash codegen target.** The codegen mode left the
  exec/bash def hash, so every `exec` or `bash` target with a `codegen` field
  gets a new def hash once and runs again on the first build after upgrading.
  plugin-go's def hashes do not move, `go_format` and the lint-fix driver
  included. Nothing else moves.
- **Every plugin cdylib must be rebuilt.** The plugin ABI is now 0.21.0, and
  the host refuses a plugin built against 0.20. See
  `crates/plugin-stabby/ABI_VERSIONING.md`.
- **An older heph removes a newer slot.** A heph from before this change reads
  a slot with `out/` or `fixes.txt` but no `log.txt` as litter and deletes it.
  Going back to an older heph can lose a failure kept by a newer one.
- **An older heph's gc removes the slots of failures before the run.** A slot
  holding only `diagnostic.json`, one whose `hashin` is `null`, and a BUILD
  file's `file:` slot all read as litter to a heph from before fill ids, and
  its `heph tool gc` deletes them. A newer heph reads an older heph's slots as
  they were (no fill id, no diagnostic).

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
