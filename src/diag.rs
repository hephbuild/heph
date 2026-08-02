//! On-demand in-flight state for diagnosing hangs. Always installed.
//!
//! When a run hangs in a locked-down CI container, every external tool is blocked:
//! ptrace is denied (no gdb/perf/`gcore`), the root fs is read-only (kernel core
//! dumps are dropped), and `--pprof-cpu` only samples a run it was passed on —
//! which is never the run that turns out to hang. The one channel left is the
//! process reporting on itself.
//!
//! Dump a stuck process by sending it `SIGQUIT` — which is `Ctrl-\\` at the
//! terminal, so a human staring at a frozen TUI needs no forethought, no flag,
//! and no recipe:
//! ```sh
//! kill -QUIT <pid>          # or just press Ctrl-\\
//! cat <home>/diag/dump-<pid>.txt
//! ```
//! `SIGQUIT` follows the Go/JVM convention and, unlike `SIGUSR1`, is not already
//! taken here (`SIGUSR2` belongs to the pprof sampler). The handler sets one
//! atomic and returns; the work happens on the sweeper thread, and the process
//! keeps running.
//!
//! # What the dump contains, and why backtraces are opt-in
//!
//! By default: the **in-flight report** — every memoized computation that is
//! open, how long it has been open, and what it is waiting on. It is produced by
//! [`write_inventory`] on an ordinary thread, allocating and locking normally,
//! and it is the half that names the stuck work. See [`inventory_report`].
//!
//! With `--diag-backtrace`: every thread's stack too. That half is captured
//! *inside a signal handler* ([`on_dump_signal`]), which calls
//! `Backtrace::force_capture` — the DWARF unwinder, which takes libgcc's global
//! `object_mutex` and re-enters `dl_iterate_phdr` — and then `format!`, which
//! allocates. Neither is async-signal-safe.
//!
//! This used to be unconditional, on the reasoning that it "targets a CPU-bound
//! hang, where the interrupted thread is looping in compute rather than inside
//! the allocator". That premise does not hold for this engine: a real run
//! measured ~11% of its CPU in drop glue alone, so at any instant some worker is
//! very likely inside `malloc`. Signal *that* thread and its handler re-enters
//! the allocator, deadlocks on the arena lock it already holds, and never
//! returns — so every other thread blocks on the same lock and the process
//! freezes with all threads at 0% CPU. Observed in practice, on the very
//! workload someone reached for `SIGQUIT` to diagnose.
//!
//! Serialising the sweep does not fix it (and the cap and inter-thread gap below
//! were never able to): the deadlock needs only *one* thread interrupted in the
//! wrong place, not two overlapping. The same class of bug made `--pprof-cpu`
//! segfault the process it was diagnosing, fixed there by walking frame pointers
//! instead of calling the unwinder. Doing that here would make the backtrace
//! half safe as well; until then it is behind a flag that says what it costs.
//!
//! Backtraces resolve to function names only when the binary keeps its symbol
//! table (the `debug` release flavour; the stripped `std` one yields addresses).

use std::backtrace::Backtrace;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tracing::warn;

/// File descriptor the handler appends dumps to; `-1` until [`install`] opens it.
static DIAG_FD: AtomicI32 = AtomicI32::new(-1);

/// Whether this run also dumps per-thread backtraces. Set once by [`install`]
/// from `--diag-backtrace`; read by [`sweep`]. See the module docs for why the
/// backtrace half can deadlock the process and the in-flight report cannot.
static BACKTRACES: AtomicBool = AtomicBool::new(false);

/// Install the `SIGQUIT` → dump handler. Called unconditionally at startup.
///
/// Unconditionally, because the opt-in version could not work: nobody passes a
/// diagnostic flag on the run they do not yet know will hang, and on a process
/// started without it `SIGQUIT` defaults to *terminating* — so reaching for the
/// dump would kill the build being inspected. The cost is one `signal(2)`.
///
/// `backtraces` gates only the *contents*, never the handler: `SIGQUIT` always
/// writes the in-flight report, so the always-on guarantee above still holds for
/// the half that is safe to produce.
pub fn install(backtraces: bool) {
    BACKTRACES.store(backtraces, Ordering::Relaxed);
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

/// The directory dumps land in, once the engine has resolved its home.
static DUMP_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Point dumps at the engine's `<home>/diag`, so they land beside the stall log
/// and the in-flight report rather than wherever the process happened to start.
///
/// Called once the home is known; before that, [`dump_dir`] falls back to the
/// launch directory. The fallback matters — a hang during startup still has to
/// produce a dump somewhere findable.
pub fn set_dump_dir(dir: &std::path::Path) {
    drop(DUMP_DIR.set(absolute(dir)));
}

/// Make `path` absolute without touching the filesystem.
///
/// `std::path::absolute` rather than `canonicalize`: the directory does not
/// exist yet on the first dump, and `canonicalize` fails on a path that is not
/// already there. Symlink resolution is not wanted here anyway — the point is a
/// path the reader can paste, not the shortest one.
fn absolute(path: &std::path::Path) -> std::path::PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn dump_dir() -> std::path::PathBuf {
    DUMP_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| absolute(std::path::Path::new(".heph3/diag")))
}

/// Where a dump lands. In-workspace so `heph tool gc` can sweep it.
///
/// **Absolute.** It used to be `.heph3/diag/dump-<pid>.txt`, resolved against
/// whatever the process's cwd happened to be — which is not something the person
/// reading a stall log, or an agent handed the file an hour later, has any way to
/// know. "Your dump is at a relative path, good luck" costs a round trip in
/// exactly the situation where the process may already be gone.
fn dump_path() -> std::path::PathBuf {
    dump_dir().join(format!("dump-{}.txt", std::process::id()))
}

/// Poll for a requested dump and perform the sweep off the signal handler.
///
/// The sweep must not run *in* the handler: it opens files, allocates, and takes
/// locks. Doing it on a plain thread is also what keeps the default path — the
/// in-flight report — entirely free of the re-entrancy hazard that put
/// `--diag-backtrace` behind a flag (module docs).
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
                            sweep(&dump_path());
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

/// Write a dump to `path`.
///
/// The path is a parameter rather than read from [`dump_path`] inside: that
/// reads a process-global `OnceLock`, so a test wanting its own file had to set
/// the global and thereby decide where *every other* test in the binary thought
/// dumps went. That is what it did — silently, ordering-dependent, green
/// locally and red on CI.
fn sweep(path: &std::path::Path) {
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

    // Opt-in, and deliberately *before* the inventory: the backtrace half is the
    // half that can wedge the process (module docs), so if it does, the reader
    // still gets a file whose contents show how far it got.
    let n = if BACKTRACES.load(Ordering::Relaxed) {
        let handler = on_dump_signal as extern "C" fn(libc::c_int);
        // SAFETY: installed before any thread is signalled below.
        unsafe {
            libc::signal(DUMP_SIGNAL, handler as libc::sighandler_t);
        }
        sweep_threads()
    } else {
        0
    };
    write_inventory(fd);
    // `warn!`, not `info!`: someone pressed `Ctrl-\` on a frozen build and the one
    // thing they need back is where the dump went. At `info!` that line sits in
    // the same stream as ordinary build chatter and scrolls past unread.
    //
    // Naming the flag matters as much as the path: a reader who needed stacks and
    // got none has no other way to learn they were available.
    if n == 0 {
        warn!(
            path = %path.display(),
            "Wrote in-flight report (no thread backtraces; pass --diag-backtrace to add them, \
             at the risk of deadlocking the process)"
        );
    } else {
        warn!(threads = n, path = %path.display(), "Wrote thread backtraces and in-flight report");
    }
}

/// Append the parked-future state to the dump, after the thread backtraces.
///
/// Thread backtraces answer "what is each *thread* doing", which for this engine
/// is the wrong question. The work lives in futures parked on the heap: a wedged
/// build shows every worker idle in `futex_wait` and every blocking-pool thread
/// on an empty queue, and not one frame names the thousands of awaits that are
/// actually stuck. The inventory is the other half of the dump, and on a lost
/// wake-up it is the *only* half that says anything.
///
/// Runs on the sweeper thread, after every signalled thread has written its
/// frames — never inside the signal handler, and never with a blocking lock (see
/// `hmemoizer::inventory`).
/// Takes the sampling rather than doing it, so the caller that owns the timing
/// decides when the picture is taken — and so the same picture can be handed to
/// this and to the watchdog's renderer and the two compared.
fn inventory_report(snapshot: &hcore::hmemoizer::ReportSnapshot) -> String {
    // Same renderer the stall watchdog writes to its companion file, so the two
    // are byte-identical and neither is the "truncated" one. No cap: this is
    // read once, on a build that has already gone wrong.
    format!("\n{}", hcore::hmemoizer::render_report(snapshot))
}

fn write_inventory(fd: libc::c_int) {
    let text = inventory_report(&hcore::hmemoizer::capture_report());
    let bytes = text.as_bytes();
    // SAFETY: appending an owned, initialised byte buffer to the diag fd.
    unsafe {
        libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The dump carries the parked-future state, and says how to turn on the
    /// two views that are still gated.
    ///
    /// Thread backtraces alone answered "every thread is idle" on a wedged run
    /// — true, and useless. The inventory is the half that names the stuck work,
    /// and the gated sections must announce themselves rather than being absent,
    /// or the next reader has no way to know a deeper view exists.
    #[test]
    fn the_dump_carries_the_in_flight_state_and_names_its_gated_sections() {
        let text = inventory_report(&hcore::hmemoizer::capture_report());
        assert!(text.contains("in-flight inventory"), "{text}");
        assert!(text.contains("memoizer wait-for graph"), "{text}");
        assert!(text.contains("memoizer phases"), "{text}");
        // Unset in a test process, so both must self-describe.
        assert!(text.contains("HEPH_DEBUG_MEMOIZER_CYCLE"), "{text}");
        assert!(text.contains("HEPH_PHASE_TRACE"), "{text}");
    }

    /// `SIGQUIT` writes the in-flight report and installs **no** signal handler
    /// unless `--diag-backtrace` asked for one.
    ///
    /// The handler is the hazard: it captures a backtrace (DWARF unwinder) and
    /// `format!`s it (allocator) from inside a signal, so a thread interrupted
    /// in `malloc` deadlocks the process — observed on a real run, which is how
    /// this default came to be. Asserting on `SIGUSR1`'s disposition rather than
    /// on the file contents is deliberate: "no backtrace text appeared" would
    /// also pass on a build where the sweep ran and simply found nothing, while
    /// a handler still installed is the actual footgun.
    ///
    /// Only the *off* path is exercised end to end. Driving the on path here
    /// would signal every thread of the test binary — i.e. reproduce the
    /// deadlock this test exists because of. For the same reason the flag round
    /// trip is asserted *inside* this test rather than beside it: `BACKTRACES`
    /// is process-global, and a sibling test setting it to `true` in parallel
    /// with the `sweep()` below is exactly that reproduction, by accident.
    #[test]
    fn a_dump_installs_no_handler_unless_backtraces_were_asked_for() {
        fn sigusr1_disposition() -> libc::sighandler_t {
            // SAFETY: all-zero is a valid initial `sigaction`; it is only an
            // out-param here.
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: a null `act` queries the disposition without installing
            // anything; `old` is a valid out-param `sigaction` only writes to.
            unsafe {
                libc::sigaction(DUMP_SIGNAL, std::ptr::null(), &mut old);
            }
            old.sa_sigaction
        }

        // The flag reaches the sweeper. Without this the gate below could be
        // wired to a constant and still pass.
        install(true);
        assert!(BACKTRACES.load(Ordering::Relaxed));
        install(false);
        assert!(!BACKTRACES.load(Ordering::Relaxed));

        let before = sigusr1_disposition();

        // Its own path, never `set_dump_dir`: that writes a process-global
        // `OnceLock`, so this test would decide where every *other* test in the
        // binary thinks dumps go — which is exactly how it broke
        // `the_dump_path_is_absolute`, ordering-dependent and only on CI.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dump.txt");

        sweep(&path);

        assert_eq!(
            sigusr1_disposition(),
            before,
            "the default dump installed a handler for signal {DUMP_SIGNAL}; that is \
             the path that deadlocks a process interrupted inside the allocator"
        );

        // The half that is always safe to produce must still be there — the
        // point of the gate is to keep it, not to make SIGQUIT do nothing.
        let text = std::fs::read_to_string(&path).expect("dump written");
        assert!(
            text.contains("in-flight inventory"),
            "the default dump must still carry the in-flight report: {text}"
        );
    }

    /// The dump lands at an absolute path.
    ///
    /// It used to be `.heph3/diag/dump-<pid>.txt`, resolved against whatever cwd
    /// the process was launched from — so telling someone where their dump went
    /// meant telling them "under the directory you started the build in", which
    /// is a round trip in exactly the situation where the process may already be
    /// gone.
    #[test]
    fn the_dump_path_is_absolute() {
        let path = dump_path();
        assert!(path.is_absolute(), "dump path must be absolute: {path:?}");
        assert!(
            path.ends_with(format!("dump-{}.txt", std::process::id())),
            "{path:?}"
        );
        assert!(
            path.parent().is_some_and(|p| p.ends_with("diag")),
            "it sits in a diag dir beside the stall log: {path:?}"
        );
    }

    /// The `SIGQUIT` dump and the watchdog's companion file must be the same
    /// text. Two formats for one thing means a reader has to work out which of
    /// them is the truncated one, during an incident, which is exactly when
    /// nobody should be reverse-engineering a diagnostic.
    ///
    /// Both sides are the *writers*, not one library function against itself:
    /// `inventory_report` is what the `SIGQUIT` sweeper appends to the dump,
    /// `InflightLog::render` is what `heph run`'s stall watchdog writes to the
    /// companion file. Change either and this fails.
    ///
    /// Both render *one* snapshot. Calling the renderers back to back compared
    /// two separate reads of process-wide memoizer state instead, and any other
    /// test in this binary building a request between them changed one read and
    /// not the other — a ~10%-per-run failure that had nothing to do with the
    /// formats being compared. Sampling once removes the only variable this test
    /// was never about.
    #[test]
    fn the_dump_and_the_watchdog_companion_file_render_identically() {
        let snapshot = hcore::hmemoizer::capture_report();
        assert_eq!(
            inventory_report(&snapshot).trim(),
            hengine::engine::diag::InflightLog::render(&snapshot).trim()
        );
    }
}
