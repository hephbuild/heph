---
name: perf-test
description: >
  Profile the rheph binary with samply and produce performance recommendations.
  Builds the profiling target, runs samply twice (first run is warmup, second is the captured profile),
  analyzes the resulting profile.json.gz + profile.json.syms.json (plus /tmp/cpu.prof if present),
  and writes findings + suggestions to PERFORMANCE.md at the repo root.
  Trigger when user says "perf test", "profile rheph", "run a perf run", "performance test", or invokes /perf-test.
---

# Performance Testing

Goal: measure rheph hot paths, surface concrete bottlenecks, propose actionable fixes.

## Steps

1. **Build profiling binary**

   ```bash
   build-profile
   ```

   Devenv script (`cargo build --profile profiling`). Assume already inside `devenv shell`.

2. **Warmup run** — prime caches, FS, allocator. Discard output. `heph r test` must run from `example/`:

   ```bash
   cd $DEVENV_ROOT/example && samply record --unstable-presymbolicate -s $DEVENV_ROOT/target/profiling/rheph r test
   ```

   `-s` = save-only (no web server). Output: `profile.json.gz` + `profile.json.syms.json` in cwd (`example/`).

3. **Measured run** — same command, second invocation, plus pprof output:

   ```bash
   cd $DEVENV_ROOT/example && samply record --unstable-presymbolicate -s $DEVENV_ROOT/target/profiling/rheph --pprof-cpu=/tmp/cpu.prof r test
   ```

   Artifacts land in `example/profile.json.gz` + `example/profile.json.syms.json` directly. pprof at `/tmp/cpu.prof`.

   Confirm with user before overwriting any existing `example/profile.json.gz`.

4. **Analyze** the artifacts:
   - `example/profile.json.gz` — gzipped samply profile (Firefox Profiler format). Decompress with `gunzip -c` and inspect with `jq`.
   - `example/profile.json.syms.json` — symbol table keyed by lib + address.
   - `/tmp/cpu.prof` — optional pprof-format CPU profile, if rheph wrote one. Inspect with `go tool pprof -top /tmp/cpu.prof` or `pprof -top` if available; skip if file missing.

   For samply JSON, focus on:
   - `threads[].samples` weights → time per stack frame
   - `threads[].stackTable` + `frameTable` + `funcTable` → resolve hot functions
   - `threads[].markers` → IO/blocking events
   - Self-time vs inclusive-time top-N functions
   - Allocator frames (`alloc::`, `__rust_alloc`, `malloc`) → allocation pressure
   - Lock/contention frames (`parking_lot`, `tokio::sync`, `Mutex`)
   - syscalls / FS frames (`read`, `write`, `stat`, `openat`)

   Use `jq` to extract hot stacks. Example pipeline:

   ```bash
   gunzip -c example/profile.json.gz \
     | jq '.threads[0] | {name, sampleCount: (.samples.length // (.samples.stack|length))}'
   ```

   Cross-reference frame indices to function names via the syms file.

5. **Write `PERFORMANCE.md`** at repo root. Refresh findings, but **preserve prior justification notes** (entries explaining why a given optimization cannot or should not be done). Drop a preserved note only when the underlying finding no longer shows up in the new profile. When in doubt, keep the note and re-anchor it to the current finding. Structure:

   ```markdown
   # Performance Report

   _Profile captured: <date> · binary: target/profiling/rheph · profiler: samply_

   ## Top hot functions (self time)
   | Rank | Function | Crate / file | Self % | Notes |
   |------|----------|--------------|--------|-------|

   ## Top hot functions (inclusive time)
   | Rank | Function | Inclusive % | Notes |

   ## Allocation hotspots
   - <function> — <observation>

   ## Lock / contention
   - <site> — <observation>

   ## IO / syscall hotspots
   - <site> — <observation>

   ## Suggestions
   1. **<change>** — file:line. Why: <reason>. Expected impact: <est %>.
   2. ...

   ## Won't-fix / constraints
   _Carried over across runs. Each entry: finding, why it can't be optimized, date noted. Remove only when finding disappears from profile._
   - **<finding>** (noted <date>) — <reason it cannot be addressed>.

   ## Methodology
   - Build profile: `cargo build --profile profiling`
   - Warmup + measured samply run, presymbolicated
   - Analyzed `example/profile.json.gz` + syms + /tmp/cpu.prof (if present)
   ```

   Each suggestion must cite a concrete `path:line` and quantify expected wins where possible. No vague advice ("optimize the loop"). Prefer fixes that align with `.claude/rust.md` (borrow over clone, iterators, avoid `Arc<Mutex>`, etc.).

## Rules

- Always run **two** samply invocations. First is throwaway warmup.
- Never claim a function is hot without backing it with a percentage from the profile.
- Do not commit `PERFORMANCE.md` automatically. Surface it for the user to review.
- If `build-profile` fails, stop and surface the build error — do not fall back to debug binary.
- If samply is unavailable (non-Darwin host), tell user and stop. devenv only ships samply on macOS.
- Keep `PERFORMANCE.md` focused on the latest run, **but never delete won't-fix / justification notes** unless the underlying finding is gone from the new profile. Before overwriting, read existing `PERFORMANCE.md` and carry forward the "Won't-fix / constraints" section.
