# Scratch caches

A **scratch** is a mutable, non-hermetic directory that a target declares, keeps
between runs, and shares with every other target referencing it. It is the one
thing inside a sandbox that is neither an input nor an output.

Everything else heph gives a target is hermetic: declared inputs in, declared
outputs out, sandbox destroyed. A scratch is the deliberate exception, and it
exists because the hermetic default is expensive for a tool that maintains its
own cache — a compiler cache, a package download cache, a content-addressed blob
store.

## The contract

> A target must produce identical outputs whether its scratch directories are
> warm, cold, or absent. Losing one is always a slowdown, never a wrong answer.

Everything below follows from that sentence.

- **Nothing about a scratch enters `hashin`** — not its contents, not its
  declaration. Contents would mean the cache never hits; the declaration would
  mean bumping a cache's `version` rebuilds the world for no correctness gain.
- **A scratch is only touched on a miss.** A cache hit takes no lock, restores
  nothing, saves nothing, costs nothing.
- **The invariant cannot be verified in general**, so the system owes an audit
  rather than a proof. `--no-scratch` is that audit.

The contract holds for content-addressed, self-verifying caches — Go's build
cache, `ccache`, `sccache` — because a stale or foreign entry is simply not
found. It does **not** hold for a directory used as durable state: a database, a
counter, an output staging area. Those are not scratch caches, and nothing here
makes them safe.

## Model

| | |
|---|---|
| **declaration** | a target with `driver = "scratch"`. Describes a cache; builds nothing. |
| **reference** | `scratch = ["//pkg:name"]` on a consuming target. A graph edge. |
| **slot** | the directory's identity: `H(addr, version)`. |
| **lineage** (scope) | one head per branch within a slot, so branches do not fight. |

A slot is shared by every target referencing the declaration. Two declarations
resolve to the same directory **if and only if** they agree on address and
`version`.

### Identity

```
slot = H(SLOT_FORMAT, addr, version)
```

heph contributes no dimension of its own. It does not fold in the host OS or
architecture, because it cannot know whether a given cache depends on them, and a
closed set of guesses could never express a toolchain release, a target triple or
a set of build tags anyway. The author states what the contents depend on:

```python
version = heph.core.os() + "/" + heph.core.arch()   # host-specific
version = goos + "/" + goarch + "/" + go_version    # target-specific
version = ""                                        # portable (the default)
```

The default is portable, which is the *less* safe direction — deliberately. A
narrow default that is also wrong (keying on the host for contents that depend on
the target) is a false sense of safety rather than safety. The risk is bounded to
`remote = True`: one machine has one host, and cannot restore a local slot onto
the wrong one.

Deliberately **not** in the key: `path`, `env`, `access`, `remote`, `max_size`.
Those are policy about how a cache is *used*, not statements about what is in it,
so none of them may split a slot.

### The reference is a pure graph edge

A reference becomes an `Input` with `hashed: false, runtime: false`:

- `hashed: false` — it must not reach the consumer's cache key.
- `runtime: false` — there is no artifact to materialize; a declaration produces
  no outputs.

`runtime: false` inputs are filtered out of `LinkedTargetDef`. That is what makes
the reference a pure edge, and also why the resolved mount cannot travel back to
the driver on that input — see *Crossing the plugin ABI*.

## Author surface

```python
target(
    name     = "gocache",
    driver   = "scratch",
    path     = ".cache/go-build",   # optional; omit for env-var-only
    env      = "GOCACHE",           # defaults to SCRATCH_<NAME>
    access   = "shared",            # "exclusive" (default) | "shared"
    version  = "",                  # the whole identity beyond the addr
    remote   = False,               # may travel through the remote cache
    max_size = "10GiB",             # over it, dropped whole
)

target(name = "build", driver = "bash", scratch = ["//build:gocache"], ...)
```

There is no new Starlark global. A scratch is declared through the existing
`target(driver = …)` builtin, exactly as `group` and `textfile` are, so the added
surface is a driver name and a config schema — not a function, not a keyword, and
nothing a workspace could already have defined.

**`path` is optional, and omitting it is the safer shape.** Most tools find their
cache through an environment variable and do not care where it lives. Without a
mount nothing is placed in the sandbox tree, so neither failure mode a mount must
be guarded against can arise.

`env` carries the **canonical slot path** — absolute, outside the sandbox, the
same string for every consumer. Tools bake absolute paths into their cache
entries, so a per-consumer path would fragment the cache silently.

## Execution

Where a scratch enters a run, and why there:

```
result(addr)
  → get_def                  a scratch is an Input; no artifact
  → inputs_result_exec       dependencies resolved
  → resolve_scratch          specs read, overlaps checked
  → acquire_scratch          locks taken, lineage resolved, remote pulled if cold
  → worker permit            <- after the lock, deliberately
  → driver.run               mounts and env injected, past the ABI seam
  → guards dropped           the lock is held for the whole run
```

Locks are taken **after** dependency resolution and **before** the worker permit.
After deps, because a dependency needing the same slot could otherwise never get
it. Before the permit, so a target queued on a contended slot holds no worker —
which also makes the wait provably bounded.

### Crossing the plugin ABI

`RunRequest.scratch: Vec<ScratchMount>` — a dedicated field, not an input.

This is forced rather than chosen. `Input` derives `Hash` including its
annotations, so a resolved slot path (which varies by lineage, scope and machine)
cannot ride there without reaching a cache key. `RunInput` is built *from an
artifact*, and a scratch has none. And `runtime: true`, the flag that would carry
it past linking, *means* "materialize into the sandbox" — there is nothing to
materialize.

A scratch mount is therefore in the same family as `sandbox_dir` and
`tree_root_path`: a host-resolved runtime path the driver needs. It is the only
optional one, which is why it reads as a bulge on an otherwise universal message.

Mounting happens past the seam because `driver-support` — which owns sandbox
creation — links into the plugin cdylibs, and under FUSE `sandbox_pkg_dir` is
redirected, so there is no earlier moment at which the directory reliably exists.

### The mount

One `symlink(2)` per mount, pointing out of the sandbox at the canonical slot,
created after every input is materialized and before the driver runs. It
hard-errors if anything already exists at that path: a mount landing on a
materialized input would destroy a real file.

A broad output glob beside a mount does **not** collect the cache. Collection
uses `symlink_metadata` and takes `is_file() || is_symlink()`, and `walkdir` does
not follow symlinks, so a glob reaching a mount captures the symlink and never
what is behind it. The artifact packer then refuses it, an absolute symlink being
unpackable. That is a rough edge rather than a hazard: heph created the symlink,
so collection ought to skip its own mounts instead of handing the author an error
about one.

## Concurrency

`access` is an assertion about the tool, not a wish.

| | |
|---|---|
| `exclusive` (default) | one consumer at a time. Most tools assume they own their cache directory. |
| `shared` | concurrent use is safe *by construction* — what `go build -p N` already relies on. |

A `KeyedRWLock` keyed on the slot: `exclusive` takes the write side, `shared` the
read side. Two backends — `flock(2)` files under `<home>/lock/scratch/`, which
serialize across separate `heph` processes on one machine, and an in-process map
for tests. Locks are acquired in sorted slot order, so a target referencing
several caches cannot deadlock against another doing the same.

The cost of `exclusive` is real: those targets stop running in parallel with each
other. It is the default because the alternative failure is corruption.

## Storage

### Local

```
<home>/scratch/<slot>/<scope>/head      the directory itself
<home>/scratch/<slot>/slot.meta         what it came from, for `ls` and `rm`
```

One head per **scope** — a lineage, normally a branch. `scratch.scope` supports
`${git:branch}`, so a developer gets branch scoping without configuring anything.

A cold scope seeds from the first warm entry in `scratch.restoreScopes` (a
recursive copy, symlinks recreated), so switching branches costs a copy rather
than a rebuild. Writes stay in the new scope: the base's head is left exactly as
it was.

`slot.meta` is stamped by `SlotMeta::new`; the format is never written by hand. A
meta whose format the reader does not recognise lists as an orphan — still
listable, still removable, because it is occupying disk either way.

### Remote

Keys under `scratch/v1/<slot>/<scope>/`. Entries are **immutable**: ordering
lives in the key, and a new entry is `parent + 1`.

That shape is forced by the backend, which has only `open_read`, `open_write`,
`exists` and `list_names` — no CAS, no delete. "Latest" therefore cannot be a
mutable pointer, because many branches publish concurrently and nothing could
arbitrate the write. Ordering is `(generation, bytes, stem)` descending, so a
same-generation fork resolves deterministically rather than by luck.

**A build pulls automatically when cold; it never pushes.** Pulling is safe to be
implicit — read-only, one list plus one fetch, and every way it can fail degrades
to a cold build. Publishing is none of those things, so it is a command:

```sh
heph tool scratch push --all --producer "$CI_RUN_ID"
```

Snapshots record the absolute path they were produced under, because a cache that
is portable in content but embeds absolute paths will restore and be inert —
present, and useless. `heph tool scratch head` reports it.

## Boundaries

A scratch is the only mutable, unhashed thing inside a sandbox, so every boundary
with the hermetic parts is enforced rather than documented.

- **scratch ∩ scratch** — two mounts may not overlap, and two declarations may
  not claim one environment variable. Referencing the same declaration twice is
  an error, not a no-op.
- **scratch ∩ inputs** — a mount landing on a materialized input is refused at
  mount time. This is the overlap that genuinely destroys something.
- **path validation** — a mount path is relative to the consumer's cwd; absolute,
  `..`-escaping and `.` are rejected at the declaration.

Path comparison is `hcore::paths::paths_overlap`: component-wise, so `.cache/go`
and `.cache/golang` are not an overlap while `gen` and `./gen` are — and an empty
path collides with nothing, because empty means *no path*, not the root.

## Lifecycle

- **Per-slot cap** (`max_size`), checked at acquisition under the guard. Over it,
  the lineage is dropped **whole** rather than trimmed: heph cannot know which of
  a foreign tool's entries are hot, and evicting a guess would degrade the cache
  while claiming to manage it. Unset by default.
- **Sweep** — `heph tool gc --scratch-max-size` / `--scratch-max-age-days`, LRU
  per (slot, scope). Branch scoping means a laptop accumulates a head per branch
  ever built; the sweep collects the merged and abandoned ones.
- **`heph tool scratch rm`** — always safe, by the contract.

## Surfaces

```sh
heph tool scratch ls      # every slot × scope: addr, access, mount, version, size
heph tool scratch head    # which lineage a build would restore from, and why
heph tool scratch path    # the canonical slot path
heph tool scratch push    # publish this scope's head (CI's last step)
heph tool scratch pull    # fetch without building
heph tool scratch rm      # drop one locally
```

`head` is the diagnostic that matters. Resolution stops at the first lineage
holding anything, which is right for resolving and useless for explaining — so
`head` prints the whole walk, local lineages first and then remote, marking the
winner. "Why did my branch start cold?" is answerable only by seeing what was
*not* found.

**`--no-scratch`** is a global build flag: every scratch absent — no directory
and no environment variable, not merely an empty one. It implies `--force`,
because a scratch never reaches `hashin`, so without a rebuild the run would
replay the result built *with* a warm cache and the audit would pass by reading
back the answer it exists to re-derive.

It is a bool rather than `--scratch=on|off` for a second reason: a valued flag
named `scratch` collides on clap's argument *id* with any subcommand wanting a
`--scratch` of its own, which is a runtime panic on access rather than a build
error.

Its sharp edge: "absent" includes the environment variable, so a target reading
`$MYCACHE` unguarded fails under `set -u` rather than running cold. Drivers that
own a cache handle this; a hand-written `bash` target should write `${MYCACHE:-}`.

`example/scratch/` is a worked package covering the three shapes.

## Observability

Scratch work happens between dependency resolution and the worker permit —
*before* `ExecuteStart`. Uninstrumented, a target blocked on a contended slot is
indistinguishable from one queued for a worker: one open `result` span and
nothing else.

`ScratchLockWaitStart/End` closes that, emitted once a slot stays contended past
five seconds. It carries the consumer, the cache, the `access` word from the
declaration, and a holder pid when one can be named. The wait is threshold-gated
because an uncontended acquire must stay silent — hundreds of targets sharing
one cache would otherwise emit two events each saying nothing was wrong.

The event is per *consumer*, so a machine reader loses nothing, but **renderers
collapse by cache**: one `exclusive` cache with hundreds of consumers produces
that many simultaneous waiters. The TUI shows
`⧗ //build:gocache — 47 waiting (exclusive, 1m12s)`; the CI view logs the first
waiter per cache and a per-cache total at the end; the GHA report renders a
contention table. The subject is the cache because that is what the user would
change.

This is deliberately **not** the result lock's `🔒` row. "Another process is
building this target" and "your build is serialized on a cache you declared
`exclusive`" are different problems with different fixes, and the stall
paragraph keeps them on separate lines for the same reason.

A holder pid appears only when the holder is *another process*. Contention
within one build reports none — the stamp would be our own pid, and naming it
would send the reader hunting a rogue process when the fix is one line in a
BUILD file.

## Who uses it

- **`plugin-go`** — one `GOCACHE` per Go module per variant, plus one shared,
  portable `GOMODCACHE`. The module is the package's nearest `go.mod`; the variant
  is `Factors::variant_id`, carried whole so no driver can key on its own subset
  of the factors. `heph.go.gocache_addr()` exposes the address to a BUILD file.
  Each module's first `go list` re-derives the standard library's metadata, which
  is the price of per-module isolation.
- **`plugin-oci`** — one registry blob store shared by every pull. Blobs are
  content-addressed by digest, so one store holds amd64 and arm64 layers side by
  side and travels between machines unchanged.

A plugin emitting a scratch spec for another crate's driver should have a test
that **parses** it. A declaration rejects unknown fields, so an emitted field the
driver no longer accepts is not a warning but a hard failure of every target
referencing the cache — which has happened twice.

## Failure modes

Every one degrades to a cold build:

| | |
|---|---|
| no remote entry, remote unreachable, corrupt meta | cold |
| a seed copy that fails | cold, with no partial tree left behind |
| a slot over its cap | dropped, then cold |
| a poisoned cache | survives until evicted; `rm` is the remedy |

The last is the one that is not self-healing, and it is why the caches this
targets all self-verify.

## Not built

Designed, and deliberately absent:

- **The drift guard** (`maxDrift`) — a long-lived branch keeps winning over its
  base and gets steadily colder. Strict precedence ships instead. The data the
  guard needs (`parent_scope`, `parent_generation`) is already recorded, so
  enabling it later is a resolution change, not a format change.
- **`heph tool scratch verify`** — the cold/warm `hashout` pair is a CI script.
- **A per-run clone** — deferred, not rejected.
- **A local BuildKit layer cache** for `docker_build`.
