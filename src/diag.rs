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
use tracing::warn;

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
const MAX_THREADS: usize = 256;

/// Pause between signalling successive threads.
const THREAD_GAP: std::time::Duration = std::time::Duration::from_millis(2);

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
        warn!(
            path = %path.display(),
            error = %std::io::Error::last_os_error(),
            "Cannot write thread dump"
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
    // `warn!`, not `info!`: someone pressed `Ctrl-\` on a frozen build and the one
    // thing they need back is where the dump went. At `info!` that line sits in
    // the same stream as ordinary build chatter and scrolls past unread.
    warn!(threads = n, path = %path.display(), "Wrote thread backtraces");
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
        std::thread::sleep(THREAD_GAP);
    }
    n
}

/// Our own Mach task port.
///
/// `libc::mach_task_self()` is deprecated in favour of the `mach2` crate; this
/// reads the same underlying static rather than pulling in a dependency for one
/// port lookup.
/// macOS has no `tgkill`; the Mach layer is the way in.
///
/// `task_threads` hands back a port for every thread in the task,
/// `pthread_from_mach_thread_np` converts each to the `pthread_t` that
/// `pthread_kill` wants, and the port array is Mach-allocated memory the caller
/// must hand back with `vm_deallocate`. Same serial pacing and cap as the Linux
/// path, for the same reason: no two threads inside the unwinder at once.
#[cfg(target_os = "macos")]
fn sweep_threads() -> usize {
    let mut ports: mach2::mach_types::thread_act_array_t = std::ptr::null_mut();
    let mut count: mach2::message::mach_msg_type_number_t = 0;

    // SAFETY: `task_threads` fills `ports`/`count` on success; both are valid
    // out-params, and `mach_task_self()` names our own task.
    // SAFETY: names our own task.
    let task = unsafe { mach2::traps::mach_task_self() };
    // SAFETY: `ports`/`count` are valid out-params; `task_threads` fills them.
    let kr = unsafe { mach2::task::task_threads(task, &mut ports, &mut count) };
    if kr != 0 || ports.is_null() {
        warn!(kern_return = kr, "Cannot enumerate threads for the dump");
        return 0;
    }

    // SAFETY: `ports` points to `count` thread ports owned by us until the
    // `vm_deallocate` below.
    let list = unsafe { std::slice::from_raw_parts(ports, count as usize) };
    let mut n = 0;
    for &port in list.iter().take(MAX_THREADS) {
        // SAFETY: `port` came from `task_threads`; a port that no longer names a
        // live thread yields a null `pthread_t`, which we skip rather than
        // signal.
        let pt = unsafe { libc::pthread_from_mach_thread_np(port) };
        // `pthread_t` is an opaque integer here; 0 means the port no longer
        // names a live thread.
        if pt == 0 {
            continue;
        }
        // SAFETY: `pt` is a live pthread of this process and the handler for
        // `DUMP_SIGNAL` is installed before this runs.
        unsafe {
            libc::pthread_kill(pt, DUMP_SIGNAL);
        }
        n += 1;
        std::thread::sleep(THREAD_GAP);
    }

    // Mach-allocated; leaking it on every dump would grow the task's address
    // space each time someone asks for one.
    let bytes = (count as usize * std::mem::size_of::<mach2::mach_types::thread_act_t>())
        as mach2::vm_types::mach_vm_size_t;
    // SAFETY: returning the exact region `task_threads` allocated.
    let _kr = unsafe {
        mach2::vm::mach_vm_deallocate(task, ports as mach2::vm_types::mach_vm_address_t, bytes)
    };
    n
}

/// Neither Linux nor macOS: no portable way to signal a specific thread, so dump
/// the caller and say so rather than silently producing one backtrace where the
/// reader expects all of them.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sweep_threads() -> usize {
    warn!("Per-thread sweep is unsupported on this platform; dumping the calling thread only");
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
