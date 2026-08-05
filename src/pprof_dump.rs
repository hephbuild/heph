//! On-demand CPU profiling for diagnosing hangs.
//!
//! `--pprof-cpu` on its own writes a profile only at process exit — useless for a
//! run that hangs (it never exits, and a CI timeout `SIGKILL`s it). Locked-down CI
//! containers also block ptrace, so gdb/perf/core dumps are unavailable. This
//! module keeps the profiler guard on a watcher thread that writes the profile
//! accumulated so far on `SIGUSR2`, so `kill -USR2 <pid>` snapshots a stuck
//! process in place (to a writable tmpfs path). The filtered final report is
//! still written at shutdown.
//!
//! Sampling happens in a `SIGPROF` handler, so how the stack is walked is a
//! correctness question, not a quality one — [`start`] documents why this build
//! walks frame pointers rather than calling the DWARF unwinder.

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
/// Every sample is a signal delivered to a running thread, and the handler runs
/// at whatever instruction it interrupted. 1000 Hz across every thread of a wide
/// build bought resolution this diagnostic never needed: it exists to show *which
/// loop* is burning a pegged worker, and a hang that has lasted minutes is not a
/// signal 199 Hz can miss. The stack walk is signal-safe either way (see
/// [`start`]) — this is about the per-sample cost heph pays while profiled, on
/// every worker, not about containing a crash.
const SAMPLE_HZ: libc::c_int = 199;

/// Libraries the sampler must not walk into.
///
/// `pprof` resolves these names against the loaded shared objects and applies
/// them twice: it drops a whole sample whose *interrupted PC* lands in one, and —
/// because this build enables the `frame-pointer` tracer (see [`start`]) — it
/// ends the walk at the first frame that reaches one. Both checks read this one
/// list, so an entry here buys frame-level containment at the price of every
/// sample that happened to be interrupted inside that library.
///
/// This used to be the *entire* defence, back when the tracer was
/// `_Unwind_Backtrace` and only the leaf PC could be tested — see [`start`] for
/// why that was never sound and what replaced it.
///
/// **The C library is deliberately not here**, and that is the difference
/// between a profile and a fiction. heph is in libc constantly — `malloc`,
/// `free`, `memcpy`, every syscall stub — so blocklisting it dropped roughly
/// half of every profile with nothing in the output to say so: a warm 85k-target
/// resolution spends ~15% of its CPU inside the allocator and ~10% in `memmove`,
/// and `--pprof-cpu` reported *none* of it. Keeping those samples costs nothing
/// in walk safety, because the frame-pointer walk never starts inside the
/// interrupted function: it starts at the return address in the innermost frame
/// record, which for a sample taken in `malloc` is heph's own code (see
/// [`start`] — this is the same one-frame shift that makes `flat` read as "time
/// in the callee"). The walk climbs heph frames from there and never enters libc
/// at all. The `libc_only` half of
/// [`tests::sampling_a_busy_process_yields_a_profile_without_crashing`] asserts
/// exactly that, and goes to zero the moment the C library is re-added.
///
/// One caveat, and it is an **x86_64-only fidelity** one rather than a
/// behavioural split (there is no `cfg` here; all three targets keep the sample
/// and attribute it to the caller). The walk reads the frame-pointer register the
/// interrupted function left behind. On both aarch64 targets the procedure call
/// standard reserves `x29` for the frame record, so it is heph's frame and the
/// stack is real. glibc on x86_64 is built `-fomit-frame-pointer` and `%rbp` is
/// merely callee-saved, so a libc routine that is *using* `%rbp` as a scratch
/// register when `SIGPROF` lands yields a stack that is fabricated rather than
/// merely shifted. It cannot fault — every frame address is probed before it is
/// dereferenced ([`start`]) — so the cost is a wrong name on some libc-leaf
/// samples there, against the whole allocator/memcpy/syscall half of the profile
/// being deleted on every target. Blocklisting the C library on x86_64 alone
/// would trade that back; it is deliberately not done, because a profiler that
/// silently omits half its subject is the worse failure of the two.
///
/// What remains are the libraries whose frames the walk must not *climb into*
/// because their unwind state is hostile rather than merely uninteresting: the
/// loader, the unwinder, and GCD.
///
/// Matched as substrings against shared-object paths, and `str::contains` is
/// **case-sensitive**: macOS ships `/usr/lib/libSystem.B.dylib`, which lowercase
/// `libsystem` does not match — so a name needing both spellings must list both.
///
/// Entries are anchored (`ld-linux`, not `ld`) because the match is a bare
/// substring and over-blocking is the quieter failure of the two: those samples
/// vanish from every profile with nothing to indicate it, which for a profiler
/// pointed at a stall drops exactly the frames worth having.
const UNWIND_BLOCKLIST: &[&str] = &[
    // The unwinder itself: libgcc's `object_mutex` and libunwind's loader reads
    // are the states that faulted the process being profiled.
    "libgcc_s",
    "libunwind",
    // The dynamic loader, on both platforms.
    "ld-linux",
    "libdyld",
    "vdso",
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

/// Whether this build keeps a frame record in every function, which is what the
/// sampler's stack walk follows (see [`start`]). Set by `build.rs::frame_pointers`
/// from the rustflags this build was compiled with.
fn have_frame_pointers() -> bool {
    cfg!(heph_frame_pointers)
}

/// Start CPU sampling plus the `SIGUSR2`-driven dump watcher, writing profiles to
/// `path`. Call [`Watcher::shutdown`] at exit for the final report.
///
/// # How the stack is walked, and why it is not the unwinder
///
/// Every sample runs inside a `SIGPROF` handler, on whichever thread the kernel
/// picked, at whatever instruction it happened to interrupt. The only code that
/// may run there is async-signal-safe code — and `_Unwind_Backtrace`, which
/// `pprof` uses by default (via the `backtrace` crate), is not. It takes
/// libgcc's global `object_mutex`, re-enters `dl_iterate_phdr`, and reads
/// loader state that the interrupted thread may have been halfway through
/// mutating. Interrupt the wrong instruction and walking that stack faults —
/// which is how `--pprof-cpu` segfaulted the very process it was asked to
/// diagnose, within seconds of startup on a wide build.
///
/// [`UNWIND_BLOCKLIST`] was the first attempt at containing that: drop a sample
/// whose *interrupted PC* is inside libc, the loader, or the unwinder. It is a
/// heuristic on one address, and it left the class open — a sample that starts
/// in heph code and climbs into libc was still handed to the unwinder in full,
/// and heph is always in libc somewhere on some thread.
///
/// So the tracer itself is replaced: the `frame-pointer` feature makes `pprof`
/// walk the frame-pointer chain instead. That walk cannot fault, by
/// construction rather than by heuristic —
///
///   - it takes no lock and calls nothing in libc or the loader,
///   - every candidate frame address is probed with a `write(2)` to a pipe
///     (`EFAULT` ⇒ unreadable) *before* it is dereferenced, so a garbage frame
///     pointer truncates the stack instead of segfaulting,
///   - the chain must climb (a frame pointer below its predecessor ends the
///     walk), so a corrupt chain cannot loop,
///   - and [`UNWIND_BLOCKLIST`] is applied to every frame, not just the leaf.
///
/// It needs frame pointers to be there: `-Cforce-frame-pointers=yes` in
/// `.cargo/config.toml` is what puts them there, and
/// [`tests::this_build_has_frame_pointers`] fails if that flag is ever dropped —
/// without it the walk does not crash, it silently reports garbage, which for a
/// profiler is the worse outcome.
///
/// The cost is one frame of resolution: the walk starts at the return address in
/// the innermost frame record, so the interrupted function is attributed to its
/// caller. For "which loop is burning a pegged worker" — the question this flag
/// exists to answer — the caller chain is what identifies the loop.
pub fn start(path: PathBuf) -> anyhow::Result<Watcher> {
    // A build without frame pointers cannot be sampled honestly: the walk reads
    // whatever the register held and reports a stack that looks real. Refuse,
    // rather than hand back fiction nobody can recognise as fiction.
    if !have_frame_pointers() {
        anyhow::bail!(
            "this heph was built without frame pointers, so CPU profiles would report \
             fabricated stacks; rebuild with `-Cforce-frame-pointers=yes` (the workspace \
             `.cargo/config.toml` sets it — an explicit RUSTFLAGS replaces it)"
        );
    }

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
    /// from being folded to a constant, so `burn`'s caller can assert on it by
    /// name rather than on a count.
    ///
    /// Split into a loop and a per-round callee, both `#[inline(never)]`, on
    /// purpose. One `#[inline(never)]` leaf is not enough: optimized, the
    /// interrupted leaf can be frameless and the unwinder attributes the sample
    /// to whatever called it — measured, an `opt-level=3` build of this very test
    /// binary named `cpu_only` in **zero** samples and failed 10/10. With the
    /// round in a callee, `cpu_only` is a *caller* — recovered from a return
    /// address on the stack, not from leaf attribution — and either name
    /// satisfies [`CPU_NEEDLE`]. The call is what makes the frame observable, so
    /// it is not an inlining hint to be removed.
    #[inline(never)]
    fn cpu_only_round(acc: u64, i: u64) -> u64 {
        let acc = acc
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(std::hint::black_box(i) | 1);
        std::hint::black_box(acc ^ (acc >> 29))
    }

    #[inline(never)]
    fn cpu_only(rounds: u64) -> u64 {
        let mut acc = std::hint::black_box(0x9E37_79B9_7F4A_7C15_u64);
        for i in 0..rounds {
            acc = cpu_only_round(acc, i);
        }
        std::hint::black_box(acc)
    }

    /// Rounds of [`cpu_only`] per `burn` iteration.
    ///
    /// A trade between the two assertions, which share one fixed burn window:
    /// every round spends time in [`cpu_only`] *and* in [`libc_only`], so rounds
    /// bought for one half are taken from the other. Up buys margin on the
    /// in-binary assertion; down leaves more of the window inside libc, which is
    /// the only part that exposes the sampler to the frames `UNWIND_BLOCKLIST`
    /// exists for.
    ///
    /// **Re-measure this whenever either half's workload changes** — it is the
    /// balance point between them, not a property of [`cpu_only`] alone. It was
    /// last set from the numbers below: 3 runs per cell, darwin/arm64, opt0,
    /// `burn(4, 1500ms)`, idle, counting sample *events* (`Sample::value`, not
    /// distinct stacks) against a ~1000-event window:
    ///
    /// | rounds | `cpu_only` stacks | `cpu_only` events | `libc_only` events |
    /// |---|---|---|---|
    /// | 25_000 | 6-14 | 6-20 | 155-169 |
    /// | 100_000 | 22-25 | 50-85 | 125-130 |
    /// | **250_000** | **27** | **142-164** | **70-87** |
    /// | 500_000 | 28 | 229-247 | 33-43 |
    /// | 1_000_000 | 28 | 227-281 | 21-31 |
    ///
    /// 250_000 is where the two halves are within ~2x of each other. Both
    /// neighbours starve one assertion or the other: 25_000 leaves `cpu_only` at
    /// ~1% of the window, and 500_000 takes `libc_only` down toward the level
    /// that already produced zero stacks on a linux/amd64 runner once (see
    /// [`LIBC_COPY_BYTES`]).
    ///
    /// 25_000 is where this constant sat until the libc half grew 32x
    /// ([`LIBC_COPY_BYTES`], 1 MiB x1 -> 4 MiB x8) without it being re-measured.
    /// That silently cut `cpu_only` from a roughly even split to ~1% of the
    /// window — 6-20 events across as few as 6 distinct stacks, for an assertion
    /// needing 1 — and the in-binary half went red on master's darwin/arm64 CI
    /// with 0 hits in a 30-stack profile. Under the same load here the recalibrated
    /// value holds 25 stacks and 87-140 events.
    ///
    /// The split depends on the allocator and on memory bandwidth, so it will not
    /// be the same on the Linux targets — the margins above are sized for that.
    const CPU_ROUNDS: u64 = 250_000;

    /// Substring identifying an in-binary frame: matches both [`cpu_only`] and
    /// [`cpu_only_round`], because which of the two a given build surfaces
    /// depends on the optimization level (see [`cpu_only`]).
    const CPU_NEEDLE: &str = "cpu_only";

    /// CPU burned with the leaf PC *inside the C library* for essentially the
    /// whole call: a large `memcpy` per round, with no heph instruction between
    /// the two ends of it.
    ///
    /// This is the shape [`UNWIND_BLOCKLIST`] used to erase. A sample landing here
    /// is interrupted inside `memmove`, so the old list matched its PC and dropped
    /// it before anything was walked. With the C library off the list the sample
    /// survives, and the frame-pointer walk — which starts at the return address
    /// in the innermost frame record, never inside the interrupted function —
    /// attributes it to this function, in heph's own text.
    ///
    /// `#[inline(never)]` so there is a frame to name, and `black_box` on both
    /// buffers so the copy is not elided or hoisted out of the loop.
    #[inline(never)]
    fn libc_only(src: &[u8], dst: &mut [u8]) {
        for _ in 0..LIBC_COPIES_PER_ROUND {
            dst.copy_from_slice(std::hint::black_box(src));
            std::hint::black_box(&dst[0]);
        }
    }

    /// Substring identifying the libc-leaf frame ([`libc_only`]).
    const LIBC_NEEDLE: &str = "libc_only";

    /// Bytes per copy, and copies per [`burn`] round.
    ///
    /// These exist to make the libc half a real *share of the window* rather
    /// than merely present in it, and the first version got that wrong: one
    /// 1 MiB copy per round is roughly 30k cycles against [`CPU_ROUNDS`]'s
    /// ~500k instructions of `cpu_only` at `opt-level = 0`, so the libc half was
    /// an order of magnitude smaller than the half it sits next to. It held up
    /// on darwin/arm64 and produced **zero** stacks on the linux/amd64 runner,
    /// where the whole profile is thinner (16 distinct stacks against 24, and
    /// `cpu_only` down to 1 from 21) — a starved assertion, not a broken one,
    /// but a red test either way.
    ///
    /// 8 x 4 MiB = 32 MiB per round puts the two halves within the same order,
    /// so neither assertion depends on the runner having a good day.
    ///
    /// Both halves run in the same round, so **changing this changes the split**:
    /// this 32x bump took `cpu_only` from a roughly even share of the window to
    /// ~1% of it, and the in-binary assertion went red on CI a week later. Whoever
    /// moves this next re-measures [`CPU_ROUNDS`] against it.
    const LIBC_COPY_BYTES: usize = 4 << 20;
    const LIBC_COPIES_PER_ROUND: usize = 8;

    /// Keep `threads` threads busy for `at_least`, in two deliberately different
    /// places: **inside libc** (allocator churn, syscalls, thread create/join) and
    /// **inside this binary's own text** ([`cpu_only`]).
    ///
    /// Both halves are load-bearing, and for opposite assertions:
    ///
    /// - The libc churn is the half that puts the sampler in front of the frames
    ///   the blocklist exists for, and in front of the states the old unwinder-based
    ///   tracer faulted in: a thread already inside `malloc`, the dynamic loader,
    ///   or a syscall stub. A pure integer loop — shallow stack, leaf PC in this
    ///   binary — exercises precisely the case that never crashed. Note what this
    ///   does *not* claim: the original segfault has not been shown to reproduce
    ///   here on any supported target (it fires on neither darwin/arm64 nor
    ///   linux/arm64, blocklist off, under a 32-thread 20s hammer of concurrent
    ///   `Backtrace::force_capture` + `dlopen`/`dlclose` + allocator churn), so
    ///   the first half of this test is "the process survives being profiled",
    ///   not a reproduction of the fault. That is why the churn is kept large
    ///   rather than trimmed to whatever the second assertion needs — and why the
    ///   real fix was to stop calling the unwinder at all ([`start`]) rather than
    ///   to keep widening a blocklist against a fault no test here can summon.
    /// - [`cpu_only`] is what the *profile has heph frames in it* assertion needs,
    ///   and it is not decoration. It was added when the C library *was*
    ///   blocklisted and `pprof` therefore dropped a libc-leaf sample by design,
    ///   which made a libc-only workload one the sampler recorded almost nothing
    ///   from. Measured on darwin/arm64 then: 0-6 distinct stacks, against 25-57
    ///   for the same burn with the blocklist switched off — 95-99% of samples
    ///   dropped, and whether the profile came out empty was decided by which
    ///   straggler happened to land (empty on 4 of 50 runs, 8 of 30 under load,
    ///   which turned this test red on master's own CI). The C library is off the
    ///   list now, so that is no longer the reason to keep [`cpu_only`] — it stays
    ///   because it is the one leaf whose attribution does not depend on the
    ///   blocklist at all, and so distinguishes "the sampler stopped working" from
    ///   "libc samples are being dropped again".
    /// - [`libc_only`] is the converse, and it is the assertion this list's
    ///   contents are actually load-bearing for: a leaf that lives inside libc for
    ///   essentially its whole duration. Re-add the C library to
    ///   [`UNWIND_BLOCKLIST`] and it disappears from the profile entirely, while
    ///   [`cpu_only`] keeps the test green.
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
                    let src = vec![0xA5u8; LIBC_COPY_BYTES];
                    let mut dst = vec![0u8; LIBC_COPY_BYTES];
                    while start.elapsed() < at_least {
                        // The libc-leaf half — a leaf PC that stays inside
                        // `memmove`. See this function's docs.
                        libc_only(&src, &mut dst);
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

    /// How many of `profile`'s **sample entries** name a function containing
    /// `needle` anywhere in their stack.
    ///
    /// Entries, not sampling events: pprof collapses identical stacks into one
    /// `Sample` carrying a count, so this is a count of *distinct stacks* and the
    /// number of `SIGPROF` deliveries behind it is larger.
    ///
    /// Walks pprof's id indirection (sample → location → line → function → string
    /// table) rather than indexing by position: ids are not promised to be their
    /// index plus one, and a profile that renumbered them would otherwise make
    /// this silently answer zero.
    fn stacks_naming(profile: &pprof::protos::Profile, needle: &str) -> usize {
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

    /// This build must carry frame pointers, and heph must know that it does.
    ///
    /// The whole stack walk rests on it (see [`start`]), and both halves of the
    /// wiring fail silently on their own. Drop `-Cforce-frame-pointers=yes` from
    /// `.cargo/config.toml` and the frame chain becomes whatever the register
    /// held; break `build.rs::frame_pointers` and the cfg goes missing while the
    /// flag is still passed, which turns `--pprof-cpu` into a hard error on a
    /// build that would have profiled perfectly well.
    ///
    /// Asserting the cfg rather than the codegen is deliberate: the test binary
    /// is built at `opt-level = 0`, where rustc keeps frame pointers anyway, so a
    /// test that walked its own frame chain would pass with the flag removed —
    /// it could not fail. The cfg is the thing that actually tracks the flag.
    ///
    /// It is the **Linux** runs of this test that carry it. On
    /// aarch64-apple-darwin the platform ABI reserves x29 for the frame record,
    /// so `build.rs` sets the cfg unconditionally and this cannot go red — which
    /// is why it must keep running on all three supported targets and not be
    /// judged by a green macOS run.
    #[test]
    fn this_build_has_frame_pointers() {
        assert!(
            have_frame_pointers(),
            "cfg(heph_frame_pointers) is unset: either `.cargo/config.toml` no longer \
             passes -Cforce-frame-pointers=yes (CPU profiles would be fabricated), or \
             build.rs stopped recognising the flag (--pprof-cpu now refuses to start)"
        );
    }

    /// The blocklist must name neither the C library nor the main binary.
    ///
    /// Both halves are silent failures, which is what makes this the load-bearing
    /// test. `pprof` applies these substrings to the *interrupted PC* as well as
    /// to each walked frame, and a sample whose PC matches is discarded whole. So
    /// naming the C library deletes every sample taken in `malloc`, `memcpy`, or a
    /// syscall stub — on a warm 85k-target resolution that is roughly half the
    /// profile, removed with nothing in the output to say so. Naming the main
    /// binary deletes every heph frame instead, so `--pprof-cpu` writes empty
    /// profiles forever — telling no one.
    ///
    /// `str::contains` is case-sensitive, which is how `libSystem.B.dylib` used to
    /// slip past a lowercase `libsystem` entry; the C library is now checked under
    /// both spellings so a re-added entry cannot pass this in one case and fail in
    /// the other.
    ///
    /// Dropping the C library costs no walk safety — see [`UNWIND_BLOCKLIST`] for
    /// why the frame-pointer walk never starts inside the interrupted function —
    /// and [`sampling_a_busy_process_yields_a_profile_without_crashing`] asserts
    /// the behaviour this name only constrains.
    ///
    /// A static build resolves `malloc` to the executable itself, which satisfies
    /// both halves here for the wrong reason; that build has no separate C library
    /// to distinguish.
    #[test]
    fn blocklist_spares_the_c_library_and_the_main_binary() {
        let libc_obj = owning_object(libc::malloc as *const std::ffi::c_void)
            .expect("dladdr must resolve malloc");
        assert!(
            !blocklisted(&libc_obj),
            "UNWIND_BLOCKLIST covers the loaded C library ({libc_obj}); every sample \
             interrupted in malloc/memcpy/a syscall stub — about half of a warm \
             resolution's CPU — is discarded before it is ever walked"
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
    /// called the DWARF unwinder. A crash takes this whole test binary down —
    /// that is the first half of the assertion, and it cannot be written any
    /// other way.
    ///
    /// The second half guards the fix rather than the bug, and guards it against
    /// two different silent failures now: an over-broad [`UNWIND_BLOCKLIST`] (one
    /// matching the main binary) would drop every sample, and a frame-pointer walk
    /// on a build without frame pointers would report stacks that never name this
    /// binary's own functions. Either leaves `--pprof-cpu` producing nothing worth
    /// reading while crashing nothing and telling no one.
    ///
    /// It asserts on [`cpu_only`] by name, not on "the profile is non-empty".
    /// Non-empty was the weaker *and* the flakier statement: with the whole burn
    /// living inside libc, nearly every sample was one the sampler is *designed*
    /// to drop, so the profile came back empty on 4 of 50 darwin/arm64 runs — and
    /// on master's own CI. Naming a frame the blocklist must never match turns a
    /// statistical claim into a structural one, and says the thing the count was
    /// only standing in for: heph's own frames survive the blocklist. It is also
    /// strictly stronger — a lone libc straggler satisfies "non-empty" while every
    /// heph frame is being dropped, which is the failure it claimed to guard.
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
        // 1.5s, not 500ms. `SIGPROF` is *process*-directed, so 199 Hz is 199
        // deliveries per second for the whole process no matter how many threads
        // burn — ~100 samples in 500ms, split across two halves and then deduped
        // into distinct stacks. That was already thin enough to turn this test
        // red on master's own CI once; it now carries a second assertion, so the
        // sample budget has to cover both.
        burn(4, Duration::from_millis(1500));
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
        // Diagnosis is branched on whether *anything* was sampled, because the two
        // states have disjoint causes and a message naming the wrong one sends the
        // reader after a bug that is not there.
        let in_binary = stacks_naming(&profile, CPU_NEEDLE);
        assert!(
            in_binary > 0,
            "no stack named `{}`, though it burned a large share of this process's \
             CPU inside the main binary for the whole sampling window. {}",
            CPU_NEEDLE,
            if profile.sample.is_empty() {
                "The profile is empty: either UNWIND_BLOCKLIST now matches the main \
                 binary and every heph frame is being dropped, or the sampler \
                 collected nothing at all."
                    .to_string()
            } else {
                format!(
                    "The profile has {} distinct stack(s), so the sampler is \
                     working — the frames did not symbolize (a stripped build), or \
                     `cpu_only`/`cpu_only_round` was renamed without updating \
                     CPU_NEEDLE, or this build folded both frames away.",
                    profile.sample.len()
                )
            }
        );

        // The half that guards [`UNWIND_BLOCKLIST`]'s contents: a leaf that sits
        // inside libc for essentially its whole duration must still be sampled,
        // and attributed to the heph frame that called into libc. Re-add the C
        // library to the blocklist and this goes to zero while `cpu_only` above
        // stays green — which is exactly how the missing half of every profile
        // went unnoticed.
        let in_libc = stacks_naming(&profile, LIBC_NEEDLE);
        assert!(
            in_libc > 0,
            "no stack named `{LIBC_NEEDLE}`, though every thread spent most of the \
             sampling window inside {LIBC_COPIES_PER_ROUND} x {LIBC_COPY_BYTES}-byte \
             memmoves per round. The profile has \
             {} distinct stack(s) and {in_binary} naming `{CPU_NEEDLE}`, so the sampler \
             is working: UNWIND_BLOCKLIST covers the C library again, and every sample \
             interrupted in malloc/memcpy/a syscall is being discarded.",
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
