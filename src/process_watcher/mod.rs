//! macOS child-exit watcher.
//!
//! A single dedicated OS thread owns one `kqueue` fd with `EVFILT_PROC` /
//! `NOTE_EXIT` filters per registered pid plus a 1-second `waitpid(WNOHANG)`
//! backstop for events the kernel silently dropped under load.
//!
//! Compared to `tokio::process::Child::wait` this bypasses tokio's
//! SIGCHLD-based reaper, which is unreliable on macOS for session leaders
//! and starves under heavy runtime load (see `RCA_MACOS_WAKER.md`).
//!
//! Registration ordering: callers must register the pid **immediately**
//! after `Command::spawn` and before any other code can race-reap it.
//! If the child has already exited by the time the watcher's `EV_ADD`
//! runs, the backend reaps it inline with `waitpid(WNOHANG)` and resolves
//! the std mpsc receiver synchronously.
//!
//! This module is macOS-only. Linux uses vanilla `tokio::process` via
//! `proc_exec::imp_linux`; the `EVFILT_USER` reliability bug that motivates
//! this watcher does not exist on Linux.

use std::io;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::sync::mpsc;

mod kqueue_macos;
use kqueue_macos as backend;

pub(crate) struct Registration {
    pub pid: i32,
    pub sender: mpsc::Sender<io::Result<ExitStatus>>,
}

type WakeFn = Box<dyn Fn() + Send + Sync + 'static>;

struct Watcher {
    tx: mpsc::Sender<Registration>,
    wake: WakeFn,
}

static WATCHER: OnceLock<Watcher> = OnceLock::new();

fn get_or_init() -> &'static Watcher {
    WATCHER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let wake = backend::start(rx).expect("start child watcher thread");
        Watcher { tx, wake }
    })
}

/// Register `pid` for exit notification. The returned receiver resolves
/// when the kernel reports the child has exited. Must be called before
/// anyone else attempts `waitpid` on `pid`.
///
/// The returned channel is `std::sync::mpsc` — `recv` blocks on a kernel
/// condvar, bypassing tokio's cross-thread waker entirely.
pub fn register(pid: i32) -> mpsc::Receiver<io::Result<ExitStatus>> {
    let w = get_or_init();
    let (tx, rx) = mpsc::channel();
    if w.tx.send(Registration { pid, sender: tx }).is_err() {
        let (tx2, rx2) = mpsc::channel();
        drop(tx2.send(Err(io::Error::other("process watcher thread died"))));
        return rx2;
    }
    (w.wake)();
    rx
}

/// Test-only register variant that intentionally does NOT call `wake()`.
/// Simulates the macOS bug where the kqueue `EVFILT_USER` trigger is
/// silently dropped. The watcher must still pick up the registration via
/// its unconditional per-iteration drain (within the 1s kevent timeout).
#[cfg(test)]
pub(crate) fn register_no_wake(pid: i32) -> mpsc::Receiver<io::Result<ExitStatus>> {
    let w = get_or_init();
    let (tx, rx) = mpsc::channel();
    if w.tx.send(Registration { pid, sender: tx }).is_err() {
        let (tx2, rx2) = mpsc::channel();
        drop(tx2.send(Err(io::Error::other("process watcher thread died"))));
        return rx2;
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// Regression: a dropped EVFILT_USER wake must not strand a
    /// registration. The pre-kevent unconditional drain in
    /// `kqueue_macos::run_loop` should pick it up within ~1s (the kevent
    /// timeout) and arm NOTE_EXIT before — or `poll_pending` should reap
    /// it inline if it has already exited.
    #[test]
    fn register_no_wake_still_resolves() {
        // Long-running child so the watcher gets a chance to arm
        // EVFILT_PROC NOTE_EXIT *after* the registration is drained
        // (rather than going through the inline `ESRCH` path).
        let mut child = Command::new("sleep")
            .arg("0.3")
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id() as i32;

        let rx = register_no_wake(pid);

        // Generous deadline: must exceed the kevent timeout (1s) plus
        // the child's own sleep (300ms) plus scheduling slack.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(status)) => {
                    // sleep 0.3 exits cleanly.
                    assert!(
                        status.success() || status.code() == Some(0),
                        "unexpected exit: {status:?}"
                    );
                    break;
                }
                Ok(Err(e)) => panic!("watcher reported error: {e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "watcher did not resolve dropped-trigger registration before deadline"
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("watcher sender disconnected without sending status")
                }
            }
        }

        // Watcher already reaped via waitpid; `try_wait` here will most
        // likely return ECHILD. We just want to make sure we don't leave
        // a dangling Child handle that tries to wait on Drop.
        drop(child.try_wait());
    }
}
