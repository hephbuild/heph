//! The session `lock` file: `flock(LOCK_EX)` held by the owner while it lives.
//!
//! A few lines of `flock`, rather than `hlock::FLock`: the `lock` crate pulls the
//! plugin crate into the TUI's dependency graph, and the owner needs none of its
//! machinery (no async, no stamped holder, no unlink-on-release — the file must
//! stay so a viewer can probe it). A viewer probes with
//! `hlock::FLock::is_path_held`, whose `LOCK_SH` is refused while this
//! `LOCK_EX` is held — the two agree on the one fact that matters.
//!
//! Every fd here is opened by `std`, which sets `O_CLOEXEC`: a child that
//! outlives heph cannot keep the session looking alive (I2).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

/// Attempts at the owner's `LOCK_EX`, and the gap between them. A viewer's
/// probe takes a transient `LOCK_SH`, which makes `LOCK_EX | LOCK_NB` fail for
/// the instant it is held; nothing else can hold a fresh session's lock.
const OWNER_ATTEMPTS: u32 = 50;
const OWNER_GAP: Duration = Duration::from_millis(10);

/// `flock(fd, op | LOCK_NB)`: `Ok(true)` acquired, `Ok(false)` held elsewhere.
fn flock_nb(file: &File, op: libc::c_int) -> io::Result<bool> {
    // SAFETY: `file` owns a valid open fd for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Create the owner's lock file (`0600`) and take `LOCK_EX` on it, retrying
/// while a viewer's probe is in the way. The returned file *is* the lock:
/// dropping it releases the session.
pub(crate) fn acquire_owner_lock(path: &Path) -> io::Result<File> {
    acquire_owner_lock_with(path, OWNER_ATTEMPTS, OWNER_GAP)
}

fn acquire_owner_lock_with(path: &Path, attempts: u32, gap: Duration) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    for attempt in 0..attempts {
        if flock_nb(&file, libc::LOCK_EX)? {
            return Ok(file);
        }
        if attempt + 1 < attempts {
            std::thread::sleep(gap);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("{} is held by another process", path.display()),
    ))
}

/// Take `LOCK_EX` on an existing lock file without waiting, for cleanup.
///
/// `Ok(Some(file))`: nobody held it, and now this process does until `file`
/// drops — so no other cleanup can be removing the same directory. `Ok(None)`:
/// held (a live session, or another cleanup). A missing file is `Ok(None)` too;
/// the caller decides what a directory without one means.
pub(crate) fn try_lock_free(path: &Path) -> io::Result<Option<File>> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(flock_nb(&file, libc::LOCK_EX)?.then_some(file))
}

/// Whether some open file description holds `path`'s lock, by the same
/// `LOCK_SH` probe `hlock::FLock::is_path_held` uses. For tests and callers in
/// this crate; a viewer in the root crate uses `FLock::is_path_held` itself.
pub fn is_held(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let acquired = flock_nb(&file, libc::LOCK_SH)?;
    if acquired {
        // SAFETY: `file` owns a valid open fd for the duration of the call.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
    Ok(!acquired)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A8: the owner's `LOCK_EX | LOCK_NB` fails while a probe's `LOCK_SH` is
    /// held; the retry rides it out instead of failing the session.
    #[test]
    fn owner_lock_retries_under_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        std::fs::write(&path, b"").expect("create");
        let probe = File::open(&path).expect("open probe");
        assert!(
            flock_nb(&probe, libc::LOCK_SH).expect("probe"),
            "probe holds LOCK_SH"
        );

        assert_eq!(
            acquire_owner_lock_with(&path, 1, Duration::ZERO)
                .expect_err("one attempt under a probe fails")
                .kind(),
            io::ErrorKind::WouldBlock
        );

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(probe);
        });
        let owner = acquire_owner_lock(&path).expect("the retry outlasts the probe");
        release.join().expect("join");
        assert!(is_held(&path).expect("probe"), "held by the owner");
        assert!(
            try_lock_free(&path).expect("try").is_none(),
            "cleanup sees it held"
        );
        drop(owner);
        assert!(!is_held(&path).expect("probe"), "released on drop");
        assert!(
            try_lock_free(&path).expect("try").is_some(),
            "free for cleanup"
        );
    }
}
