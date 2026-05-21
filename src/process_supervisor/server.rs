use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

use crate::process_supervisor::protocol::Msg;

/// Supervisor entry point. Reads `TRACK`/`UNTRACK` lines from `ipc_fd` until
/// EOF (parent process died) or read error, then SIGKILLs every tracked
/// process group and exits.
///
/// Intentionally synchronous and tokio-free so the supervisor stays small and
/// has no surprise dependencies that could fail under memory pressure.
pub fn run_supervisor_main(ipc_fd: i32) -> ! {
    // Block SIGPIPE so a closed peer never raises a signal in the supervisor.
    // SAFETY: signal(2) at startup before any threads exist — safe on unix.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // SAFETY: ipc_fd was inherited from the parent across exec as the child
    // end of a socketpair created in process_supervisor::init. Ownership is
    // transferred here exactly once.
    let sock = unsafe { UnixStream::from_raw_fd(ipc_fd) };
    let reader = BufReader::new(sock);
    let mut tracked: HashSet<i32> = HashSet::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        match Msg::parse(&line) {
            Ok(Msg::Track(p)) => {
                tracked.insert(p);
            }
            Ok(Msg::Untrack(p)) => {
                tracked.remove(&p);
            }
            Err(_) => {
                // Malformed line — ignore. Parent and supervisor share a
                // version, so this should only happen on corruption.
            }
        }
    }

    // Parent died (EOF) or socket errored. Reap everything we know about.
    for pgid in &tracked {
        // SAFETY: killpg with SIGKILL on a pgid we recorded; kernel rejects
        // unknown pgids cleanly.
        unsafe {
            libc::killpg(*pgid, libc::SIGKILL);
        }
    }
    std::process::exit(0);
}
