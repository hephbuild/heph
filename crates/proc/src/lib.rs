//! Process execution + lifecycle: spawn/reap subprocess groups (`proc_exec`),
//! the out-of-process supervisor sidecar that reaps orphaned groups
//! (`process_supervisor`), and the macOS kqueue-based exit watcher
//! (`process_watcher`). Depends only on `heph-core` (cancellation).

pub mod fork_safety;
pub mod proc_exec;
pub mod process_supervisor;
#[cfg(target_os = "macos")]
pub mod process_watcher;
pub mod pty;

/// Whether a process with this pid exists, whoever owns it.
///
/// A pid answers for a *process*, not for one particular run: once the process
/// ends its pid can be reused. Good enough to tell litter from a live owner's
/// files, which is what the dead-pid sweeps use it for; not a lock.
///
/// A pid of 0 or below is dead: `kill` reads it as a process group (or every
/// process), not as one process.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs the permission check and returns,
    // sending nothing. It takes a plain `pid_t` and touches no memory we own,
    // so there is no pointer or lifetime obligation.
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    // ESRCH means no such process; EPERM means it exists and belongs to someone
    // else, which still counts as alive.
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_process_is_alive() {
        assert!(super::pid_alive(std::process::id() as i32));
    }

    /// pid 1 (init/launchd) always exists and, for a non-root caller, belongs to
    /// someone else: `kill` answers EPERM, which must still read as alive.
    #[test]
    fn another_users_process_is_alive() {
        assert!(super::pid_alive(1));
    }

    /// Above every platform's pid ceiling, so no process can hold it: ESRCH.
    #[test]
    fn a_pid_no_process_can_hold_is_dead() {
        assert!(!super::pid_alive(i32::MAX));
    }

    /// `kill(0, 0)` probes the caller's process group and `kill(-1, 0)` every
    /// process it may signal; both succeed, and neither names one process. A
    /// pid read from a corrupt or hand-edited file must not read as a live
    /// owner.
    #[test]
    fn zero_and_negative_pids_are_dead() {
        assert!(!super::pid_alive(0), "0 is the caller's process group");
        assert!(!super::pid_alive(-1), "-1 is every process");
    }
}
