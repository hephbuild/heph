//! On-demand thread backtraces for diagnosing hangs (opt-in via `--diag-backtrace`).
//!
//! When a run hangs in a locked-down CI container, every external tool is blocked:
//! ptrace is denied (no gdb/perf/`gcore`), the root fs is read-only (kernel core
//! dumps are dropped), and pprof's sampler can segfault on a static binary. The
//! one channel left is the process dumping its own stacks. A `SIGUSR1` handler
//! writes the *signalled* thread's backtrace to stderr (the CI log), so signalling
//! the busy thread reveals the loop.
//!
//! Sweep every thread of a stuck process (the busy one shows the loop; parked
//! threads show `epoll_wait`):
//! ```sh
//! PID=<pid>
//! for t in /proc/$PID/task/*; do
//!   python3 - "$PID" "$(basename "$t")" <<'PY'
//! import ctypes, signal, sys
//! libc = ctypes.CDLL("libc.so.6", use_errno=True)
//! libc.syscall(234, int(sys.argv[1]), int(sys.argv[2]), signal.SIGUSR1)  # tgkill
//! PY
//! done
//! ```
//!
//! The handler is not strictly async-signal-safe (capturing a backtrace
//! allocates), but it targets a CPU-bound hang, where the interrupted thread is
//! looping in compute rather than inside the allocator — a pragmatic trade for a
//! diagnostic that is off unless `--diag-backtrace` is passed.

use std::backtrace::Backtrace;

/// Install the `SIGUSR1` → backtrace-to-stderr handler. Call once at startup when
/// `--diag-backtrace` is set.
pub fn install() {
    let handler = on_sigusr1 as extern "C" fn(libc::c_int);
    // SAFETY: installed once at startup; the handler writes owned bytes to stderr.
    unsafe {
        libc::signal(libc::SIGUSR1, handler as libc::sighandler_t);
    }
}

/// The OS thread id, for correlating a dump with the `tgkill`ed thread. Only
/// meaningful on Linux (where this diagnostic is used); `-1` elsewhere.
#[cfg(target_os = "linux")]
fn os_tid() -> i64 {
    // SAFETY: gettid is a plain syscall, async-signal-safe.
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}
#[cfg(not(target_os = "linux"))]
fn os_tid() -> i64 {
    -1
}

extern "C" fn on_sigusr1(_sig: libc::c_int) {
    let tid = os_tid();
    let bt = Backtrace::force_capture();
    let msg = format!("\n=== heph SIGUSR1 backtrace (tid {tid}) ===\n{bt}\n");
    let bytes = msg.as_bytes();
    // Write straight to fd 2, bypassing Rust's stderr lock (which the interrupted
    // thread might hold), in one call.
    // SAFETY: writing an owned, initialised byte buffer to stderr.
    unsafe {
        libc::write(2, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}
