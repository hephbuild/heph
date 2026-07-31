# RCA — macOS tokio runtime hang under heavy spawn / spawn_blocking load

> **Provenance.** Written 2026-05-23 during field debugging, and checked into the
> repo verbatim afterwards: this document is the evidence behind several
> load-bearing design decisions (`hcore::blocking` instead of `spawn_blocking`,
> the 250ms wake backstop, `sandbox_cleaner`'s dedicated OS threads, the macOS
> `proc_exec` condvar paths, the TUI ticker as the only unlosable wake), and it
> previously lived only in a gitignored scratch directory. File paths inside
> refer to the pre-workspace-split layout (`src/engine/…` is now
> `crates/engine/src/engine/…`, etc.); they are kept as written. The hypothesis
> below has **no isolated reproducer** — re-testing it on current tokio is
> tracked work, and any decision newly leaning on this document should re-run
> that experiment first.

## Symptom

`heph3 r test //mgmt/go/...` deadlocks after ~30s on macOS arm64. Tokio runtime workers parked in `__psynch_cvwait` / `kevent` indefinitely. Memoizer stall watchdog (`HEPH_MEMOIZER_STALL_SECS`) fires. Subprocesses already exited (no zombies, no live children except the supervisor sidecar). Cycle detector finds no dep cycle.

## Leading hypothesis

**Tokio's cross-thread waker is unreliable on macOS under heavy concurrent load.** On macOS, `mio::Waker` is implemented as an `EVFILT_USER` registration on the kqueue used by tokio's IO driver; cross-thread wakes are triggered via `kevent()` with `NOTE_TRIGGER` from another thread (verified in mio source: `mio/src/sys/unix/selector/kqueue.rs`). Under heavy spawn / IO load the observed behavior is that user events appear to not be delivered to a parked `kevent`, so any tokio task awaiting a wakeup originating from a non-runtime thread stalls forever.

**Confidence:** Hypothesis fits all observed symptoms. We have not produced an isolated reproducer. No upstream patch exists. The fix below works whether the underlying mechanism is `EVFILT_USER` specifically or any other primitive on the same wake path.

### Confirmed-affected primitives

These were directly observed stuck in phase traces and resolved by the fix:

- `tokio::sync::oneshot::Receiver::await` when the sender lives on a `std::thread`
- `tokio::task::spawn_blocking` JoinHandle await (and anything routed through it, e.g. `tokio::fs::*`)

### Possibly affected (consistent with hypothesis, not independently isolated)

- `tokio::process::Child::wait` — uses tokio's internal SIGCHLD-based reaper, a different primitive. Also hangs in practice. Mechanism may be distinct (SIGCHLD reaper, not `EVFILT_USER`); fix is the same shape (bypass via dedicated watcher).
- `tokio::time::sleep` / `tokio::time::interval` — timer driver wakes route through the IO driver. In practice the TUI 80ms ticker works under field load (`src/tui/backend/interactive.rs:55`, regression test at `:205`); short-budget sleep polls inside loops recover via the polling outer loop. Not directly implicated, but theoretically exposed.
- `tokio::task::yield_now` — listed for completeness; no direct evidence in our traces.

### External symptom matches (not proof)

- tokio#6770 ("Command .wait hanging on MacOS", closed) — single-`echo`-spawn hang, different scale but same family of symptom.
- codex#14470 ("codex exec --json resume can hang indefinitely on macOS after MCP helpers start", open) — `__psynch_cvwait` / `kevent` thread state matches; the reporter attributes it to MCP client startup logic rather than a tokio waker bug, so the diagnosis is not shared.

Treat these as "consistent reports", not corroborating proofs.

## Fix

**Bypass tokio's cross-thread waker entirely on the hot path.** Run any synchronous work (subprocess wait, filesystem op, sqlite op, build-file evaluation) inside `tokio::task::block_in_place` on the multi-thread runtime, or inline on the current-thread runtime. The runtime tolerates one blocked worker per task by spawning a replacement worker from the blocking pool (see Caveats).

For subprocess waits specifically, on macOS we use a dedicated `kqueue EVFILT_PROC` watcher thread that reaps zombies and signals completion through a `std::sync::mpsc::channel`. The caller blocks on the channel with `Receiver::recv()` inside `block_in_place` — kernel `thread::park` wake, not tokio waker.

### Changes

- **New: `src/process_watcher/` (macOS only)** — child-exit watcher.
  - `kqueue_macos.rs`: shared `kqueue`; each registered pid gets an `EVFILT_PROC NOTE_EXIT EV_ONESHOT EV_RECEIPT` filter. `EV_RECEIPT` catches `ESRCH` (child exited before registration) synchronously and reaps inline (`kqueue_macos.rs:182-203`). Main loop uses a 1-second `kevent` timeout + `waitpid(WNOHANG)` backstop poll on every pending pid (`kqueue_macos.rs:63-71, 120-244`). The backstop has been observed to fire in production logs (`process_watcher: backstop poll caught exited pid (kqueue dropped NOTE_EXIT)`); whether the kernel actually dropped a `NOTE_EXIT` event or it merely fires before kqueue dispatch is not isolated — the backstop is defense-in-depth and worth keeping either way.
  - Linux still uses `tokio::process::Child::wait` via `proc_exec/imp_linux.rs:12,124` — no pidfd watcher has been written. The macOS bug has not been observed on Linux.
- **`src/process_watcher/mod.rs::register`**: returns `std::sync::mpsc::Receiver<ExitStatus>`. Callers call `.recv()` inside `block_or_inline` so the wake travels via the kernel condvar inside std mpsc, never via tokio's waker.
- **`src/engine/execute.rs:192` (`sync_fs_op_on_thread`)**: `block_or_inline(f)` for `sandbox_remove` / `sandbox_create` instead of `tokio::fs::*`.
- **`src/engine/local_cache.rs:170, 351`**: `cache_artifact_locally` + `artifacts_from_local_cache` use `block_or_inline`.
- **`src/plugingo/provider.rs:411, 1441, 1491`**: `list_packages`, `read_golist_package`, `read_golist_package_addrs` use `block_or_inline`. `go.mod` read converted from `tokio::fs::read_to_string` to `std::fs::read_to_string` inside `block_or_inline`.
- **`src/pluginbuildfile/run_file.rs:544` (`run_pkg`)** and **`src/pluginbuildfile/provider.rs:190` (`list_packages`)**: `block_or_inline`.
- **`src/main.rs:51, 55`**: `signal(SIGTTOU, SIG_IGN)` + `signal(SIGTTIN, SIG_IGN)` at startup. Independent fix for a SEPARATE deadlock symptom (process stopped, `state == T`) where a test subprocess (Go test runner) takes the foreground process group and exits without restoring; the next TUI `tcsetattr` then signals SIGTTOU back to heph3 → kernel stops the process. Without this, the macOS waker investigation kept getting masked by an OS-level pause.

`heph_DISABLE_REAPER=1` (`src/process_supervisor/mod.rs:37`) is retained as a debug knob: it bypasses both the supervisor sidecar and the watcher, falling back to `tokio::process::Child::wait().await`. Useful for bisecting whether a hang lives in this layer.

## Diagnosis methodology

The decisive piece was the phase histogram from `heph_PHASE_TRACE=1`:

```
grep -nE "^    inv [0-9]+ @ " /tmp/heph.log | awk -F'@ ' '{print $2}' | sort | uniq -c | sort -rn
```

In the failing run this showed N invocations stuck at `wait_polling:rx_await` plus a cascade at `execute:semaphore_acquire`. With the watcher + `recv()` inside `block_in_place` in place, that phase disappeared; the next hang surfaced at `execute_cache:cache_locally` (also `spawn_blocking`), then at `buildfile_pkg` (also `spawn_blocking`). Each hot-path `spawn_blocking` site had to be converted in turn.

Live diagnostics during a hang:

```bash
# phase histogram
grep -nE "^    inv [0-9]+ @ " /tmp/heph.log | awk -F'@ ' '{print $2}' | sort | uniq -c | sort -rn

# what each stuck invocation is waiting on
grep -nE "^    inv <id> ->" /tmp/heph.log

# live children vs zombies
PID=$(pgrep -nf '^.*heph3 r ')
pgrep -P $PID | xargs -I{} ps -o pid=,stat=,etime=,command= -p {} 2>/dev/null
ps -A -o pid=,ppid=,stat=,command= | awk -v p=$PID '$2==p && $3 ~ /Z/'

# thread states
sample $PID 3 -mayDie
```

## Red herrings (chronological)

1. **Memoizer dependency cycle** — ruled out by phase trace.
2. **Reaper / sidecar supervisor** — `heph_DISABLE_REAPER=1` still hung.
3. **`waitpid(pid, 0)` blocking-thread race** — macOS doesn't wake other waiters when tokio reaps first; replaced with WNOHANG polling. Still hung.
4. **`child.try_wait()` polling** — reads tokio's starved reaper cache. Still hung.
5. **Spin-wait `waitpid(WNOHANG)` + `std::thread::sleep` on `block_in_place` worker** — worked but CPU-burning and loses status on ECHILD races. Initial fix; replaced by the watcher.
6. **EVFILT_PROC / pidfd watcher + `oneshot::Receiver::await`** — moved the bug. NOTE_EXIT fires, watcher reaps, oneshot.send fires, but the awaiting tokio task never re-polls because `Waker::wake` from the watcher OS thread depends on the same broken cross-thread wake path. Replaced `.await` with `block_in_place + recv()` to use kernel `thread::park` instead.
7. **SIGTTOU process stop masquerading as deadlock** — fixed by ignoring SIGTTOU/SIGTTIN at startup. Unrelated but had to be fixed first to stop masking the real bug.

## What proved the fix-shape was correct

The decisive test was successively eliminating `spawn_blocking` sites from the hot path. Each removal made the hang move to the next `spawn_blocking` site (visible as a different phase in the trace), never to a `block_in_place`-based call. That's the signature of "any cross-thread wake to a parked tokio task is the broken primitive" — kernel mechanisms (kqueue, SIGCHLD, std mpsc condvar) all work; tokio's wake delivery to a parked task does not. This is consistent with the `EVFILT_USER` hypothesis but does not isolate it.

## Caveats of the current fix

- **`block_in_place` parks a runtime worker and spawns a replacement from the blocking pool** (verified in tokio source `tokio/src/runtime/scheduler/multi_thread/worker.rs`: `block_in_place` calls `runtime::spawn_blocking(move || run(worker))` to host the migrated core). Replacement workers are counted against `max_blocking_threads` (default 512). With `2 × parallelism` execute permits, up to `2 × parallelism` workers may be parked simultaneously; each spawns a replacement, so worker creation pressure scales with permit count. Per tokio docs: if `max_blocking_threads` is saturated, `block_in_place` degrades to running the closure inline on the calling worker without handing off the core — at that point the runtime cannot make progress on other tasks until the closure returns. Tune `max_blocking_threads` if `2 × parallelism` is large.
- The watcher's reap path can synthesize `ExitStatus::from_raw(0)` if `waitpid(WNOHANG)` returns 0 or `ECHILD` after `NOTE_EXIT` fires (another reaper got there first). Acceptable: we never reach the watcher recv for an explicitly-killed child (cancel arm in `pluginexec::run_inner` handles that path separately).
- **Linux only**: `tokio::process::Child::wait` is still used (`proc_exec/imp_linux.rs:12,124`) with `kill_on_drop(true)`. On Drop tokio sends SIGKILL to the pid; if the kernel has already reused the pid, this could signal an unrelated process. Window is microseconds and `kill` returns ESRCH for unknown pids. Not observed in practice; not a macOS concern at all (watcher path doesn't use `tokio::process::Child`).
- The current-thread runtime (tests) goes through the same `block_or_inline` path, which falls back to a direct synchronous call. `Receiver::recv()` will block the only thread but the watcher runs on its own OS thread so the wake still arrives. Tests pass.

## Ecosystem state

No off-the-shelf solution as of 2026-05-23.

- **tokio**: no formal fix. `mio::Waker` on macOS still uses `EVFILT_USER`. No upstream patch.
- **smol / async-process**: different driver, untested whether it avoids the same bug.
- **Buck2, Bazel, Cargo, rust-analyzer**: each uses non-tokio process management or non-async I/O for the relevant paths and would not exercise tokio's cross-thread waker the way heph does. Whether any of them would hit the same hang at heph's local-macOS scale has not been validated.

## Sites still using `spawn_blocking` / `tokio::fs`

Not yet converted, lower priority because not on the await chain that was failing:

- `src/engine/result.rs:511` — deferred sandbox cleanup. Fire-and-forget; nothing awaits its completion. Spawn-blocking pool exhaustion would queue these forever but doesn't block any waiter. (Also handled by the dedicated `sandbox_cleaner` thread, see `src/engine/sandbox_cleaner.rs:33`.)
- `src/pluginnix/mod.rs:376, 381, 451, 457, 462, 468, 470` — `tokio::fs::*` for gcroot management and wrapper binary install. Used outside the heavy subprocess load.

If new hangs appear with phases pointing at sites outside the converted set, apply the same `block_or_inline` pattern.

## Verification

- `cargo build` clean.
- `cargo clippy --lib -- -D warnings` clean.
- `cargo test --lib` 519 passed, 2 ignored.
- `tests/process_watcher_stress.rs` — 500 concurrent short-lived children, every one resolves with real exit status. Passes in ~8s.
- `heph_PHASE_TRACE=1 HEPH_MEMOIZER_STALL_SECS=30 HEPH_DEBUG_MEMOIZER_CYCLE=1 heph3 r --no-tui test //mgmt/go/lib/...` completes without hanging (per user confirmation).
