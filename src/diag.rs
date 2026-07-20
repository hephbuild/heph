//! On-demand thread backtraces for diagnosing hangs (opt-in via `--diag-backtrace`).
//!
//! When a run hangs in a locked-down CI container, every external tool is blocked:
//! ptrace is denied (no gdb/perf/`gcore`), the root fs is read-only (kernel core
//! dumps are dropped), and pprof's sampler can segfault on a static binary. The
//! one channel left is the process dumping its own stacks. A `SIGUSR1` handler
//! appends the *signalled* thread's backtrace to a file, so signalling the busy
//! thread reveals the loop — and a file beats stderr when hundreds of threads
//! dump at once.
//!
//! Sweep every thread of a stuck process (the busy one shows the loop; parked
//! threads show `epoll_wait`), then read the file:
//! ```sh
//! PID=<pid>
//! for t in /proc/$PID/task/*; do
//!   python3 - "$PID" "$(basename "$t")" <<'PY'
//! import ctypes, signal, sys
//! libc = ctypes.CDLL("libc.so.6", use_errno=True)
//! libc.syscall(234, int(sys.argv[1]), int(sys.argv[2]), signal.SIGUSR1)  # tgkill
//! PY
//! done
//! cat /tmp/heph-backtrace.log   # or whatever path --diag-backtrace was given
//! ```
//!
//! The handler is not strictly async-signal-safe (capturing a backtrace
//! allocates), but it targets a CPU-bound hang, where the interrupted thread is
//! looping in compute rather than inside the allocator — a pragmatic trade for a
//! diagnostic that is off unless `--diag-backtrace` is passed. Backtraces resolve
//! to function names only when the binary keeps its symbol table
//! (`strip = "debuginfo"`, not `strip = true`).

use std::backtrace::Backtrace;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

/// File descriptor the handler appends dumps to; `-1` until [`install`] opens it.
static DIAG_FD: AtomicI32 = AtomicI32::new(-1);

/// Open `path` (append/create) and install the `SIGUSR1` → backtrace-to-file
/// handler. Call once at startup when `--diag-backtrace` is set.
pub fn install(path: &Path) {
    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("diag: bad --diag-backtrace path {}: {e}", path.display());
            return;
        }
    };
    // Truncate once here (fresh file per run), but keep O_APPEND so the many
    // per-thread writes of a `tgkill` sweep each land at the end rather than
    // racing the shared offset.
    // SAFETY: opening a file by C path; fd stored for the handler to write to.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND,
            0o644,
        )
    };
    if fd < 0 {
        eprintln!(
            "diag: cannot open --diag-backtrace file {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        return;
    }
    DIAG_FD.store(fd, Ordering::Relaxed);

    let handler = on_sigusr1 as extern "C" fn(libc::c_int);
    // SAFETY: installed once at startup; the handler only appends to the fd.
    unsafe {
        libc::signal(libc::SIGUSR1, handler as libc::sighandler_t);
    }
    eprintln!(
        "diag: SIGUSR1 appends thread backtraces to {}",
        path.display()
    );
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
    let fd = DIAG_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let tid = os_tid();
    let bt = Backtrace::force_capture();
    let msg = format!("\n=== heph SIGUSR1 backtrace (tid {tid}) ===\n{bt}\n");
    let bytes = msg.as_bytes();
    // SAFETY: appending an owned, initialised byte buffer to the diag fd.
    unsafe {
        libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}
