//! How a child process ended, and the signal vocabulary heph speaks to one.
//!
//! These live in the leaf crate because two layers that do not depend on each
//! other both need them: the driver contract (`hplugin`: `ProcessFailed.exit`,
//! `RunStdio::Inherit`) and the subprocess layer (`execrunner`'s foreground
//! spawn), which turns them into `kill`/`sigaction` calls.
//!
//! Every number here is an **OS-local** signal number, meaningful only on the
//! host that produced it (SIGUSR1 is 10 on Linux and 30 on macOS). It crosses
//! the plugin seam between a host and a cdylib loaded into the same process,
//! never a machine boundary, and is never persisted: no cache entry, event or
//! hash holds one.

/// How a process ended, when it did not succeed or when the caller needs the
/// status itself (an entrypoint's code is heph's exit code).
///
/// Not std's `ExitStatus`: that has no portable constructor, so it cannot be
/// rebuilt from the plugin wire, and its `Display` is not a contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Exit {
    /// Exited with this status.
    Code(i32),
    /// Killed by this signal number. A caller that mirrors it re-raises the
    /// signal rather than exiting `128 + n`: a shell tells the two apart.
    Signal(i32),
}

impl Exit {
    /// The status of a finished `std` child, or `None` for one that is neither
    /// (a stopped process, which `wait` does not report without `WUNTRACED`).
    pub fn from_std(status: std::process::ExitStatus) -> Option<Self> {
        use std::os::unix::process::ExitStatusExt as _;
        match (status.code(), status.signal()) {
            (Some(code), _) => Some(Exit::Code(code)),
            (None, Some(sig)) => Some(Exit::Signal(sig)),
            (None, None) => None,
        }
    }

    /// Whether this is a status a process can end a failed run with, so heph
    /// can exit with it: a code in 1–255 (what a wait status carries), or a
    /// signal whose default action terminates. A driver — a plugin's above
    /// all — that reports anything else (code 0 or 300, a SIGSTOP, a SIGCHLD,
    /// which re-raised would not end heph) is reporting heph's own failure.
    pub fn is_failure_status(self) -> bool {
        match self {
            Exit::Code(code) => (1..=255).contains(&code),
            Exit::Signal(sig) => (1..=MAX_SIGNAL).contains(&sig) && !NON_TERMINATING.contains(&sig),
        }
    }
}

/// Signals whose default action stops, continues or ignores rather than
/// terminates: no process dies of one.
const NON_TERMINATING: &[libc::c_int] = &[
    libc::SIGSTOP,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGCONT,
    libc::SIGCHLD,
    libc::SIGWINCH,
    libc::SIGURG,
    #[cfg(target_os = "macos")]
    libc::SIGINFO,
];

/// An OS signal number, as a type, so a signal is never confused with an exit
/// code or a pid. OS-local (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Signal(i32);

impl Signal {
    /// Wrap a raw signal number (`libc::SIGTERM`, …). No validation: an
    /// out-of-range number is refused by `kill`/`sigaction` where it is used.
    pub const fn from_raw(signo: i32) -> Self {
        Self(signo)
    }

    /// The raw signal number, for `kill(2)` and `sigaction(2)`.
    pub const fn as_raw(self) -> i32 {
        self.0
    }
}

/// What a signal's disposition is set to in a child before it execs.
///
/// Only the two dispositions that survive `execve` exist here: a handler is
/// reset to the default by the kernel anyway, so the set is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Disposition {
    /// `SIG_DFL`.
    Default,
    /// `SIG_IGN`: the child inherits the signal ignored, as under `nohup`.
    Ignore,
}

/// The caller's state a program run in its terminal gets back: what the host
/// recorded at startup and sends with the run (the driver contract's
/// `Foreground` implements it), and what the foreground spawn restores in the
/// child (`ForegroundCommand::with_foreground`). A trait here, in the leaf
/// crate, because neither of those two crates depends on the other.
pub trait CallerState {
    /// Dispositions to set in the child before it execs.
    fn dispositions(&self) -> &[(Signal, Disposition)];
    /// The `RLIMIT_NOFILE` soft limit to set; `None` leaves the process's.
    fn nofile(&self) -> Option<u64>;
    /// The signals to block in the child.
    fn blocked(&self) -> &[Signal];
}

/// The signals whose inherited disposition heph records: every signal heph
/// itself changes (TTOU/TTIN ignored at startup, INT/QUIT/USR1/USR2 handled,
/// the relay's TERM/HUP/WINCH), plus PIPE (see [`record_inherited`]).
const RECORDED_SIGNALS: [libc::c_int; 10] = [
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
    libc::SIGTERM,
    libc::SIGPIPE,
    libc::SIGTTOU,
    libc::SIGTTIN,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
];

/// What heph was started with, before it changed any of it.
#[derive(Debug)]
struct Inherited {
    dispositions: Vec<(Signal, Disposition)>,
    /// The soft `RLIMIT_NOFILE`, or `None` when it could not be read.
    nofile: Option<libc::rlim_t>,
    /// The signals blocked in the calling thread's mask (the caller's: `execve`
    /// keeps the mask, and nothing in heph changes it before `main`).
    blocked: Vec<Signal>,
}

static INHERITED: std::sync::OnceLock<Inherited> = std::sync::OnceLock::new();

/// SIGPIPE's disposition as the caller gave it, noted before Rust's runtime
/// ignores it: 0 not noted, 1 default, 2 ignored. See [`note_inherited_sigpipe`].
static INHERITED_SIGPIPE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Note SIGPIPE's disposition before std's runtime setup sets it to `SIG_IGN`.
///
/// For a binary's pre-`main` constructor (an `.init_array` /
/// `__mod_init_func` entry), which the loader runs before std's `lang_start`
/// ignores SIGPIPE. Only queries the disposition and stores an atomic, so it
/// is sound that early. Without it (a library, a test binary) SIGPIPE is
/// recorded as [`Disposition::Default`].
pub fn note_inherited_sigpipe() {
    let noted = if is_ignored(libc::SIGPIPE) { 2 } else { 1 };
    INHERITED_SIGPIPE.store(noted, std::sync::atomic::Ordering::Relaxed);
}

/// Every signal number the mask could hold, for reading it back as a list.
#[cfg(target_os = "linux")]
const MAX_SIGNAL: libc::c_int = 64;
#[cfg(target_os = "macos")]
const MAX_SIGNAL: libc::c_int = 31;

/// Record the signal dispositions, the signal mask and the open-file soft
/// limit this process inherited, so a program heph runs in the caller's
/// terminal gets them back (N4, R8) rather than heph's own.
///
/// Call once at the very top of `main`, on the main thread, before anything
/// ignores, handles or raises them; a later call keeps the first record.
/// SIGPIPE is whatever [`note_inherited_sigpipe`] saw before `main` — Rust
/// ignores it before `main` runs — and [`Disposition::Default`] if nothing
/// noted it.
pub fn record_inherited() {
    INHERITED.get_or_init(|| {
        let dispositions = RECORDED_SIGNALS
            .iter()
            .map(|&sig| {
                let ignored = if sig == libc::SIGPIPE {
                    INHERITED_SIGPIPE.load(std::sync::atomic::Ordering::Relaxed) == 2
                } else {
                    is_ignored(sig)
                };
                let disposition = if ignored {
                    Disposition::Ignore
                } else {
                    Disposition::Default
                };
                (Signal::from_raw(sig), disposition)
            })
            .collect();
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: a valid resource and a pointer to a live `rlimit`.
        let nofile = (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0)
            .then_some(lim.rlim_cur);
        Inherited {
            dispositions,
            nofile,
            blocked: current_mask(),
        }
    });
}

/// The calling thread's blocked signals.
fn current_mask() -> Vec<Signal> {
    // SAFETY: an all-zero `sigset_t` is a valid out-parameter.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a null new set only queries; `set` is live and writable.
    if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut set) } != 0 {
        return Vec::new();
    }
    (1..=MAX_SIGNAL)
        // SAFETY: `set` was filled in above; an out-of-range number is 0 or -1.
        .filter(|&sig| unsafe { libc::sigismember(&set, sig) } == 1)
        .map(Signal::from_raw)
        .collect()
}

/// A signal mask holding exactly `blocked`, built in the parent so a
/// `pre_exec` only has to install it ([`set_mask_for_exec`]).
pub fn mask_of(blocked: &[Signal]) -> libc::sigset_t {
    // SAFETY: an all-zero `sigset_t` is valid storage for `sigemptyset`.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is live and writable.
    unsafe { libc::sigemptyset(&mut set) };
    for sig in blocked {
        // SAFETY: as above; an invalid number is refused with EINVAL, leaving
        // the set as it was.
        unsafe { libc::sigaddset(&mut set, sig.as_raw()) };
    }
    set
}

/// Install `mask` as the calling thread's signal mask. Async-signal-safe and
/// allocation-free: fit for a `pre_exec` (std clears the mask in every child
/// before its `pre_exec` closures run, so this is what puts the caller's back).
pub fn set_mask_for_exec(mask: &libc::sigset_t) {
    // SAFETY: `mask` is a live, initialized set; a failure leaves the mask as
    // it was, which is all a caller could do anyway.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
}

/// Whether `sig` is currently `SIG_IGN`. Reads the disposition without
/// changing it.
fn is_ignored(sig: libc::c_int) -> bool {
    // SAFETY: an all-zero `sigaction` is a valid out-parameter.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: a null new action only queries; `old` is live and writable.
    let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

/// The dispositions [`record_inherited`] recorded, for every signal heph
/// touches; empty when nothing was recorded (a library, a test, a cdylib).
pub fn inherited_dispositions() -> &'static [(Signal, Disposition)] {
    INHERITED.get().map_or(&[], |i| i.dispositions.as_slice())
}

/// The open-file soft limit [`record_inherited`] recorded.
pub fn inherited_nofile() -> Option<libc::rlim_t> {
    INHERITED.get()?.nofile
}

/// The signals [`record_inherited`] saw blocked; empty when nothing was
/// recorded.
pub fn inherited_blocked() -> &'static [Signal] {
    INHERITED.get().map_or(&[], |i| i.blocked.as_slice())
}

/// Put back what [`record_inherited`] recorded — every recorded disposition,
/// the signal mask and the open-file soft limit — in *this* process, right before it replaces
/// itself with `execve` (the self-upgrade). The new image then records what
/// the caller gave, not what this one changed (R8). Allocates nothing and
/// calls only async-signal-safe functions, so it is fit for a `pre_exec`.
/// A no-op when nothing was recorded.
pub fn restore_inherited_for_exec() {
    let Some(inherited) = INHERITED.get() else {
        return;
    };
    for &(sig, disposition) in &inherited.dispositions {
        let handler = match disposition {
            Disposition::Default => libc::SIG_DFL,
            Disposition::Ignore => libc::SIG_IGN,
        };
        // SAFETY: `signal(2)` with a default or ignore disposition.
        unsafe { libc::signal(sig.as_raw(), handler) };
    }
    set_mask_for_exec(&mask_of(&inherited.blocked));
    if let Some(cur) = inherited.nofile {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: a valid resource and a pointer to a live `rlimit`.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
            lim.rlim_cur = cur.min(lim.rlim_max);
            // SAFETY: lowering the soft limit under the hard one.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

    /// Which reported statuses heph can exit with, and which mean the driver
    /// reported something no failed process ends with.
    #[test]
    fn failure_status_table() {
        let rows = [
            (Exit::Code(1), true),
            (Exit::Code(255), true),
            (Exit::Code(0), false),
            (Exit::Code(256), false),
            (Exit::Code(300), false),
            (Exit::Code(-1), false),
            (Exit::Signal(libc::SIGTERM), true),
            (Exit::Signal(libc::SIGKILL), true),
            (Exit::Signal(libc::SIGSEGV), true),
            (Exit::Signal(libc::SIGSTOP), false),
            (Exit::Signal(libc::SIGTSTP), false),
            (Exit::Signal(libc::SIGCHLD), false),
            (Exit::Signal(libc::SIGWINCH), false),
            (Exit::Signal(0), false),
            (Exit::Signal(-15), false),
            (Exit::Signal(MAX_SIGNAL + 1), false),
        ];
        for (exit, want) in rows {
            assert_eq!(exit.is_failure_status(), want, "{exit:?}");
        }
    }

    /// Set in the re-executed copy of this test binary, which changes its own
    /// dispositions: doing that in the shared test process would leak into
    /// every parallel test.
    const RECORD_CHILD: &str = "HEPH_CORE_RECORD_INHERITED_CHILD";

    /// What the caller ignored is recorded as ignored, what it left default as
    /// default, and SIGPIPE — ignored by Rust before `main`, and noted before
    /// it only by heph's binary (`bin-e2e`'s `entrypoint_keeps_inherited_dispositions`)
    /// — as default.
    /// Changes made after the record do not move it, and restoring puts the
    /// recorded state back.
    #[test]
    fn record_inherited_reads_what_the_caller_gave() {
        if std::env::var_os(RECORD_CHILD).is_some() {
            return;
        }
        let exe = std::env::current_exe().expect("current_exe");
        use std::os::unix::process::CommandExt as _;
        // `nohup`-like: the child starts with SIGINT and SIGTTOU ignored, and
        // with SIGUSR1 blocked (`sh` and `exec` keep the mask).
        let mut cmd = std::process::Command::new("/bin/sh");
        let mask = mask_of(&[Signal::from_raw(libc::SIGUSR1)]);
        // SAFETY: only installs a mask built before the fork.
        unsafe {
            cmd.pre_exec(move || {
                set_mask_for_exec(&mask);
                Ok(())
            })
        };
        let out = cmd
            .args(["-c", "trap '' INT TTOU; exec \"$0\" \"$@\""])
            .arg(exe)
            .args([
                "--exact",
                "process::tests::record_inherited_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RECORD_CHILD, "1")
            .output()
            .expect("re-exec the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{}\nstdout: {stdout}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "child ran nothing: {stdout}");
    }

    /// Only does anything in the re-executed binary (see above).
    #[test]
    fn record_inherited_child() {
        if std::env::var_os(RECORD_CHILD).is_none() {
            return;
        }
        record_inherited();
        let get = |sig| {
            inherited_dispositions()
                .iter()
                .find(|(s, _)| s.as_raw() == sig)
                .map(|&(_, d)| d)
        };
        assert_eq!(get(libc::SIGINT), Some(Disposition::Ignore));
        assert_eq!(get(libc::SIGTTOU), Some(Disposition::Ignore));
        assert_eq!(get(libc::SIGTTIN), Some(Disposition::Default));
        assert_eq!(get(libc::SIGTERM), Some(Disposition::Default));
        assert_eq!(get(libc::SIGPIPE), Some(Disposition::Default), "not noted");
        assert_eq!(inherited_dispositions().len(), RECORDED_SIGNALS.len());
        assert!(inherited_nofile().is_some());
        assert_eq!(
            inherited_blocked(),
            &[Signal::from_raw(libc::SIGUSR1)],
            "the caller's mask"
        );

        // What heph does after the record: ignore TTIN, handle-or-default INT.
        // SAFETY: this re-executed process runs only this test.
        unsafe { libc::signal(libc::SIGTTIN, libc::SIG_IGN) };
        // SAFETY: as above.
        unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
        record_inherited();
        assert_eq!(
            get(libc::SIGTTIN),
            Some(Disposition::Default),
            "first record wins"
        );

        restore_inherited_for_exec();
        assert!(is_ignored(libc::SIGINT), "INT back to ignored");
        assert!(!is_ignored(libc::SIGTTIN), "TTIN back to default");
        assert!(is_ignored(libc::SIGTTOU), "TTOU stays ignored");

        // A mask changed after the record is put back too.
        set_mask_for_exec(&mask_of(&[]));
        assert!(current_mask().is_empty());
        restore_inherited_for_exec();
        assert_eq!(current_mask(), [Signal::from_raw(libc::SIGUSR1)]);
    }

    #[test]
    fn exit_from_std_reads_code_and_signal() {
        // Raw wait statuses: code 3 is `3 << 8`, signal 15 is `15`.
        assert_eq!(
            Exit::from_std(std::process::ExitStatus::from_raw(3 << 8)),
            Some(Exit::Code(3))
        );
        assert_eq!(
            Exit::from_std(std::process::ExitStatus::from_raw(15)),
            Some(Exit::Signal(15))
        );
    }
}
