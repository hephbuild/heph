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

Everything else lives in the shared home: the cache store of every
remote-eligible target (`cache/cache.db`, `cache/blobs/`), `cache/remote-tmp/`,
`auth/`, `scratch/`, the gateway, revision and rebuild locks, `nix-gcroots`
(one root per target revision, so two checkouts do not replace each other's),
and `diag/`.

The OCI runner mounts the workspace tree, the checkout's home (its sandboxes)
and the shared home (a sandbox's scratch mounts are symlinks into it) into its
container. A plugin gets the shared home as `CreateConfig.shared_home` /
`PluginInit.shared_home`.

### Costs of sharing

- **One target, one build at a time, across checkouts.** The gateway lock is in
  the shared home, so a build of `//p:a` in one checkout waits for a build of
  `//p:a` in another — whichever revision each is building. Different targets
  never wait on each other.
- **`cache.history` counts across checkouts.** It is per target in the shared
  store. At the default of 1, two checkouts building different revisions of one
  target (a branch that changed its sources) keep evicting each other's, and
  each rebuilds on its next run. Raise `history` for targets you build on
  several branches at once.
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
4. **The old `.heph3` home.** The default home moved from `.heph3` to `.heph`;
   heph hints at a leftover `.heph3` in the checkout's root. Its cache is not
   read.
5. **The target is `remote: false`.** Its entries are per checkout (above), so
   the first build in each worktree is a miss by design.
6. **`cache.history` evicted it.** Another checkout built a different revision
   of the same target (above).
7. **An `oci_runner` consumer.** Its key includes host paths (the runner's
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

History trimming (`cache.history`) still runs. The checkout's own store
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

## `${git:branch}`

A scope that names `${git:branch}` reads the branch of the checkout found by
the same detection: the nearest `.git` at or above the workspace root. In a
linked worktree that is the worktree's own `HEAD`. A workspace nested inside an
outer repository without a `.git` of its own gets the **outer** repository's
branch.
