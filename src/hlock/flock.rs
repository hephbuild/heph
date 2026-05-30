//! Filesystem lock backend built on `flock(2)`.
//!
//! `flock` locks are advisory and associated with the *open file description*,
//! not recursive within a process — re-locking the same fd only converts the
//! mode. So a single fd per [`FLockState`] is shared by all in-process guards
//! and an in-process reference count governs the underlying lock (mirrors the
//! Go `Flock.rc` model). Cross-process exclusion is provided by `flock` itself.
//!
//! ## Cancellable blocking acquire
//!
//! A plain `flock` wait is an uninterruptible syscall. To stay cancellable
//! without leaking a parked blocking thread, the blocking acquire polls with
//! `LOCK_NB` and an exponential async backoff (1ms → [`MAX_BACKOFF`]), checking
//! the cancellation token each iteration. The trade-off is up to `MAX_BACKOFF`
//! of wakeup latency — acceptable for coarse cross-process build locks.

use crate::engine::error::CancelledError;
use crate::hasync::Cancellable;
use crate::hlock::traits::{Ctoken, Lock, RWLock};
use anyhow::{Context, Result};
use async_trait::async_trait;
use libc::c_int;
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct FdState {
    /// Open while any in-process guard is held; closed (releasing the lock)
    /// when `readers == 0 && !writer`.
    file: Option<File>,
    readers: usize,
    writer: bool,
}

#[derive(Debug)]
struct FLockState {
    path: PathBuf,
    fd: Mutex<FdState>,
}

impl FLockState {
    fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            path,
            fd: Mutex::new(FdState {
                file: None,
                readers: 0,
                writer: false,
            }),
        })
    }

    /// Open the lock file if not already open, returning its raw fd.
    fn ensure_open(&self, st: &mut FdState) -> Result<RawFd> {
        if st.file.is_none() {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&self.path)
                .with_context(|| format!("opening flock file {}", self.path.display()))?;
            st.file = Some(f);
        }
        Ok(st.file.as_ref().expect("just opened").as_raw_fd())
    }

    /// Drop the fd when no guard is held, releasing the OS lock.
    fn maybe_close(&self, st: &mut FdState) {
        if st.readers == 0 && !st.writer {
            if let Some(f) = &st.file {
                // Explicit unlock for clarity; close() below also releases.
                // SAFETY: `f` owns a valid open fd for the duration of this call.
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
            }
            st.file = None;
        }
    }

    /// Non-blocking shared-lock attempt. `Ok(true)` on acquire (readers
    /// incremented), `Ok(false)` if held elsewhere.
    fn try_acquire_read(&self) -> Result<bool> {
        let mut st = self.fd.lock();
        if st.writer {
            return Ok(false);
        }
        if st.readers > 0 {
            st.readers += 1;
            return Ok(true);
        }
        let fd = self.ensure_open(&mut st)?;
        match flock_nb(fd, libc::LOCK_SH)? {
            true => {
                st.readers = 1;
                Ok(true)
            }
            false => {
                self.maybe_close(&mut st);
                Ok(false)
            }
        }
    }

    /// Non-blocking exclusive-lock attempt.
    fn try_acquire_write(&self) -> Result<bool> {
        let mut st = self.fd.lock();
        if st.writer || st.readers > 0 {
            return Ok(false);
        }
        let fd = self.ensure_open(&mut st)?;
        match flock_nb(fd, libc::LOCK_EX)? {
            true => {
                st.writer = true;
                Ok(true)
            }
            false => {
                self.maybe_close(&mut st);
                Ok(false)
            }
        }
    }

    /// Overwrite the lock file's contents through the already-open lock fd
    /// (no second `open`). Only valid while a guard is held — the fd is open
    /// exactly then. Truncates to `bytes.len()` so stale trailing bytes from a
    /// prior, longer payload do not linger.
    fn write_contents(&self, bytes: &[u8]) -> Result<()> {
        let st = self.fd.lock();
        let mut f = st
            .file
            .as_ref()
            .context("write_contents requires a held lock (fd open)")?;
        f.rewind().context("seeking lock file to start")?;
        f.write_all(bytes).context("writing lock file contents")?;
        f.set_len(bytes.len() as u64)
            .context("truncating lock file to payload length")?;
        Ok(())
    }

    fn release_read(&self) {
        let mut st = self.fd.lock();
        debug_assert!(st.readers > 0, "read guard released without a read lock");
        st.readers = st.readers.saturating_sub(1);
        self.maybe_close(&mut st);
    }

    fn release_write(&self) {
        let mut st = self.fd.lock();
        st.writer = false;
        self.maybe_close(&mut st);
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, bool, bool) {
        let st = self.fd.lock();
        (st.readers, st.writer, st.file.is_some())
    }
}

/// Issue a non-blocking `flock`. `Ok(true)` on success, `Ok(false)` on
/// would-block, retrying on `EINTR`.
fn flock_nb(fd: RawFd, op: c_int) -> Result<bool> {
    loop {
        // SAFETY: `fd` is a valid open fd held alive by the caller's `FdState`.
        let r = unsafe { libc::flock(fd, op | libc::LOCK_NB) };
        if r == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK => return Ok(false),
            Some(code) if code == libc::EINTR => continue,
            _ => return Err(anyhow::Error::new(e).context("flock syscall failed")),
        }
    }
}

/// Poll `attempt` with async backoff until it succeeds or the token cancels.
async fn poll_acquire(
    ctoken: &(dyn Cancellable + Send + Sync),
    what: &'static str,
    mut attempt: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let mut backoff = Duration::from_millis(1);
    loop {
        if ctoken.is_cancelled() {
            return Err(anyhow::Error::new(CancelledError)).context(what);
        }
        if attempt().context(what)? {
            return Ok(());
        }
        tokio::select! {
            biased;
            () = ctoken.cancelled() => return Err(anyhow::Error::new(CancelledError)).context(what),
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Filesystem exclusive lock over a lock file.
#[derive(Clone, Debug)]
pub struct FLock {
    state: Arc<FLockState>,
}

impl FLock {
    /// Create a lock backed by `path`. The file is created lazily on first
    /// acquisition.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            state: FLockState::new(path.as_ref().to_path_buf()),
        }
    }
}

/// Filesystem reader/writer lock over a lock file.
#[derive(Clone, Debug)]
pub struct FRWLock {
    state: Arc<FLockState>,
}

impl FRWLock {
    /// Create a reader/writer lock backed by `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            state: FLockState::new(path.as_ref().to_path_buf()),
        }
    }
}

/// Shared read guard; releases the read lock on drop.
#[derive(Debug)]
pub struct FReadGuard {
    state: Arc<FLockState>,
}

impl Drop for FReadGuard {
    fn drop(&mut self) {
        self.state.release_read();
    }
}

/// Exclusive write guard; releases the write lock on drop.
#[derive(Debug)]
pub struct FWriteGuard {
    state: Arc<FLockState>,
}

impl FWriteGuard {
    /// Overwrite the lock file's contents through the held lock fd, reusing the
    /// open file description rather than opening the path again. Holding the
    /// exclusive guard guarantees no other writer races this.
    pub fn write_contents(&self, bytes: &[u8]) -> Result<()> {
        self.state.write_contents(bytes)
    }
}

impl Drop for FWriteGuard {
    fn drop(&mut self) {
        self.state.release_write();
    }
}

#[async_trait]
impl Lock for FLock {
    type Guard = FWriteGuard;

    async fn lock(&self, ctoken: Ctoken<'_>) -> Result<FWriteGuard> {
        let state = Arc::clone(&self.state);
        poll_acquire(ctoken, "acquiring file lock", || state.try_acquire_write()).await?;
        Ok(FWriteGuard {
            state: Arc::clone(&self.state),
        })
    }

    fn try_lock(&self) -> Result<Option<FWriteGuard>> {
        Ok(self.state.try_acquire_write()?.then(|| FWriteGuard {
            state: Arc::clone(&self.state),
        }))
    }
}

#[async_trait]
impl RWLock for FRWLock {
    type ReadGuard = FReadGuard;
    type WriteGuard = FWriteGuard;

    async fn read(&self, ctoken: Ctoken<'_>) -> Result<FReadGuard> {
        let state = Arc::clone(&self.state);
        poll_acquire(ctoken, "acquiring file read lock", || {
            state.try_acquire_read()
        })
        .await?;
        Ok(FReadGuard {
            state: Arc::clone(&self.state),
        })
    }

    async fn write(&self, ctoken: Ctoken<'_>) -> Result<FWriteGuard> {
        let state = Arc::clone(&self.state);
        poll_acquire(ctoken, "acquiring file write lock", || {
            state.try_acquire_write()
        })
        .await?;
        Ok(FWriteGuard {
            state: Arc::clone(&self.state),
        })
    }

    fn try_read(&self) -> Result<Option<FReadGuard>> {
        Ok(self.state.try_acquire_read()?.then(|| FReadGuard {
            state: Arc::clone(&self.state),
        }))
    }

    fn try_write(&self) -> Result<Option<FWriteGuard>> {
        Ok(self.state.try_acquire_write()?.then(|| FWriteGuard {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasync::StdCancellationToken;
    use std::time::Duration;

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    #[tokio::test]
    async fn in_process_read_count_keeps_fd_until_last_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let l = FRWLock::new(dir.path().join("lock"));

        let r1 = l.read(&ct()).await.unwrap();
        let r2 = l.read(&ct()).await.unwrap();
        assert_eq!(l.state.counts().0, 2, "two in-process readers");
        // A writer on the SAME instance is blocked while readers are held.
        assert!(l.try_write().unwrap().is_none());

        drop(r1);
        assert_eq!(l.state.counts().0, 1);
        drop(r2);
        let (readers, writer, fd_open) = l.state.counts();
        assert_eq!((readers, writer, fd_open), (0, false, false), "fd closed");

        assert!(
            l.try_write().unwrap().is_some(),
            "writer after readers drain"
        );
    }

    #[tokio::test]
    async fn cross_instance_contention() {
        // Two independent instances on the same path model two processes:
        // each has its own open file description, so flock contends.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let a = FRWLock::new(&path);
        let b = FRWLock::new(&path);

        let held = a.read(&ct()).await.unwrap();
        assert!(b.try_write().unwrap().is_none(), "EX blocked by other SH");
        assert!(
            b.try_read().unwrap().is_some(),
            "SH shared across instances"
        );

        drop(held);
        assert!(b.try_write().unwrap().is_some(), "EX ok after SH released");
    }

    #[tokio::test]
    async fn write_contents_persists_and_truncates_through_held_fd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        let g = l.lock(&ct()).await.unwrap();
        g.write_contents(b"123456").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"123456");

        // A shorter payload truncates the trailing bytes of the longer one.
        g.write_contents(b"42").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"42");
        drop(g);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_write_is_cancellable_and_leaves_clean_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let a = FLock::new(&path);
        let b = FLock::new(&path);

        let _held = a.lock(&ct()).await.unwrap();

        let token = ct();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token2.cancel();
        });

        let res = tokio::time::timeout(Duration::from_secs(5), b.lock(&token)).await;
        let err = res
            .expect("did not hang")
            .expect_err("contended write must be cancelled");
        assert!(crate::commands::errors::is_cancelled(&err));

        let (readers, writer, fd_open) = b.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "clean after cancel"
        );
    }
}
