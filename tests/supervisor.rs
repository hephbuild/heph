//! Integration tests for the process supervisor sidecar.
//!
//! These spawn the actual `heph __supervisor` subprocess (path resolved via
//! `CARGO_BIN_EXE_heph`) with a socketpair, then verify that:
//!   1. Tracked process-groups are reaped when the parent's socket end closes.
//!   2. Untracked groups are *not* killed on EOF.
//!   3. Grandchildren are reaped via the process-group kill (setsid → killpg).
//!
//! Tests use `tempfile::TempDir` for any filesystem state and unique pids
//! everywhere so concurrent runs cannot collide.

#![cfg(unix)]

use std::io::Write as _;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Serializes every `Command::spawn` in this test binary.
///
/// The four tests run as threads in a single process under plain `cargo test`.
/// Spawning a child is fork+exec, and a fork snapshots the entire fd table.
/// While one thread has a socketpair fd with `CLOEXEC` temporarily cleared —
/// on macOS `socketpair` + `fcntl` is not atomic, and we deliberately clear it
/// on the supervisor end so it survives `exec` — a concurrent fork on another
/// thread inherits that fd. If the leaked fd is the *parent* end of a
/// supervisor socket, that supervisor never observes EOF when its owning test
/// drops the parent: it blocks in `read` forever and the tracked child is
/// never reaped (15s timeout). Holding this gate across each spawn's fd-setup
/// window guarantees no fork ever overlaps another thread's cleared-`CLOEXEC`
/// window, so no socket end can leak between tests.
static SPAWN_GATE: Mutex<()> = Mutex::new(());

fn spawn_gate() -> MutexGuard<'static, ()> {
    SPAWN_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How long to wait for the supervisor subprocess to observe socket EOF and
/// SIGKILL its tracked targets. Generous because under a loaded CI runner
/// (`cargo nextest` spawns one process per test, saturating cores) the
/// supervisor can take seconds just to get scheduled. Locally this is hit in
/// milliseconds — the headroom only matters under contention.
const REAP_TIMEOUT: Duration = Duration::from_secs(15);

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

fn set_cloexec(fd: i32, on: bool) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "fcntl get + set are paired"
    )]
    // SAFETY: fcntl on a fd we own; F_GETFD/F_SETFD are total functions.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "F_GETFD failed");
        let new = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        let r = libc::fcntl(fd, libc::F_SETFD, new);
        assert!(r >= 0, "F_SETFD failed");
    }
}

/// Spawn `sleep <secs>` in its own session/pgid so `killpg(pid, SIGKILL)`
/// reaps the whole tree without touching this test process.
fn spawn_sleep_session_leader(secs: u64) -> std::process::Child {
    let mut cmd = Command::new("sleep");
    cmd.arg(secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec install + setsid syscall"
    )]
    // SAFETY: setsid is async-signal-safe in the post-fork pre-exec window.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _gate = spawn_gate();
    cmd.spawn().expect("spawn sleep")
}

/// Spawn the heph supervisor with one end of a fresh socketpair. Returns the
/// parent's end and the supervisor's `Child` handle. The parent end has
/// CLOEXEC set; the supervisor's end has CLOEXEC cleared.
fn spawn_supervisor() -> (UnixStream, std::process::Child) {
    // Hold the gate across the whole fd-setup window: socketpair creation, the
    // CLOEXEC toggles, the supervisor fork, and closing our copy of the child
    // end. No other thread may fork while any of these fds is CLOEXEC-cleared.
    let _gate = spawn_gate();
    let (parent, child) = UnixStream::pair().expect("socketpair");
    set_cloexec(parent.as_raw_fd(), true);
    let child_fd = child.into_raw_fd();
    set_cloexec(child_fd, false);

    let mut cmd = Command::new(heph_bin());
    cmd.arg("__supervisor")
        .arg("--ipc-fd")
        .arg(child_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let fd_for_child = child_fd;
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec install + fcntl pair"
    )]
    // SAFETY: pre_exec runs between fork and exec; fcntl is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd_for_child, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd_for_child, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let supervisor = cmd.spawn().expect("spawn supervisor");
    // SAFETY: we transferred ownership of child_fd via into_raw_fd; close once.
    unsafe {
        libc::close(child_fd);
    }
    (parent, supervisor)
}

/// Poll up to `timeout` for the child to exit (reaping it). Returns true on
/// success. Uses `try_wait` so the test reaps the zombie itself — relying on
/// `kill(pid, 0)` would falsely report a zombie as "still alive".
fn await_child_exit(child: &mut std::process::Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return false,
        }
    }
}

/// True iff the given pid is gone *and* it is not our child (or has been
/// reaped). Useful when the pid is a grandchild we never parented.
fn pid_gone(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a probe with no side effects.
    let r = unsafe { libc::kill(pid as i32, 0) };
    if r == 0 {
        return false;
    }
    let err = std::io::Error::last_os_error().raw_os_error();
    matches!(err, Some(libc::ESRCH))
}

fn await_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pid_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    pid_gone(pid)
}

#[test]
fn supervisor_reaps_tracked_pgid_on_eof() {
    let mut sleep_child = spawn_sleep_session_leader(120);
    let sleep_pid = sleep_child.id();
    let (mut parent, mut supervisor) = spawn_supervisor();

    // Tell supervisor to track our sleep's pgid (== pid because of setsid).
    writeln!(parent, "TRACK {sleep_pid}").expect("write TRACK");
    parent.flush().expect("flush");

    // Give the supervisor a beat to read the line before we close.
    std::thread::sleep(Duration::from_millis(100));

    // Close our end of the socket → supervisor reads EOF → kills the pgid.
    drop(parent);

    assert!(
        await_child_exit(&mut sleep_child, REAP_TIMEOUT),
        "supervisor should have SIGKILLed pid {sleep_pid} after socket EOF"
    );

    drop(supervisor.wait());
}

#[test]
fn supervisor_does_not_kill_untracked_pgid_on_eof() {
    let mut sleep_child = spawn_sleep_session_leader(120);
    let (parent, mut supervisor) = spawn_supervisor();

    // Close immediately without sending any TRACK lines.
    drop(parent);

    // Give the supervisor a beat to read EOF and exit. If it were going to
    // kill untracked groups it would happen now.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        sleep_child.try_wait().expect("try_wait").is_none(),
        "untracked process must not be killed on supervisor EOF"
    );

    // Clean up.
    drop(sleep_child.kill());
    drop(sleep_child.wait());
    drop(supervisor.wait());
}

#[test]
fn supervisor_reaps_non_setsid_child_via_direct_kill() {
    // Drivers that do not setsid (plugingo, pluginnix) register the direct
    // pid. Supervisor must SIGKILL the pid even though it is not a group
    // leader.
    let mut cmd = Command::new("sleep");
    cmd.arg("120")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut sleep_child = {
        let _gate = spawn_gate();
        cmd.spawn().expect("spawn sleep without setsid")
    };
    let sleep_pid = sleep_child.id();

    let (mut parent, mut supervisor) = spawn_supervisor();
    writeln!(parent, "TRACK {sleep_pid}").expect("write TRACK");
    parent.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(100));
    drop(parent);

    assert!(
        await_child_exit(&mut sleep_child, REAP_TIMEOUT),
        "supervisor must kill non-setsid pid {sleep_pid}"
    );
    drop(supervisor.wait());
}

#[test]
fn supervisor_reaps_grandchildren_via_pgid_kill() {
    // Spawn an sh that itself forks another sleep, all in one session.
    // Killing the pgid must reap both the sh and the inner sleep.
    //
    // The backgrounded sleep inherits all of sh's open fds, so we cannot use
    // a pipe to ferry the grandchild pid back (read would block until the
    // backgrounded sleep also closed its inherited end, i.e. for 120s). A
    // tempfile dodges this.
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("gc.pid");
    let script = format!("sleep 120 & echo $! > '{}'; wait", pid_file.display());
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec install + setsid syscall"
    )]
    // SAFETY: setsid in pre_exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut sh = {
        let _gate = spawn_gate();
        cmd.spawn().expect("spawn sh")
    };
    let sh_pid = sh.id();

    // Poll for the pid file to appear.
    let deadline = Instant::now() + REAP_TIMEOUT;
    let gc_pid: u32 = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "grandchild pid file never appeared"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    let (mut parent, mut supervisor) = spawn_supervisor();
    writeln!(parent, "TRACK {sh_pid}").expect("write TRACK");
    parent.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(100));
    drop(parent);

    // sh is our direct child; reap it via try_wait so we don't trip on the
    // zombie still being present in the kernel process table.
    assert!(
        await_child_exit(&mut sh, REAP_TIMEOUT),
        "sh leader {sh_pid} must die"
    );
    // The grandchild was re-parented to init when the bash leader exited;
    // it's not our child, so kill-probe semantics work cleanly.
    assert!(
        await_pid_gone(gc_pid, REAP_TIMEOUT),
        "grandchild {gc_pid} must die"
    );
    drop(supervisor.wait());
}
