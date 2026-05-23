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
