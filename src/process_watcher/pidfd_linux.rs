//! Linux pidfd+epoll backend for `process_watcher`.
//!
//! Each registered pid is opened as a `pidfd` via the `pidfd_open(2)`
//! syscall and added to a shared epoll set. The pidfd becomes readable
//! when the child exits; the watcher thread then reaps with
//! `waitpid(WNOHANG)` and resolves the caller's oneshot.
//!
//! Registrations arrive on an `mpsc` and the thread is woken via an
//! `eventfd`.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::mpsc;
use tokio::sync::oneshot;

use super::Registration;

const WAKE_TOKEN: u64 = u64::MAX;

pub(super) fn start(rx: mpsc::Receiver<Registration>) -> io::Result<super::WakeFn> {
    // SAFETY: epoll_create1 returns a new fd or -1.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if ep < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: eventfd returns a new fd or -1.
    let evfd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if evfd < 0 {
        let err = io::Error::last_os_error();
        // SAFETY: ep is the live fd we created above.
        unsafe { libc::close(ep) };
        return Err(err);
    }

    let mut event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: WAKE_TOKEN,
    };
    // SAFETY: ep and evfd are live fds owned by us.
    let r = unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, evfd, &mut event) };
    if r < 0 {
        let err = io::Error::last_os_error();
        // SAFETY: closing fds we created above.
        unsafe {
            libc::close(evfd);
            libc::close(ep);
        }
        return Err(err);
    }

    std::thread::Builder::new()
        .name("rheph-child-watcher".into())
        .spawn(move || run_loop(ep, evfd, rx))
        .map_err(|e| io::Error::other(format!("spawn watcher thread: {e}")))?;

    let wake: super::WakeFn = Box::new(move || {
        let val: u64 = 1;
        // SAFETY: evfd is a live eventfd; an 8-byte write is required.
        let _ = unsafe {
            libc::write(
                evfd,
                std::ptr::addr_of!(val) as *const libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
    });
    Ok(wake)
}

struct PendingEntry {
    pid: i32,
    sender: oneshot::Sender<io::Result<ExitStatus>>,
    _pidfd: OwnedFd,
}

fn run_loop(ep: RawFd, evfd: RawFd, rx: mpsc::Receiver<Registration>) {
    let mut pending: HashMap<RawFd, PendingEntry> = HashMap::new();
    // SAFETY: epoll_event is plain-data; zero-init is valid.
    let mut events: [libc::epoll_event; 64] = unsafe { std::mem::zeroed() };

    loop {
        // SAFETY: ep live; events buffer owned.
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), events.len() as i32, -1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!(error = %err, "process_watcher: epoll_wait failed; thread exiting");
            return;
        }
        for ev in &events[..n as usize] {
            if ev.u64 == WAKE_TOKEN {
                let mut buf = [0u8; 8];
                // SAFETY: evfd is a live eventfd; an 8-byte read drains it.
                unsafe {
                    libc::read(evfd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                }
                drain_registrations(ep, &rx, &mut pending);
            } else {
                let pidfd = ev.u64 as RawFd;
                if let Some(entry) = pending.remove(&pidfd) {
                    let _ = entry.sender.send(reap(entry.pid));
                    // _pidfd drops; close removes it from epoll automatically.
                }
            }
        }
    }
}

fn drain_registrations(
    ep: RawFd,
    rx: &mpsc::Receiver<Registration>,
    pending: &mut HashMap<RawFd, PendingEntry>,
) {
    while let Ok(reg) = rx.try_recv() {
        match pidfd_open(reg.pid) {
            Ok(pidfd) => {
                let raw = pidfd.as_raw_fd();
                let mut event = libc::epoll_event {
                    events: libc::EPOLLIN as u32,
                    u64: raw as u64,
                };
                // SAFETY: ep + raw pidfd live.
                let r = unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, raw, &mut event) };
                if r < 0 {
                    let _ = reg.sender.send(Err(io::Error::last_os_error()));
                    continue;
                }
                pending.insert(
                    raw,
                    PendingEntry {
                        pid: reg.pid,
                        sender: reg.sender,
                        _pidfd: pidfd,
                    },
                );
            }
            Err(err) if err.raw_os_error() == Some(libc::ESRCH) => {
                let _ = reg.sender.send(reap(reg.pid));
            }
            Err(err) => {
                let _ = reg.sender.send(Err(err));
            }
        }
    }
}

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // SAFETY: SYS_pidfd_open with valid pid; returns fd or -1.
    let r = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: r is a freshly returned fd we own.
    Ok(unsafe { OwnedFd::from_raw_fd(r as RawFd) })
}

fn reap(pid: i32) -> io::Result<ExitStatus> {
    let mut status: libc::c_int = 0;
    // SAFETY: WNOHANG waitpid is non-blocking.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r > 0 {
        return Ok(ExitStatus::from_raw(status));
    }
    if r == 0 {
        tracing::debug!(
            pid,
            "process_watcher: pidfd ready but waitpid returned 0; status lost"
        );
        return Ok(ExitStatus::from_raw(0));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ECHILD) {
        tracing::debug!(
            pid,
            "process_watcher: ECHILD; another reaper got the child first"
        );
        return Ok(ExitStatus::from_raw(0));
    }
    Err(err)
}
