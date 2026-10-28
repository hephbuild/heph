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
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

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
