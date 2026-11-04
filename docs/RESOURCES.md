# Target resources

A target says what it costs to run; the engine admits targets so that the sum
of what is running fits the machine, and tells each target what it got.

```python
target(
    name = "build",
    driver = "bash",
    run = "make -j$HEPH_CPUS",
    resources = {"cpu": 4, "memory": "8G"},
)
```

**`-j N` now means CPUs, not targets.** `-j 8` lets the run reserve 8 CPUs at
once: eight targets that declare nothing (each costs 1), or two that declare
`cpu: 4`. The default is the core count, where it used to be `2 × cores`
target slots — see [Compatibility](#compatibility).

heph **tells**, it does not police. Nothing kills, throttles or pins a process
that uses more than it reserved (no cgroups, `ulimit` or affinity), and no
resource value reaches any hash.

## The model

A **pool** is a named capacity. Three kinds:

| Pool | Unit | Default capacity |
|---|---|---|
| `cpu` | CPUs; held as micro-CPUs (10⁻⁶ CPU) | the engine's parallelism (the core count) |
| `memory` | bytes | the memory this process may use: on Linux the cgroup limit when one is set (v2 `memory.max`, v1 `memory.limit_in_bytes`, capped at physical memory), else physical memory; on macOS physical memory |
| a custom pool (`docker`, `gpu`, …) | a count | none: 0 until the config or a run gives it one |

The `memory` default differs by platform on purpose: cgroup-aware on Linux,
physical memory on macOS. A target that declares `memory` can therefore wait
in a Linux CI container (limited to, say, 8G) where it ran at once on a 32G
laptop; set `resources: {memory: …}` to make the two agree.

Pool names are `[a-z][a-z0-9_]{0,63}`; a target names at most 16.

A target reserves from pools with `resources`, a dict of pool to amount:

- `cpu`: a number ≥ 0, integer or decimal in steps of 0.000001
  (`0.5` is two per core), rounded to the nearest step; a value above 0 and
  below one step is an error.
- `memory`: a whole number of bytes or a size string: `"4G"`, `"4g"`,
  `"4GiB"`, `"4GB"`, `"4096M"`, `"4096"` (binary units throughout; `4G` is
  4·2³⁰). No spaces, decimals, signs or separators.
- a custom pool: a whole number ≥ 0.

Non-string keys and any other value shape are errors that fail the driver's
`parse` with the target named.

**Defaults.** A target that declares no `cpu` reserves **1 CPU**, whatever its
driver, including a def from a plugin built before resources existed. Two
exceptions: `fs` targets and *relocating* `group`s reserve 0.00001 CPU (10
micro-CPUs) unless they declare their own, so they still queue through the
`cpu` pool in order but 100,000 of them fit in one CPU. An aggregate `group`
(no transform) never executes, so whatever it declares reserves nothing.
Undeclared `memory` and custom pools count as 0.

**The light lane.** A target that asks for 0 of everything (`cpu: 0` and
nothing else) takes one slot of a separate `light` lane instead, whose
capacity is `max(8 × cpu capacity in CPUs, 64)`. It never waits behind `cpu`
requests, and the number of concurrent executes stays bounded. `cpu: 0` with
`memory: "8G"` still queues on memory.

`resources` is **ordinary driver config**, not a keyword of `target()`: it
rides the target's config to the driver's `parse`, which moves it onto the def
with the SDK helper (`Resources::from_config`). That is how a BUILD file, a
plugin function's declared target and a provider all reach it the same way.
Every builtin driver that runs something reads it; `scratch` and `credential`
declarations run nothing and refuse it, and an `exec`/`bash` entrypoint
refuses it at parse (an entrypoint reserves nothing, see below).

## Capacities

In `.hephconfig`, one entry per pool:

```yaml
resources:
  cpu: 8          # or 50%: a percentage of the engine's default
  memory: 24G     # or 75%
  docker: 2
  gpu: 0          # the pool exists; the run sets its capacity
```

`resources:` merges **per pool** across `HEPH_PROFILES`: a profile that sets
`cpu` keeps the base's other pools.

A capacity of **`0` in the config** means "this pool exists; the caller sets
its capacity per run". BUILD files naming it validate on every machine, and a
run that reaches a target needing it fails at admission, before the target
runs:

```
//p:t acquires gpu, but pool gpu has capacity 0 and must be set: --resource gpu=<n>, HEPH_RESOURCE_GPU=<n> or resources: {gpu: <n>} in .hephconfig
```

A pool that is not declared at all is an error when the def is linked, which
`heph validate` reports too:

```
//p:t declares resources.docker, but docker is not a pool: add resources: {docker: <n>} to .hephconfig (known: cpu, memory)
```

Per-run overrides, each at least 1:

- `--resource <name>=<n>`, global and repeatable.
- `HEPH_RESOURCE_<NAME>=<n>` in heph's own environment (`<NAME>` is
  lowercased into the pool name). A malformed one is an error naming the
  variable, never ignored.
- `-j N` / `--jobs N` is `--resource cpu=N`: CPUs the run may reserve at once,
  **not** a target count. Giving both `-j` and `--resource cpu=` is an error.

`cpu` and `memory` also take a percentage of the engine's default in all three
places (`-j 50%`, `--resource memory=75%`, `cpu: 50%`): cpu rounds to the
nearest micro-CPU, memory floors to bytes, over 100% is an overcommit by
choice; `0%` and a percentage on a custom pool (no default to take it of) are
errors.

Precedence: flag, then environment, then config, then the engine's default
(which exists only for `cpu` and `memory`).

`-j` sizes the `cpu` pool only. The engine's own fan-outs (cache pipes, gc and
clean loops) keep following its parallelism, and `-j 1` serialises neither
them nor `cpu: 0` targets.

## Admission

Each execute reserves its amounts in every pool it names before it runs and
releases them when the run ends (the cache write after it is outside the
reservation).

- **Strict FIFO.** The head of the queue waits until every pool it needs fits
  at once, and nothing behind it is admitted while it waits. No starvation,
  deterministic, at the cost of idle capacity while a large request waits.
- **Atomic.** A grant covers every pool or none.
- **Clamped.** A request above a pool's capacity is reduced to the capacity:
  `cpu: 16` on 4 waits for an empty pool, runs alone and is told 4. The clamp
  is logged at `info` once per target per request and carried on the event
  (`declared` vs `reserved`); the TUI row and the CI `running` line show it
  as `cpu 16→4`.

`ExecuteStart.queued_ms` is how long each admitted run waited, so a short
wait (under the notice threshold below) is still answerable from the stream.

**Seeing it.** A wait that outlasts the notice threshold (5 s) is announced as
`AdmissionWaitStart` / `AdmissionWaitEnd { addr, pool, need, free, ahead }`
and rendered as a row in the TUI and the GitHub Actions report:

```
⧗ //a:b waits for cpu 4 (2 free, 6 ahead, 41s)
```

`RequestConfig` announces each pool's capacity, and `ExecuteStart` carries the
`declared` and `reserved` amounts. The in-flight report (SIGQUIT) prints each
pool's held/capacity, the queue length and what the head waits for and for how
long.

**Entrypoints** (`heph run` of an entrypoint target) reserve nothing and are
told the full `cpu` capacity: they run in the caller's terminal, one at a time.

## What a target sees

The allotment reaches the driver as `RunRequest.allotment` (across the plugin
ABI as `ManagedRunRequest.allotment`, since 0.17.4). The `exec` and `bash`
drivers export it:

| Variable | Value |
|---|---|
| `HEPH_CPUS` | whole CPUs: the granted amount rounded up, at least 1 (a `cpu: 0` target is told 1) |
| `HEPH_MEMORY_BYTES` | bytes, only when the target declared `memory` |

They are inserted after the `pass_env` / `runtime_pass_env` wildcard
passthroughs, so a nested heph sees its own allotment rather than its
parent's, and a target's own `env` / `runtime_env` entry of the same name wins.
They pass through an exec runner like the rest of the environment. Drivers that
spawn nothing ignore the allotment.

heph exports **only its own names**. Mapping them onto a tool is the author's,
through a `${resource:cpu}` / `${resource:memory}` deferred value in an `env`,
`runtime_env` or transitive `env` value (see `docs/DEFERRED_VALUES.md`). Those
are the only fields that take one: anywhere else (a `run` line, a `template`
var, an `oci` push `dest`, a credential presentation) the number could reach
an output and make its hashout per-machine, so heph refuses it when the def
is built. It is substituted at run with the same text as
`HEPH_CPUS` / `HEPH_MEMORY_BYTES`; a transitive value resolves with the
*dependent's* allotment. The def hash covers the reference text, so the mapping
never moves a key:

```python
target(
    name = "lib",
    driver = "bash",
    run = "make",
    env = {"MAKEFLAGS": "-j${resource:cpu}"},
    resources = {"cpu": 4},
)

target(
    name = "crate",
    driver = "bash",
    run = "cargo build --release",
    env = {"CARGO_BUILD_JOBS": "${resource:cpu}"},
    resources = {"cpu": 8},
)
```

In a bash `run`, `$HEPH_CPUS` is ordinary shell expansion of the exported
variable. `${resource:memory}` on a target that declared no memory is an
error, not an empty string. Only the two exact names are heph's:
`${resource:cpus}`, `${resource:-x}` and any other `${resource:…}` pass
through untouched, like any `${…}` heph does not own (custom pools are not
exposed).

**Go.** heph sets neither `GOMAXPROCS` nor `GOMEMLIMIT`, for any generated Go
target or Go driver: a Go process sizes itself from the host as before, and
the pool bounds how many run at once. A workspace that wants it tighter maps
`GOMAXPROCS` itself with `${resource:cpu}`.

## Sizing a declaration

`heph.core.resource("cpu")` returns the host's CPU capacity as a float (cores)
and `heph.core.resource("memory")` the host's memory in bytes as an int, as the
engine reads them (cgroup-aware). It is the **host's** value, not this run's
pool: `-j` and `--resource` do not change it. Custom pools have no host value.

```python
target(name = "link", ..., resources = {"cpu": heph.core.resource("cpu")})       # full width
target(name = "half", ..., resources = {"cpu": heph.core.resource("cpu") / 2})
```

The intended use is exactly that, a `resources` amount. Anything else puts a
per-machine number into driver config and so into the def hash of the target
and every consumer, which nothing can check for you.

**Go tests.** The go `provider_state`'s `test` struct takes the same map, for
the generated test run targets (`test`, `xtest` and their `_race`
counterparts; not compiles, links or `build_test`):

```python
provider_state(provider = "go", test = {"resources": {"cpu": 4, "memory": "2G"}}, recursive = True)
```

States combine shallow to deep: per pool, the deepest declaration wins.

## Hashing and the contract

**Nothing about resources reaches any hash.** The declaration is kept off every
driver's def hash (exec's `Hash for TargetDef` skips it as it skips
`runtime_env`), and the allotment env is runtime-only. Changing `resources`, or
running under another `-j`, keeps the cache hit.

The contract that follows: **a target's outputs must not depend on its
allotment.** An artifact built under `HEPH_CPUS=8` is served to a run that
would have been told 1. Use the allotment to size parallelism, never to
choose content.

A **custom pool serialises access to an external thing** (a docker daemon, a
GPU, a licence server): `docker: 1` means one target at a time talks to it.
The thing itself stays an **undeclared input**: the pool says nothing about
what it is or what state it is in, and a target whose output depends on it is
no more hermetic for holding the pool.

## Compatibility

The repository keeps no release notes file; these are the notices for this
change (plugin ABI 0.17.3 → 0.17.4, patch, lane `^0.17.0`):

- **`resources` is ordinary driver config, not a reserved keyword.** A custom
  cdylib driver that already reads a config key of that name keeps it, and the
  engine now also reads whatever it puts on `TargetDef.resources`. No builtin
  driver read the key before.
- **`-j N` changed meaning.** It is CPUs the run may reserve at once, not a
  count of targets, and the default went from `2 × cores` slots to `cores`
  CPUs. A workspace whose targets declare nothing runs half as many at once
  as before by default; `-j 200%` (or `resources: {cpu: 200%}`) restores
  the old concurrency.
- **Source break for third-party SDK code.** `TargetDef` gains a field and is
  not `#[non_exhaustive]`, so a `TargetDef { … }` literal without
  `..Default::default()` stops compiling at rebuild. `RunRequest` is
  `#[non_exhaustive]` (built with `RunRequest::new`), so its new `allotment`
  field breaks nothing; but `RunRequest::resolve` now returns
  `Cow<'_, str>` instead of `&str`, so a caller that bound the result as
  `&str` needs a `&*` or `.as_ref()`. The wire is unchanged for old plugins.
- **`RequestConfig.max_workers`** is now the `cpu` capacity in CPUs, rounded
  up — no longer the worker-slot count. A reader comparing it across versions
  sees it halve by default, and differ under `-j 50%` or a fractional
  capacity.
- **Every `exec`/`bash` target now sees `HEPH_CPUS`** (`1` for a target that
  declares nothing). Nothing reaches a key through it, but an output that
  captures its own environment — a runner whose fingerprint hashes `env` —
  changes once, so its consumers miss once on upgrade.
- **Non-string dict keys in BUILD config are an error.** `env = {1: "x"}` or
  any config dict with a non-string key used to drop the entry silently; it
  now fails naming the key.
- **Old plugin, new host.** A 0.17.3 cdylib sends no `resources` (its targets
  cost 1 CPU) and ignores the allotment (its tools are told nothing).
- **New plugin, old host.** A 0.17.3 host ignores a new plugin's `resources`:
  every declaration, BUILD-written or generated, costs one worker there, and a
  custom-pool exclusion is lost silently. A plugin on an old host is told no
  allotment and sets no env.
- **`.hephconfig`.** `resources:` is refused loudly by an older heph
  (`ConfigYaml` denies unknown fields).
