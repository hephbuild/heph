//! On-demand thread backtraces for diagnosing hangs. Always installed.
//!
//! When a run hangs in a locked-down CI container, every external tool is blocked:
//! ptrace is denied (no gdb/perf/`gcore`), the root fs is read-only (kernel core
//! dumps are dropped), and pprof's sampler can segfault on a static binary. The
//! one channel left is the process dumping its own stacks. A `SIGUSR1` handler
//! appends the *signalled* thread's backtrace to a file, so signalling the busy
//! thread reveals the loop — and a file beats stderr when hundreds of threads
//! dump at once.
//!
//! Dump every thread of a stuck process by sending it `SIGQUIT` — which is
//! `Ctrl-\\` at the terminal, so a human staring at a frozen TUI needs no
//! forethought, no flag, and no recipe:
//! ```sh
//! kill -QUIT <pid>          # or just press Ctrl-\\
//! cat .heph3/diag/dump-<pid>.txt
//! ```
//! `SIGQUIT` follows the Go/JVM convention and, unlike `SIGUSR1`, is not already
//! taken here (`SIGUSR2` belongs to the pprof sampler). The handler dumps and
//! continues; it never terminates the process.
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
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// File descriptor the handler appends dumps to; `-1` until [`install`] opens it.
static DIAG_FD: AtomicI32 = AtomicI32::new(-1);

/// Install the `SIGQUIT` → backtrace-to-file handler. Called unconditionally at
/// startup.
///
/// Unconditionally, because the opt-in version could not work: nobody passes a
/// diagnostic flag on the run they do not yet know will hang, and on a process
/// started without it `SIGQUIT` defaults to *terminating* — so reaching for the
/// dump would kill the build being inspected. The cost is one `signal(2)`.
pub fn install() {
    let handler = on_sigquit as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an `AtomicBool` (async-signal-safe);
    // installed once at startup before the runtime matters.
    unsafe {
        libc::signal(libc::SIGQUIT, handler as libc::sighandler_t);
    }
    spawn_sweeper();
}

/// Set by the `SIGQUIT` handler, polled by the sweeper thread.
static DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `SIGQUIT` handler: request a dump and return. Stores to one atomic, so it is
/// async-signal-safe — and it *returns*, so the process keeps running rather than
/// taking the default `SIGQUIT` action of terminating.
extern "C" fn on_sigquit(_sig: libc::c_int) {
    DUMP_REQUESTED.store(true, Ordering::Relaxed);
}

/// Where a dump lands. In-workspace so `heph tool gc` can sweep it.
fn dump_path() -> std::path::PathBuf {
    std::path::PathBuf::from(".heph3/diag").join(format!("dump-{}.txt", std::process::id()))
}

/// Poll for a requested dump and perform the sweep off the signal handler.
///
/// The sweep must not run *in* the handler: capturing a backtrace allocates and
/// takes the unwinder's global lock, and signalling every thread at once puts all
/// of them inside `_Unwind_Backtrace` together — one interrupted mid-`malloc`
/// then re-enters the allocator from its handler. That is the same class of bug
/// that made `--pprof-cpu` segfault the process it was diagnosing, and heph runs
/// a lot of threads (tokio workers, the blocking pool, tokio's own blocking pool,
/// the sandbox cleaner). So: serial, with a gap, and capped.
fn spawn_sweeper() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        drop(
            std::thread::Builder::new()
                .name("heph-diag-sweeper".to_string())
                .spawn(|| {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        if DUMP_REQUESTED.swap(false, Ordering::Relaxed) {
                            sweep();
                        }
                    }
                }),
        );
    });
}

/// Max threads dumped in one sweep. A cap is not arbitrary caution: each dump is
/// an unwind, and a process with hundreds of threads would otherwise spend a long
/// time with the unwinder lock changing hands.
#[cfg(target_os = "linux")]
const MAX_THREADS: usize = 256;

fn sweep() {
    let path = dump_path();
    if let Some(dir) = path.parent() {
        drop(std::fs::create_dir_all(dir));
    }
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: opening a file by C path; the fd is stored for the handler below.
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_APPEND,
            0o644,
        )
    };
    if fd < 0 {
        eprintln!(
            "heph: cannot write {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        return;
    }
    DIAG_FD.store(fd, Ordering::Relaxed);

    let handler = on_dump_signal as extern "C" fn(libc::c_int);
    // SAFETY: installed before any thread is signalled below.
    unsafe {
        libc::signal(DUMP_SIGNAL, handler as libc::sighandler_t);
    }

    let n = sweep_threads();
    eprintln!("heph: wrote {} thread backtraces to {}", n, path.display());
}

/// Signal used to make each thread dump itself. `SIGUSR1` is free here —
/// `SIGUSR2` belongs to the pprof sampler and `SIGQUIT` is the trigger.
const DUMP_SIGNAL: libc::c_int = libc::SIGUSR1;

/// Signal every thread of this process in turn, pausing between each so no two
/// are inside the unwinder at once.
#[cfg(target_os = "linux")]
fn sweep_threads() -> usize {
    let pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten().take(MAX_THREADS) {
        let Ok(tid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // SAFETY: `tgkill` on our own process group with a handler installed.
        // `SYS_tgkill` rather than a hardcoded number — it is 234 on x86_64 but
        // 131 on aarch64, and the wrong constant silently signals nothing (or
        // something else entirely).
        unsafe {
            libc::syscall(libc::SYS_tgkill, pid as i32, tid, DUMP_SIGNAL);
        }
        n += 1;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    n
}

/// macOS has no `tgkill`, and no portable way to signal a specific thread by id.
/// Dump the calling thread only, and be explicit rather than silently producing
/// one backtrace where the reader expects all of them.
#[cfg(not(target_os = "linux"))]
fn sweep_threads() -> usize {
    eprintln!("heph: per-thread sweep is Linux-only; dumping this thread only");
    on_dump_signal(DUMP_SIGNAL);
    1
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

extern "C" fn on_dump_signal(_sig: libc::c_int) {
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
