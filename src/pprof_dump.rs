//! On-demand CPU profiling for diagnosing hangs.
//!
//! `--pprof-cpu` on its own writes a profile only at process exit — useless for a
//! run that hangs (it never exits, and a CI timeout `SIGKILL`s it). Locked-down CI
//! containers also block ptrace, so gdb/perf/core dumps are unavailable. This
//! module keeps the profiler guard on a watcher thread that writes the profile
//! accumulated so far on `SIGUSR2`, so `kill -USR2 <pid>` snapshots a stuck
//! process in place (to a writable tmpfs path). The filtered final report is
//! still written at shutdown.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

/// Set by the `SIGUSR2` handler, polled by the watcher: request an on-demand dump.
static DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by [`Watcher::shutdown`]: write the final report and stop the watcher.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Sampling frequency, Hz.
///
/// Every sample is a signal delivered to a running thread, and each one is a
/// chance to interrupt somewhere the unwinder cannot walk (see
/// [`UNWIND_BLOCKLIST`]). 1000 Hz across every thread of a wide build multiplied
/// that exposure for resolution this diagnostic never needed: it exists to show
/// *which loop* is burning a pegged worker, and a hang that has lasted minutes
/// is not a signal 199 Hz can miss.
const SAMPLE_HZ: libc::c_int = 199;

/// Libraries the sampler must not unwind out of.
///
/// `pprof` unwinds from inside its `SIGPROF` handler via the `backtrace` crate,
/// i.e. `_Unwind_Backtrace`, which takes a global lock and is not
/// async-signal-safe. Interrupt a thread that is already inside `libc`, the
/// dynamic loader, or the unwinder itself, and walking that frame faults — which
/// is how `--pprof-cpu` segfaulted the process it was meant to diagnose, within
/// seconds of startup on a wide run.
///
/// `pprof` resolves these names against the loaded shared objects and checks the
/// interrupted PC *before* attempting any unwind, so a sample landing in one of
/// them is dropped whole rather than walked. That costs only samples attributable
/// to libc frames, which carry no information about heph's own hot loops.
///
/// Only the **leaf** PC is checked: the per-frame variant of this test is
/// `#[cfg(feature = "frame-pointer")]` in pprof, and this build uses the
/// `backtrace`/`_Unwind_Backtrace` tracer instead. So a sample that *starts* in
/// heph code and walks up into libc is still unwound in full. This removes the
/// dominant crash class, not the class — see the module TODO for the durable fix.
///
/// Matched as substrings against shared-object paths, and `str::contains` is
/// **case-sensitive**: macOS ships `/usr/lib/libSystem.B.dylib`, which lowercase
/// `libsystem` does not match. Both spellings are listed for that reason, and
/// [`tests::blocklist_covers_the_c_library_but_not_the_main_binary`] fails if any
/// platform's C library stops being covered.
///
/// Entries are anchored (`libc.so`, not `libc`) because the match is a bare
/// substring and over-blocking is the quieter failure of the two: an unanchored
/// `libc` also swallows `libc++abi`, `libcharset`, `libcorecrypto` — and
/// `libcrypto`/`libcurl` the day a dependency links them. Those samples would
/// then vanish from every profile with nothing to indicate it, which for a
/// profiler pointed at a network stall drops exactly the frames worth having.
const UNWIND_BLOCKLIST: &[&str] = &[
    // Linux/glibc.
    "libc.so",
    "libgcc_s",
    "libunwind",
    "libpthread",
    "ld-linux",
    "vdso",
    // macOS. Both cases: `libSystem.B.dylib` and `libsystem_c.dylib` both exist.
    "libsystem",
    "libSystem",
    "libc.dylib",
    "libdyld",
    // GCD frames are their own unwind hazard on Darwin.
    "libdispatch",
];

/// Handle to the running pprof watcher thread.
#[derive(Debug)]
pub struct Watcher {
    handle: JoinHandle<()>,
}

impl Watcher {
    /// Signal the watcher to write its final (filtered) report, then join it.
    pub fn shutdown(self) {
        SHUTDOWN.store(true, Ordering::Relaxed);
        if let Err(e) = self.handle.join() {
            warn!("pprof watcher thread panicked: {e:?}");
        }
    }
}

/// Start CPU sampling plus the `SIGUSR2`-driven dump watcher, writing profiles to
/// `path`. Call [`Watcher::shutdown`] at exit for the final report.
pub fn start(path: PathBuf) -> anyhow::Result<Watcher> {
    // Fail here, where `main` already surfaces the error, rather than at dump
    // time where an unwritable path is a `warn!` the user may never see — by
    // which point they have re-run a multi-minute hang to get nothing. Probed by
    // creating and removing, so no empty file is left to be mistaken for a
    // profile.
    std::fs::write(&path, b"")
        .and_then(|()| std::fs::remove_file(&path))
        .with_context(|| format!("prepare CPU profile path {}", path.display()))?;

    // These outlive any single `Watcher`, so a second `start` in one process
    // (tests; a future re-arm) would otherwise inherit a set `SHUTDOWN` and get a
    // watcher that exits on its first tick, silently ignoring every `SIGUSR2`.
    SHUTDOWN.store(false, Ordering::Relaxed);
    DUMP_REQUESTED.store(false, Ordering::Relaxed);

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(SAMPLE_HZ)
        .blocklist(UNWIND_BLOCKLIST)
        // `pprof::Error` wraps `nix`/`io` errors, so keep it as a source rather
        // than stringifying it away.
        .build()
        .context("start CPU profiler")?;
    install_signal();
    Ok(Watcher {
        handle: spawn_watcher(guard, path),
    })
}

/// Ask the watcher to write a snapshot of the profile accumulated so far.
///
/// The `SIGUSR2` handler is one caller; the other is the test, which drives this
/// path directly rather than raising a signal at its own process — a signal is an
/// untestable side channel, and the handler's whole body is this one store.
fn request_dump() {
    DUMP_REQUESTED.store(true, Ordering::Relaxed);
}

/// `SIGUSR2` handler: request a dump. Only stores to an atomic, so it is
/// async-signal-safe.
extern "C" fn on_sigusr2(_sig: libc::c_int) {
    request_dump();
}

fn install_signal() {
    let handler = on_sigusr2 as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an `AtomicBool` (async-signal-safe), and
    // this runs once at startup before the tokio runtime matters.
    unsafe {
        libc::signal(libc::SIGUSR2, handler as libc::sighandler_t);
    }
}

/// Own the profiler guard on a dedicated thread: poll for `SIGUSR2` dump requests
/// (mid-run snapshots) and, on shutdown, write a final filtered report.
fn spawn_watcher(guard: pprof::ProfilerGuard<'static>, path: PathBuf) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("pprof-dump".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if DUMP_REQUESTED.swap(false, Ordering::Relaxed) {
                    // Unfiltered on purpose: a hang might be *in* the runtime, so
                    // keep every frame.
                    dump(&guard, &path, false);
                }
                if SHUTDOWN.load(Ordering::Relaxed) {
                    break;
                }
            }
            dump(&guard, &path, true);
        })
        .expect("spawn pprof-dump thread")
}

/// Build the current pprof report and write it to `path`. When `filter_runtime`
/// is set, drop pure tokio/std scheduler frames (the exit-time report); the
/// on-demand dump keeps everything.
fn dump(guard: &pprof::ProfilerGuard<'_>, path: &Path, filter_runtime: bool) {
    use pprof::protos::Message;
    let report = if filter_runtime {
        guard
            .report()
            .frames_post_processor(filter_runtime_frames)
            .build()
    } else {
        guard.report().build()
    };
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to build CPU profile report");
            return;
        }
    };
    let profile = match report.pprof() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to build pprof profile");
            return;
        }
    };
    let mut content = Vec::new();
    if let Err(e) = profile.encode(&mut content) {
        warn!(error = %e, "Failed to encode pprof profile");
        return;
    }
    match write_atomic(path, &content) {
        Ok(()) => info!(path = %path.display(), "CPU profile written"),
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to write pprof file"),
    }
}

/// Write `content` to `path` via a sibling temp file plus a rename.
///
/// The whole premise of the on-demand dump is that someone reads the file while
/// the process is still stuck, so a plain `fs::write` — truncate, then fill —
/// hands them an empty or half-written profile whenever they read during the
/// write. The temp file is a sibling so the rename stays within one filesystem
/// and is therefore atomic.
fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// Retain only frames that aren't pure tokio/std scheduler machinery, so the
/// final report shows application work rather than runtime noise.
fn filter_runtime_frames(frames: &mut pprof::Frames) {
    frames.frames.retain(|syms| {
        syms.iter().all(|s| {
            let name = s.name();
            !name.starts_with("tokio::runtime")
                && !name.starts_with("tokio::task")
                && !name.starts_with("tokio::park")
                && !name.starts_with("tokio::loom")
                && !name.starts_with("tokio::time::driver")
                && !name.starts_with("std::thread")
                && !name.starts_with("std::panicking")
                && !name.starts_with("_pthread")
                && !name.starts_with("__pthread")
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use pprof::Symbol;
    use pprof::protos::Message;
    use std::time::Instant;

    /// CPU burned entirely inside this binary's own text: an integer mix that
    /// calls nothing, allocates nothing, and enters no shared object, so a sample
    /// that interrupts it has a leaf PC no [`UNWIND_BLOCKLIST`] entry can match.
    ///
    /// `black_box` on the accumulator and the loop variable keeps the whole thing
    /// from being folded to a constant; `#[inline(never)]` gives it a frame of its
    /// own, so `burn`'s caller can assert on it by name rather than on a count.
    #[inline(never)]
    fn cpu_only(rounds: u64) -> u64 {
        let mut acc = std::hint::black_box(0x9E37_79B9_7F4A_7C15_u64);
        for i in 0..rounds {
            acc = acc
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(std::hint::black_box(i) | 1);
            acc ^= acc >> 29;
        }
        std::hint::black_box(acc)
    }

    /// Rounds of [`cpu_only`] per `burn` iteration — enough that the in-binary
    /// half is comparable to the libc half rather than a rounding error on it.
    /// Sized by measurement, not by feel: see the sample counts in `burn`'s docs.
    const CPU_ROUNDS: u64 = 50_000;

    /// Keep `threads` threads busy for `at_least`, in two deliberately different
    /// places: **inside libc** (allocator churn, syscalls, thread create/join) and
    /// **inside this binary's own text** ([`cpu_only`]).
    ///
    /// Both halves are load-bearing, and for opposite assertions:
    ///
    /// - The libc churn is what the *crash* regression needs. The unwinder faults
    ///   when it interrupts a thread already inside `malloc`, the dynamic loader
    ///   or a syscall stub, so a pure integer loop — shallow stack, leaf PC in
    ///   this binary — exercises precisely the case that never crashed.
    /// - [`cpu_only`] is what the *profile has heph frames in it* assertion needs,
    ///   and it is not decoration. `pprof` drops a sample whose leaf PC is
    ///   blocklisted **by design**, so a workload built only from the first half
    ///   is one the sampler is meant to record almost nothing from. Measured on
    ///   darwin/arm64: a libc-only burn produced 0-6 distinct stacks, against
    ///   25-57 for the same burn with the blocklist switched off — the blocklist
    ///   was legitimately dropping 95-99% of the samples, and whether the profile
    ///   came out empty was decided by which straggler happened to land. It came
    ///   out empty on 4 of 50 runs, and 8 of 30 under load, which is what turned
    ///   this test red on master's own CI. The fix is not a longer burn or a
    ///   retry: it is giving the sampler a leaf it is *supposed* to keep, present
    ///   for the whole window, on every thread.
    ///
    /// Interleaved rather than run on a thread of its own, so it does not matter
    /// which thread the kernel picks to deliver `SIGPROF` to — a process-directed
    /// timer signal is delivered to *some* eligible thread, and on Darwin that is
    /// routinely not the one burning the most CPU.
    fn burn(threads: usize, at_least: Duration) {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    let start = Instant::now();
                    while start.elapsed() < at_least {
                        // Allocator churn across size classes: malloc/free, and
                        // large sizes push glibc into mmap/munmap.
                        let mut kept: Vec<Vec<u8>> = Vec::new();
                        for i in 0..64usize {
                            let byte = u8::try_from(i % 256).unwrap_or(0);
                            kept.push(vec![byte; 1 << (6 + (i % 12))]);
                        }
                        kept.retain(|v| v.len() % 3 == 0);
                        // Syscalls, and string formatting on top of the allocator.
                        for _ in 0..16 {
                            drop(std::fs::metadata(
                                std::env::current_exe().unwrap_or_default(),
                            ));
                        }
                        // Nested thread create/join: pthread + loader paths, and
                        // it deepens the stack the sampler has to walk.
                        let inner = std::thread::spawn(|| format!("{:?}", Instant::now()));
                        drop(inner.join());
                        // The in-binary half — the only leaves the blocklist is
                        // not meant to drop. See this function's docs.
                        std::hint::black_box(cpu_only(CPU_ROUNDS));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("burn thread must not panic");
        }
    }

    /// How many of `profile`'s samples name a function containing `needle`
    /// anywhere in their stack.
    ///
    /// Walks pprof's id indirection (sample → location → line → function → string
    /// table) rather than indexing by position: ids are not promised to be their
    /// index plus one, and a profile that renumbered them would otherwise make
    /// this silently answer zero.
    fn samples_naming(profile: &pprof::protos::Profile, needle: &str) -> usize {
        use std::collections::{HashMap, HashSet};
        let names: HashMap<u64, &str> = profile
            .function
            .iter()
            .filter_map(|f| {
                let idx = usize::try_from(f.name).ok()?;
                Some((f.id, profile.string_table.get(idx)?.as_str()))
            })
            .collect();
        let hits: HashSet<u64> = profile
            .location
            .iter()
            .filter(|loc| {
                loc.line.iter().any(|l| {
                    names
                        .get(&l.function_id)
                        .is_some_and(|n| n.contains(needle))
                })
            })
            .map(|loc| loc.id)
            .collect();
        profile
            .sample
            .iter()
            .filter(|s| s.location_id.iter().any(|id| hits.contains(id)))
            .count()
    }

    /// Resolve the shared object owning `sym`, as the dynamic loader sees it —
    /// the same name `pprof` matches [`UNWIND_BLOCKLIST`] against.
    fn owning_object(sym: *const std::ffi::c_void) -> Option<String> {
        // SAFETY: `Dl_info` is a plain C struct of pointers and integers, for
        // which all-zero is a valid initial state; `dladdr` fills it.
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: `sym` is a code address and `info` is a live, correctly-typed
        // out-param. `dladdr` only writes through it.
        let found = unsafe { libc::dladdr(sym, &mut info) };
        if found == 0 || info.dli_fname.is_null() {
            return None;
        }
        // SAFETY: `dladdr` succeeded and `dli_fname` is non-null, so it points at
        // a NUL-terminated string owned by the loader and valid for this read.
        let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
        Some(name.to_string_lossy().into_owned())
    }

    fn blocklisted(path: &str) -> bool {
        UNWIND_BLOCKLIST.iter().any(|b| path.contains(b))
    }

    /// The blocklist has to name the C library this process actually loaded, and
    /// must not name the main binary.
    ///
    /// Both halves are silent failures otherwise, which is what makes this the
    /// load-bearing test. `pprof` resolves these substrings against loaded shared
    /// objects **once**, at `start()`, and stores address ranges: match nothing
    /// and `blocklist_segments` is empty, so the fix is a no-op and the sampler
    /// goes back to faulting. Match the main binary and every heph frame is
    /// dropped, so `--pprof-cpu` writes empty profiles forever — crashing
    /// nothing, telling no one. `str::contains` is case-sensitive, which is how
    /// `libSystem.B.dylib` slips past a lowercase entry.
    ///
    /// A static build resolves `malloc` to the executable itself and fails here.
    /// That is correct: the blocklist genuinely cannot work in that build, and
    /// `src/diag.rs` already records that the sampler segfaults on static
    /// binaries.
    #[test]
    fn blocklist_covers_the_c_library_but_not_the_main_binary() {
        let libc_obj = owning_object(libc::malloc as *const std::ffi::c_void)
            .expect("dladdr must resolve malloc");
        assert!(
            blocklisted(&libc_obj),
            "UNWIND_BLOCKLIST does not cover the loaded C library ({libc_obj}); \
             the blocklist resolves to no address ranges and the fix is a no-op"
        );

        let own_obj = owning_object(blocklisted as *const std::ffi::c_void)
            .expect("dladdr must resolve a local function");
        assert!(
            !blocklisted(&own_obj),
            "UNWIND_BLOCKLIST matches the main binary ({own_obj}); \
             every heph sample would be dropped"
        );
    }

    fn sym(name: &str) -> Symbol {
        Symbol {
            name: Some(name.as_bytes().to_vec()),
            addr: None,
            lineno: None,
            filename: None,
        }
    }

    fn frames(groups: Vec<Vec<Symbol>>) -> pprof::Frames {
        pprof::Frames {
            frames: groups,
            thread_name: "test".to_string(),
            thread_id: 0,
            sample_timestamp: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// The exit-time report drops pure-runtime frames and keeps heph's own.
    ///
    /// This is the filter behind the profile `--pprof-cpu` produces when nobody
    /// sends a signal — i.e. the flag's default output.
    #[test]
    fn filter_runtime_frames_drops_runtime_and_keeps_app_frames() {
        let mut f = frames(vec![
            vec![sym("tokio::runtime::park::park")],
            vec![sym("heph::engine::result::result_addr")],
            vec![sym("std::thread::spawn")],
        ]);
        filter_runtime_frames(&mut f);
        let kept: Vec<String> = f.frames.iter().map(|g| g[0].name()).collect();
        assert_eq!(kept, vec!["heph::engine::result::result_addr".to_string()]);
    }

    /// The rules are matched against `Symbol::name()`, which demangles first — so
    /// they must fire on the mangled symbols a real profile actually carries.
    #[test]
    fn filter_runtime_frames_matches_mangled_symbols() {
        let mut f = frames(vec![vec![sym(
            "_ZN5tokio7runtime4park4park17h0123456789abcdefE",
        )]]);
        filter_runtime_frames(&mut f);
        assert!(
            f.frames.is_empty(),
            "mangled runtime symbol survived the filter: demangling is not being applied"
        );
    }

    /// An inline group is dropped if *any* symbol in it is runtime, so a heph
    /// frame with an inlined tokio call disappears from the exit-time report.
    /// Freezing the behavior because it is a real, non-obvious consequence of
    /// `all()` — not because it is obviously the right choice.
    #[test]
    fn filter_runtime_frames_drops_a_whole_inline_group() {
        let mut f = frames(vec![vec![
            sym("heph::engine::execute::run"),
            sym("tokio::task::spawn_blocking"),
        ]]);
        filter_runtime_frames(&mut f);
        assert!(f.frames.is_empty());
    }

    /// Profiling a busy multi-threaded process must produce a decodable profile
    /// with samples in it.
    ///
    /// Regression: `--pprof-cpu` segfaulted the process it was meant to diagnose,
    /// within seconds of startup on a wide run, because the `SIGPROF` handler
    /// unwound through libc frames. A crash takes this whole test binary down —
    /// that is the first half of the assertion, and it cannot be written any
    /// other way.
    ///
    /// The second half guards the fix rather than the bug: an over-broad
    /// [`UNWIND_BLOCKLIST`] (one matching the main binary) would drop every sample
    /// and leave `--pprof-cpu` writing empty profiles forever, crashing nothing
    /// and telling no one.
    ///
    /// It asserts on [`cpu_only`] by name, not on "the profile is non-empty".
    /// Non-empty was the weaker *and* the flakier statement: with the whole burn
    /// living inside libc, nearly every sample was one the sampler is *designed*
    /// to drop, so the profile came back empty on 4 of 50 darwin/arm64 runs — and
    /// on master's own CI. Naming a frame the blocklist must never match turns a
    /// statistical claim into a structural one, and says the thing the count was
    /// only standing in for: heph's own frames survive the blocklist.
    ///
    /// **Only one test in this binary may call [`start`].** pprof's `PROFILER` is
    /// a process singleton: a concurrent second `start` fails with
    /// `Error::Running`, and a sequential one would silently profile under the
    /// previous run's state. The deterministic properties are asserted by the
    /// sampler-free tests below precisely so this stays the only one.
    #[test]
    fn sampling_a_busy_process_yields_a_profile_without_crashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cpu.pb");

        let watcher = start(path.clone()).expect("start profiler");
        burn(4, Duration::from_millis(500));
        request_dump();

        // The watcher wakes on a 200ms tick, then encodes and renames into place.
        // Poll for the file rather than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut snapshot = None;
        while Instant::now() < deadline {
            if let Ok(content) = std::fs::read(&path) {
                snapshot = Some(content);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Before any assertion: an `assert!` that escapes here would leave the
        // watcher thread looping forever, holding the `ProfilerGuard` and keeping
        // SIGPROF firing for the rest of this test binary's life, with the
        // `TempDir` deleted out from under its next write.
        let snapshot = snapshot;
        watcher.shutdown();
        let final_report = std::fs::read(&path);

        let snapshot = snapshot.unwrap_or_else(|| panic!("no profile at {}", path.display()));
        let profile = pprof::protos::Profile::decode(snapshot.as_slice())
            .expect("on-demand dump must be a decodable pprof profile — never a partial write");
        let in_binary = samples_naming(&profile, "cpu_only");
        assert!(
            in_binary > 0,
            "no sample reached `cpu_only`, which burned a large share of this \
             process's CPU inside the main binary for the whole sampling window: \
             either UNWIND_BLOCKLIST now matches the main binary and every heph \
             frame is being dropped, or the sampler collected nothing at all \
             ({} sample(s) in total)",
            profile.sample.len()
        );

        // `shutdown` writes the filtered exit-time report — the profile the flag
        // produces when nobody sends a signal — and swallows a watcher panic into
        // a `warn!`, so this is the only thing that can catch it failing.
        let final_report = final_report.expect("exit-time report must be written on shutdown");
        pprof::protos::Profile::decode(final_report.as_slice())
            .expect("exit-time report must be a decodable pprof profile");
    }
}
