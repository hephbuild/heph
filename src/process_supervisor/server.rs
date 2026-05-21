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
    // Issue BOTH killpg and kill for each tracked pid:
    //   - killpg only does anything if `id` is a session/group leader (i.e.
    //     the child called setsid). For pluginexec shell mode this reaps
    //     the entire descendant tree.
    //   - kill targets the direct pid for drivers that did NOT setsid
    //     (plugingo, pluginnix — single-process invocations).
    // PIDs are unique system-wide and a foreign pgid==our_pid is impossible
    // (pgids are pids of group leaders), so this is safe.
    for id in &tracked {
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "killpg and kill are a paired best-effort reap"
        )]
        // SAFETY: SIGKILL via killpg/kill on ids we recorded; ESRCH for
        // already-dead targets is ignored.
        unsafe {
            libc::killpg(*id, libc::SIGKILL);
            libc::kill(*id, libc::SIGKILL);
        }
    }
    std::process::exit(0);
}
