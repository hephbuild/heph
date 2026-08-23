# Codegen provenance: replacing the xattr with a per-directory `.hephgen` registry

Status: §2–§5 and §7–§10 are implemented. §6's orphan handling is **not**: a
record whose owning target was deleted still hides its file (which is the
behaviour the xattr had, and the safe direction — an unreconciled orphan is never
promoted back into a compiled input), and reclamation lands with the end-of-run
reconciliation in a follow-up. `heph tool migrate-codegen` (§8) is likewise still
to come; until it exists the xattr fallback stays on.

Scope: how heph decides that a file sitting in the source tree is a `codegen =
"copy"` output rather than raw source — and therefore must be excluded from
`glob()` / `file()`, and must not be clobbered by an `in_place` target.

## 1. Problem

`user.heph.codegen` (`crates/builtins/src/pluginfs/mod.rs:37`) is the authority
for four decisions:

| Decision | Site |
|---|---|
| Exclude from a glob walk | `crates/builtins/src/pluginfs/mod.rs:843` |
| `file()` resolves to nothing | `crates/builtins/src/pluginfs/mod.rs:1085` |
| Skip in the go provider's raw dir scan | `crates/plugin-go/src/plugingo/provider.rs:2979` |
| `in_place` must not clobber a copy-owned file (frozen check + write-back) | `crates/engine/src/engine/result.rs:2834`, `:2915` |

`has_codegen_xattr` treats *any* failure as "not stamped"
(`pluginfs/mod.rs:46`), so a stripped attribute does not fail — it silently
produces a **wrong build**: the generated file becomes visible raw source, gets
double-sourced into its consumer, and for a `copy` target whose own package is
globbed it folds its output back into its input hash.

The underlying defect is that the record's lifetime is decoupled from the file's.
`git checkout`, `tar` / `actions/cache` round-trips, `rsync` without `-X`, `cp`
without `-p`, editor atomic-save, and any filesystem without user xattrs all
preserve the bytes and drop the metadata.

Two secondary costs:

- `stamp_codegen_xattr` (`result.rs:878`) **hard-fails the build** on a
  filesystem with no xattr support, making `codegen = "copy"` silently
  non-portable.
- Restamping a stripped file changes glob membership but does not set `wrote`
  (`result.rs:2776` says so explicitly), so the fixpoint recompute is skipped on
  exactly the run that repaired the damage.

## 2. Design

Every directory that receives net-new `copy` outputs gets a `.hephgen`, written
by the write-back from the target's own declared output paths — `O(files
written)`, no graph construction, and no user action at any point.

The record lives in the same directory as the files it describes, so `cp -r`,
`mv`, `tar`, `rsync`, docker `COPY` and `git clean -xd` move or delete both
together. That coupling is the whole point: it is what the xattr failed to
provide, and it is why a repo-level store in `.heph3` was rejected (see §11).

## 3. Format

Line-oriented, tab-separated, sorted by name. Describes only its own directory —
never a nested path — so directory-local reasoning holds and moving a directory
moves its truth with it.

```
heph-codegen 1
bar.pb.go	//pkg:gen	h=9a1c0f4e…
foo.pb.go	//pkg:gen	h=8f2a77b1…	prev=1c04dd90…
gen	//pkg:gendir	->../../.heph3/cache/blob/…
```

- **Column 1 is always the entry name.** Declared as a format invariant so a
  future version stays readable by an older binary — it degrades to names-only
  (no hash verification) instead of mis-sourcing.
- Column 2 is the owning target's addr. Lookups are owner-aware, which makes the
  two write-back guards more precise than the xattr allowed: "owned by a copy
  target *other than me*" rather than "stamped by someone".
- `h=` is exactly the `hashout` string `CachedWalker::file_hash` already produces
  and caches (`file_hashout` folds the exec bit in), so verification is string
  equality against a value the walk computed anyway. An exec-bit flip counts as
  divergence.
- `prev=` is present only inside a rewrite window (§5).
- A symlink entry (a `Content::DirPath` copy output, materialized as a symlink
  into `.heph3`) carries `->target` in place of the hash.
- `.hephgen` starts with `.heph`, so the existing name rule in
  `entry_resolves_into_heph_dir` (`pluginfs/mod.rs:96`) already keeps it out of
  globs. No self-reference trick needed.

The format carries a version line and is therefore a `compatibility` surface.

## 4. Read path

Fold the parsed entries into `DirListing` and bump `DIR_LISTING_VERSION`
(`crates/walk/src/cached_walker.rs:71` exists for exactly this class of change —
"which entries go in a listing").

- Steady state: **zero** cost beyond the listing fetch that already happens; the
  registry rides inside the same cached, mtime-validated blob.
- Directories with no `.hephgen`: **zero** cost — the absence is visible in the
  listing already in hand.
- Cold: one small read + parse per directory that has one.
- Per entry: a hash lookup, replacing **a `getxattr` syscall per globbed file**.

## 5. Write path — two invariants

**Register before publish.** Per directory: update `.hephgen` first, then write
the files (tmp + rename in the same directory, tmp name prefixed `.heph` so a
concurrent walk skips it). Any walker that can see the file already sees it
registered; registered-but-absent matches nothing and is inert. Today's ordering
is the unsafe one — the bytes land at `result.rs:2969` and the stamp goes on
afterwards.

**`prev=` covers rewrites.** Net-new files are safe by ordering alone. A rewrite
is not: between the entry update and the rename, disk holds the old bytes while
the registry names the new hash, and a concurrent walk would see a mismatch and
source the file. So the update writes `h=new prev=old`, the rename lands, and a
follow-up write drops `prev`. On-disk content matches *some* accepted hash at
every instant.

Mechanics:

- The update is a read-modify-write: `entries owned by other targets ∪ what I
  emit now`. A target's own stale entries are dropped; another target's are
  preserved.
- Serialized by an `hlock` flock on the registry file, committed by atomic
  rename. Contention is sharded per directory and arises only when the *set* of
  names changes, not on every regeneration.
- Writing `.hephgen` bumps the directory mtime, which invalidates the cached
  listing the registry rides in. **Cache coherence is free**, cross-process
  included — there is no snapshot, generation counter, or invalidation protocol
  to get wrong.

One existing hole closes here: the write-back must return "registry changed" and
OR it into `wrote`, because registration can change glob membership with no bytes
changing.

## 6. Deleted targets and orphans

| Case | Handling |
|---|---|
| Target exists, stopped emitting a file | The RMW drops its own stale entry on the next run. Free, no detection needed. |
| Target deleted or renamed | End-of-run reconciliation (below). A rename largely self-heals: the new target re-registers the same paths under its own addr. |
| The path became real source (branch switch, hand-written) | Content does not match `h=` ⇒ not heph's file ⇒ treated as source immediately. No lag, no graph, plus a warning naming the registered owner. |
| Registry deleted by hand | Files go visible; the next run of the owner re-registers them. Recovery equals today's, but requires a deliberate `rm`. |

**End-of-run reconciliation**, bounded by what the run actually read: every
registry a walk parsed contributes its owner addrs to a per-run set. After the
graph work completes — so it is not re-entrant, since resolving a target
evaluates a BUILD file, which calls `glob()`, which is what read the registry —
`get_def` each distinct owner. Nearly all are already memoized by the run itself.
A `TargetNotFoundError`, or a def that no longer declares the path, is the orphan
signal.

**Orphan policy.** An unreconciled orphan **stays hidden** — it is never silently
promoted into a compiled input. Then:

- **Reclamation is on by default.** Delete the file when, and only when, its
  content still matches `h=` — proof the bytes are heph's own untouched output,
  containing nothing a human wrote, and reproducible by definition. Print what
  was removed.
- **Content diverged ⇒ never delete.** Report it; that file holds something heph
  did not put there.
- `heph validate` fails on orphans in CI, using the full graph it already builds.

## 7. Git noise, and writing to `.git/info/exclude`

`.hephgen` is untracked, so by default it appears in `git status` — landing on
exactly the friction that ruled out making the committed root `.gitignore` the
authority, since ignoring it via `heph tool gen-gitignore` is a manual step.

**Decision: on the first creation of a `.hephgen` in a run, append
`**/.hephgen` to `.git/info/exclude`, once, idempotently. On by default, with a
config kill-switch.** That file exists precisely for local, uncommitted,
workflow-specific ignores: nothing is committed, no diff appears, nobody reviews
anything, and a repo with no copy codegen is never touched.

Implementation notes:

- Resolve the gitdir **without requiring the `git` binary**: `.git` may be a
  *file* containing `gitdir: …` (linked worktrees — how this repo is developed),
  and from that gitdir read the `commondir` file to reach the shared
  `info/exclude`.
- Check for the exact line before appending; write it under a one-line marker
  comment.
- Skip silently when there is no `.git`, or the file is not writable.
- Config kill-switch for users who refuse any write under `.git/`.

Where the line is drawn, and why:

- The **generated files'** ignore status is a repo-wide fact and belongs in a
  committed `.gitignore` — which `heph tool gen-gitignore` emits and `heph
  validate` enforces (`src/commands/validate.rs:180`).
- `.hephgen` is heph machinery that exists only where heph has run — a per-clone
  fact, which is what `info/exclude` is for.
- For teams that want it committed anyway, `gen-gitignore` also emits a static
  `**/.hephgen` line into its managed section; the local exclude covers the
  interim.

This is the one part of the plan that touches a directory heph does not own. It
is append-only, once per repo.

## 8. The xattr: kept as a fallback for one release

**Decision: do not delete the xattr in this change.**

- **Read** — a per-file fallback: when a name is not listed in its directory's
  `.hephgen`, consult `getxattr` as today. Per-file rather than per-directory,
  because the RMW only creates entries for the target that ran, so a legacy
  file's entry does not appear until its own target runs again — a directory can
  legitimately hold both a registered and a legacy-stamped file.
- **Write** — keep stamping, but **best-effort**: downgrade `stamp_codegen_xattr`
  (`result.rs:878`) from a hard error to a warn-once. This makes
  `codegen = "copy"` work on xattr-less filesystems immediately, and keeps a
  mixed fleet safe — a tree written by a new binary is still readable by an old
  one.
- **Cost, stated plainly**: the `getxattr` syscall survives for every unlisted
  file, which is nearly every file, so the read-path perf win does not land until
  the fallback is removed.
- **Exit**: `heph tool migrate-codegen` walks the tree once, converts stamps into
  `.hephgen` files, and records a flag in `.heph3` that disables the fallback;
  the fallback and the stamping are deleted one release later. Track with a
  `TODO(#issue)` on both sites.

## 9. Call sites to change

- `crates/builtins/src/pluginfs/mod.rs:843`, `:1085` — owner-aware lookup.
- `crates/plugin-go/src/plugingo/provider.rs:2979` — same; the registry reaches
  the go cdylib through the options channel that already carries `walk_db`
  (`crates/plugin-go-cdylib/src/lib.rs:72`).
- `crates/engine/src/engine/result.rs:2834`, `:2915` — "owned by a copy target
  other than me".
- `crates/engine/src/engine/result.rs:2767` (`materialize_codegen_tree`) —
  registry RMW, ordering, `prev=`, and the `wrote` fix.
- `crates/walk/src/cached_walker.rs` — `DirListing` carries the parsed section;
  `DIR_LISTING_VERSION` bump.
- `resolves_into_heph_dir` and the dir-output symlink path are untouched.

## 10. Tests

Unit:

- Format round-trip, including `prev=` and symlink entries.
- Forward compatibility: a `heph-codegen 2` file with extra columns still yields
  names.
- RMW preserves another target's entries and drops the running target's stale
  ones.
- `.git/info/exclude` append is idempotent and worktree-aware (`.git` as a file →
  `gitdir:` → `commondir`).

`crates/e2e` (nothing here meets the `crates/bin-e2e` bar — no `dlopen`, no PTY,
no exit code):

- The regression itself: a `copy` output stays excluded from globs and its
  consumer's input hash is unchanged, with no xattr present anywhere.
- An `in_place` target refuses to clobber a copy-owned file with no xattr.
- Content divergence flips a registered path back to source.
- An orphan stays hidden, and is reclaimed only on a hash match; a diverged
  orphan is reported and never deleted.
- Registry lost ⇒ next run of the owner re-registers.

## 11. Alternative considered: one sqlite table per repo

A `.heph3/codegen.db` maintained by the write-back was rejected.

It wins on tree cleanliness, whole-repo queries (a one-`SELECT` orphan sweep),
and wide multi-directory writes (one transaction vs. N flock+rename cycles). It
loses on the axis the feature exists for: it reintroduces the xattr's defect in a
narrower form. `rsync --exclude='.*'`, a `tar` of the source, a container build
that COPYs source only, a worktree copied to another machine, or someone deleting
`.heph3` to reset all leave generated files with no registry — and the failure is
**correlated**, taking out every file's provenance at once, with a recovery window
lasting until each owning target happens to run again.

It also needs a coherence protocol that per-directory files get for free from the
directory mtime (snapshot + `PRAGMA data_version` / generation counter, or
explicit glob-memo invalidation), serializes every write-back in the repo behind
sqlite's single writer, and — unlike its disposable neighbour `fswalk.db`, which
runs `synchronous = OFF` (`cached_walker.rs:507`) — is not reconstructable, so it
would have to pay real fsyncs.

If the orphan sweep ever gets slow, the hybrid is cheap: keep `.hephgen`
authoritative and add a derived sqlite index in `.heph3` purely as an
accelerator, rebuildable from a walk and never consulted for correctness. Not
worth building now.

## 12. Review-board gates

Per `CLAUDE.md`:

- `hermeticity` and `compatibility` at **design** time — the exclusion feeds
  input hashes and cache keys, and `.hephgen` is a new versioned on-disk format.
- `feature-quality` on corner cases and per-target cost.
- `code-quality` before commit.
- `perf-measurement` after implementation, on the glob walk.
