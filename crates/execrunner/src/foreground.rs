//! Running an entrypoint's program in the foreground, the way `system(3)` does.
//!
//! The one spawn in heph that is *not* a build step: the program inherits
//! heph's stdin, stdout and stderr, runs in the caller's cwd with the env it is
//! handed, and its exit status becomes heph's. Nothing is captured, tee'd or
//! sandboxed.
//!
//! # A temporary process model
//!
//! The tool-shims spec's model (D2) is a child in its own process group, with
//! a lazy terminal handoff and a signal relay. That model is blocked on open
//! design questions, so this is the stand-in: the child stays in heph's
//! process group, exactly as under `system(3)`. Terminal signals (Ctrl-C,
//! Ctrl-\) reach the whole group, so heph ignores `SIGINT` and `SIGQUIT` for
//! the length of the wait — the child sees and handles them itself — and
//! restores both, plus `SIGTTOU`/`SIGTTIN` (which heph ignores for its whole
//! life), to their defaults in the child before it execs, since an ignored
//! disposition survives `exec`.
//!
//! Every subprocess goes through this crate; this is where that holds for the
//! one heph waits on in the foreground.

use std::ffi::OsString;
use std::path::Path;

/// How the program ended: the driver contract's [`Exit`](hcore::process::Exit),
/// which this enum was merged into (R6).
// TODO(entrypoint-L3): drop the alias with the binary's spawn, which is its
// last user; the driver-run path names `hcore::process::Exit` directly.
pub use hcore::process::Exit as ForegroundExit;

/// `sigaction` dispositions saved on construction and restored on drop.
struct IgnoredSignals {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

impl IgnoredSignals {
    fn ignore(signals: &[libc::c_int]) -> std::io::Result<Self> {
        let mut saved = Vec::with_capacity(signals.len());
        for &sig in signals {
            // SAFETY: zeroed `sigaction` is a valid "no flags, empty mask"
            // value, filled in below before use.
            let mut ignore: libc::sigaction = unsafe { std::mem::zeroed() };
            ignore.sa_sigaction = libc::SIG_IGN;
            // SAFETY: as above; `old` is written by `sigaction`.
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: both pointers are to live, properly aligned values.
            if unsafe { libc::sigaction(sig, &ignore, &mut old) } != 0 {
                let err = std::io::Error::last_os_error();
                drop(Self { saved });
                return Err(err);
            }
            saved.push((sig, old));
        }
        Ok(Self { saved })
    }
}

impl Drop for IgnoredSignals {
    fn drop(&mut self) {
        for (sig, old) in self.saved.drain(..).rev() {
            // SAFETY: `old` is the disposition `sigaction` returned for `sig`.
            unsafe { libc::sigaction(sig, &old, std::ptr::null_mut()) };
        }
    }
}

/// Spawn `argv` with exactly `env` in `cwd`, stdio inherited, and wait for it.
///
/// `argv[0]` without a `/` is looked up on the `PATH` in `env`, not heph's.
/// Blocking: call it off the async runtime's workers.
pub fn run_foreground(
    argv: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
) -> std::io::Result<ForegroundExit> {
    use std::os::unix::process::CommandExt as _;

    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd);
    let restore_defaults = || {
        for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTTOU, libc::SIGTTIN] {
            // SAFETY: `signal(2)` is async-signal-safe, as the code between
            // fork and exec must be.
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
        Ok(())
    };
    // SAFETY: the closure runs between fork and exec and calls only
    // async-signal-safe functions; it allocates nothing.
    unsafe { cmd.pre_exec(restore_defaults) };

    // Before the fork, as `system(3)` does: a Ctrl-C between the fork and the
    // wait must not take heph down while its child lives.
    let _ignored = IgnoredSignals::ignore(&[libc::SIGINT, libc::SIGQUIT])?;
    let status = cmd.spawn()?.wait()?;
    // Neither exited nor signalled is a stop, which `wait` does not report.
    Ok(ForegroundExit::from_std(status).unwrap_or(ForegroundExit::Code(1)))
}

/// Die of `sig`, as the program did: restore its default disposition, make
/// sure no core is dumped whatever the caller's limit (the program already
/// dumped its own, if any), unblock it and raise it. Falls back to exiting
/// `128 + sig` if the signal does not kill (it always should).
pub fn reraise(sig: i32) -> ! {
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource and a pointer to a live `rlimit`.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) };
    // SAFETY: restoring a default disposition.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
    // SAFETY: a zeroed `sigset_t` is initialized by `sigemptyset` just below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a live, writable `sigset_t`.
    unsafe { libc::sigemptyset(&mut set) };
    // SAFETY: as above.
    unsafe { libc::sigaddset(&mut set, sig) };
    // SAFETY: `set` is initialized; a null old-mask pointer is allowed.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
    // SAFETY: raising a signal at this process.
    unsafe { libc::raise(sig) };
    std::process::exit(128 + sig)
}
