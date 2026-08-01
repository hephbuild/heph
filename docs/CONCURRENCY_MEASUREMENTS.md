# Concurrency folklore re-measurements — 2026-07-31

Two load-bearing claims in the tree cited evidence that was gitignored (and, for
the first, since overwritten). Both were re-tested on 2026-07-31. Environment:
macOS 26.5.2, arm64 (10-core M-series), tokio 1.52.3 (the version pinned in
`Cargo.lock`), release builds, no other load on the machine, runs strictly
serial.

## 1. `block_in_place` "0.94 → 0.74 concurrency regression" (#180)

**Claim** (`crates/core/src/blocking.rs` module doc, PR #180 body):
`tokio::task::block_in_place` measured a concurrency regression 0.94 → 0.74 vs
the dedicated `hcore::blocking` pool. The raw data lived in a gitignored
`ai-docs/PERFORMANCE.md` that has since been rewritten; the workload, the host,
and even the metric's definition are unrecoverable.

**Method.** Env-gated variant in `hcore::blocking::run`: with
`HEPH_BENCH_BLOCK_IN_PLACE` set, every job runs via
`tokio::task::block_in_place(catch_unwind(f))` instead of being queued to the
pool. One binary serves both sides (no build-to-build variance); the candidate
side is a wrapper script exporting the env var. `heph-bench` Tier A
(`run inprocess`, interleaved reps), default corpus: 1000 bash targets across
100 packages, seed 0.

**Results** (candidate = `block_in_place`, baseline = dedicated pool):

| Scenario | Baseline (ms) | Candidate (ms) | Delta | Threshold | Verdict |
|---|---:|---:|---:|---:|---|
| cold | 19558.2 (n=5) | 19334.3 (n=5) | −1.1% | 8.0% | ok |
| full-hit | 963.5 (n=8) | 990.4 (n=8) | +2.8% | 15.3% | ok |
| incremental | 1015.0 (n=6) | 1036.1 (n=6) | +2.1% | 44.4% | ok |

**Verdict: not reproduced.** Parity within noise on all three scenarios. The
mild slowdowns on the short-job-dominated scenarios (full-hit, incremental) are
directionally consistent with per-call handoff overhead but well under the
noise thresholds.

**What this does and does not change.**

- The 0.94→0.74 figure should no longer be cited as the primary justification
  for `hcore::blocking` — on the current code, on this host, at this corpus
  size, it is not there.
- The pool's other two justifications are untouched and remain sufficient:
  inline-on-worker execution parks the runtime (reactor + timer wheel) — the
  hang #180 actually fixed — and `block_in_place`/`spawn_blocking` require a
  runtime context that does not exist when a cdylib plugin's future is polled
  by a host worker.
- Caveats: the original measurement's workload (a real monorepo with the go
  plugin and remote cache), host (possibly Linux, possibly 2–4 core CI shape),
  and era (#180-vintage code) are all different. Tier A exercises bash targets
  with no plugin seam. A low-core-count re-run on Linux would strengthen the
  verdict; this run refutes the number only for the macOS/many-core shape.

## 2. macOS dropped cross-thread wake (`docs/RCA_MACOS_WAKER.md`)

**Claim.** Tokio's cross-thread waker (mio `EVFILT_USER` on kqueue) can drop
wake-ups on macOS under heavy load, stranding any task awaiting an off-runtime
wake — the RCA's confirmed-affected primitives were `spawn_blocking` JoinHandle
awaits and `oneshot` with a `std::thread` sender. No isolated reproducer
existed.

**Method.** Standalone stress binary (source in the appendix), tokio pinned to
1.52.3, exercising all RCA-affected primitives concurrently with per-slot
progress watermarks and a pure-`std` watchdog (20 s stall threshold):

- `spawn_blocking` storm — 4×workers loops of 500 µs blocking jobs
- cross-thread `oneshot` — senders fired from dedicated std threads
- `tokio::process` child churn (`/usr/bin/true`) — SIGCHLD reaper path
- task-spawn churn + `tokio::fs` traffic (routes through `spawn_blocking`)

Two configurations, 9 minutes each: `workers = 10` (field shape) and
`workers = 2` (CI shape — fewest rescuer polls if a wake is lost).

**Results.**

| Config | Duration | Total completed awaits | Stalls > 20 s |
|---|---:|---:|---:|
| 10 workers | 540 s | 27,422,082 | 0 |
| 2 workers | 540 s | 12,648,232 | 0 |

**Verdict: not reproduced** — ~40M cross-thread wake deliveries with zero
losses on the exact tokio version heph ships. This does not prove the field
failure impossible (it had FUSE, memory pressure, hundreds of live processes,
and hours of sustained load; the RCA itself never isolated a reproducer), but
it is strong evidence that the *plain* primitives are not broken on current
macOS + tokio 1.52.3, i.e. the hazard should be treated as a defense-in-depth
concern, not a prohibition.

**Consequences if a longer/harsher campaign also fails to reproduce:**
`spawn_blocking` and off-runtime `oneshot` awaits become usable again on
host-side paths, which unblocks simplification of `sandbox_cleaner`'s no-tokio
stance, the macOS `proc_exec` condvar loops, and the TUI's
tick-as-only-unlosable-wake reasoning. The `hcore::blocking` pool itself is
justified independently (no-runtime-context at the plugin seam, see above).

## Appendix: waker stress reproducer

`Cargo.toml`:

```toml
[package]
name = "waker-repro"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "=1.52.3", features = ["full"] }

[profile.release]
debug = true
```

`src/main.rs`: see `docs/waker_repro.rs` (checked in next to this file, exactly
as run).
