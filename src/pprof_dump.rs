//! On-demand CPU profiling for diagnosing hangs.
//!
//! `--pprof-cpu` on its own writes a profile only at process exit — useless for a
//! run that hangs (it never exits, and a CI timeout `SIGKILL`s it). Locked-down CI
//! containers also block ptrace, so gdb/perf/core dumps are unavailable. This
//! module keeps the profiler guard on a watcher thread that writes the profile
//! accumulated so far on `SIGUSR2`, so `kill -USR2 <pid>` snapshots a stuck
//! process in place (to a writable tmpfs path). The filtered final report is
//! still written at shutdown.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

/// Set by the `SIGUSR2` handler, polled by the watcher: request an on-demand dump.
static DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by [`Watcher::shutdown`]: write the final report and stop the watcher.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Handle to the running pprof watcher thread.
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
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .build()
        .map_err(|e| anyhow::anyhow!("start CPU profiler: {e}"))?;
    install_signal();
    Ok(Watcher {
        handle: spawn_watcher(guard, path),
    })
}

/// `SIGUSR2` handler: request a dump. Only stores to an atomic, so it is
/// async-signal-safe.
extern "C" fn on_sigusr2(_sig: libc::c_int) {
    DUMP_REQUESTED.store(true, Ordering::Relaxed);
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
    match std::fs::write(path, &content) {
        Ok(()) => info!(path = %path.display(), "CPU profile written"),
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to write pprof file"),
    }
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
