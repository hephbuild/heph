//! macOS kqueue backend for `process_watcher`.
//!
//! Single dedicated thread owns one kqueue fd. Each registered pid is
//! added as an `EVFILT_PROC` filter with `NOTE_EXIT | EV_ONESHOT`. New
//! registrations arrive over an `mpsc` channel and are picked up when the
//! main loop is woken via an `EVFILT_USER` trigger.
//!
//! `EV_RECEIPT` is used on `EV_ADD` so we synchronously detect `ESRCH`
//! (child already exited before we could register). In that case we reap
//! inline with `waitpid(WNOHANG)` and resolve the caller's oneshot
//! immediately — the kernel guarantees the zombie is collectable until
//! someone reaps it.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
use std::sync::mpsc;
use tokio::sync::oneshot;

use super::Registration;

const WAKE_IDENT: libc::uintptr_t = 0;
const EVENT_BATCH: usize = 64;

pub(super) fn start(rx: mpsc::Receiver<Registration>) -> io::Result<super::WakeFn> {
    // SAFETY: kqueue() returns a new fd or -1 on error.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }

    let user_ev = make_kevent(
        WAKE_IDENT,
        libc::EVFILT_USER,
        libc::EV_ADD | libc::EV_CLEAR,
        0,
        0,
    );
    submit_kevents(kq, &[user_ev])?;

    std::thread::Builder::new()
        .name("rheph-child-watcher".into())
        .spawn(move || run_loop(kq, rx))
        .map_err(|e| io::Error::other(format!("spawn watcher thread: {e}")))?;

    let wake: super::WakeFn = Box::new(move || {
        let trigger = make_kevent(WAKE_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
        // Best-effort: a failed trigger is harmless because the next
        // process-exit event will wake the loop and drain the mpsc.
        drop(submit_kevents(kq, &[trigger]));
    });
    Ok(wake)
}

fn run_loop(kq: i32, rx: mpsc::Receiver<Registration>) {
    let mut pending: HashMap<i32, oneshot::Sender<io::Result<ExitStatus>>> = HashMap::new();
    // SAFETY: kevent is plain-data; zero-init is a valid representation.
    let mut events: [libc::kevent; EVENT_BATCH] = unsafe { std::mem::zeroed() };

    // 1 second poll backstop. macOS `EVFILT_PROC NOTE_EXIT` is observed
    // to silently drop events under heavy concurrent spawn load (see
    // `RCA_MACOS_WAKER.md`). Every second we iterate `pending` and probe
    // each pid with `waitpid(WNOHANG)`; any pid that has actually exited
    // is resolved even if the kernel never delivered NOTE_EXIT.
    let timeout = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };

    loop {
        // SAFETY: kq is a live fd we own; events buffer is owned and sized
        // to EVENT_BATCH; timeout points to a valid stack-local timespec.
        let n = unsafe {
            libc::kevent(
                kq,
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                EVENT_BATCH as i32,
                &timeout,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            tracing::error!(error = %err, "process_watcher: kevent failed; thread exiting");
            return;
        }
        let count = usize::try_from(n).unwrap_or(0).min(EVENT_BATCH);
        for ev in events.iter().take(count) {
            if ev.filter == libc::EVFILT_USER {
                drain_registrations(kq, &rx, &mut pending);
            } else if ev.filter == libc::EVFILT_PROC {
                let pid = ev.ident as i32;
                if let Some(sender) = pending.remove(&pid) {
                    drop(sender.send(reap(pid)));
                }
            }
        }
        poll_pending(&mut pending);
    }
}

/// Backstop poll: probe every pending pid with `waitpid(WNOHANG)` and
/// resolve those that have already exited. Catches `NOTE_EXIT` events
/// that the kernel silently dropped under load.
fn poll_pending(pending: &mut HashMap<i32, oneshot::Sender<io::Result<ExitStatus>>>) {
    if pending.is_empty() {
        return;
    }
    let mut resolved: Vec<(i32, io::Result<ExitStatus>)> = Vec::new();
    let total = pending.len();
    for &pid in pending.keys() {
        let mut status: libc::c_int = 0;
        // SAFETY: WNOHANG waitpid is non-blocking; pid is one we registered.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r > 0 {
            tracing::warn!(
                pid,
                "process_watcher: backstop poll caught exited pid (kqueue dropped NOTE_EXIT)"
            );
            resolved.push((pid, Ok(ExitStatus::from_raw(status))));
        } else if r < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                tracing::warn!(
                    pid,
                    "process_watcher: backstop poll hit ECHILD; another reaper got it"
                );
                resolved.push((pid, Ok(ExitStatus::from_raw(0))));
            }
        }
    }
    if !resolved.is_empty() {
        tracing::warn!(
            recovered = resolved.len(),
            pending_total = total,
            "process_watcher: backstop recovered missed exits"
        );
    }
    for (pid, result) in resolved {
        if let Some(sender) = pending.remove(&pid) {
            drop(sender.send(result));
        }
    }
}

fn drain_registrations(
    kq: i32,
    rx: &mpsc::Receiver<Registration>,
    pending: &mut HashMap<i32, oneshot::Sender<io::Result<ExitStatus>>>,
) {
    while let Ok(reg) = rx.try_recv() {
        // pid is a positive i32 from a freshly-spawned child; treat as
        // unsigned for the kevent ident field.
        let ident = u32::try_from(reg.pid).unwrap_or(0) as libc::uintptr_t;
        let ev = make_kevent(
            ident,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT | libc::EV_RECEIPT,
            libc::NOTE_EXIT,
            0,
        );
        // SAFETY: kevent is plain-data; zero-init is valid.
        let mut out: [libc::kevent; 1] = unsafe { std::mem::zeroed() };
        // SAFETY: kq live; `ev` lives for the duration of the call;
        // `out` is owned and sized.
        let r = unsafe { libc::kevent(kq, &ev, 1, out.as_mut_ptr(), 1, std::ptr::null()) };
        if r < 0 {
            drop(reg.sender.send(Err(io::Error::last_os_error())));
            continue;
        }
        // EV_RECEIPT guarantees exactly one event back with EV_ERROR set;
        // data is the errno (0 on success).
        let recv = &out[0];
        if recv.flags & libc::EV_ERROR != 0 {
            let errno = recv.data as i32;
            if errno == 0 {
                pending.insert(reg.pid, reg.sender);
            } else if errno == libc::ESRCH {
                drop(reg.sender.send(reap(reg.pid)));
            } else {
                drop(reg.sender.send(Err(io::Error::from_raw_os_error(errno))));
            }
        } else {
            pending.insert(reg.pid, reg.sender);
        }
    }
}

fn reap(pid: i32) -> io::Result<ExitStatus> {
    let mut status: libc::c_int = 0;
    // SAFETY: WNOHANG waitpid is non-blocking. The kernel has just told us
    // the child exited (NOTE_EXIT) or `EV_ADD` returned ESRCH meaning it
    // exited before registration; either way a zombie should be collectable
    // unless another reaper got there first.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r > 0 {
        return Ok(ExitStatus::from_raw(status));
    }
    if r == 0 {
        tracing::debug!(
            pid,
            "process_watcher: NOTE_EXIT fired but waitpid returned 0; status lost"
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

fn make_kevent(
    ident: libc::uintptr_t,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: libc::intptr_t,
) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata: std::ptr::null_mut(),
    }
}

fn submit_kevents(kq: i32, evs: &[libc::kevent]) -> io::Result<()> {
    let len: i32 = i32::try_from(evs.len())
        .map_err(|e| io::Error::other(format!("kevent batch too large: {e}")))?;
    // SAFETY: kq live; evs slice owned and sized; len matches.
    let r = unsafe {
        libc::kevent(
            kq,
            evs.as_ptr(),
            len,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
