# The heph home

The home is where a workspace's local state lives: the cache, sandboxes, locks,
credentials, scratch caches and diagnostics. It is `<root>/.heph` unless the
config file's `homeDir` says otherwise. A relative `homeDir` is joined onto the
workspace root, and an absolute one is used as written.

The homes are decided in one place, `Homes::resolve` in
`crates/config/src/homes.rs`. It builds each home with `HomeDir::resolve`
(`crates/config/src/home_dir.rs`), which refuses a home that is its root or
above it. No other code joins a home name onto the root. Each home writes a
`.gitignore` of `*` into itself.

## Worktrees

When heph runs inside a linked git worktree (`git worktree add`), it uses the
**main checkout's** home. Every worktree of a repository then reads and writes
one cache, so a target built in one checkout is a cache hit in all the others.

```yaml
# .hephconfig2: give each checkout its own home instead
worktree:
  shareHome: false
```

Detection reads files and never runs `git`. From the workspace root (with
symlinks resolved), heph looks for the nearest `.git`:

- A **directory** means a main checkout, so there is nothing to share from.
- A **file** names the worktree's git dir (`gitdir: …`). That dir's `commondir`
  names the repository. The main checkout is the parent of a common dir named
  `.git`.

heph then maps the workspace root to the same relative path in the main checkout.
For example, a root at `<worktree>/sub` uses `<main>/sub/.heph`.

heph keeps the checkout's own home, with no warning, when:

- the root is in a submodule (there is no `commondir`);
- the repository is bare (the common dir is not named `.git`);
- the root has no counterpart in the main checkout;
- the `.git` file or `commondir` is malformed or dangling;
- the worktree sits inside the home it would share;
- `homeDir` is absolute, in which case it is used as written and no detection runs.

Run with `HEPH_LOG=debug` to see which home was chosen and why. heph logs
`heph home: shared from <main> (linked worktree <name>; …)` or
`own home (<reason>)`.

### What stays per checkout

Even with a shared home, these live under the checkout's own `<root>/<homeDir>`,
because they belong to one working tree:

| Path | Why |
|---|---|
| `sandbox/` and `sandboxfuse<pid>/` | A sandbox is a working copy of this checkout's inputs. `WORKSPACE_ROOT` and the sandbox paths are exactly what they are without sharing. |
| `lock/<addr>.execute.lock` | Guards the sandbox path, so each checkout can run its own copy of a target. The gateway and revision locks (`.outer.lock`, `.inner.lock`) guard the shared cache entry, so they stay in the shared home. |
| `cache/fswalk.db`, and the Go plugin's `heph-plugin-go-fswalk.db` | Caches this tree's directory listings. A plugin's `home` (`CreateConfig.home` for a cdylib, `PluginInit.home` in process) is the checkout's home. |
| What the OCI runner mounts | The checkout's home, because this checkout's sandboxes are there. |
| `approval/` | Two checkouts prompting at once must not overwrite each other's notice. |

Everything else lives in the shared home: `cache/cache.db`, `cache/blobs/`,
`cache/remote-tmp/`, `stage/`, `auth/`, `scratch/`, the gateway and revision
locks, `nix-gcroots`, and `diag/`.

### `heph tool gc`

`gc` normally drops every cached target that no longer resolves. With a shared
home, a target that is missing in this checkout may be defined on another
worktree's branch. So `gc` **skips the orphan sweep** whenever the home is or may
be shared:

- it runs in a linked worktree that uses the main checkout's home, or
- it runs in a main checkout whose repository has any linked worktrees
  (`.git/worktrees/*`).

History trimming (`cache.history`) still runs. `gc` prints the reason
(`Orphan sweep skipped (N target(s) that do not resolve here kept): home is …`).
A worktree that was deleted without `git worktree prune` still counts, so prune
stale worktrees to bring the orphan sweep back.

An absolute `homeDir` turns detection off, so `gc` treats the home as unshared
and runs the full orphan sweep. If several checkouts point an absolute `homeDir`
at one directory, a `gc` in any of them drops the targets that only the others
define. Use a relative `homeDir` (shared through detection) or
`shareHome: false` if checkouts need their own caches.
