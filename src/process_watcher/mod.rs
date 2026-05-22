//! Cross-platform child-exit watcher.
//!
//! macOS: one shared `kqueue` fd + `EVFILT_PROC` / `NOTE_EXIT` per child.
//! Linux: one shared `epoll` fd + `pidfd_open` per child.
//!
//! A single dedicated OS thread owns the kernel-level event loop and
//! dispatches per-pid exit notifications onto `tokio::sync::oneshot`
//! channels handed back to callers. Compared to `tokio::process::Child::wait`
//! this bypasses tokio's SIGCHLD-based reaper, which is unreliable on macOS
//! for session leaders and starves under heavy runtime load (see
//! `RCA_MACOS_WAKER.md`).
//!
//! Registration ordering: callers must register the pid **immediately**
//! after `Command::spawn` and before any other code can race-reap it.
//! If the child has already exited by the time the watcher's `EV_ADD` /
//! `pidfd_open` runs, the backend reaps it inline with `waitpid(WNOHANG)`
//! and resolves the oneshot synchronously.

use std::io;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::sync::mpsc;
use tokio::sync::oneshot;

#[cfg(target_os = "macos")]
mod kqueue_macos;
#[cfg(target_os = "macos")]
use kqueue_macos as backend;

#[cfg(target_os = "linux")]
mod pidfd_linux;
#[cfg(target_os = "linux")]
use pidfd_linux as backend;

pub(crate) struct Registration {
    pub pid: i32,
    pub sender: oneshot::Sender<io::Result<ExitStatus>>,
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
pub fn register(pid: i32) -> oneshot::Receiver<io::Result<ExitStatus>> {
    let w = get_or_init();
    let (tx, rx) = oneshot::channel();
    if w.tx.send(Registration { pid, sender: tx }).is_err() {
        let (tx2, rx2) = oneshot::channel();
        drop(tx2.send(Err(io::Error::other("process watcher thread died"))));
        return rx2;
    }
    (w.wake)();
    rx
}
