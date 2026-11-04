# Target resources

A target says what it costs to run; the engine admits targets so that the sum
of what is running fits the machine, and a target that wants to know what it
got asks for it by name.

```python
target(
    name = "build",
    driver = "bash",
    run = "make",
    env = {"MAKEFLAGS": "-j${resource:cpu}"},
    resources = {"cpu": 4, "memory": "8G"},
)
```

**`-j N` now means CPUs, not targets.** `-j 8` lets the run reserve 8 CPUs at
once: eight targets that reserve 1 CPU each, two that declare `cpu: 4`, or
eighty downloads at 0.1 each. The default is the core count, where it used to
be `2 × cores` target slots; see [Compatibility](#compatibility).

heph **tells**, it does not police. Nothing kills, throttles or pins a process
that uses more than it reserved (no cgroups, `ulimit` or affinity), heph
exports nothing to a target's environment, and no resource value reaches any
hash.

## Declaring what a target costs

`resources` on `target()` is a dict of pool name to amount:

```python
resources = {"cpu": 0.5, "memory": "2G", "docker": 1}
```

An amount is a number or a string, read in **its pool's unit** when the def
is linked (the unit lives in the config, which a driver never sees):

| Unit | Written as | Example |
|---|---|---|
| `cpu` | a decimal ≥ 0 in steps of 0.000001, rounded half up; a value above 0 and below one step is an error | `4`, `0.5`, `"0.25"` |
| `bytes` | a whole number of bytes, or digits followed at once by `K`, `M`, `G` or `T` (optionally `B` or `iB`, any case), binary throughout: `4G` is 4·2³⁰ | `"8G"`, `"4096M"`, `"4GiB"`, `1024` |
| `count` | a whole number | `1`, `"2"` |

No spaces, signs, exponents or separators. The `cpu` pool's unit is `cpu`, the
`memory` pool's is `bytes`, and a custom pool's is whatever the config gives
it (`count` unless it says otherwise). A target names at most 16 pools; pool
names are `[a-z][a-z0-9_]{0,63}`.

A pool that is not configured is an error when the def is linked, so
`heph validate` refuses it too; `--resource <name>=<n>` declares the pool for
one run (see [Per-run overrides](#per-run-overrides)):

```
//p:t declares resources.docker, but docker is not a pool: add resources: {docker: <n>} to .hephconfig, or --resource docker=<n> for one run (known: cpu, memory)
```

When the amount is not a count, the entry to add names its unit:
`resources = {"scratch": "64G"}` suggests `{scratch: {unit: bytes, value: <n>}}`,
since a bare `<n>` would declare a count pool and then refuse `"64G"`. A name
close to a known pool adds `(did you mean <pool>?)`.

A wrong shape (not a dict, a non-string key, a list as an amount) fails the
driver's `parse` with the target named; an amount its unit refuses fails at
link, naming the target, the pool and its unit (`docker: 0.5` on a `count`
pool: `"0.5" is not a count`).

`resources` is **ordinary driver config**, not a keyword of `target()`: it
rides the target's config to the driver's `parse`, which moves it onto the def
as written (`Resources::from_config`). That is how a BUILD file, a plugin
function's declared target and a provider all reach it the same way. Every
builtin driver that runs something reads it; `scratch` and `credential`
declarations run nothing and refuse it, and an `exec`/`bash` entrypoint
refuses it at parse (an entrypoint reserves nothing, see
[Admission](#admission)).

### What a target costs when it declares nothing

| Driver | Undeclared `cpu` costs |
|---|---|
| any driver not below, including `exec`, `bash`, `docker_build`, the runners, and a def from a plugin built before resources existed | 1 (the engine's default) |
| `http`; OCI `pull`, `push`, `load`, `layer`, `index`, `image`; `go.download` | 0.1, advertised by the driver: IO-bound, so ten downloads share one CPU |
| `fs`, a *relocating* `group` | 0.0001, advertised by the driver: they still queue through the `cpu` pool in order, but 10,000 fit in one CPU |
| an aggregate `group` (no transform) | nothing, whatever it declares: it never executes |

A driver whose targets cost other than one CPU **advertises** it: when the
target declares no `cpu`, the driver writes its own amount into the def's
`resources` as an ordinary entry (`Resources::advertise`). A declared `cpu`
always wins. There is no separate default field: the advertised `cpu` shows
in `heph inspect def` as the driver's declaration, crosses the plugin ABI in
`TargetDef.resources` like any other entry, and, like every declaration, is
never hashed. Undeclared `memory` and custom pools cost 0.

**The light lane.** A request with no bounded need — 0 of every bounded pool
(`cpu: 0` and nothing else, or `cpu: 0` plus amounts of unbounded pools only)
— takes one slot of a separate `light` lane instead, whose capacity is
`max(8 × cpu capacity in CPUs, 64)`. Its unbounded amounts are counted as held
from the grant. It never waits behind `cpu` requests, and the number of
concurrent executes stays bounded. `cpu: 0` with `memory: "8G"` still queues
on memory.

## Configuring pools

`cpu` and `memory` always exist. Any other pool is declared in `.hephconfig`,
one entry per pool, either in full or as a shorthand:

```yaml
resources:
  cpu: 8                                   # shorthand: CPUs, or 50% of the default
  memory: 24G                              # shorthand: a size, or 75%
  docker: 2                                # shorthand: a count
  scratch_disk: {unit: bytes, value: 64G}  # full: a custom pool in another unit
  tokens: {unit: count}                    # no value: unbounded
  gpu: 0                                   # 0: the run sets it
```

- **`{unit, value}`**: `unit` is `count`, `cpu` or `bytes`; `value` is an
  amount in it. A builtin's unit is fixed: `cpu: {unit: bytes}` is an error.
  With no `unit` the pool's default unit applies, so a new pool written
  `{value: 3}` is a count.
- **Shorthand**: the amount alone, in the pool's default unit (`cpu` for
  `cpu`, `bytes` for `memory`, `count` for anything else).
- **No value**: a custom pool is **unbounded**: every request is granted at
  once and nothing is clamped, but what is held is still counted and shown.
  `cpu` and `memory` without a value keep their defaults.
- **`0`**: "this pool exists; the caller sets its capacity per run". BUILD
  files naming it validate on every machine, and a run that reaches a target
  needing it fails at admission, before the target runs:

  ```
  //p:t acquires gpu, but pool gpu has capacity 0 and must be set: --resource gpu=<n>, HEPH_RESOURCE_GPU=<n> or resources: {gpu: <n>} in .hephconfig
  ```

**Defaults.** `cpu` is the engine's parallelism (the core count). `memory` is
the memory this process may use: on Linux the cgroup limit when one is set
(v2 `memory.max`, v1 `memory.limit_in_bytes`, capped at physical memory), else
physical memory; on macOS physical memory. When the host's memory cannot be
read at all, `memory` is unbounded rather than an invented number. The
difference between platforms is on purpose: a target that declares `memory`
can wait in a Linux CI container limited to 8G where it ran at once on a 32G
laptop. Set `resources: {memory: …}` to make the two agree.

**Percentages** (`cpu: 50%`, `memory: 75%`, and the same on the command line)
are of the builtin's default and exist only for `cpu` and `memory`: a custom
pool has no default to take one of. cpu rounds to the nearest micro-CPU,
memory floors to bytes, over 100% is an overcommit by choice, and `0%` is an
error.

`resources:` merges **per pool** across `HEPH_PROFILES`: a profile that sets
`cpu` keeps the base's other pools, and one that sets only a pool's `value`
keeps the base's `unit`. A pool's unit cannot change across profiles; a
profile that tries is an error naming both.

### Per-run overrides

- `--resource <name>=<n>`, global and repeatable, once per pool.
- `HEPH_RESOURCE_<NAME>=<n>` in heph's own environment (`<NAME>` is
  lowercased into the pool name). A malformed one is an error naming the
  variable; only the shell-completion and credential helpers, which run no
  targets, ignore it with a warning.
- `-j N` / `--jobs N` is `--resource cpu=N`: CPUs the run may reserve at once,
  **not** a target count. Giving both `-j` and `--resource cpu=` is an error.

Each is parsed in the pool's configured unit and must be above 0. An
override of a pool the config does not declare declares it for this run, in
its default unit (`--resource gpu=2` is a `count` pool of 2). Precedence: flag, then environment, then config, then
the default.

`-j` sizes the `cpu` pool only. The engine's own fan-outs (cache pipes, gc and
clean loops) keep following its parallelism, and `-j 1` serialises neither
them nor `cpu: 0` targets.

## Reading the allotment

heph exports **nothing** to a target's environment. A target that wants its
allotment names it with a `${resource:<pool>}` deferred value (see
`docs/DEFERRED_VALUES.md`), and maps it onto the tool itself:

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

It is substituted at run with the pool's granted amount, rendered by its
unit: `cpu` in whole CPUs rounded up and at least 1 (a `cpu: 0.5` or `cpu: 0`
target is told 1), `bytes` and `count` as the integer. A transitive `env`
value resolves with the *dependent's* allotment. The def hash covers the
reference text, so the mapping never moves a key.

- **Any deferrable field takes one**: `env`, `runtime_env`, a transitive
  `env`, a `run` line, a `template` var, an `oci` push `dest`. Outside `env`
  the number can end up in an output, and then that output is per-machine
  (a `template` rendering `${resource:cpu}` writes a different file on an
  8-core and a 16-core host, with the same cache key). That is the author's
  to avoid; see [Hashing](#hashing-and-the-contract).
- **A configured pool the target does not reserve is a link error**
  (`cpu` is always reserved):

  ```
  //p:t: `env.MEM` reads ${resource:memory}, but the target reserves no memory: add resources = {"memory": <amount>} to it
  ```

  A pool declared at 0 (`resources = {"gpu": 0}`) reserves none either, and
  the message says so: `… reads ${resource:gpu}, but the target declares
  gpu: 0, so it reserves none: declare an amount above 0`.
- **A pool-shaped name that is no configured pool is a link error** too, with
  a did-you-mean and the `--resource` route:

  ```
  //p:t: `env.X` reads ${resource:cpus}, but cpus is not a pool (did you mean cpu?): add resources: {cpus: <n>} to .hephconfig, or --resource cpus=<n> for one run (known: cpu, memory)
  ```

  Pool-shaped is the pool-name grammar, `[a-z][a-z0-9_]{0,63}`. The same two
  checks hold a dependency's transitive `env` to the dependent's reservation,
  over the merged sandbox env.
- **Only text that is not pool-shaped passes through untouched**, like any
  `${…}` heph does not own: `${resource:CPU}`, `${resource:-x}`,
  `${resource:2}`. Bash's substring form with a lowercase variable
  (`${resource:offset}`) is pool-shaped and so refused; write it
  `${resource:$offset}` or `${resource: offset}`.

From bash, put the allotment in a variable and read it:
`env = {"N": "${resource:cpu}"}`, then `run = "make -j$N"`.

**Rounding.** `cpu` renders in whole CPUs rounded up, so a `cpu: 0.5` target
is told `1`. An author mapping it into `make -j` or a thread count gets one
job per started CPU, not half of one; declare whole CPUs where that matters.

**Entrypoints** (`heph run` of an entrypoint target) reserve nothing: they run
in the caller's terminal, one at a time, and `${resource:cpu}` is the whole
`cpu` capacity.

**Go.** heph sets neither `GOMAXPROCS` nor `GOMEMLIMIT`, for any generated Go
target or Go driver: a Go process sizes itself from the host as before, and
the pool bounds how many run at once. A workspace that wants it tighter maps
`GOMAXPROCS` itself with `${resource:cpu}`.

## Admission

Each execute reserves its amounts in every pool it names before it runs and
releases them when the run ends (the cache write after it is outside the
reservation).

- **Strict FIFO.** The head of the queue waits until every pool it needs fits
  at once, and nothing behind it is admitted while it waits. No starvation,
  deterministic, at the cost of idle capacity while a large request waits.
- **Atomic.** A grant covers every pool or none, so a target never holds its
  CPUs while it waits for memory.
- **Clamped.** A request above a pool's capacity is reduced to the capacity:
  `cpu: 16` on 4 waits for an empty pool, runs alone and is told 4. The clamp
  is logged at `info` once per target per request and carried on the event
  (`declared` vs `reserved`). An unbounded pool never clamps.

**Seeing it.** Every surface renders each pool in its own unit (`cpu` in
CPUs, `bytes` human-sized, `count` as an integer), from the `units` that
`RequestConfig` announces beside each pool's capacity:

- The TUI header shows `cpu` as worker cells and every other pool that
  something holds as `<pool> <held>/<capacity>` (an unbounded one as just
  `<pool> <held>`, e.g. `tokens 5`).
- A clamped run's TUI row, CI `running` line and GitHub Actions report entry
  say so: `[cpu 16→4]`, `[scratch_disk 8.0 GiB→4.0 GiB]`.
- A wait that outlasts the notice threshold (5 s) is announced as
  `AdmissionWaitStart` / `AdmissionWaitEnd { addr, pool, need, free, ahead }`
  and rendered as a row in the TUI and the GitHub Actions report:

  ```
  ⧗ //a:b waits for cpu 4 (2 free, 6 ahead, 41s)
  ```

- `ExecuteStart` carries the `declared` and `reserved` amounts and
  `queued_ms`, how long the run waited, so a wait under the threshold is
  still answerable from the event stream.
- The in-flight report (SIGQUIT) prints each pool's held/capacity in its
  unit, the queue length, and what the head waits for and for how long.

## Sizing a declaration

`heph.core.resource("cpu")` returns the host's CPU capacity as a float (cores)
and `heph.core.resource("memory")` the host's memory in bytes as an int, as the
engine reads them (cgroup-aware). It is the **host's** value, not this run's
pool: `-j` and `--resource` do not change it. A custom pool has no host value,
so any other name is an error.

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
`runtime_env`), and the allotment is substituted at run only. Changing
`resources`, running under another `-j`, or reconfiguring a pool keeps the
cache hit.

The contract that follows: **a target's outputs must not depend on its
allotment.** An artifact built when `${resource:cpu}` was 8 is served to a run
that would have been told 1. Use the allotment to size parallelism, never to
choose content; a target that cannot help it says so with `cache = False`.

A **custom pool serialises access to an external thing** (a docker daemon, a
GPU, a licence server): `docker: 1` in the config means one target at a time
talks to it. The thing itself stays an **undeclared input**: the pool says
nothing about what it is or what state it is in, and a target whose output
depends on it is no more hermetic for holding the pool.

**Nothing is enforced.** A reservation is a promise the target makes, not a
limit heph applies: a `cpu: 1` target that spawns 32 threads gets 32 threads.

## Measured

The new `-j` default (cores, where it was `2 × cores` slots) was timed on a
12-core macOS laptop under load 23–35, on the strollplanner-v2 `core` module,
n=3 each:

| Loop | `-j 100%` (default) | `-j 200%` (old concurrency) |
|---|---|---|
| inner loop (one package's tests), median | 0.91 s | 0.90 s |
| cold build (817 targets) | 28.45 s | 26.68 s |

The cold build is ≈6.6% slower at cores, but an earlier set of runs went the
other way: read it as a weak signal of a few percent at most. A workspace that
measures otherwise sets `-j 200%`.

To repeat a cold build, clear heph's home. The read-only input stage under
`.heph/stage/` (the Go SDK among it) is marked read-only like the Go
module cache, so a plain `rm -rf` fails on it: clear it with `heph tool gc`,
or `chmod -R u+w .heph/stage` first.

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
- **The wire.** `TargetDef.resources` (12) and `ManagedRunRequest.allotment`
  (15) are both `map<string, string>`, new in 0.17.4: the def carries each
  declared amount as written (`"0.5"`, `"2G"`), which the host parses in the
  pool's unit at link; the allotment carries every granted pool (`cpu`
  always) rendered by its unit, which is what `${resource:<pool>}` becomes.
  A cdylib driver's advertised cost (`cpu: "0.1"`) is an ordinary entry of
  `TargetDef.resources`; there is no other resources field.
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
- **Non-string dict keys in BUILD config are an error.** `env = {1: "x"}` or
  any config dict with a non-string key used to drop the entry silently; it
  now fails naming the key.
- **Old plugin, new host.** A 0.17.3 cdylib sends no `resources` (its targets
  cost 1 CPU) and ignores the allotment.
- **New plugin, old host.** A 0.17.3 host ignores a new plugin's `resources`:
  every declaration, BUILD-written or generated, costs one worker there, and a
  custom-pool exclusion is lost silently. A plugin on an old host is sent an
  empty allotment, so a `${resource:…}` reference fails its run.
- **`.hephconfig`.** `resources:` is refused loudly by an older heph
  (`ConfigYaml` denies unknown fields).
