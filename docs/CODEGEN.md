# Codegen: writing outputs back into the tree

A target can ask heph to write its outputs back into the source tree, with
`codegen` on the output:

```python
target(
    name = "gen",
    driver = "bash",
    deps = ["schema.proto"],
    out = "foo.pb.go",
    codegen = "copy",
    run = "protoc ... > $OUT",
)
```

- **`codegen = "in_place"`** rewrites tracked, committed source files (a
  formatter, a lint fixer). Those files are sources: globs see them, git
  tracks them, and nothing below applies to them. `example/fmt` shows both
  modes side by side.
- **`codegen = "copy"`** writes a *generated* file into the tree. It is a
  build output that happens to live next to the sources: git ignores it,
  globs skip it, and a consumer depends on the target that writes it, never on
  the file.

This document is about `copy`: how heph knows which tree files it wrote, how
every reader keeps them out of its inputs, and what happens to a file when
its target changes or goes away.

## Two records: the section and the index

**The managed `.gitignore` section says which paths are generated
territory. The codegen index says which bytes heph wrote there.**

### The managed section

The root `.gitignore` holds a section heph manages, one pattern per `copy`
output, each with the target that declares it on the line above:

```gitignore
# BEGIN heph-generated (managed by `heph tool codegen gitignore` — do not edit)
# //p:gen
/p/foo.pb.go
# END heph-generated
```

The section is derived from the target definitions, so it is the same on every
machine, and it is committed. Only commands write it: `heph tool codegen
gitignore`, `keep` and `discard`. A build never does. A whole-workspace
`heph validate` fails when the section is stale, including a line whose target
no longer exists and a BEGIN line with the old marker text. `validate` fails
only on what the commit decides: findings local to one checkout (anything
`codegen status` lists) are warnings pointing at `codegen status`.

Lines mean to heph what they mean to git: heph matches them with git's rules,
case-sensitive on every platform. A directory line such as `/p/gen` covers
everything under it, `!x` negates, `{a,b}` is literal. A line heph did not
write (no `# //pkg:target` above it, or a shape heph never emits such as
`*.pb.go`) still counts; `codegen status` lists it as a `foreign_line` and
`codegen why` names it. A malformed section (END without BEGIN, END before
BEGIN) reads as no section, with a warning.

### The codegen index

Every `copy` write-back records the files it wrote, with their content hash,
in a sqlite database:

- **Where:** `<home>/codegen/index.db`, beside `cache/`, never under it. The
  home defaults to `.heph/` at the workspace root. When the home is outside
  the root (an absolute `home:`), several checkouts can share it, and each
  gets its own file, `<home>/codegen/index-<hash of the root>.db`.
- **One per checkout.** It describes one working tree. It is never shared
  across worktrees or machines.
- **Never pruned.** `heph tool clean`, `heph tool gc` and the walker's cache
  pruning never touch it. Unlike the caches, losing it changes answers rather
  than costing time.
- **Created on first need.** The first write-back that records something (or
  `heph tool codegen import`) creates it. `--help`, `--version`, a query or a
  fresh CI checkout that builds no `copy` target create nothing. Until it
  exists, heph reads it as empty.
- **Loud when broken.** A newer, corrupt, unreadable or locked index is an
  error, never an empty index.

Each row holds the path, the target that wrote it (`addr`), the output it came
from, a state, the content hash and a stat of the file:

| State | Meaning |
| --- | --- |
| `written` | a write-back of this target wrote these bytes |
| `imported` | recorded from an older heph's xattr by `codegen import`; never deleted on its record |
| `dropped` | the target was rebuilt and no longer produces this path; the file was kept |
| pending | a write-back is in flight (or crashed): the row records the bytes it is about to write |

A write-back records its rows *before* it moves a byte, and a warm run whose
bytes are unchanged writes nothing to the index at all.

## How heph classifies a tree file

Every reader asks the index for a file's status:

| Status | Reason | When | Read as |
| --- | --- | --- | --- |
| `Generated` | `recorded` | a row matches the bytes on disk, and a section line covers the path, or none ever did, or the workspace has no section | excluded |
| `Generated` | `pending` | a row is pending, whatever the bytes | excluded |
| `Generated` | `unrecorded` | no row, but a section line covers the path (fresh index, lost home, a file the import missed) | excluded |
| `Modified` | `edited` | a row exists, a section line covers the path, and the bytes differ | excluded, with a warning |
| `Released` | — | a row matches, a section line covered the path once, and none covers it now | source |
| `Source` | — | anything else | source |

So a file stays excluded while git ignores it, and becomes source when git
stops ignoring it. The one gap: a file nobody recorded and no section line
covers is source. That happens after the index is lost in a workspace with no
section, or before `codegen import` ran in an upgraded checkout.

The bytes are verified by stat first (size, mtime, inode, device, mode, with
git's racy-clean rule) and by content hash when the stat moved. On a
case-insensitive filesystem another casing of a recorded path counts as the
same file only when both names lead to the same inode.

An edited generated file stays excluded, and heph warns once per run:

```
p/foo.pb.go was written by //p:gen and has been edited since; the next build of
//p:gen overwrites it. To keep your version, remove that output from //p:gen and
run heph tool codegen keep p/foo.pb.go.
```

The next write-back overwrites it and says so ("overwrote hand edits to
p/foo.pb.go; the previous content is gone").

## What each reader does

### Globs

The fs `glob()` target and the BUILD-time `heph.fs.glob()` skip `Generated`
and `Modified` files. A run that excluded any prints one line when it ends,
by default: "12 generated files were excluded from globs (9 recorded, 2
unrecorded, 1 edited); heph tool codegen why <path> explains one". Each path is
counted once per run, however many globs skipped it. `-v` and `--json` list
each path, and `-v` adds each glob's own count ("excluded 3 generated files
(…)"). Reading the index needs no target definition, no engine call
and no write, so it is safe inside a BUILD evaluation.

### fs `file` targets

The Starlark `file("p/foo.pb.go")` builtin checks nothing: it only builds the
address of the fs `file` target, so a `file()` call, and the def hash of a
target that names one, are independent of the local index. (`heph.fs.glob` is
not: it reads the index while the BUILD file evaluates, see Globs and Cache
keys.) The fs `file` target checks when it runs:

- **A recorded or pending generated path, or an edited one, fails:**

  ```
  p/foo.pb.go is generated by //p:gen; depend on //p:gen, not the file
  (heph tool codegen why p/foo.pb.go)
  ```

  The fix is to depend on the generator target. Its outputs reach the
  consumer through the dependency, at the same path.
- **A file whose target no longer produces it, or no longer exists,** fails
  with the commands that settle it instead:

  ```
  p/foo.pb.go was generated by //p:gen, which no longer exists; keep it with
  heph tool codegen keep p/foo.pb.go, or delete it with heph tool codegen
  discard p/foo.pb.go
  ```

  ("which no longer produces it" when the target exists but dropped the output.)
- **An unrecorded path under a section line** resolves to nothing, with a
  warning that names the line:

  ```
  p/foo.pb.go matches section line `.gitignore:42 /p/*.pb.go` (# //p:gen) and is
  excluded; depend on //p:gen, or drop the line with heph tool codegen keep
  ```
- **`Released` or `Source`:** the file, as before.

fs targets are never cached, so a failure is never stored or served from a
cache.

**This failure depends on the checkout's history.** A path the generator
wrote in *this* checkout fails; a fresh checkout that has not built the
generator has no row and reads the file as source (or as missing). So a laptop
that built the generator fails where a fresh CI checkout passes. Within one
request that builds both, an fs target that runs before the generator's
write-back finds no row and one that runs after it fails; the next run fails
either way. Depending on the generator is correct everywhere.

### Cache keys

`heph.fs.glob` reads this checkout's codegen index and the root `.gitignore`
section, so what it returns, and every def hash built from it, can differ
between two checkouts of one commit. The index never reaches a cache key
directly, only through which paths a glob returns. For one commit with the same
files on disk, cache keys agree across machines when `heph tool codegen status
--check` passes on both and no git-tracked file sits under a section line or a
`copy` output. Outside that, a key can differ across machines, which costs a
cache miss, never a wrong hit.

### `copy_output`: a target reading its own outputs

`//@heph/introspect:outputs` over a `copy` target's own outputs expands to
`//@heph/fs:copy_output@f=<path>` rather than `file`. `copy_output` means: an
excluded path is zero artifacts, with no error and no warning, and never its
bytes, so a target never hashes its own previous output. Any other path is the
file, as with `file`. Only the engine emits it; everything else (user BUILD
files, `in_place` outputs, the Go fallback below) uses the strict `file`.

### In-place targets

The write-back of an `in_place` target skips a generated path that another
target recorded, as before. A path recorded for the *same* target (it changed
from `copy` to `in_place`) is refused, naming `heph tool codegen keep`: keep
the file first, then build. `--frozen` skips the same paths and writes
nothing.

### Go

- The `format` target reads its package's sources through an fs glob target
  (`<pkg>/*.go`, with the package path escaped for the glob syntax), and
  declares the same glob as its `in_place` output. Generated `.go` files are
  therefore never formatted or written back. A package whose source set is
  unchanged keeps its cache entry; one that held generated `.go` files misses
  once.
- A package whose only `.go` files are generated now gets a `format` target
  (it lists in `heph query`); it stages nothing and does nothing.
- A Go package whose directory path contains a `\` cannot get a `format`
  target that runs: the glob syntax cannot express a literal `\`. Go refuses
  `\` in import paths anyway.
- A Go compile that names a generated file directly, rather than reaching it
  through its generator target, fails with the fs `file` error above. It used
  to compile silently without the file.

## Lifecycle

| Event | Index | File | Globs | You |
| --- | --- | --- | --- | --- |
| `copy` target first built | recorded | written | excluded | run `codegen gitignore` (the write-back warns until you do) |
| rebuilt, same bytes | no write | untouched | excluded | nothing |
| output added | new rows | written | excluded | `codegen gitignore` |
| a glob output produces fewer files | the file heph wrote is deleted (its hash still matches) | deleted | gone | nothing |
| output removed or renamed, target rebuilt | old rows `dropped` | kept | excluded | `codegen status`, then keep or discard |
| output or target removed, not rebuilt | unchanged | kept | excluded | the same; `validate` fails on the stale section |
| target renamed, same path | the next write-back takes the rows | rewritten | excluded | `codegen gitignore` |
| `copy` → `in_place` | the in-place write-back refuses the path | kept | excluded | `codegen keep`, then build |
| a BUILD file breaks | untouched | kept | excluded | fix it; `status` reports the package `unresolved` |
| you edit a generated file | bytes differ | edited | `Modified`: excluded, warned | nothing, or remove the output and `keep` |
| you delete a generated file | dangling row | gone | — | nothing |
| branch switch where the path is tracked source | row kept, no section line | git's file | `Source` | nothing |
| `git clean -fdX` | dangling rows | gone | — | nothing |
| crash during a write-back | pending rows | old, new or partial | excluded | the next build settles it; `status` lists it until then |
| index lost | empty | kept | `unrecorded` under a section line, `Source` without one | `codegen import` |
| tracked file inside a new `copy` output | row over a tracked file | overwritten | excluded; git shows it modified | `git rm --cached` |

heph deletes a file only when it wrote those exact bytes: a `written` row, a
fresh uncached hash equal to the record, and either a write-back of the same
target whose current outputs still cover the path, or `codegen discard`. It
never deletes an `imported` row's file on its record (only `discard --force`
does).

### "This file should now be checked in"

1. Remove the output, or the target. Nothing changes yet: the file stays
   excluded and git still ignores it.
2. `heph tool codegen status` lists it:

   ```
   p/foo.pb.go  no longer generated (was //p:gen)  keep: …  discard: …
   ```

   `codegen why`, the fs `file` error and `validate`'s warning all point here.
3. `heph tool codegen keep p/foo.pb.go` drops the row and the section line and
   prints `git add p/foo.pb.go .gitignore`. Or `heph tool codegen discard
   p/foo.pb.go` deletes it.

If you do nothing, the file stays excluded: a consumer that needs it fails at
run with the fs `file` error, and a whole-workspace `validate` fails on the
stale section. A `codegen gitignore` run releases every such file at once and
lists them ("released p/foo.pb.go (was //p:gen): now untracked; keep or
discard it").

## Commands

Every command lives under `heph tool codegen` and takes `--json`. Path
arguments may be relative to the current directory, relative to the
workspace root (`//p/foo.go`), or absolute; a path outside the workspace is
refused. A bare `p/foo.go` typed in a subdirectory that names nothing there,
while `//p/foo.go` exists, is `unknown` with "did you mean //p/foo.go?".

**Exit codes** (these commands differ from the rest of heph, where every error
exits 1, so that `--check` can tell findings from failures):

| Code | Meaning |
| --- | --- |
| 0 | OK |
| 1 | findings (`--check`), or a refused or unknown path |
| 2 | an error, including a usage error (unknown flag, missing argument) |

### `heph tool codegen gitignore [--check] [MATCHER]`

Rewrites the managed section from the target definitions, and nothing else.
Prints one `+` / `-` line per pattern with its target, then each recorded file
the run releases. With a matcher, only lines of targets under it change.
`--check` writes nothing and exits 1 when the section is stale; CI and
`validate` use it. A section with the old marker text is stale with its own
message: "section marker predates heph vX; run heph tool codegen gitignore
once and commit".

```json
{"schema": 1, "stale": true, "reasons": ["lines"], "written": true,
 "lines": [{"op": "add", "pattern": "/p/foo.pb.go", "addr": "//p:gen"}],
 "released": [{"path": "p/old.pb.go", "addr": "//p:gen"}]}
```

`reasons` is a sorted subset of `old_marker`, `lines` and `malformed`, empty
when the section is current. `--check` always reports `"written": false`.

### `heph tool codegen status [--check]`

Lists what needs a decision, one line per file, sorted by path, each ending
with the command that settles it:

| Kind | Meaning |
| --- | --- |
| `no_longer_generated` | a dropped or released row |
| `target_missing` | the row's target no longer exists |
| `modified` | the bytes were edited |
| `interrupted` | a pending row: a write-back was interrupted; rebuild the target |
| `not_in_section` | a recorded file no section line covers: run `codegen gitignore` |
| `foreign_line` | a section line heph did not write |
| `unresolved` | a package whose BUILD file fails to evaluate (named once, with its error) |
| `section_stale` | the section differs from the definitions |

A broken BUILD file never fails `status`. `--check` exits 1 when anything is
listed.

```
{"schema": 1, "items": [{"path", "kind", "addr", "output", "fix"}, ...]}
```

An `unresolved` item has `package` and `error` instead of `path`: branch on
`kind` first.

### `heph tool codegen keep <path>...`

Makes files source from now on. Removes the row, removes the old heph xattr
from the file, drops the section lines that covered the paths (exactly those a
full `codegen gitignore` would drop), lists any other file those lines covered,
and prints `git add <path> .gitignore`. An unrecorded path under a section line
is accepted. A path a current target still declares as a `copy` output is
refused, naming the output and its BUILD file.

### `heph tool codegen discard <path>... | --all [--force] [--dry-run]`

Deletes files that are no longer generated, after a fresh hash equals the
record, and drops their rows and lines as `keep` does. An edited or imported
file needs `--force`. A path a current target still declares is refused
(rebuild instead), and so is a file heph never wrote. `--all` covers every
file `status` lists and drops dangling rows. `--dry-run` prints what it would
do and changes nothing.

`keep` and `discard` print one result per path and exit 1 when any is refused
or unknown:

```
{"schema": 1, "results": [{"path", "result", "reason"}, ...],
 "lines": [{"op": "remove", "pattern", "addr"}, ...], "released": ["p/other.pb.go", ...]}
```

`result` is one of `kept`, `discarded`, `would_discard`, `refused`, `unknown`.
`lines` are the section lines dropped, `released` the other recorded files
they covered.

### `heph tool codegen why <path>...`

Explains any path, sources included: its status and reason, the row (target,
output, state, when it was written, pending or not), how the bytes were
verified (stat, hash, or another casing by inode), and the section line that
matched with its line number and whether heph wrote it, or "no section line
matches". Exits 1 when a path is unknown.

```
{"schema": 1, "hint": "...",
 "results": [{"path", "result", "reason", "status", "status_reason",
              "row": {"path", "addr", "output", "state", "written_at", "pending", "verified"},
              "section_line": {"line", "pattern", "heph_wrote", "negated"}}, ...]}
```

`result` is `explained`, `unknown` or `refused` (outside the workspace).
`status` is `generated`, `modified`, `released` or `source`; `status_reason`
is `recorded`, `pending`, `unrecorded` or `edited`, or null. `row.state` is
`written`, `imported` or `dropped`; `row.verified` is `stat`, `hash`,
`pending`, `missing` or `another_casing`. `row` and `section_line` are null
when there is none; `section_line.negated` is true for a `!` line that
re-includes the path. `hint` (also on `status`) is present only when the
index is empty but section lines match files on disk.

### `heph tool codegen import`

Records the files an older heph stamped with the `user.heph.codegen` xattr,
as `imported` rows (see Upgrading). Idempotent; exits 0 when it recorded
nothing new.

```
recorded 412 generated files from the previous heph's xattrs; 3 skipped, 0 unchanged
```

```json
{"schema": 1, "recorded": 412, "skipped": 3, "unchanged": 0}
```

A file whose xattr value is empty or not a target address is `skipped` with a
warning; a section line still covers it as `unrecorded`. Removed in v1.1.0.

### `heph tool gen-gitignore` (removed)

Fails with exit 2: "heph tool gen-gitignore was removed; use heph tool codegen
gitignore (and --check for the up-to-date test)". With `--json`:
`{"schema": 1, "error": "command_removed", "use": "heph tool codegen gitignore"}`.

### JSON

Every `--json` output is `{"schema": 1, ...}` with sorted arrays and a `kind`
or `result` enum. A new field is additive; a renamed or removed field bumps
`schema`. Consumers must ignore fields they do not know, and treat a `kind`,
`result`, `reason`, `status`, `status_reason` or `state` value they do not
know as opaque: a new value is additive too.

A failure (exit 2) prints `{"schema": 1, "error": "<message>"}` on stdout and
the message on stderr. A usage error is clap's, before any command runs: it
prints nothing on stdout. The removed `gen-gitignore` prints its own document
(above).

## Upgrading from the xattr

Earlier versions marked each written-back file with a `user.heph.codegen`
xattr, which `cp`, editors that save through a rename and filesystems without
xattrs lose, and which nothing could list. This version reads only the index.

**After upgrading, run `heph tool codegen import` once in each checkout.** It
walks the tree once (about half a second on a 31,000-file workspace) and
records every stamped file. Nothing runs it for you. Until it runs, a file the
old heph wrote reads as `Generated` (`unrecorded`) where a section line covers
it, and as source where none does. `codegen status` and `codegen why` print a
hint when the index is empty but section lines match files on disk.

Until v1.1.0 write-backs still *set* the xattr (no lookup reads it), so
rolling back to the previous version still excludes what this one wrote. After
a round trip in which the previous version wrote new files, run
`heph tool codegen import` again. Running two versions against one checkout
is unsupported.

## Release notes

- **`heph tool gen-gitignore` is removed**, with no alias. Use
  `heph tool codegen gitignore`, and `heph tool codegen gitignore --check` for
  the up-to-date test. The old name exits 2 with a message naming the new one;
  scripts and CI steps that ran it go red until updated.
- **New BEGIN marker text.** The managed section's BEGIN line now names
  `heph tool codegen gitignore`. `validate` and `--check` report a section with
  the old text as stale (`old_marker`): run `heph tool codegen gitignore` once
  and commit. A previous version running `gen-gitignore` writes the old text
  back.
- **`validate`'s stale-section message changed** to "… is out of date; run
  heph tool codegen gitignore", and `validate` now also fails on a section
  with the old marker text (`old_marker`) or a malformed one (`malformed`: a
  BEGIN or END marker without its pair, or out of order). Anything matching
  the old text must match the new.
- **After upgrading, run `heph tool codegen import` once in each checkout.**
  It records the files the previous version stamped with the xattr. Until it
  runs, such a file is excluded only where a section line covers it.
- **An fs `file` input of a generated path fails at run.** A target that
  reads a recorded generated file through `file(...)` (or an fs `file` addr)
  fails, naming the generator. Migration: depend on the generator target, not
  the file. Whether it fails depends on the checkout's history: a laptop that
  built the generator fails, while a fresh CI checkout that has not built it
  passes.
- **A Go compile that names a generated file directly fails** with the same
  error, instead of silently compiling without it. Reach the file through its
  generator target.
- **New `format` targets in `heph query`** for Go packages whose only `.go`
  files are generated. They do nothing.
- **A Go package whose directory path contains `\`** can no longer get a
  `format` target that runs (the glob syntax cannot express a literal `\`; Go
  refuses `\` in import paths).
- **`introspect:outputs` over a `copy` target's outputs** now expands to the
  `copy_output` fs target.
- **Rollback and mixed versions are unsupported.** Until v1.1.0 the xattr is
  still set, so a rollback keeps excluding generated files; after one, rerun
  `heph tool codegen import`.
- **v1.1.0** removes the xattr, `heph tool codegen import` and its hint, and
  raises the plugin ABI (`ABI_SEMVER` and `MIN_PLUGIN_ABI`) to the next minor
  above the ABI this release ships (0.20.0 at the time of writing), so every
  cdylib plugin built before it, this release's included, is refused and must
  be rebuilt.
