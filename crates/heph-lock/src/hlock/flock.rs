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
//!
//! ## Delete on unlock
//!
//! Releasing an *exclusive* lock unlinks the lock file so it does not linger on
//! disk. `flock`-plus-unlink is racy on its own: a waiter blocked on the
//! about-to-be-removed inode can win the lock on a now-unlinked file while a
//! fresh process locks a newly created file at the same path — two holders. Two
//! invariants close the race:
//!
//! 1. The unlink happens *while the lock is still held* (before `LOCK_UN`), so
//!    the removed inode is exactly the one being released.
//! 2. Every fresh acquire re-`stat`s the path after taking the lock and bails
//!    (releases + retries) if the locked fd's inode no longer matches the path
//!    — i.e. it locked a stale, unlinked file.
//!
//! Only the exclusive-release path deletes; read release never does. A
//! cross-process shared reader holds its lock on the inode, and deleting it
//! would let a new writer lock a fresh file at the same path while that reader
//! still believes it holds a shared lock. A writer only deletes after every
//! reader has drained, so no live holder ever sits on a removed inode.

use heph_plugin::error::CancelledError;
use heph_core::hasync::Cancellable;
use crate::hlock::traits::{Ctoken, Lock, RWLock};
use anyhow::{Context, Result};
use async_trait::async_trait;
use libc::c_int;
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MAX_BACKOFF: Duration = Duration::from_millis(100);

/// Bound on re-open attempts when a fresh acquire keeps locking stale (unlinked)
/// inodes. Each iteration corresponds to a releaser having deleted the file in
/// the tiny window between our open and lock; a handful suffices in practice,
/// and exceeding it simply defers to the caller's backoff retry.
const STALE_RETRIES: usize = 16;

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
            self.discard_fd(st);
        }
    }

    /// Unconditionally release the OS lock and close the fd. Used both to drop
    /// the fd when no guard remains and to abandon a stale (unlinked) fd before
    /// re-opening.
    fn discard_fd(&self, st: &mut FdState) {
        if let Some(f) = &st.file {
            // Explicit unlock for clarity; close() below also releases.
            // SAFETY: `f` owns a valid open fd for the duration of this call.
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        }
        st.file = None;
    }

    /// Whether the currently open fd still names the inode at `path`. A `false`
    /// means a releaser unlinked the file out from under us between our open and
    /// our lock, so we hold a lock on a dead inode and must re-open.
    fn fd_matches_path(&self, st: &FdState) -> Result<bool> {
        let f = st.file.as_ref().context("fd must be open to validate")?;
        let fd_meta = f.metadata().context("fstat-ing held lock fd")?;
        match std::fs::metadata(&self.path) {
            Ok(path_meta) => {
                Ok(path_meta.dev() == fd_meta.dev() && path_meta.ino() == fd_meta.ino())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::Error::new(e))
                .with_context(|| format!("stat-ing lock file {}", self.path.display())),
        }
    }

    /// Fresh open + non-blocking `flock(op)` + inode validation, retrying past
    /// stale (unlinked) inodes. Only valid when no in-process guard is held.
    /// `Ok(true)` leaves the fd open and locked; `Ok(false)` means would-block
    /// (or repeated staleness) with the fd closed.
    fn try_lock_fresh(&self, st: &mut FdState, op: c_int) -> Result<bool> {
        for _ in 0..STALE_RETRIES {
            let fd = self.ensure_open(st)?;
            if !flock_nb(fd, op)? {
                self.discard_fd(st);
                return Ok(false);
            }
            if self.fd_matches_path(st)? {
                return Ok(true);
            }
            // Locked a file that a releaser already unlinked; drop it and retry
            // a fresh open (which re-creates the path).
            self.discard_fd(st);
        }
        Ok(false)
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
        if self.try_lock_fresh(&mut st, libc::LOCK_SH)? {
            st.readers = 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Non-blocking exclusive-lock attempt.
    fn try_acquire_write(&self) -> Result<bool> {
        let mut st = self.fd.lock();
        if st.writer || st.readers > 0 {
            return Ok(false);
        }
        if self.try_lock_fresh(&mut st, libc::LOCK_EX)? {
            st.writer = true;
            Ok(true)
        } else {
            Ok(false)
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
        // Delete the lock file *while still holding the exclusive lock*, so any
        // waiter that grabbed this inode fails its post-lock inode check and
        // re-opens, and any fresh acquire creates a new file. Best-effort: a
        // missing file (someone raced us, or it was never created) is fine.
        if st.file.is_some() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::debug!(error = %e, path = %self.path.display(), "removing lock file on unlock")
                }
            }
        }
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
    use heph_core::hasync::StdCancellationToken;
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

    #[tokio::test]
    async fn write_unlock_deletes_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        let g = l.lock(&ct()).await.unwrap();
        assert!(path.exists(), "lock file present while held");
        drop(g);
        assert!(!path.exists(), "lock file removed on unlock");
    }

    #[tokio::test]
    async fn read_unlock_keeps_lock_file() {
        // Read release must NOT delete: a cross-process shared reader could
        // still hold the inode, and removing it would let a new writer lock a
        // fresh file at the same path concurrently.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FRWLock::new(&path);

        let r = l.read(&ct()).await.unwrap();
        assert!(path.exists());
        drop(r);
        assert!(path.exists(), "read release leaves the lock file in place");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serializes_across_instances_after_delete() {
        // After a holder deletes the file on release, an independent instance
        // (modeling another process) re-creates and locks it cleanly.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let a = FLock::new(&path);
        let b = FLock::new(&path);

        {
            let _g = a.lock(&ct()).await.unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists(), "deleted on release");

        let g = b.lock(&ct()).await.unwrap();
        assert!(path.exists(), "re-created by fresh acquire");
        let (readers, writer, fd_open) = b.state.counts();
        assert_eq!((readers, writer, fd_open), (0, true, true));
        drop(g);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn stale_inode_is_rejected_and_reopened() {
        // Simulate the race tail: a lock file is unlinked and a *different*
        // inode now sits at the path. A fresh acquire must not be fooled by the
        // post-lock inode check — it re-opens the live file and succeeds.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        // Create then remove the original file, leaving a new inode behind.
        std::fs::write(&path, b"stale").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"fresh").unwrap();

        let g = l.lock(&ct()).await.unwrap();
        let (_, writer, fd_open) = l.state.counts();
        assert!(writer && fd_open, "acquired the live file");
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
        assert!(heph_plugin::error::is_cancelled(&err));

        let (readers, writer, fd_open) = b.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "clean after cancel"
        );
    }
}
