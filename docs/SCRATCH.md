# Design: named scratch caches (GitHub-Actions-style per-target caching)

**Status:** phases 1–3 implemented and in review (PR #403, #407, #412) — a
scratch now mounts, locks and sets its env var, so the feature works locally.
Phases 4–9 (lineage, scopes, remote, retrofits) designed, not built. §18 has the ordering; the notes below record what building it
changed about the design.

Review-board consults (`product-vision`, `feature-quality`, `hermeticity`,
`compatibility`) are *pending*; §16 and §21 list what each has to rule on. The
`compatibility` consult has a landed trigger to rule on: mounting added an
additive `RunRequest` field and took `ABI_SEMVER` to 0.6.0 — see §5.3.

### Implementation notes

Two things building phases 1–2 changed:

1. **§5.3's ABI claim was half wrong.** Declaration and reference are ABI-free, as
   designed and as shipped. The *resolved mount* cannot be — `link.rs:36` filters
   `runtime: false` inputs out of `LinkedTargetDef`, which is exactly what makes a
   scratch a pure graph edge, and therefore also why it never becomes a `RunInput`
   with annotations to travel on. The design saw only the flattering half of that
   fact. Corrected in place.
2. **§17's `hashout` assertions did not test what they claimed.** A target whose
   `hashin` moved still produces identical bytes, so those tests would have passed
   while the cache missed on every run. They compare def hashes now, with a
   precondition that the comparison discriminates at all.

**The ask, restated:** a target can declare a directory that persists across runs.
Before the target executes, heph restores the directory as of the most recent
save; after it executes, heph saves it back. The directory is **declared once and
referred to by address**, so several targets can share one, and targets sharing
one are serialized so
the read/modify/write is not concurrent — unless the tool is known to be safe
under concurrency, in which case it is trusted and left parallel. It must work
through the **remote cache** so a cold CI runner is warm, which needs a *lineage*
over stored entries to make "the latest run" well defined — and that lineage is
per-branch, with restore falling back across branches (current branch, then
`master`), all of it configurable, and applying **locally as well as remotely** so
that switching branches on a laptop works the same way. Pulling from the remote is
transparent; **publishing to it is an explicit CLI command** that CI runs at the
end of its job. `pluginexec` exposes each scratch path as an environment variable.
Scratch content must not overlap with dependency inputs.

---

## 1. Summary

Add a **mutable, non-hermetic scratch directory** as a declarable target — state
carried between runs to make a build *faster*, and which must never make it
*different*.

```starlark
# //build/BUILD — declared once
target(name = "cargo-target", driver = "scratch", path = "target", remote = True)

# //cmd/app/BUILD — referred to by addr, by as many targets as want it
target(
    name    = "app",
    driver  = "bash",
    run     = ["cargo build --release"],
    out     = "target/release/app",
    scratch = ["//build:cargo-target"],
)
```

Four decisions carry the design:

1. **The working copy is the canonical directory, and nothing is copied per
   target or per run.** Every sandbox is symlinked at one directory per slot per
   branch: restore is a `symlink(2)`, save is nothing. §2.1 is why. The only copy
   anywhere is seeding a new branch from its fallback, once per branch. §7.
2. **Pull is automatic, push is a command.** A build transparently resolves local,
   then remote, so a cold CI runner warms itself with no configuration beyond
   having a remote. Publishing back is `heph tool scratch push`, which CI runs as
   its last step. That asymmetry is deliberate: pull is cheap, read-only, and
   fails into a slow build; push is expensive, mutating, and needs a directory
   nobody is writing — which a command may block for and an implicit step may
   not. §8.1, §10.10.
3. **State is a *tree* of lineages, one per branch, in both stores.** A cache —
   local or remote — holds a live lineage for `master` and one for every branch
   anyone has worked on, so there is no single "latest" for a pointer to name.
   Each entry records the branch and generation it descends from, and resolution
   is "try the current branch, then fall back to `master`", with a guard against
   inheriting a branch head that has gone stale. §10.
4. **A scratch is a target — a builtin driver like `group` — and consumers
   reference it by addr.** Its settings live in one place, so consumers cannot
   disagree about them, and `heph query revdeps` answers "who shares this cache?".
   Nothing new crosses the plugin ABI: it rides on `TargetSpec.transitive` and
   `Input.annotations`, which already exist. Targets referencing one serialize
   against each other via a keyed lock, with a `shared` "trust the tool" mode for
   caches that are concurrency-safe by construction. §5, §8.

Two plugins in this tree already want it. `plugin-go` has hand-rolled a
single-driver version of it (`golist_gocache.rs`) and left three more caching sites
unfixed; retrofitting it is not a follow-up but the **acceptance test** for §5's
surface. `plugin-oci` has a smaller, genuine win. §11.

## 2. Motivation

Every sandbox today starts empty. That is the hermetic default and it is correct,
but it is why heph pays, on every miss, for work that is byte-identical across
targets and across runs: compiler caches (`GOCACHE`, `ccache`, `sccache`, rustc's
incremental dir), dependency caches (`GOMODCACHE`, `~/.cargo/registry`), and
tool-internal indexes.

heph has already hit this once and solved it *by hand, inside one driver*:
`crates/plugin-go/src/plugingo/golist_gocache.rs` gives every `_golist` run a
shared `GOCACHE` under the engine home instead of a sandbox-local empty one.
Measured on a 500-package corpus: `go list` was **778s of CPU across 1945
invocations** (687s of it *system* time), against **15s for every `go tool compile`
combined**. Sharing the cache took the run from **205s to 84s wall (2.4x)**.

That module is a one-off: hardcoded key struct, no lock, no eviction, no remote,
no visibility, and available to exactly one driver. Every other ecosystem that
wants the same thing must reimplement it inside its own plugin, or reach out of
the sandbox and touch the host — the thing the sandbox exists to prevent.

### 2.1 The measurement that constrains the design

The *rejected* variant from that work matters more than the accepted one:

> A hermetic seed (a warm cache hardlinked into each sandbox's `.heph-gocache`)
> cut `go list` CPU 778s → 309s and moved wall time **not at all** — interleaved
> A/B: base 217.3s vs seeded 219.5s. The shared-dir ceiling won because it creates
> and destroys *zero* entries per sandbox; the seed still materializes ~500 per
> sandbox.

**Cold wall time here is filesystem-churn-bound, not CPU-bound.** A design that
materializes the cache into every sandbox and tears it down again spends its
winnings on `mkdir`/`link`/`unlink`. This is the single most important input to
§7.

## 3. Non-goals

- **Not a replacement for the target cache.** Scratch is never an output, never a
  `hashout`, never something a dependent consumes. If a downstream target needs
  the bytes, they are an output.
- **Not a channel between targets.** Two targets sharing a slot share a *cache*,
  not a *pipe*. Ordering beyond mutual exclusion is unspecified; a target reading
  what another wrote breaks the first time one of them is a cache hit.
- **Not hermetic.** §4. A declared, visible, auditable escape hatch, and the
  design's job is to keep it exactly that narrow.
- **Not a network fetch primitive.** "Download a toolchain once" is a target with
  outputs (`plugin-http`, `plugin-nix`).

## 4. The contract

> **A target must produce identical outputs whether its scratch directories are
> warm, cold, or absent. Losing a scratch directory is always a slowdown and never
> a wrong answer.**

Consequences, each a constraint further down:

1. **Nothing about scratch enters `hashin`** — not its content, and not its
   declaration either. If content did, the cache would never hit; if the
   declaration did, bumping a cache's `version` would rebuild the world for no
   correctness gain. §6.3.
2. **Scratch is only touched on a miss.** A cache hit does not restore, does not
   save, does not take the slot lock, and costs nothing.
3. **Scratch is not an output and is not an input.** It must be unreachable by
   output collection *and* disjoint from every path dependency inputs materialize
   into. Enforced, not documented — §13, which is where the non-overlap
   requirement is discharged.
4. **The invariant is unverifiable in general**, so the design owes an *audit
   mode* rather than a proof: `--scratch=off` (§13.2) forces every scratch cold
   and is what CI runs to prove warm and cold agree.

This is the same contract Go's build cache, `ccache` and `sccache` already make.
It holds for content-addressed, self-verifying caches; it does **not** hold for a
directory used as durable state (a database, a counter, an output staging dir).
The docs must say that in those words.

## 5. Author surface

A scratch cache is **declared once as a target** and **referred to by addr** by
every target that uses it. Sharing is then the ordinary thing addrs already do,
and there is no second namespace to invent.

### 5.1 Declaring one

```starlark
# //build/BUILD
target(
    name     = "gocache",
    driver   = "scratch",
    path     = ".cache/go-build",   # where it mounts, in every consumer
    env      = "GOCACHE",           # the variable the tool reads it from
    access   = "shared",            # "exclusive" (default) | "shared"
    platform = "os_arch",           # what the contents are specific to (§6.1)
    remote   = True,                # may be pulled/pushed (§10)
    version  = "go1.23",            # busts the slot when it changes
    max_size = "10GiB",
)
```

**There is no new Starlark global.** A scratch is declared through the existing
`target(driver = "scratch", …)` builtin, exactly as `group` and `textfile` are, and
`#[derive(Spec)]` on the driver's config supplies both the parser and the
BUILD-file LSP schema. So the surface added here is a driver name and a set of
config fields — not a function, not a keyword, and nothing a workspace could
already have defined.

| field | default | meaning |
|---|---|---|
| `name` | required | target name; with its package it is the cache's identity (§6.1) |
| `path` | required | mount point in a consumer's sandbox, relative to that target's cwd. The same for every consumer (§5.2) |
| `env` | `SCRATCH_<NAME>` | environment variable carrying the directory's path (§5.5) |
| `access` | `"exclusive"` | `exclusive` = one consumer at a time (the ask). `shared` = "trust the tool": concurrent, for caches safe by construction (§8.1) |
| `platform` | `"os_arch"` | `os_arch` = contents are specific to both. `os` = arch-independent. `any` = portable, and shareable across every machine (§6.1) |
| `version` | `""` | opaque string folded into the slot key; the author's bust handle |
| `remote` | `False` | may be pulled from the remote automatically and pushed to it by `heph tool scratch push` |
| `max_size` | config default | per-slot cap; over it the slot is dropped (§11) |

Every field describes **the cache**. Nothing here is a property of a particular
consumer, which is why §5.2 has nothing to configure.

`scratch` is **a builtin driver**, not a new kind of thing. It has the same shape
as `plugingroup`: a `DRIVER_NAME`, a `#[derive(Spec)]` config struct that supplies
both the parser and the BUILD-file LSP schema, and registration through
`Engine::register_driver`. Adding a driver *is* how heph gains a target kind —
this introduces no mechanism that `group`, `text_file` and `host_bin` did not
already use.

It is a target in the ordinary sense — it has an addr, it appears in `heph query`,
it is subject to visibility — and degenerate in every other: no inputs, no
outputs, never executes, never cached. `parse` returns a def; `run` is
unreachable.

### 5.2 Using one

```starlark
# //cmd/server/BUILD
target(
    name    = "server",
    driver  = "bash",
    run     = ["go build ./..."],
    scratch = ["//build:gocache"],
)
```

A list of addrs, resolved like any other addr — so `:gocache` works within a
package. There is no per-use configuration and **no way to mount a scratch
somewhere other than its declared `path`**.

That is deliberate, and it is the same argument as §5.5's canonical-path rule. A
cache's location is part of what the cache *is*: tools bake absolute paths into
their entries (Go's action IDs are the standing example — see
`hermetic-goroot-stable-path`), and a per-consumer mount would give the same cache
a different shape in every target that used it. Beyond that it is a knob with no
demonstrated use: a cache is mounted where its tool expects it, and its tool
expects one place.

So the whole class of "two consumers disagree about a slot" is not detected — it
is inexpressible. An earlier revision declared settings at each use site and
needed validation that they all matched; there is now exactly one copy of each.

`scratch` is a new attribute on `exec`, but not a new *concept*: it joins `tools`,
`read_only_deps`, `hash_deps` and `runtime_deps` as one more differently-routed
dep list, and it lives entirely in pluginexec's own spec — it is not engine or ABI
surface (§5.3).

It stays a separate list rather than folding into `deps` because §14.2 spends a
whole section on scratch **not** being an input, and putting the two in one
attribute undoes that in the place an author is most likely to read. The
alternative — reference a scratch in `deps` and let `apply_transitive` demote the
input — is workable and is noted in §19.

### 5.3 What crosses the plugin ABI: nothing new

Scratch adds **no proto message, no `TargetDef` field, and no `ABI_SEMVER` bump**.
It rides entirely on wire structures that already exist and already carry exactly
this kind of information. Three facts in the current tree make that work, each
checkable:

**1. `TargetSpec.transitive` is already "a dep contributes config to its
consumers."** `sandbox::Sandbox` (`plugin/src/driver.rs:219`) carries `deps`,
`tools` and `env`, and `Dep` already has the two flags a scratch reference needs —
`runtime: bool` (materialize into the sandbox) and `hash: bool` (fold into the
parent's key). A scratch target declares its transitive sandbox with
`Dep { runtime: false, hash: false }` plus the `SCRATCH_*` env entry, and every
consumer inherits both through the `apply_transitive` call the engine already
makes.

**2. Anything arriving via `apply_transitive` is outside the def hash by
construction.** `parse()` sets `def.hash`, and the engine calls `apply_transitive`
*after* parse and never recomputes it (`engine/result.rs:3595-3635` — the only
later check is that the hash is non-empty). So §6.3's "scratch contributes nothing
to `hashin`" stops being a rule someone has to remember and becomes a structural
property of where the data enters.

**3. `Input.annotations` is host-visible and is already the producer→host
channel.** It is a `map<string, string>` on the `Input` proto message and
`driver-support` already acts on producer-set annotations this exact way —
`READ_ONLY_ANNOTATION` and `STAGE_PER_FILE_ANNOTATION` in `stage.rs` are the
precedent. A scratch reference is marked with one, and the engine recognizes it
where it already recognizes `read_only`.

So **declaring and referencing a scratch costs no ABI surface at all**, and that
half is implemented and shipped: the reference is an annotated `Input`, the
settings are read from the declaration's spec config, and no proto message,
`TargetDef` field or `ABI_SEMVER` bump was needed.

> **Correction, from implementing it.** An earlier revision of this section
> claimed the same for the *resolved* mount and env — that they would ride to the
> driver on `RunInput.annotations`. They cannot. `link.rs:36` filters
> `runtime: false` inputs out of `LinkedTargetDef`, which is what makes a scratch
> a pure graph edge in the first place — so a scratch reference never becomes a
> `RunInput` and has no annotation map to travel on. The two facts are the same
> fact seen from both sides, and the design only noticed the flattering one.
>
> Mounting therefore does need a driver-visible channel, because the *bridge*
> owns sandbox creation (it may redirect the path into a FUSE mount, which is why
> the engine cannot pre-create anything there). The resolved list goes on
> `RunRequest` — free for in-process managed drivers like pluginexec, and an
> additive proto field for the cdylib transport, which does mean a `ABI_SEMVER`
> bump and does mean a third-party cdylib driver must be rebuilt to mount a
> scratch. Declaration and reference remain ABI-free; only mounting is not.

### 5.4 The reference as a graph edge

A scratch reference is an `Input` with **`hashed: false, runtime: false`** — the
one combination nothing else uses. It materializes no files (there are none) and
contributes nothing to `hashin`. It is there so the *edge* exists, which buys three
things at no cost:

- `heph query revdeps //build:gocache` answers "which targets share this cache?" —
  the question every serialization surprise starts with (§8.3)
- a reference to a missing or non-scratch addr is an ordinary
  `TargetNotFoundError` / wrong-kind error, rendered like every other bad addr,
  rather than a bespoke "unknown scratch name"
- visibility, packages and `//...` matchers apply without special-casing

### 5.5 What `pluginexec` puts in the environment

A tool almost never finds its cache by convention — it is told, through an
environment variable (`GOCACHE`, `CCACHE_DIR`, `CARGO_TARGET_DIR`,
`XDG_CACHE_HOME`). The declaration names that variable, because which variable a
cache is read from is a property of the cache, not of whoever uses it:

```starlark
target(name = "gocache", driver = "scratch", path = ".cache/go-build", env = "GOCACHE")
```

```starlark
# GOCACHE is set for the run; the consumer wires nothing.
target(name = "b", driver = "bash", scratch = ["//build:gocache"], run = ["go build ./..."])
```

`env` defaults to `SCRATCH_<NAME>`, where `<NAME>` is the referenced target's
**name** (not its package) through the existing `env_key_segment` helper —
uppercased, every character outside `[A-Z0-9_]` replaced with `_` — following the
`OUT_<GROUP>` / `SRC_<GROUP>` convention already documented in
`crates/plugin-exec/src/pluginexec/SANDBOX.md`. So `//build:go-cache` gives
`SCRATCH_GO_CACHE` unless it says otherwise.

Setting `env` to the tool's own variable is the common case and removes a step:
without it every consumer would write `runtime_env = {"GOCACHE":
"$SCRATCH_GOCACHE"}`, which is boilerplate that can be got wrong and that says
nothing the declaration does not already know.

Three rules, each a real decision rather than a detail:

**The value is the canonical slot path, not the in-sandbox mount.** The declared
`path` is a symlink to `<home>/scratch/<slot>/…/head`, and the variable carries the
*target* of that symlink. This is §10.9's path-stability argument cashed out: Go's
action IDs (and several other tools' cache keys) incorporate absolute paths, so if
every consumer saw its own sandbox path for the same cache, the cache would be
present and inert — the `hermetic-goroot-stable-path` trap. One canonical string
for every consumer is what makes the cache actually hit. It does contain the branch
scope, so a branch switch re-keys such a cache; that is the cost of branch
isolation (§10.3), and it is why seeding on fork matters — the seeded copy is warm
even though its path is new.

**It is injected at execute time and never hashed** — the same class as
`runtime_env`, and for a stronger reason. The value contains `<home>` and the
branch, so it differs per machine, per checkout and per branch; hashing it would
make every cache key machine-specific and defeat the remote cache entirely.

**Two references resolving to the same variable are a parse error**, naming both
addrs. `env_key_segment` is lossy and the default is derived from the target name
alone, so `//a:go-cache` and `//b:go.cache` collide; so, more obviously, do two
declarations that both set `env = "GOCACHE"`. Harmless for dep groups, where the
group name is local to one target; a silent shadow here, where one of two real
caches becomes unreachable. The fix is to set `env` explicitly on one of the
*declarations* — there is nothing to fix at the use site, because the use site
configures nothing (§5.2). Likewise a target's own `env`/`runtime_env` may not
declare a variable that a referenced scratch owns.

There is no `LIST_SCRATCH_*`. A scratch is one directory, not an enumerated file
set, so the list-file mechanism that exists for `SRC_*` has nothing to carry.

Other drivers are not bound by any of this: they read the resolved path from their
`RunInput` annotations (§5.3) and surface it however suits them.

### 5.6 Naming

`scratch` over the obvious `cache` because `cache` is comprehensively taken —
`cache = True` on a target, the local cache, the remote cache, `cache.history`,
`heph tool cache`, `heph tool clean`, `LocalCacheHit`. A second meaning makes every
existing sentence ambiguous. `scratch` also *states the contract*: not an output,
not hashed, disposable, loss is never wrong. Alternatives for the record: `state`
(implies durability we do not offer), `warm` (opaque), `state_cache` (accurate,
long, still overloads "cache"). **A `product-vision` call, not settled here.**

## 6. Identity and keying

### 6.1 Slot key

```
slot = H( addr, version, platform_components(platform) )

platform_components("os_arch") = (os, arch)     # default
platform_components("os")      = (os,)
platform_components("any")     = ()
```

- **`addr`** — the declaring target's address. The addr *is* the cache's identity,
  so packages give namespacing for free: `//go:gocache` and `//rust:gocache` are
  different caches without anyone agreeing on a prefix convention, and a driver's
  own cache is just `//@heph/go:golist-gocache` rather than a reserved name.
  Moving or renaming the declaration busts the slot, which is the same rule every
  other target already follows.
- **`version`** — the author's bust handle.
- **`platform`** — how much of the host the contents are specific to. Not a
  behavioural difference between platforms (which CLAUDE.md reserves for the
  user); it is the author stating a property of *their cache*, and the mechanism
  is identical on all three supported targets.

  `os_arch` is the default because the failure it prevents is the bad one: a
  `GOCACHE` from `darwin/arm64` restored on `linux/amd64`, or a `node_modules`
  holding native addons built for the wrong host. `os` suits a cache keyed on
  OS-shaped paths or APIs but not on instruction set. **`any` makes the cache one
  slot for every machine**, which is the interesting case — a JS package cache, a
  resolver's metadata, a download cache — and it means CI on `linux/amd64` warms a
  laptop on `darwin/arm64` from the same lineage.

  `any` is an author assertion in the same class as `access = "shared"`, and it
  asserts **two** things, not one: that the contents do not depend on the host
  *and* that they do not embed absolute paths (§10.9). A cache that is genuinely
  platform-independent but path-sensitive will restore and be inert — present, and
  useless. The narrower `platform` is the safer mistake.

Deliberately **not** in the key: `path` (where a cache lives is not what is in
it — and since §5.2 fixes it per declaration, it cannot vary anyway), `env`,
`access`, `remote` and `max_size` (policy about how a cache is used, not what is
in it — changing one must not throw the contents away).

Also folded in, but as an opaque *store-format* component rather than a user
concept: a `SCRATCH_FORMAT` constant, bumped whenever the on-disk or snapshot
layout changes, so an old slot is never found rather than misread.

### 6.2 Where a slot lives

Local layout is in §10.7 (it is scope-structured, so it belongs with the lineage);
the remote key layout is in §10.8. The slot id is the directory name in both.

### 6.3 What scratch contributes to `hashin`: nothing

Not the content, not the declaration, not the mount path, not `version`.

This is a direct consequence of §4 and it is worth stating as its own rule because
the tempting answer is different. It seems prudent to fold the *declaration* into
the consumer's `hashin` — so that bumping `version` rebuilds everything that uses
the cache. But §4 says a target's outputs are identical whether its scratch is
warm, cold, or absent. If that holds, a new slot changes nothing about the outputs
and the rebuild is pure waste. If it does not hold, the target is already broken
and a rebuild is not the fix — `--scratch=off` (§13.2) is how that gets caught.

So bumping `version` gives every consumer a fresh, empty slot and **does not
invalidate a single cached result**, which is exactly what you want when the reason
for bumping it is "the old cache had gone bad". An earlier revision of this design
had the declaration in the def hash; it was over-hashing, and the kind that costs
cache hits while buying no correctness.

The scratch *reference* still appears in `TargetDef.inputs` (§5.4) — with
`hashed: false`, so it is a graph edge and not a hash contribution.

Better than a rule, this is now **structural**: the reference and its settings
reach a consumer's def through `apply_transitive`, which the engine calls *after*
`parse()` has already set `def.hash` and never recomputes (§5.3). There is no code
path by which a scratch could enter the def hash, so the rule cannot be broken by
someone later adding a field and forgetting.

## 7. Storage mode

There is one mode in v1: the canonical slot directory *is* the working copy.
Everything that once needed a per-run copy — a coherent moment to snapshot, a
place to roll back to — is served instead by making **push an explicit command**
(§10.10), which can afford to take a lock and block in a way that an implicit
end-of-run step cannot.

### 7.1 `live` — the canonical slot is the working copy

Materializing a slot into a sandbox is one `symlink(2)`:

```
<sandbox>/ws/<pkg>/target  ->  <home>/scratch/<slot>/<scope>/head
```

Restore is `O(1)`. Save is *nothing* — the target wrote into the slot directly.
Teardown removes the symlink, not the tree (`remove_dir_all` does not follow
symlinks; this is already how `driver-support/src/stage.rs` gets O(1) teardown for
the 11k-file Go SDK).

Why this rather than a copy per target: §2.1 measured that exact shape at a 60%
CPU saving and **zero** wall-clock improvement, because the win was never CPU — it
was not creating and destroying half a thousand filesystem entries per sandbox.
`live` creates and destroys **one inode per target**, on every filesystem, with no
`clonefile`/`FICLONE` dependency anywhere.

The one copy in the whole design is §10.7's **seed on fork**: the first build on a
new branch copies the fallback scope's head into its own. That is once per (slot,
branch), amortized over every subsequent build on that branch, and it is compared
against a *cold rebuild* rather than against nothing. It can be turned off
(`scratch.seedOnFork: false`), in which case a new branch simply starts cold.

What `live` gives up is per-run rollback: a target that corrupts the directory
corrupts it until the slot is evicted or re-pulled, and a cancelled run leaves
partial writes behind. Mitigations are `heph tool scratch rm`, `heph tool scratch
pull --force` to re-fetch a known-good head, a `dirty` flag on the meta, and the
fact that the caches this targets all self-verify and self-heal.

### 7.2 The per-run clone — deferred, not rejected

An earlier revision of this design had `mode = "run"`: clone the slot once per
invocation, let every target read and write the clone, commit at the end. It is
attractive — private working copy, whole-run rollback, isolation between
concurrent `heph` processes — and it is **not in v1**, for one reason.

Its clone is a reflink on APFS/btrfs/XFS and a **full tree copy on ext4**, which is
what most hosted Linux CI runners use. That cost is *per invocation*, so unlike the
seed-on-fork copy it never amortizes: every `heph run` would copy the whole cache
before doing anything. And its main justification — providing a coherent moment for
the end-of-run push — evaporated once push became an explicit command that takes
the slot's write lock itself (§8.1).

What remains is rollback and process isolation, which are real but do not justify a
per-run copy on the platform where the feature matters most. Reconsider it as an
opt-in on CoW filesystems once the rest is shipped and measured (§21-Q3).

### 7.3 `snapshot` — rejected

Restore untars into the sandbox per target; save tars per target. The literal
reading of the original ask, and rejected on §2.1: a copy per target is the shape
that was measured to buy nothing. Recorded in §19.

## 8. Concurrency and serialization

### 8.1 The lock

One `KeyedTLock` keyed by slot, built exactly like `engine/result_lock.rs`: an
`flock(2)` gateway plus an inner reader/writer file under `<home>/scratch/`, so
mutual exclusion holds **across processes**, not just across tasks. Follows the
existing `lock.backend` config (`fs` default, `mem` for tests).

- `access = "exclusive"` → write guard. The requirement as stated: targets sharing
  a name run one at a time.
- `access = "shared"` → read guard. Concurrent, for caches safe by construction.
  **This escape valve is not optional.** `GOCACHE` is the motivating workload and
  is concurrency-safe by design — it is what `go build -p N` does — so making the
  golist targets run serially would turn a 2.4x win into a large loss.
  `golist_gocache.rs` ships today with no lock at all and that is correct. Ship
  `shared` in the same PR as `exclusive`, or the first real user is worse off than
  before the feature existed.

The same lock does double duty: it orders targets against each other during a
build, and it is what `heph tool scratch push` takes to get a directory nobody is
writing. That second use is the whole reason push is a command rather than a step
— see below.

#### `shared`, and where the coherent moment comes from

`shared` is an assertion by the author — *this tool's cache is safe under
concurrent access* — and heph cannot check it. That is the same trust the existing
`GOMODCACHE`/`GOPROXY` passthrough already extends, and it is granted the same
way: explicitly, per slot, in the BUILD file.

The worry it raises is that a concurrently-written directory has no coherent
moment to snapshot — and all three parts of a lineage entry are undefined while N
processes write: the **tar** is torn, the **content hash** is not a function of
anything, and **`parent + 1`** presumes a single writer that restored a known head.

**Making push explicit is what answers this.** `heph tool scratch push` takes the
slot's *write* guard, which excludes both `shared` readers and `exclusive` writers,
in this process and every other. It then has the directory to itself and every part
of the entry is well defined.

The reason that works here and not as an end-of-run step is entirely about who is
waiting. An implicit step must not block a build for an unbounded time, so it would
have to skip when the lock is contended — silently, and precisely when the machine
is busy enough for the cache to matter. A command someone typed (or a CI step that
ran) may block, must report that it is blocking, and fails loudly rather than
quietly if it cannot proceed. Same lock, same barrier; the difference is that
`heph tool scratch push` is allowed to say *"waiting for 3 readers to finish"* and
mean it.

So `access` and `remote` are independent: a `shared` slot pushes exactly like an
`exclusive` one.

### 8.2 Acquisition order (deadlock safety)

Three rules, each forced by existing behaviour in `engine/execute.rs`:

1. **After dependency resolution.** `execute.rs` already takes the worker permit
   after `inputs_result_exec` for exactly this reason ("prevents the classic
   diamond deadlock where mid-nodes hold permits while waiting for a leaf that
   also needs a permit"). A scratch lock held across dep resolution has the same
   failure mode: a dep needing the same slot can never get it.
2. **Before the worker permit** — the shape the approval gate already uses ("runs
   before the execute semaphore is acquired, so a waiting prompt holds no worker
   permit"). If the slot lock came after the permit, N targets sharing a slot
   would pin N-1 permits doing nothing. Because the lock is always taken first, no
   permit holder is ever blocked on a slot, so the slot holder always eventually
   gets a permit — the wait is bounded, not circular.
3. **All of a target's slots, in sorted addr order.** Two targets referencing
   `[a, b]` and `[b, a]` deadlock under any other rule. One `sort_unstable` on a
   list that is essentially always ≤ 2.

```
resolve deps → acquire slot locks (sorted) → worker permit → restore
             → driver.run → save → release permit → release slot locks
```

### 8.3 Throughput cost, stated plainly

`exclusive` on a slot referenced by *K* targets serializes those *K* targets. That
is what was asked for and is correct for a read-modify-write cache — but it is a
potentially large parallelism loss that will not be obvious to whoever writes the
BUILD file. Two obligations follow: **make the wait visible**
(`ScratchLockWaitStart/End`, surfaced like `ResultLockWaitStart/End` already is),
and **warn on a bad shape** — when one `exclusive` slot is referenced by more
targets than there are workers, say so once, name the slot's addr, and point at
both `access = "shared"` and `heph query revdeps` for the list.

## 9. Engine integration

Restore and save belong to the **engine**, not to any driver: the engine owns the
sandbox path, the home dir, the lock backend, the caches, GC and the event stream,
and the point is that every driver gets this without reimplementing it.

| file | change |
|---|---|
| ~~`proto/plugin/v1/*.proto`~~ | **no change** — scratch rides on `TargetSpec.transitive` and `Input.annotations`, both already on the wire (§5.3) |
| ~~`crates/plugin-abi/`~~ | **no change, no `ABI_SEMVER` bump** |
| `crates/driver-support/src/lib.rs` | annotation-key constants next to `READ_ONLY_ANNOTATION` / `STAGE_PER_FILE_ANNOTATION`, so the keys have one definition |
| **`crates/engine/src/engine/scratch.rs`** *(new)* | slot key, local store, lock, mount, meta, eviction |
| **`crates/engine/src/engine/scratch_lineage.rs`** *(new)* | the store-agnostic model: scopes, generations, fork points, the §10.4 resolution and §10.6 order |
| **`crates/engine/src/engine/scratch_remote.rs`** *(new)* | remote transport: list, pull (automatic), push (driven by the command) — §10.8, §10.10 |
| `crates/engine/src/engine/execute.rs` | acquire locks after `inputs_result_exec`, before `result_permits.acquire()`; restore after `remove_stale`, before `driver.run`; save before the sandbox teardown completes |
| `crates/engine/src/engine/validate.rs` | output↔scratch and dep↔scratch overlap checks (§13), reusing `paths_overlap`/`is_ancestor` |
| `crates/driver-support/src/driver_managed.rs` | the *authoritative* dep-overlap check at materialization (§14.2) |
| `crates/engine/src/engine/event.rs` | `ScratchRestoreStart/End`, `ScratchSaveStart/End`, `ScratchLockWaitStart/End` |
| `crates/engine/src/engine/gc.rs` | slot sweep by size/age |
| `crates/engine/src/engine/clean.rs` | `--scratch [name]` on `heph tool clean` |
| **`crates/builtins/src/pluginscratch/`** *(new)* | the `scratch()` builtin and its provider — a declaration target with no inputs, no outputs and no execution, alongside `plugingroup`/`pluginfs` |
| `crates/plugin-exec/src/pluginexec/spec.rs` | `scratch` attribute (list or dict of addrs) → inputs with `hashed: false, runtime: false` |
| `crates/plugin-exec/src/pluginexec/mod.rs` | `apply_transitive`: fold the scratch target's transitive env and annotations into this def |
| `crates/plugin-exec/src/pluginexec/mod.rs` | inject `SCRATCH_<NAME>` at execute time (§5.5), next to the existing `OUT_*`/`SRC_*` routing |
| `crates/plugin-exec/src/pluginexec/SANDBOX.md` | document `SCRATCH_<NAME>` in the auto-injected env table |
| *(none)* | a driver reads the resolved slot path from its `RunInput` annotations, which `execute.rs` already populates — no `RunRequest` change |
| `crates/config/src/config_yaml.rs` | `scratch: { root, max_bytes, max_age_days, scope, restore_scopes, max_drift, seed_on_fork }`, plus the `${git:branch}` interpolation |
| `src/commands/tool/scratch.rs` *(new)* | `heph tool scratch ls\|head\|path\|pull\|push\|rm\|verify` |
| `src/commands/global.rs` | `--scratch=on\|refresh\|off` build flag |

Paths that must **not** restore or save, each for a concrete reason:

- **cache hit** — never reaches `execute` (§4.2).
- **`rs.hash_only()`** — the caller holds guards it cannot release; it may hash and
  probe but must never build.
- **`--shell`** — restores (the shell should see a warm cache), never saves; a
  human poking at a directory is not a cache write.
- **`use_tmp_cache`** (`cache = False` or `--force`) — restores, saves nothing.
  Such a target may benefit from a warm cache, but its runs should not define the
  slot's contents.
- **cancellation** — never saves; the directory is mid-write by definition.

## 10. Lineage

Lineage is what makes "restore the latest run" well defined. It is **one model
used by both stores** — the local slot store and the remote object store — because
the question they answer is the same one: *given this branch, which saved state
should this run start from?* Local and remote differ only in how an entry is
stored and how far it travels.

### 10.1 Why "latest" cannot be a pointer

The tempting design is a mutable `HEAD` object naming the newest snapshot. It is
wrong here for a structural reason and an implementation reason, in that order of
importance.

**Structural: one cache serves many branches at once.** There is no single
"latest". A remote holds a live lineage for `master`, one for each open PR, one
for each long-running feature branch — all advancing concurrently and all
legitimately different. The same is true of a *developer's laptop*, which
accumulates a lineage per branch they have worked on. A single pointer would have
those branches overwrite each other's heads continuously, which is not a race to
be fixed but a mis-modelling: the branches are not competing to be latest, they
are *separate lineages*. A pointer *per* branch models that and creates two new
problems — an unbounded set of mutable objects, and no answer for the cross-branch
restore in §10.3, which needs to compare heads that no single pointer relates.

**Implementation: the remote store cannot do it safely anyway.**
`RemoteCacheBackend` (`engine/remote_cache.rs:351`) offers exactly four
operations:

```rust
async fn open_read(&self, key: &str) -> Result<Option<...>>;
async fn open_write(&self, key: &str) -> Result<...>;
async fn exists(&self, key: &str) -> Result<bool>;
async fn list_names(&self, prefix: &str) -> Result<Vec<String>>;
```

No compare-and-swap and no delete. So even for a single branch, two jobs finishing
together race, and the *loser* can be the one that finishes last: a job that
started from an older cache overwrites the pointer with older content. "Latest"
would mean "written most recently", which is not the same as "descended from the
most work".

So: **entries are immutable, and ordering is carried in the key.** Resolution is a
prefix list and a max — no mutation, no coordination, correct under concurrent
writers, and it extends to many branches by listing more than one prefix.

### 10.2 Lineage is a tree, not a line

Every entry records where it came from:

```
(scope, generation)   identity      — which branch lineage, and how far along it
(parent_scope, parent_generation)   — the entry it was restored from
```

**Generations advance at publish time, not at build time**, and that makes the
model exactly git's: the local head is a working tree, its meta records the
`parent` it was last pulled or seeded from, and `heph tool scratch push` (§10.10)
is the commit. Between pushes the head is simply *dirty* relative to its parent.

Within one scope, `generation(pushed) = generation(parent) + 1`. A machine that
pulled `master` generation 11, built against it, and pushed produces `master`
generation 12 with parent `(master, 11)`. A machine that started genuinely cold
pushes generation 0 with no parent.

A **fork** is the ordinary case, not an exception. A build on a feature branch
seeds from `master` generation 12 and pushes into its *own* scope, producing
`feat-x` generation 0 with parent `(master, 12)`. `master` carries on to 13, 14, …
independently. Neither lineage is behind the other in any meaningful sense; they
are different branches of a tree, and the recorded fork point is what relates them.

Two rules follow, and everything in §10.3–§10.6 is a consequence:

- **Generations are only ever compared within a single scope.** `feat-x` at
  generation 40 is not "ahead of" `master` at generation 12. Comparing across
  scopes is a category error and the code must not offer the operation.
- **The relation between scopes is the recorded fork point**, a pure lineage
  measure needing no clock. It is what §10.4 uses to notice a branch has gone
  stale.

Generation, not wall-clock time, and this is the point of calling it a lineage: a
slow runner that restored generation 5 an hour ago and finishes now writes
generation 6, which correctly **loses** to a chain that has since reached 12. A
timestamp would have that backwards, and clock skew across runners makes it worse.
Generations need no clock and no coordinator, only the parent each writer already
restored.

### 10.3 Scopes — one lineage per branch, everywhere

A scope *is* a lineage. `scope: ""` (the default) gives one shared lineage, right
for a single long-lived branch and for a developer who never switches; setting it
per branch gives the tree of §10.2.

```yaml
scratch:
  scope: ${git:branch}           # the lineage this run writes to. "" = one lineage
  restoreScopes: [master]        # ordered read fallback: try scope, then these
  maxDrift: 100                  # discard a branch head this far behind its fork
                                 # point (§10.4). Unset = strict precedence
```

**This applies locally, not only in CI.** A developer switching branches is the
same event as a CI job running on a PR: the work already done on the branch they
are leaving should stay put, and the branch they arrive at should start from
somewhere warm rather than from nothing. Without scoping, `git checkout feat-x`
silently hands `feat-x`'s build the cache state `master` left behind, then hands it
back mutated on the way home — every switch degrading both. With scoping, each
branch keeps its own head and a switch seeds from the fallback.

This is what seeding on fork (§10.7) is for: on the first build after a checkout,
the new branch's head is copied from `master`'s rather than starting empty, so the
cost of switching branches is one copy instead of a full rebuild.

`${git:branch}` is a config interpolation heph resolves itself, so a developer
gets branch scoping without wiring anything; CI overrides it with
`${GITHUB_REF_NAME}` or whatever names the branch there. Both resolve to a plain
string; nothing downstream knows about git.

The rest of the scope contract:

- **Writes always go to `scope`, and only to `scope`.** A PR job can never advance
  `master`'s lineage — the isolation that makes this safe to enable on untrusted
  PR CI at all. A bad PR poisons its own branch and nothing else. Locally, a
  broken experiment on a branch cannot corrupt the cache you go back to.
- **Reads try `scope`, then each `restoreScopes` entry in order**, subject to the
  drift guard. The list is ordered, so a three-level convention (`feature` →
  `develop` → `master`) works without special-casing.
- **The fork point is recorded on the first commit after a cross-scope restore**,
  so a branch always knows where it came from.
- All of it is config, not BUILD-file, because it is repo/CI policy rather than a
  property of a target. A per-slot override would let one BUILD file opt out of
  the branch isolation the repo just configured, which is exactly backwards.
- **Scoping multiplies entries by branch count**, locally and remotely alike, and
  nothing revisits a scope it stopped listing. Dead branches are collected by age
  — §10.11 remotely, §11 locally — which is why those are requirements rather than
  niceties once scoping is on.

### 10.4 Resolution: current branch first, then fall back

The behaviour asked for — *try the current branch, then try master* — with one
addition that stops it from picking a head that is technically a hit and
practically useless. The same algorithm runs against the local store and the
remote one; only `list_heads` differs.

```
resolve_head(slot, store):
  candidates = []
  for scope in [cfg.scope] + cfg.restoreScopes:            # ordered, configurable
      head = store.head(slot, scope)                       # within-scope max, §10.6
      if head: candidates.push((scope, head))
  if candidates is empty: return None                      # cold

  primary = candidates[0]
  if cfg.maxDrift is set and primary is not the last candidate:
      fallback = the next candidate whose scope is primary's parent_scope
      if fallback and drift(primary, fallback) > cfg.maxDrift:
          return fallback                                  # primary has gone stale
  return primary

drift(primary, fallback) = fallback.generation - primary.parent_generation
```

**Local first, then remote — automatically, in every mode, with nothing to
configure.** A build resolves against the local store; only if that is cold does it
consult the remote, and then only for a slot with `remote = True`. A warm laptop
therefore makes no network call at all, and a fresh CI runner — always locally cold
— always goes to the remote and warms itself without the workflow mentioning
scratch at all. `--scratch=refresh` forces the remote check when a developer wants
CI's head rather than their own.

Pull is automatic precisely because it is safe to be: it is read-only, it is one
list plus one fetch, and every way it can fail — no entry, remote down, corrupt
meta — degrades to a cold build (§10.12). Push is none of those things, which is
why it is a command (§10.10).

**Why the drift guard.** Strict precedence — first scope with any entry wins — is
GHA's behaviour and it has a well-known failure: a branch whose cache was written
three weeks ago at `master` generation 12 keeps winning over `master`'s current
generation 400, so the branch gets steadily colder the longer it lives, and the
build that most needs a warm cache is the one guaranteed not to get one. A
long-lived local feature branch has exactly the same problem.

`drift` measures that staleness **in lineage, not in time**: the primary head
forked at `master` 12, `master` is now at 400, so the branch is 388 generations
behind and is discarded in favour of `master`'s head. Had it forked at 398, drift
is 2 and the branch's own incremental work is kept. No clock, no skew, no
heuristic about "recent".

`maxDrift` unset restores strict GHA precedence exactly. Default proposed:
**100**, explicitly a number wanted from real data rather than reasoning
(§21-Q4). Drift is computed one level only — against the immediate `parent_scope`
— because a chain of forks is rare and a transitive walk costs a lookup per hop.

### 10.5 Relation to GHA's `key` + `restore-keys`

The obvious model to copy is GitHub Actions': a string `key`, plus an ordered list
of `restore-keys` matched by **prefix**, with ties broken by recency. It is
well-understood and it works. This design ends up somewhere close, and the
differences are worth being explicit about, because two of them are deliberate and
one was an oversight this comparison caught.

**Why not literal string prefixes.** GHA needs them because it has no model of what
a cache belongs to: everything an author knows about a cache — the OS, the tool
version, the lockfile hash, the branch — has to be flattened into one string, and
prefix matching is how you then peel dimensions off the end. heph already has those
dimensions as first-class things, so the flattening has nothing to do:

| GHA key fragment | here |
|---|---|
| `${{ runner.os }}-${{ runner.arch }}` | `platform` on the declaration (§6.1) |
| a name like `go-build-` | the declaring target's addr (§6.1) |
| `${{ hashFiles('go.sum') }}` | `version` |
| the branch, when people remember to add it | `scope` (§10.3) |
| `restore-keys:` fallback list | `restoreScopes`, and `version_fallback` below |

Typed dimensions beat a string here for a reason beyond tidiness: a prefix is only
peelable in the order the author happened to concatenate. `go-linux-x64-abc123`
can fall back to "any lockfile on linux/x64", but never to "this lockfile on
another arch", because that dimension is buried mid-string. Named dimensions fall
back in whatever order suits.

**Where GHA is right and this design was wrong.** GHA's canonical example —
`key: node-${{ hashFiles('package-lock.json') }}`, `restore-keys: node-` — falls
back to *the previous lockfile's cache*, which is the whole point: you get 95% of
the packages and download the delta. An earlier revision here put `version`
straight into the slot key with no fallback, so bumping it meant fully cold. For a
`version` that is a lockfile hash rather than a bust handle, that is simply worse.

So `version` gets a fallback too, opt-in on the declaration:

```starlark
target(
    name             = "node-modules",
    driver           = "scratch",
    path             = "node_modules/.cache",
    version          = hash_files("package-lock.json"),
    version_fallback = True,     # inherit the previous version's cache
    platform         = "any",
)
```

With it on, resolution — having exhausted every scope for the current version —
retries the scope list against the most recently published *other* version of the
same slot, and seeds from it.

It is **off by default**, and that asymmetry with `scope` (which always falls back)
is the point: `version` is documented as the *bust handle*. If an author bumps it
because the cache went bad, silently inheriting the bad cache is the one thing they
were trying to avoid. `version_fallback = True` says "this version is a
fingerprint, not a bust" — which is exactly what GHA's `hashFiles` key means, and
exactly what a bare `version = "go1.23"` does not.

**Where this design keeps its own model: ordering.** GHA breaks ties by recency —
the most recently created matching entry wins. That is the one part not worth
copying, for the reason §10.2 gives: recency is not descent. Under concurrent
writers a slow job that started from an old cache finishes last and wins, and a
long-lived branch's stale entry keeps beating a much-advanced trunk. Generations
and the drift guard (§10.4) fix both without a clock.

Worth conceding how much that buys, though: by §4 a slightly-worse restore is a
*slowdown, never a wrong answer*, so recency would be **adequate** — GHA's model is
not broken, it is merely leaky, and the leak costs cache warmth rather than
correctness. The lineage is an optimization, and its whole cost is one `parent`
field in the meta and a `+ 1` in the push command. It is cheap enough to keep, and
honest to describe as an optimization rather than a requirement.

### 10.6 Same-generation forks and the total order

Two runs on the *same* branch both restore generation 11 and both commit
generation 12. That is a same-generation fork, and it is expected: by §4 either
side is *correct*, so the tie-break only needs to be **deterministic and stable**,
so every reader converges on the same head rather than oscillating.

Order entries **within a scope** by:

```
(generation, bytes, content_hash)   descending
```

`bytes` second is a deliberate heuristic rather than an arbitrary tie-break: among
two equally-derived caches, the larger has more warm entries, so it is the better
one to inherit. `content_hash` last makes the order total.

Stated so nobody builds it later by accident: **there is no merge**, neither
across same-generation forks nor across branches. A fork discards one side's
incremental work. That is a lost speedup, never a wrong answer, and it is exactly
the latitude §4 buys.

### 10.7 The local store

```
<home>/scratch/
  <slot>/<scope>/head/          the canonical directory for that branch lineage
  <slot>/<scope>/head.meta      borsh: generation, parent_scope, parent_generation,
                                bytes, last_used, dirty, produced_at_path
  <slot>/<scope>/head.lock      flock gateway (§8.1)
```

`store.head(slot, scope)` is a single `head.meta` read — the local store keeps only
the head per scope, not a history, because a local lineage's older generations have
no reader. (`cache.history` exists for target revisions because a *different*
`hashin` may want an older one; a scratch lineage has exactly one useful entry per
branch.)

**`dirty`** is set the first time a build takes the slot's lock for a scope, and
cleared by a successful push. It is what `heph tool scratch push` uses to skip a
head that has not changed since it was last published, and what `heph tool scratch
ls` reports so "do I have unpublished cache state?" is answerable.

**Seed on fork.** When resolution falls back to another scope (§10.4) — the first
build on a new branch — the fallback's head is **copied** into this scope's head
before the build starts, and `parent` is set to the fallback's `(scope,
generation)`. Writes then land on this branch's own head and the branch it came
from is untouched, which is the isolation property of §10.3.

That copy is the only one in the design. It is once per (slot, branch), it is a
reflink where the filesystem offers one, and where it does not it is a plain copy
whose alternative is a cold rebuild. `scratch.seedOnFork: false` turns it off and
new branches start cold — worth having for someone on ext4 with a very large slot,
and not the default because the whole point of branch scoping is that switching
branches should not throw the cache away.

### 10.8 The remote store

```
scratch/v1/<slot>/<scope>/<gen:016x>.<content_hash>.tar.zst    # the snapshot
scratch/v1/<slot>/<scope>/<gen:016x>.<content_hash>.meta       # small sidecar
```

Zero-padded hex generation as the *leading* component within a scope means
lexicographic order **is** generation order, so a sorted `list_names(prefix)`
answers `store.head` with no fetches. The content hash disambiguates a
same-generation fork (§10.6) and makes the object self-identifying.

The `.meta` sidecar (borsh, a few hundred bytes) carries the same fields as the
local one plus the producing heph version, `SCRATCH_FORMAT`, and a free-form
`producer` string (`--producer` on the push command; CI passes its run id) for
diagnosis. It is fetched only for candidate
heads, so resolution costs **one list plus one small GET per candidate scope**, and
the tar is fetched only if the local store was cold.

Unlike the local store, the remote keeps **every** generation — it has no cheap way
to delete one (§10.11) — which is why the key sorts and why pruning is a lifecycle
rule rather than a write-path concern.

### 10.9 Path stability across machines

The slot path is stable across runs and across targets on a machine, which is what
local sharing needs — but it is under `<home>` = `<root>/.heph3/…`, so a checkout
at a different absolute path on a CI runner produces a different one, and it
contains the branch scope, so a branch switch changes it too.
For a cache whose entries embed absolute paths (Go's action IDs; see
`hermetic-goroot-stable-path`), a restored snapshot is then *present but inert* —
the worst outcome, because it looks like a hit and performs like a miss.

Two things follow, and the second is the one that matters:

- The snapshot's `.meta` records the absolute slot path it was produced at.
  Restoring at a different path is a `debug!` line naming both paths, so
  "the cache restored and the build was still slow" is diagnosable rather than
  mysterious.
- **`scratch.root` is configurable**, and CI that wants path-sensitive caches to
  travel sets it to a fixed absolute path (`/var/tmp/heph-scratch`) on every
  runner. Default stays `<home>/scratch`.

### 10.10 Publishing: `heph tool scratch push`

There is **no automatic push**. Publishing a slot to the remote is an explicit
command, which CI runs as its last step:

```yaml
- run: heph run build //...
- run: heph tool scratch push --all --producer "$GITHUB_RUN_ID"
  if: always()
```

```
heph tool scratch push [<addr>...] [--all] [--scope <s>] [--force]
                       [--producer <s>] [--dry-run]
```

For each selected slot, in the current scope:

1. Take the slot's **write guard**, reporting the wait if there is one (§8.1). This
   is the coherent moment; nothing else in the design has to manufacture it.
2. Skip unless the head is `dirty`, or `--force`. A run that changed nothing
   publishes nothing.
3. Tar, hash, and upload as `(scope, parent_generation + 1)` — or `(scope, 0)` when
   this scope has never been published (§10.2).
4. Update the local `parent` to the entry just written and clear `dirty`.
5. Report per slot: pushed with its new generation and byte count, skipped because
   clean, or failed with why. Exit non-zero if any selected slot failed.

`--all` selects every slot with `remote = True` that the workspace declares.
Naming slots explicitly is for a job that only wants to publish some of them —
a lint job with no business republishing the compiler cache, say.

**Why this is not automatic.** The end-of-run push in an earlier revision of this
design depended on a per-run clone to give it a directory nobody was writing, and
that clone is a full tree copy on ext4 (§7.2). Making push a command removes the
dependency entirely: a command may take the lock and block, and it is *expected*
to take time and to be able to fail. An implicit step has neither privilege — it
would have to skip when contended, silently, on exactly the busy machines where
the cache matters most.

It also puts the decision where it belongs. Whether a given job's cache state
deserves to become the branch's published head is a CI-policy question — a job
that failed halfway, a job on a fork, a job that ran a subset of targets — and it
is answered far better by `if:` conditions in a workflow than by a heuristic
inside heph.

- **Where:** into `cfg.scope` — the branch this run is on — and never into any
  `restoreScopes` entry, even the one the head was seeded from (§10.3).
- **`access` is irrelevant.** A `shared` slot pushes exactly like an `exclusive`
  one; the write guard is what separates them from the tar (§8.1).
- Respects the per-cache `write: false` config, so a read-only remote needs no new
  switch — the command reports the refusal rather than pretending to push.
- No head re-check before uploading: an entry that turns out to be behind simply
  loses at resolution (§10.6), and a check would be a race rather than a guard.


### 10.11 Remote pruning

Old generations accumulate and there is **no `delete` on the backend trait**. Two
options, and the recommendation is the second for v1:

1. Add `async fn delete(&self, key: &str)` to `RemoteCacheBackend` and have the
   writer prune all but the newest K (K=3, so a reader mid-download is never
   orphaned) after a successful push. Costs a new trait method every backend and
   test double must implement — a `compatibility` surface change.
2. **Object-store lifecycle rules.** S3 and R2 both expire objects by prefix and
   age; `scratch/v1/` is a dedicated prefix precisely so this is a one-line bucket
   policy. Zero new code, zero new trait surface, and the store is already the
   component that knows about storage cost.

Ship (2), document the prefix and a suggested 30-day rule, and revisit (1) only if
someone needs bounded object *counts* rather than bounded age.

Branch scoping (§10.3) makes this **mandatory rather than advisory**: object count
now grows with branch count, every merged PR leaves a dead lineage behind, and
nothing in the resolver ever revisits a scope it stopped listing. An age rule
collects all of it, which is the other reason `<scope>` sits inside the
`scratch/v1/` prefix rather than above it.

### 10.12 Failure modes

| situation | behaviour |
|---|---|
| no entry for the slot in any scope | cold run; `debug!`, never an error |
| `list_names` fails / remote down | cold run; `warn!` once per invocation |
| head `.meta` unreadable or `SCRATCH_FORMAT` mismatch | skip to the next candidate in order, then cold |
| tar fetch fails midway | cold run; the partial slot is removed before use |
| two runners on one branch fork the lineage | §10.6; one side's increment is lost |
| a push writes a generation ≤ the current head | uploaded anyway (immutable, distinct key); loses at resolution. No coordination needed |
| `heph tool scratch push` cannot take the write guard | blocks, reporting the wait; never skips silently (§8.1) |
| `heph tool scratch push` fails to upload | non-zero exit naming the slot; `dirty` stays set, so the next push retries |
| the current branch's head is far behind its fork point | drift guard falls back to the parent scope (§10.4); reported by `heph tool scratch head` |
| the branch's `parent_scope` no longer exists (base branch deleted) | drift is uncomputable, so the primary head is used as-is — strict precedence, the pre-guard behaviour |
| a `restoreScopes` entry names a scope that never existed | skipped silently; it is a fallback list, not an assertion |
| slot restored at a different absolute path than produced | works; `debug!` names both paths (§10.9) |

## 11. Retrofits: what already wants this

The design is only worth building if it subsumes the hand-rolled caching already in
the tree. Two plugins have it, and the Go one is the reason this exists.

### 11.1 `plugin-go` — required, and the acceptance test for the whole design

`golist_gocache.rs` is this feature, implemented once, for one driver, by hand. It
gives every `_golist` run a shared `GOCACHE` under the engine home, and it is worth
the measured 2.4x in §2. What it does not have: a lock, eviction, a remote, any
visibility, or availability to anything but golist. Replacing it is phase 7, and it
is the proof that the general mechanism works — **if it cannot express what that
module does, the design is wrong and this is where that surfaces.**

Four sites, in the order they should move:

**1. `driver_golist.rs` — the shared golist `GOCACHE`.** Today
`GolistGocache::resolve` maps a hand-written `GocacheKey { goroot, goos, goarch,
build_tags, goexperiment, race }` to a directory, falling back to a sandbox-local
one when no home dir was configured. That key becomes a declaration:

```starlark
target(
    name     = "golist-gocache",
    driver   = "scratch",
    path     = ".heph-gocache",
    env      = "GOCACHE",
    access   = "shared",          # Go's cache is concurrency-safe by design (§8.1)
    platform = "os_arch",
    version  = go_cache_version(...),   # goroot + tags + goexperiment + race
    remote   = True,
)
```

Everything the module hand-rolls maps onto something declared: the key's variable
parts become `version`, the fixed ones become `platform`, and the "concurrent
access is fine" property that today is expressed by *having no lock at all*
becomes `access = "shared"` — stated rather than implied. The fallback-to-
sandbox-local path disappears: a slot always resolves, and a cold one is just cold.

Note the key currently includes `goroot`, an absolute path, purely because Go's
action IDs do. Under this design the canonical slot path is stable (§5.5), so
whether `goroot` still needs to be in `version` should be re-measured rather than
carried over — see `hermetic-goroot-stable-path`.

**2. `driver_compile.rs` — the one that was never fixed.** It still does this,
per target, in every sandbox:

```rust
let gocache = pkg_dir.join(".heph-gocache");
std::fs::create_dir_all(&gocache)?;
```

That is precisely the pattern `golist_gocache.rs` was written to remove — an empty
per-sandbox `GOCACHE` — left in place for compile because the fix was
driver-local and nobody generalized it. §2 measured compilation at 15s against
golist's 778s, so the *upside here is smaller*, but it is the same shape of waste
and it costs a `mkdir` plus a teardown per compiled package on top of it. Worth
doing, worth measuring separately, and worth not assuming: §2.1 is the standing
reminder that the obvious cache win moved wall time by zero.

**3. `thirdparty.rs` — `GOMODCACHE`, `GOPATH`, `GOPROXY` passed through from the
host.** These are the *most* interesting, because passing them through is a
hermeticity hole heph accepts today on the explicit grounds that modules are
content-addressed and `go.sum`-verified. A declared scratch is strictly better on
every axis: heph owns the directory instead of borrowing the host's, it is bounded
and evictable, it is visible in `heph tool scratch ls`, and it can be published so
a CI runner does not re-download the module graph on every job. It is also a clean
`platform = "any"` — the module cache holds source, not objects.

This is the change with the largest CI effect and the one to sequence last, since
it narrows a hole rather than widening one and deserves its own before/after.

**4. `pkg_analysis.rs`** enumerates `GOROOT`/`GOPATH`/`GOMODCACHE`/`HOME`/`PATH`/
`GOCACHE` for hashing; whatever moves above has to move here too, or the analysis
hashes an env var that no longer describes what the run reads.

### 11.2 `plugin-oci` — a real but smaller win

Two places, neither urgent, both genuine.

**Registry blobs, shared across pull targets.** `oci_pull` caches per target, keyed
on ref + platform, and `platform.rs` is careful about that key because "an arm64
and an amd64 machine would share one cache entry for two different images". But the
*blobs* underneath are content-addressed by digest, so two images sharing a base
layer re-download it once per target today. A blob-store scratch fixes that, and it
is the design's cleanest `platform = "any"`: the **image** is platform-specific,
the **blob store** is not, and one slot can hold amd64 and arm64 layers with no
risk, shared by every machine on the lineage. `access = "shared"` too — a
digest-addressed store is safe under concurrent writers for the same reason Go's
cache is.

**A local BuildKit layer cache.** `docker_build` already wires `cache_from` /
`cache_to` to `docker buildx --cache-from/--cache-to`, but those want a **registry**
the user has to configure, and heph's own docs note that on a heph cache hit no
build runs and they have no effect at all. BuildKit also supports
`type=local,dest=<dir>` — which is a directory, which is exactly what a scratch is.
That would give layer caching with no registry, published through heph's own
lineage, so a CI runner inherits warm layers from the branch. It is the neatest fit
for the feature in the whole tree, and it is second in priority because
`docker_build` is already well served by the input-hash cache for the common case.

Neither OCI change is a prerequisite for anything. They belong after phase 7,
as evidence that a second ecosystem adopts the mechanism without it growing new
knobs — which is the real test of whether §5's surface is right.

## 12. Lifecycle and eviction (local)

Nothing else bounds a slot's growth — there is no `hashin` to age out and no
`cache.history` to trim against.

- **Accounting.** `<slot>.meta` records `bytes` and `last_used`, updated after a
  save or by the GC walk, never by re-walking the tree per run.
- **Per-slot cap** (`max_size` on the declaration, default **10 GiB**). Over the cap the slot is
  **dropped whole**, not trimmed: heph does not know which of a foreign tool's
  entries are hot, and guessing would silently degrade the cache while claiming to
  manage it. Dropping is honest and self-correcting.
- **Global cap and age** (`scratch.max_bytes`, `scratch.max_age_days`, proposed
  50 GiB / 30 days) swept by `heph tool gc`, LRU by `last_used`, **per (slot,
  scope)** rather than per slot. Branch scoping means a laptop accumulates a head
  per branch ever built, and a merged or abandoned branch's head is never read
  again — the LRU is what collects it. On a CoW filesystem those heads share
  bytes with each other, so the disk cost of many branches is far below the sum of
  their sizes, and `bytes` in the meta is the logical size, not the exclusive one.
- **`heph tool clean --scratch[=<addr>]`**, `heph tool scratch rm <addr>`.
- The underlying tools' own self-limiting still applies and is a feature: Go trims
  entries unused for five days.

## 13. Observability

"Why did it do that?" is a design-time requirement here more than anywhere else,
because scratch changes both *timing* and *parallelism* invisibly.

### 13.1 Events and reporting

- `ScratchRestoreStart/End { slot, name, mode, source: live|local|remote|cold, scope, generation, forked_from, bytes }` — `scope` and `forked_from` are what answer "why did my branch build start cold?"
- `ScratchSeedStart/End { slot, name, from_scope, to_scope, method: reflink|copy, bytes, ms }` — the one-per-branch copy of §10.7, and the only place a build can spend real time on scratch
- `ScratchPushStart/End { slot, name, scope, generation, bytes, skipped_reason }` — emitted by the command, so `--json` consumers see publishing the same way they see a build
- `ScratchLockWaitStart/End { slot, name, access }` — surfaced in the TUI as
  `ResultLockWaitStart/End` already is, so a serialized build *looks* serialized.
- `heph tool scratch ls` → name, slot, mode, access, size, generation, last used,
  declaring targets.
- `heph tool scratch head <addr>` → **every candidate scope that was listed, in order**,
  each with its head generation, fork point, computed drift, and whether it was
  chosen, skipped by the drift guard, or never reached. Plus the winner's producer
  and the path it was produced at. This is the command someone runs when CI is
  slow, and "which branch did my cache come from, and why not the other one" is
  the question it has to answer in one screen.
- `heph inspect <addr>` shows the target's scratch references and their resolved
  slots; `heph inspect //build:gocache` shows the declaration itself.
- **`heph query revdeps //build:gocache`** lists every target sharing the cache —
  no new command, because §5.4 makes the reference a real graph edge. This is the
  answer to "why is my build serial?" and it costs nothing to provide.
- Telemetry: restore hit/miss, source breakdown, bytes moved, aggregate lock wait
  per slot. Lock wait is the number that tells a user their `exclusive` was a
  mistake.

### 13.2 Audit mode

`--scratch=on|refresh|off`:

- `on` (default) — as designed.
- `refresh` — consult the remote even when the local store is warm (§10.4), for a
  developer who wants CI's head rather than their own.
- `off` — every scratch directory absent; the target runs fully cold.

`off` is how §4 is checked in practice, and CI should run it on a schedule: build
with `--scratch=off`, compare `hashout`s against the warm build, fail on
divergence. A later `heph tool scratch verify <matcher>` can do the cold/warm pair
and the diff in one command.

### 13.3 CLI surface

Everything that *operates on* scratch state is a subcommand of **`heph tool
scratch`**, alongside the existing `heph tool cache`, `heph tool gc` and `heph tool
clean`. `heph tool` is already the maintenance group — commands that inspect or
repair heph's own state rather than build anything — and a scratch slot is exactly
that kind of state. Nothing new lands at the top level.

| command | does |
|---|---|
| `heph tool scratch push [<addr>…] [--all]` | **publish** the current scope's head to the remote (§10.10). The command CI runs as its last step. Blocks on the slot's write guard; skips a clean head; exits non-zero if any slot fails |
| `heph tool scratch pull [<addr>…] [--force]` | fetch the remote head without building. Builds do this automatically (§10.4); this is for warming a machine ahead of time, or recovering from a corrupted local head |
| `heph tool scratch ls` | every slot **× scope**: addr, branch, access, size, generation, fork point, **dirty**, last used, and the count of referencing targets (`heph query revdeps` for the list) |
| `heph tool scratch head <addr>` | the resolution trace, **local and remote** — every candidate scope, its head, fork point, drift, and why it won or lost |
| `heph tool scratch path <addr>` | the canonical slot path, for pointing another tool at it or `du`-ing it |
| `heph tool scratch rm <addr>` | drop a slot locally. The remedy for a poisoned cache (§7.1) |
| `heph tool scratch verify <matcher>` | run the selected targets cold and warm, diff the `hashout`s (§13.2) |
| `heph tool clean --scratch[=<addr>]` | a flag on the existing command, since it already means "delete what I point at" |
| `heph tool gc` | already sweeps; gains the slot sweep (§11) with no new surface |

Each carries the `///` doc-comment-with-`Example:` shape the other `ToolCommands`
variants use, so `--help` reads consistently.

**`--scratch=on|refresh|off` stays a global build flag**, not a `tool` subcommand.
It modifies a build rather than being one — it belongs next to `--force` and
`--frozen` on `run`, and `heph tool scratch verify` is the command that *uses* it.

`push` and `pull` sitting in the same group as `ls` and `rm` is what makes the
asymmetry legible: a build pulls on its own and never pushes, so the only
scratch verb anyone ever has to *type* in CI is `push`.

## 14. Non-overlap: scratch vs. deps vs. outputs

Scratch is the only unhashed, mutable thing inside a sandbox, so every boundary
between it and the hashed world is a place a wrong build can enter. Three
disjointness requirements, in increasing order of how badly they fail.

### 14.1 Scratch ∩ outputs = ∅

An output collected from inside a scratch directory is an artifact whose bytes
came from an unhashed source, cached under a `hashin` that does not describe them.

Not hypothetical: `plugin-go/src/plugingo/thirdparty.rs` carries a comment about
exactly this — "the `**/*` output glob would otherwise capture the cache dir" —
and works around it by hand today.

Checked in `validate.rs`, which already has `paths_overlap` and `is_ancestor` for
codegen collisions. A def whose output path, dir, or glob **could** capture a
scratch path is a **hard error at parse time**. Globs are compared conservatively:
if the glob's literal prefix is an ancestor of the scratch path, it is rejected —
"might match" is treated as "does match", because the alternative is a check that
passes until the day a file appears.

### 14.2 Scratch ∩ dependency inputs = ∅

This is the one the ask names, and it is the most dangerous of the three, because
it fails in *both* directions:

- **Dep → scratch (poisoning).** A dependency materialized at a path under a
  scratch mount is written *into the slot* — and in `live` mode the slot is a
  symlink out of the sandbox, so the write lands in shared, persistent, unhashed
  storage and then travels to every other target on the slot and, with
  `remote = True`, to every other machine. Hermetic input bytes become
  unhermetic shared state.
- **Scratch → dep (shadowing).** Restored scratch content sitting at a path where
  a dependency is expected shadows that dependency. The target then reads bytes
  that no `hashin` describes while heph believes it read the dep. That is a
  **silent wrong build**, and it is the one failure mode this whole design must
  not permit — everything else here degrades to "slower".

Enforced in two places, because neither alone is sufficient:

1. **Static, at validate time.** Compare each scratch path against the target's
   own declared input/tool/support-file materialization roots and against the
   declared output paths of its direct deps, which the linked def already knows.
   Cheap, catches the authoring mistake at the moment it is made, and reports both
   addrs.
2. **Dynamic, at materialization time — authoritative.** The sandbox materializer
   (`driver-support/src/driver_managed.rs`, and the FUSE bridge) already visits
   every input's destination path to link or copy it. Before writing, check the
   destination against the target's scratch roots — a prefix comparison against a
   sorted list that is essentially always ≤ 2 entries, so `O(1)` per file and
   invisible in the profile. A hit is a **hard error**, not a skip: a silently
   dropped input is the same wrong build by another route.

The static check exists for the error message; the dynamic check exists for the
guarantee. Only the dynamic one sees the paths that dep outputs actually resolve
to, and only the static one can name the BUILD-file line.

### 14.3 Scratch ∩ scratch = ∅, and reference well-formedness

- Two referenced scratches whose declared `path`s overlap may not be referenced by
  the same target, and the same scratch may not be referenced twice.
- A declared `path` must be **relative, `..`-free, and inside the sandbox ws** —
  checked on the *declaration*, so it is reported once at the source rather than
  once per consumer. A scratch directory is a symlink *out*; its mount point is
  not negotiable, and §5.2 gives no way to move it.
- A referenced addr must resolve, and must resolve to a **scratch** target. Both
  are ordinary addr errors rendered the way heph renders every other one (§5.4).
- **Two references resolving to the same environment variable** are a hard error
  naming both addrs (§5.5), as is a target's own `env`/`runtime_env` declaring a
  variable a referenced scratch owns.

Note what is *absent*: an earlier revision declared the slot's settings inline at
every use site and therefore needed a rule that all consumers of a name agree
about `access`, `version`, `remote` and `max_size`. With a declaration target
there is one copy of each, so the disagreement is not detectable — it is
inexpressible. Likewise the reserved `heph`-prefixed name namespace is gone:
packages already namespace, so a driver's internal cache is `//@heph/go:golist`.

### 14.4 Snapshot hygiene

When tarring a slot for a remote push, refuse to follow symlinks out of the slot
and refuse to archive sockets/devices. A slot that has acquired a
symlink into the stage tree or into another sandbox would otherwise publish a
dangling absolute path to every machine that restores it.

## 15. Cost budget

`feature-quality`'s question — what it costs when nobody uses it, and when they do:

| path | cost |
|---|---|
| target with no `scratch` | one empty `Vec` on `TargetDef`, one `is_empty()` branch in `execute`. Zero syscalls, zero allocations |
| cache hit on a scratch target | **zero** — never reached (§4.2) |
| miss, warm slot | one `symlink(2)` in, one `unlink(2)` out, one lock acquire (two `flock`s) |
| first build on a new branch | one seed copy of the slot — reflink where available, otherwise a plain copy. **Once per (slot, branch)**, never per run (§10.7) |
| dep-overlap check | one prefix comparison per materialized input against a ≤2-entry sorted list |
| local resolve | one `head.meta` read per candidate scope (typically 2), once per slot per invocation. No network |
| remote resolve | one `list_names` + one small `.meta` GET per candidate scope, **only when the local store is cold** |
| remote pull | one object fetch + untar, only when the local store is cold |
| publishing | zero during a build. One tar + upload when `heph tool scratch push` is run, which is once per CI job |
| remote push | one tar + upload on the background lane, **once per slot per invocation** |
| memory | `O(slots)`, not `O(targets)` — a key, a path, a lock handle |

The number to watch in the bench harness is **`exclusive` lock wait**: the only
cost here that scales with the graph rather than with the slots.

## 16. Compatibility

`compatibility` is a mechanical trigger — this touches `proto/`,
`plugin-abi`/`ABI_SEMVER`, the CLI surface, the on-disk format and the remote wire
format. Claims to be checked:

- **Plugin ABI, declaration + reference: unchanged.** No new message, no new
  field, no bump. A reference travels on `Input.annotations` and the settings are
  read from the declaration's spec config, both of which every ABI version already
  carries. An old plugin and a new host, and the reverse, simply see an annotation
  they do not act on.
- **Plugin ABI, mounting: one additive field on `RunRequest`** and an
  `ABI_SEMVER` minor bump (§5.3's correction). Free for in-process managed
  drivers; a cdylib driver must be rebuilt before it can mount a scratch, and
  until it is, its targets run without one — a *slowdown, not a wrong build*,
  because the engine's lock is keyed on a declaration the old plugin never sees,
  so there is no shared directory to race either. Worth keeping that degradation
  intact when the field lands.
- **On-disk:** `<home>/scratch/` is a new tree; nothing existing changes shape.
  `<slot>.meta` is versioned borsh. An unreadable or unknown-`SCRATCH_FORMAT` slot
  is *deleted*, not migrated — it is a cache.
- **Remote wire:** a new, dedicated `scratch/v1/` prefix, disjoint from the
  existing blob namespace. An older heph never lists it and never fetches it. The
  `v1` and the `SCRATCH_FORMAT` in `.meta` are the two independent version knobs —
  the first for the key layout, the second for the payload.
- **Remote GC:** §10.11 recommends bucket lifecycle rules precisely to avoid adding
  `delete` to `RemoteCacheBackend`, which every backend and test double would have
  to implement.
- **Scope naming is part of the key**, so changing `scratch.scope`'s expression
  (say from `${GITHUB_REF_NAME}` to a slug of it) orphans every existing lineage.
  That degrades to a cold run and self-corrects on the next push, which is the
  right severity — but it means the scope expression is closer to configuration
  than to a value, and renaming it should be a deliberate act. Worth a line in
  the config docs, not a version knob.
- **Starlark:** a new driver name (`driver = "scratch"`) with its config fields,
  and a new optional `scratch` attribute on `exec`. **No new global**, so nothing
  a workspace may already have defined can collide, and existing BUILD files are
  untouched. Both are API once shipped, so §5's shape should be settled before,
  not after — but this is the only genuinely new, permanent surface here; the
  plugin ABI, the proto and the cache formats all stay put.
- **CLI:** new `heph tool scratch` subcommand group, new `--scratch` flag, new event kinds in
  `--json`. Additive; consumers ignoring unknown event kinds are unaffected, which
  is the existing contract.

## 17. Testing

Per `.claude/testing.md`, everything is `crates/e2e` (in-process, real `Engine`)
unless it structurally cannot be.

**Unit (`scratch.rs`, `scratch_remote.rs`):** slot key stability and sensitivity
(addr/version/os/arch each move it; `path`, `access` and `remote` do not); meta
round-trip; sorted
multi-slot lock ordering; the `(generation, bytes, content_hash)` total order,
including forks; key formatting and lexicographic-equals-generation ordering with
generations spanning the zero-padding width; scope precedence.

**`crates/e2e`:**
- a target writes a marker into scratch; a later run with a *changed input*
  (forcing a miss) sees the marker
- a cache hit neither restores nor takes the lock
- two targets sharing an `exclusive` slot never overlap (each writes an
  exclusivity token and asserts it was alone); they *do* overlap under `shared`
- `[a, b]` and `[b, a]` complete without deadlock (§8.2 rule 3)
- a failing target does not clobber the previous head; a cancelled run does not commit
- cancellation saves nothing
- **an output glob overlapping a scratch path is a parse error** (§14.1)
- **a dep materializing under a scratch path is a hard error** — both the static
  and the dynamic check, each with the other disabled, so neither is silently
  load-bearing for the other's coverage (§14.2)
- **restored scratch content cannot shadow a dep** — the wrong-build case, asserted
  as an error rather than as a hashout
- a reference to a non-scratch addr, and two references colliding on one
  `SCRATCH_*` variable, each error naming both addrs
- `--scratch=off` produces byte-identical `hashout`s to a warm run — §4 as an
  executable test
- eviction: a slot over `max_size` is dropped; `heph tool clean --scratch` removes it
- **a build never pushes.** Asserted directly against a counting remote: a full
  build of a graph with `remote = True` slots issues **zero** writes to the remote,
  however many targets ran and whether they passed or failed
- **`heph tool scratch push`:** publishes a dirty head at `parent + 1`; skips a
  clean one; `--force` publishes anyway; a failed upload leaves `dirty` set so the
  next push retries; `write: false` refuses and says so; exits non-zero when any
  selected slot fails; `--all` selects exactly the `remote = True` slots
- **push takes the write guard:** with a `shared` reader held, the push blocks
  rather than tarring under it, and completes once the reader drops — the §8.1
  claim, asserted rather than asserted-in-prose
- **seed on fork:** the first build in a new scope copies the fallback's head,
  sets `parent` to it, and leaves the fallback's head byte-identical;
  `seedOnFork: false` starts cold instead
- **no ABI surface:** a driver built against the pre-scratch `ABI_SEMVER` loads,
  runs, and its targets build — the scratch annotations pass through it untouched.
  This is the §5.3 claim, and it is the one that would silently stop being true
  the first time someone adds a typed field
- **`apply_transitive` ordering:** a consumer's `def.hash` is byte-identical with
  and without a scratch reference, and unchanged by a `version` bump on the
  referenced scratch — the structural half of §6.3
- **the declaration/reference split:** two targets referencing one scratch resolve
  to the same slot; `heph query revdeps` on the declaration lists both; a reference
  to a missing addr is `TargetNotFoundError`; a reference to a non-scratch target
  is a wrong-kind error naming both
- **nothing about scratch reaches `hashin`** (§6.3), asserted against the def hash
  *directly* — a consumer's def hash must be byte-identical with and without a
  reference, and unchanged by a `version` bump on the referenced scratch.
  Asserting on `hashout` instead is **not** sufficient and is a trap worth naming:
  a target whose key moved still produces identical bytes, so a hashout comparison
  passes while the cache silently misses on every run. The `version` bump is the
  case most likely to be "fixed" into a rebuild later, so it gets its own test.
  Watch this when the mounting path lands: pluginexec's `apply_transitive`
  re-seeds the def hash, so routing anything scratch-shaped through the transitive
  sandbox would move the key of every consumer that has one
- **`platform`:** `os_arch` gives different slots on different os/arch; `any`
  gives one slot across all of them; `os` splits on os only — asserted on the key,
  not on behaviour
- **`version_fallback`:** off, a version bump is fully cold; on, it seeds from the
  most recent other version of the same slot, and the scope fallback is exhausted
  first (§10.5)
- **no mount override exists:** a scratch mounts at its declared `path` in every
  consumer, and there is no attribute form that says otherwise (§5.2)
- **`SCRATCH_<NAME>` env:** injected with the canonical path, not the sandbox
  symlink; absent when no scratch is referenced; derived from the target name, not
  the package; two references colliding after sanitization is a parse error; the
  explicit `env =` form overrides
- **lineage, against the in-memory remote backend:**
  - generation increments from the restored head; a cold runner starts at 0
  - the head is the max under the §10.6 order; a same-generation fork converges
    (two writers at N+1 → every reader picks the same one)
  - a *late* writer descending from an old generation does not become the head
  - a build issues no remote writes at all, whatever the target count
- **branch lineage, the §10.2/§10.4 model — run against the local store *and* the
  remote one, from the same table, because it is one algorithm (§10):**
  - a run in scope `pr` restoring from `master` writes into `pr`, never `master`,
    and records the fork point — the isolation property, asserted directly
  - `restoreScopes` order decides: with entries in both `pr` and `master`, `pr`
    wins; with `pr` empty, `master` is used; with neither, cold
  - **generations are never compared across scopes** — `pr` at generation 40 does
    not beat `master` at 12 by generation, and the resolver has no path that could
  - **drift guard:** a `pr` head forked at `master` 12 with `master` now at 400
    and `maxDrift = 100` resolves to `master`; forked at 398, it resolves to `pr`;
    `maxDrift` unset resolves to `pr` in both cases (strict precedence)
  - a missing or deleted `parent_scope` leaves drift uncomputable and falls back
    to strict precedence rather than erroring
  - a corrupt head `.meta` falls through to the next candidate, then to cold
- **the local branch-switch story, end to end:** build on `master`, switch scope
  to `feat-x`, build again — the second run seeds from `master`'s local head,
  writes into `feat-x`, and leaves `master`'s head exactly as it was; switching
  back finds `master` unchanged
- **local-first resolution:** a warm local head makes no remote call at all;
  a cold one pulls transparently with nothing configured beyond the remote;
  `--scratch=refresh` pulls even when warm

**`crates/bin-e2e`:** only the cross-process lock — two `heph` processes
contending for one `exclusive` slot must serialize. No in-process form exists (the
`mem` backend serializes only within a process), which is exactly the bar that
crate sets.

**`crates/bench`:** a corpus scenario with warm vs cold scratch, so the win is a
tracked number rather than a claim. Given §2.1, **no performance claim in this
document should be believed until it is an interleaved A/B on the corpus.**

## 18. Phasing

1. **The declaration.** ✅ *PR #403.* The `scratch` builtin driver, its config
   schema, and mount-path validation at the declaration — `target(driver =
   "scratch", …)`, no new Starlark global.
2. **The reference.** ✅ *PR #407.* The `scratch` attribute on exec, resolved to an
   `Input` with `hashed: false, runtime: false`, plus every check in §13 that does
   not need storage: wrong-kind addr, env collisions, mount overlaps, duplicates.
   Nothing observable happens yet beyond `heph query` — deliberately, so the
   checks land before anything can write.
3. **Mount + lock + env.** ✅ *PR #412.* The symlink (created by the bridge,
   which owns sandbox creation and may redirect into a FUSE mount), the keyed
   cross-process slot lock — shipped *with* `access`, since an `exclusive` that
   does not serialize is a silent lie — and the declaration's env var. Events
   deferred to the observability phase. Where the `golist_gocache` win becomes
   general.
4. **The lineage model + scopes, local only** (§10.1–§10.7). Branch-aware local
   caches, `${git:branch}`, the drift guard. Reviewable without any network, and
   the piece the remote phase then reuses wholesale rather than reimplements.
5. **Eviction + the read-only half of `heph tool scratch` + `--scratch` modes.**
   Makes it operable and inspectable.
6. **Remote pull** (§10.4, §10.8) — resolve, fetch, seed. A CI runner gets warm
   from whatever is already published, with no workflow change. Useful on its own
   the moment anything is in the bucket.
7. **`heph tool scratch push`** (§10.10) — the publish half, and the only part CI
   has to be told about.
8. **Retrofit `plugin-go`** (§11.1), in its own order: the golist `GOCACHE` first
   (delete `golist_gocache.rs`), then compile's per-sandbox one, then the
   `GOMODCACHE`/`GOPROXY` passthrough — each measured separately, because §2.1 is
   the standing reminder that an obvious cache win can move wall time by zero.
   This is the proof that the general mechanism subsumes the special case.
9. **Retrofit `plugin-oci`** (§11.2) — the shared blob store, then the local
   BuildKit layer cache. Not a prerequisite for anything; it is the evidence that
   a second ecosystem adopts the mechanism without it growing new knobs.

1→2 is a genuine stack (2 cannot be reviewed without 1). 3 and 4 branch off
`master` in parallel once 2 lands; 5 needs 3, and 6 needs 5. Ordering 3 before the
remote phases is the deliberate choice: it puts the lineage model — the part with
the subtle semantics — in front of a reviewer with no network, no tarballs and no
CI fixture in the way, and leaves 5 and 6 as plumbing over an algorithm that
already has tests. Splitting 5 from 6 matters too: pull is what makes CI faster
and it lands without anyone editing a workflow.

7 is the gate, not the victory lap: if the Go caches cannot be expressed as
declarations, §5's surface is wrong, and it is much cheaper to learn that before 8
than after.

## 19. Alternatives considered

- **Per-target restore/save into every sandbox (the literal ask).** This was
  `mode = "snapshot"`, and §7.3 now proposes dropping it: §2.1 measured that exact
  shape at a 60% CPU saving and *zero* wall-clock improvement.
- **Pushing automatically at the end of a run.** Needs a directory nobody is
  writing, which means either a per-run clone (a full tree copy on ext4, §7.2) or
  a barrier that must skip silently when contended. Making push a command gets the
  same barrier with none of the cost, and moves the "should this become the
  branch's head?" decision into CI's `if:` conditions where it is answerable.
- **A per-run clone** (`mode = "run"`). Deferred rather than rejected — §7.2.
- **Scoping the remote by branch but not the local store.** The cheaper half, and
  it leaves the developer with the bug the whole mechanism exists to fix: every
  `git checkout` hands the arriving branch the departing branch's cache state and
  hands it back mutated. Branch switching is a local event first and a CI event
  second.
- **A mutable `HEAD` pointer object in the remote.** The obvious "latest"
  mechanism, and wrong before you even reach the backend: one remote serves many
  branches concurrently, so there is no single latest for a pointer to name. A
  pointer *per* branch models it, at the cost of an unbounded set of mutable
  objects and no way to relate the heads the cross-branch restore has to compare.
  The backend then rules out even that — no CAS, and last-writer-wins ranks by
  finish time rather than by descent. §10.1.
- **Timestamps instead of generations.** No coordination needed, but wrong under
  clock skew across runners, and wrong in principle: recency is not descent.
  Note this is *not* the same question as cross-branch staleness, which §10.4
  answers with fork distance — also without a clock.
- **Strict scope precedence, GHA's exact behaviour.** Available (`maxDrift`
  unset) and rejected as the default: a long-lived branch's own cache keeps
  winning as it ages, so the build that most needs a warm cache is the one
  guaranteed a stale one. §10.4.
- **Merging forked lineages.** Cannot be done correctly without understanding each
  tool's cache format, and by §4 is never *necessary*. Explicitly out of scope,
  for same-branch forks and cross-branch alike.
- **A typed `ScratchRef` on `TargetDef`, with a proto message and an ABI bump.**
  What an earlier revision of this design specified. Cleaner to read, and it costs
  a version negotiation, `plugin-abi` conversion code, and — the real price —
  third-party drivers must be recompiled to participate. `transitive` +
  `annotations` gets the same behaviour with none of it (§5.3), at the cost of
  stringly-typed settings on the wire, which is the trade `read_only` and
  `stage_per_file` already made.
- **Referencing a scratch in `deps`**, letting `apply_transitive` demote the input
  to `hashed: false, runtime: false`. Works, and needs no new attribute anywhere —
  but it puts scratch in the same list as real inputs, which is exactly the
  distinction §14.2 exists to protect, in the place an author reads first. A
  separate list costs one spec field in pluginexec and no ABI surface (§5.2).
- **Declaring the slot inline at each use site**, joined by a bare string name.
  The original shape here, and it made every consumer restate `access`, `version`,
  `remote` and `max_size`, then needed validation that they all agreed — a
  discovered error where a declaration target makes it inexpressible. It also put
  cache identity in a flat global string namespace with no packages, no
  visibility, and no way to ask who uses one.
- **Let each driver do it.** The status quo (`golist_gocache.rs`). No lock, no
  eviction, no visibility, no remote, and a fresh reimplementation per ecosystem —
  each an unreviewed hole in the sandbox.
- **A "cache target" whose output is the directory.** Reuses all the existing
  machinery, but the directory is then content-addressed by `hashin` — which is
  exactly the property that makes it useless. A cache readable only at the input
  hash that produced it is not a cache.
- **Reuse the target cache with a sentinel `hashin`.** Cheaper to build, but puts
  mutable, unhashed state inside a store whose entire contract is "keyed by
  inputs, immutable per key"; every invariant in `local_cache.rs`, `gc.rs` and
  `remote_cache.rs` would need an "unless it is scratch" clause.
- **`bind`/`overlayfs` instead of a symlink.** Real isolation with rollback, but
  Linux-only (macOS has neither), so it violates the uniform-across-the-three-
  targets rule. A symlink behaves identically everywhere.

## 20. Risks

- **The shadowing hole (§14.2) is the only path here to a wrong build.** It is the
  one thing in this document that must be got right; everything else degrades to
  "slower". Both checks, and the tests that disable each one, are non-negotiable.
- **`exclusive` is a footgun** at scale (§8.3). Mitigated by visibility and a warn,
  not eliminated.
- **A path-sensitive cache can restore and still be inert** across machines
  (§10.9) — it looks like a hit and performs like a miss. Mitigated by the
  `debug!` and by `scratch.root`; not eliminated.
- **Both stores grow with branch count** once scoping is on. Remotely nothing in
  heph collects a dead branch's lineage at all (§10.11) — only the bucket's
  lifecycle rules do. Locally the LRU collects it, but a developer who works on
  many branches holds many heads, and only a CoW filesystem makes that cheap.
- **Nothing publishes unless someone remembers to run `push`.** A workflow that
  omits it gets a cache that pulls forever and never advances — fast enough that
  nobody notices it is stale. That is the cost of explicitness, and the mitigation
  is diagnostic rather than structural: `heph tool scratch ls` shows `dirty`, and
  `heph tool scratch head` shows a head that has not moved.
- **The seed-on-fork copy is a real cost on ext4 for a large slot** (§10.7).
  Amortized over a branch's lifetime and comparable to a cold rebuild, but it lands
  as a one-off pause on the first build after a checkout, which is a surprising
  place to spend time. `ScratchSeedStart/End` is what makes it visible.

- **`maxDrift` is a number nobody has data for yet.** Too low and every branch
  build throws away its own incremental work; too high and the guard never fires.
  It is configurable and observable (`heph tool scratch head`) precisely because the
  default will be wrong for someone.
- **A wrongly-declared `platform = "any"` is a real wrong-answer risk**, and the
  only author assertion here that is. `access = "shared"` and `version_fallback`
  fail into slowness; a cache that turns out to be host-specific — a
  `node_modules` with native addons, a store holding compiled objects — restores
  onto the wrong machine and is used. §6.1 states what `any` asserts; the default
  is the narrow one for exactly this reason.
- **A poisoned slot survives until evicted.** Self-healing for the caches this
  targets; `heph tool scratch rm` for the rest.

## 21. Decisions taken, and what is still open

### Settled

- **A scratch is a declaration target, referenced by addr.** Settings live in one
  place; sharing is what addrs already do; `heph query revdeps` answers who shares
  a cache. §5.
- **It is a builtin *driver*, shaped like `plugingroup`** — the existing way heph
  gains a target kind, with `#[derive(Spec)]` supplying the parser and the LSP
  schema. §5.1.
- **A scratch mounts at its declared `path`, everywhere.** No per-consumer
  override; `scratch = [addrs]` is the only reference form. Location is part of
  what a cache is (§5.2).
- **`platform` is declared, not assumed.** `os_arch` by default, `any` for a
  cache that is genuinely portable — which makes one slot serve every machine on
  the lineage. §6.1.
- **`version` gets an opt-in fallback**, credited to GHA's `restore-keys`: a
  `version` that is a lockfile fingerprint should inherit the previous version's
  cache, while a `version` that is a bust handle must not. §10.5.
- **Nothing new crosses the plugin ABI.** No proto message, no `TargetDef` field,
  no `ABI_SEMVER` bump: scratch rides on `TargetSpec.transitive` and
  `Input.annotations`, which already exist and already carry producer→host
  metadata. A side effect worth naming: because settings arrive via
  `apply_transitive`, which runs *after* `def.hash` is set, §6.3 is enforced by
  the shape of the code rather than by a rule. §5.3.
- **Scratch contributes nothing to `hashin`** — not content, not declaration, not
  `version`. It follows from §4, and the intuitive alternative is an over-hash.
  §6.3.
- **`access = "shared"` ships in v1**, as a "trust the tool" assertion by the
  author. The golist targets must not run serially. §8.1.
- **Pull is automatic; push is `heph tool scratch push`.** No end-of-run push, in
  any mode. This supersedes both the earlier "a `shared` slot is local-only" and
  the `run`-mode-required rule: the write guard the command takes is the coherent
  moment, so `access` and `remote` are independent and `GOCACHE` gets its CI cache.
  §8.1, §10.10.
- **The per-run clone is deferred**, not adopted: its cost is a full tree copy per
  invocation on ext4 and its justification went away with the automatic push. §7.2.
- **State is a tree of per-branch lineages**, not one line, and not a mutable
  pointer per branch either. §10.1, §10.2.
- **Cross-branch restore is first-class and configurable**: try the current
  branch, then the configured fallbacks, in order. §10.3.
- **The lineage applies locally too**, not only in CI — a developer switching
  branches gets the same isolation and the same fallback, and seeding on fork makes
  a branch switch cost one copy rather than a rebuild. §10.3, §10.7.

### Open

- **Q1 — Naming.** `scratch`, or something else? (§5.6) Locks in the Starlark API
  and the CLI, so it is cheapest to settle now.
- **Q2 — Does `seedOnFork` default on?** On is what makes branch switching worth
  anything, and it is the one copy in the design — a plain copy on ext4 for a slot
  that could be gigabytes, paid as a pause on the first build after a checkout
  (§10.7). Off means new branches start cold and nothing ever copies. This is the
  design's remaining **per-platform cost question, which CLAUDE.md reserves for
  you**.
- **Q2b — Is `version_fallback` off by default the right call?** §10.5. Off treats
  `version` as a bust handle, which is what `version = "go1.23"` means; on treats
  it as a fingerprint, which is what `hash_files(...)` means. Inferring which from
  the value would be magic, so it is a flag — but a flag whose default is wrong
  half the time is worth a second look.
- **Q3 — Is the deferred per-run clone worth revisiting later?** §7.2. It buys
  whole-run rollback and isolation between concurrent `heph` processes, on CoW
  filesystems only. Cheap to add once the rest is shipped; easy to never miss.
- **Q4 — Does the drift guard ship on by default, and at what `maxDrift`?** Off
  (unset) is GHA's exact behaviour and predictable; on avoids the long-lived
  branch going quietly cold. §10.4. The number itself wants data, not reasoning.
- **Q5 — Is `${git:branch}` the right local default for `scope`?** Branch scoping
  on by default is what makes local branch switching work with no configuration —
  but it multiplies a developer's cache by branch count, and `scope: ""` (one
  lineage, today's behaviour) is the conservative alternative.
- **Q6 — Remote pruning:** bucket lifecycle rules (§10.11, recommended) or a new
  `delete` on `RemoteCacheBackend`? Scoping makes this urgent rather than tidy.
- **Q7 — May a target with scratch push its *outputs* to the remote cache?** Yes
  by default (else CI gains nothing) means the remote can carry an artifact built
  against a warm, unaudited cache. Forcing `remote_enabled = false` on scratch
  targets is much safer and guts the main use case. §13.2's audit mode is the
  proposed middle path.
- **Q8 — Should CI publish after a failed build?** heph no longer decides this —
  it is an `if:` condition on the push step. The recommendation to put in the docs
  is `if: always()`: a cache warmed by a partially failing build is still warm, and
  the head it publishes is no less valid than one from a green run. Worth stating
  as guidance, since the intuitive choice is `if: success()` and it is wrong.

---

## Appendix: notes from implementing phases 1–3

Things that only showed up in code, recorded so the next phase does not re-learn
them.

- **An `EResult` holds a riding read lock on its addr.** A test that keeps a run's
  result alive across `reopen()` deadlocks the second engine's *write* lock
  against it — and only when the second run actually re-executes, since a cache
  hit takes a read lock and coexists happily. Cost ~30 minutes of hang before it
  surfaced as an unrelated-looking `ENOENT` on a lock file. `engine_core.rs`
  already had the idiom (`drop(result)` before reopening); it is worth stating.
- **`cargo build --workspace` does not build test targets.** Adding a field to
  `RunRequest` compiled clean and then broke 28 construction sites in `#[cfg(test)]`
  code. `--all-targets` is the check that matters, exactly as `.claude/rust.md`
  says for clippy.
- **Zero outputs is a supported target shape.** A declaration is executed like any
  other target when someone resolves it directly, so its `run` must succeed and
  produce nothing. The first version treated being run as an engine bug, which was
  simply wrong.
- **Two `ScratchMount` types is one too many.** The engine's resolved form and the
  driver-facing form are the same thing; keeping the contract type and deleting the
  engine-local duplicate removed a conversion that could drift.
