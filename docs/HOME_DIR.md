# The heph home

The home is where a workspace's local state lives: the cache, sandboxes, locks,
credentials, scratch caches and diagnostics. It is `<root>/.heph` unless the
config file's `homeDir` says otherwise. A relative `homeDir` is joined onto the
workspace root, and an absolute one is used as written.

The homes are decided in one place, `Homes::resolve` in
`crates/config/src/homes.rs`. It builds each home with `HomeDir::resolve`
(`crates/config/src/home_dir.rs`), which refuses a home that is its root or
above it. No other code joins a home name onto the root. heph writes a
`.gitignore` of `*` into each home it uses (both, in a linked worktree), so a
home never shows up in `git status`.

## Worktrees

When heph runs inside a linked git worktree (`git worktree add`), it uses the
**main checkout's** home. Every worktree of a repository then reads and writes
one cache, so a target built in one checkout is a cache hit in all the others.

```yaml
# .hephconfig: give each checkout its own home instead
worktree:
  shareHome: false
```

`shareHome` decides whether *this* checkout uses another's home. A main
checkout's own `shareHome: false` does not stop its worktrees from using its
home: to opt out, set it where the worktrees read it (normally the committed
config, which every checkout has).

Detection reads files and never runs `git`. It behaves the same on linux and
macOS, x86_64 and aarch64: it is file reads and one `access(2)` call. From the
workspace root (with symlinks resolved), heph looks for the nearest `.git`:

- A **directory** means a main checkout, so there is nothing to share from.
- A **file** names the worktree's git dir (`gitdir: …`). That dir's `commondir`
  names the repository. The main checkout is the parent of a common dir named
  `.git`.

The nearest `.git` wins even when it is broken: a dangling `.git` symlink is a
fallback (below), not a reason to read an outer repository.

heph then maps the workspace root to the same relative path in the main
checkout. For example, a root at `<worktree>/sub` uses `<main>/sub/.heph`. The
shared home is where the **main checkout's** config puts it, so the main
checkout must be a heph workspace whose `homeDir` resolves to the same place.

heph keeps the checkout's own home, without failing the run, when:

- the root is in a submodule (there is no `commondir`), or in a worktree of a
  submodule;
- the repository is bare (the common dir is not named `.git`);
- the root has no counterpart in the main checkout;
- the main checkout's counterpart is this very root (through a symlink);
- the main checkout's counterpart is not a heph workspace (no `.hephconfig`),
  its config fails to load, or its `homeDir` differs from this checkout's;
- the main checkout's home (or, before it exists, its nearest existing parent)
  is not writable;
- the `.git` file or `commondir` is malformed or dangling;
- the worktree sits inside the home it would share;
- `homeDir` is absolute, in which case it is used as written and no detection runs.

heph logs `using shared home <path> (linked worktree <name> of <main>)` at
`info` once at startup when it shares. A fallback is logged at `debug`
(`HEPH_LOG=debug`), except a malformed or dangling `.git` or `commondir`, which
is a `warn`: a checkout is there, heph just cannot read it.

### What stays per checkout

Even with a shared home, these live under the checkout's own `<root>/<homeDir>`,
because they belong to one working tree:

| Path | Why |
|---|---|
| `sandbox/` and `sandboxfuse<pid>/` | A sandbox is a working copy of this checkout's inputs. `WORKSPACE_ROOT` and the sandbox paths are exactly what they are without sharing. |
| `stage/` | Read-only inputs are staged once, then hardlinked or symlinked into sandboxes. A hardlink from the shared home would fail (EXDEV) when a worktree is on another filesystem. Rule: a sandbox only links to things in its own checkout home. Reading or copying from the shared cache is fine. |
| `lock/<addr>.execute.lock` | Guards the sandbox path, so each checkout can run its own copy of a target. The gateway, revision and rebuild locks (`.outer.lock`, `.inner.lock`, `.rebuild.lock`) guard the shared cache entry, so they stay in the shared home. |
| `cache/cache.db` and `cache/blobs/` **for `remote: false` targets** | A target that never goes to a remote cache (`cache = {"remote": False}`, and every runner target: devenv, nix, `oci_runner`) is one whose bytes may name this checkout's absolute paths (a `runner.json`'s `cwd` or `mounts`), under a key every checkout computes the same. Shared, a worktree would run in the main checkout's paths. So these entries live in a store of the checkout's own, opened the first time such a target is built. Lookup and write pick the store by the same rule from the target's def. |
| `cache/fswalk.db`, and the Go plugin's `heph-plugin-go-fswalk.db` | Caches this tree's directory listings. A plugin's `home` (`CreateConfig.home` for a cdylib, `PluginInit.home` in process) is the checkout's home. |
| `approval/` | Two checkouts prompting at once must not overwrite each other's notice. |
| `nix-driver/nix-gcroots/` | One gcroot per cached revision of a nix target, keeping its store paths from `nix-collect-garbage`. A nix target is `remote: false`, so its revisions are in the checkout's own store, and its roots sit next to them. `heph tool gc` removes the roots of revisions this checkout no longer caches (below). |

Everything else lives in the shared home: the cache store of every
remote-eligible target (`cache/cache.db`, `cache/blobs/`), `cache/remote-tmp/`,
`auth/`, `scratch/`, the gateway, revision and rebuild locks, and `diag/`.

The OCI runner mounts the workspace tree, the checkout's home (its sandboxes)
and the shared home's `scratch/` into its container — only `scratch/`: a
sandbox's scratch mounts are symlinks into it, and nothing else in a sandbox
points into the shared home (staged inputs are in the checkout's home,
presented credential files in the sandbox). Outside a linked worktree
`scratch/` is inside the one home, so the mounts are the tree and the home. A plugin gets the shared home as `CreateConfig.shared_home` /
`PluginInit.shared_home`.

### Costs of sharing

- **One target, one build at a time, across checkouts.** The gateway lock is in
  the shared home, so a build of `//p:a` in one checkout waits for a build of
  `//p:a` in another — whichever revision each is building. Different targets
  never wait on each other.
- **The shared cache grows with the number of worktrees, up to 4×.** The
  shared store holds every checkout's revisions of a target, so when the home
  is shared it keeps up to `history × min(checkouts, 4)` revisions per target
  instead of `history`. `checkouts` is the main checkout plus each linked
  worktree that still exists on disk (a registered worktree whose tree is gone
  does not count), counted once when heph starts. Two checkouts building
  different revisions of one target (a branch that changed its sources) then
  keep both rather than evicting each other's. The cap of 4 covers the main
  checkout plus a few branches in flight, and keeps disk bounded when a
  repository has many worktrees (agents spawn dozens); past 4 checkouts,
  worktrees building different revisions of one target evict each other again.
  The checkout's own store (`remote: false` entries) keeps plain `history`, and
  so does an unshared home.
- **A `remote: False` target is stored once per worktree.** Its entries are
  per checkout (above), so N checkouts that build it hold N copies: N × its
  output size, and N builds.
- **A worktree's nix gcroots are swept only by that worktree's `gc`.** They
  live in its own home, next to the revisions they pin; a `gc` in the main
  checkout or another worktree does not see them. They go away with the
  worktree's directory (`git worktree remove`), after which
  `nix-collect-garbage` can reclaim their store paths.
- **Detection costs a few syscalls per registered worktree.** Every heph
  process reads `.git/worktrees/` and, per registered worktree, its `gitdir`
  file and a stat of the path it names: about 4–5 syscalls each. Negligible
  for a handful; it grows linearly with stale registrations, which
  `git worktree prune` removes.
- **A worktree on another filesystem** pays a byte copy for each blob it writes
  into the shared cache: the write is a rename where the filesystems agree and
  falls back to a copy where they do not.
- **Credentials are shared too.** The token cache in `auth/` is keyed by the
  credential's address, its source declaration and its parents' keys — not by
  the checkout. An `exec` source whose relative script differs between two
  branches is served the same cached token in both. See
  [CREDENTIALS.md](CREDENTIALS.md).

### Why is my cache cold?

Check, in order:

1. **`homeDir` is absolute.** Detection is off; every checkout that names the
   same directory shares it, and nothing else does.
2. **`worktree.shareHome: false`** in the config this checkout reads.
3. **A fallback.** Run with `HEPH_LOG=debug` and look for `heph home:` — the
   reason is one of the list above (not a heph workspace in the main checkout,
   a different `homeDir`, an unwritable main home, a submodule, a broken
   `.git`, …).
4. **The target is `remote: false`.** Its entries are per checkout (above), so
   the first build in each worktree is a miss by design.
5. **`cache.history` evicted it.** The shared store keeps
   `history × min(checkouts, 4)` revisions of a target (above), so this takes
   more new revisions than that budget across all checkouts: one checkout
   building several revisions in a row, or more than 4 checkouts building
   different revisions of the target.
6. **An `oci_runner` consumer.** Its key includes host paths (the runner's
   mounts carry the checkout's absolute paths), so it differs per checkout.
   This predates sharing and is known.

### `heph tool gc`

`gc` normally drops every cached target that no longer resolves. With a shared
home, a target that is missing in this checkout may be defined on another
worktree's branch. So `gc` **skips the orphan sweep of the shared store**
whenever the home is or may be shared:

- it runs in a linked worktree that uses the main checkout's home, or
- it runs in a main checkout whose repository has any linked worktrees
  (`.git/worktrees/*`), whatever its own `shareHome` says.

History trimming (`cache.history`, scaled in the shared store by the number
of checkouts, at most 4×; see "Costs of sharing") still runs. The checkout's own store
(`remote: false` entries) belongs to this checkout alone, so it always gets the
full sweep. `gc` prints why it skipped, e.g. `Orphan sweep skipped: the home is
shared with 2 linked worktree(s), so 3 target(s) that do not resolve here were
kept. 1 registered worktree(s) no longer exist; run `git worktree prune`. To
give each checkout its own home, set `worktree.shareHome: false`.` A worktree
that was deleted without `git worktree prune` still counts, so prune stale
worktrees to bring the orphan sweep back. `heph clean` cleans a target in both
stores.

An absolute `homeDir` turns detection off, so `gc` treats the home as unshared
and runs the full orphan sweep. If several checkouts point an absolute `homeDir`
at one directory, a `gc` in any of them drops the targets that only the others
define. Use a relative `homeDir` (shared through detection) or
`shareHome: false` if checkouts need their own caches.

After the trim, `gc` removes the nix gcroot of every revision it no longer
caches, so `nix-collect-garbage` can reclaim the store paths. The nix driver
keeps one root per cached revision, with a sidecar file naming that revision,
in `<checkout home>/nix-driver/nix-gcroots/`. A root is kept while its
revision is in either of this checkout's stores, while its target is being
built, while its revision is being read, and for an hour after its sidecar
was written (a build releases its locks before its cache write has landed on
disk, so a younger root may belong to a revision about to appear). A root with
no sidecar names no revision and is never removed.

Roots written before this scheme are named `<16-hex addr hash>` alone, with
no `.rev` sidecar, in the same `nix-driver/nix-gcroots/` of the home heph
used then (each checkout had its own). `gc` never removes them. Deleting them by hand lets `nix-collect-garbage`
collect their store paths. A target whose cached revision used such a path
then does **not** rebuild on its own: the cache hit serves the cached
wrapper, a `#!/bin/sh` script that `exec`s the store path, and running it
fails loudly (`exec: /nix/store/…: not found`, exit 127). Clean the target
(`heph tool clean //pkg:target`) or run with `--force` to rebuild it.

## `${git:branch}`

A scope that names `${git:branch}` reads the branch of the checkout found by
the same detection: the nearest `.git` at or above the workspace root. In a
linked worktree that is the worktree's own `HEAD`. A workspace nested inside an
outer repository without a `.git` of its own gets the **outer** repository's
branch.
