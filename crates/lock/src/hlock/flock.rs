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

use crate::hlock::traits::{Ctoken, Lock, RWLock};
use anyhow::{Context, Result};
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hplugin::error::CancelledError;
use libc::c_int;
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
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
    /// Whether the stale-retry budget has already been reported exhausted since
    /// the last successful acquire. `try_lock_fresh` is the `poll_acquire`
    /// attempt closure, so a permanently stale path re-exhausts every backoff
    /// round — ten times a second once the backoff saturates. Latching keeps the
    /// first occurrence at `warn!` and drops the repeats to `debug!`.
    stale_warned: bool,
    /// Test-only: number of remaining [`FLockState::fd_matches_path`] calls that
    /// must report "stale" regardless of the real inodes.
    ///
    /// A genuine stale inode needs a releaser to `unlink` in the window between
    /// our `open` and our `flock` — nothing in-process can schedule that, and
    /// racing for it would make the test flake green. Lives in `FdState` rather
    /// than beside it so "only touched under the `fd` mutex" is structural
    /// instead of a comment (and there is no atomic ordering to get wrong).
    #[cfg(test)]
    force_stale: usize,
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
                stale_warned: false,
                #[cfg(test)]
                force_stale: 0,
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
    fn fd_matches_path(&self, st: &mut FdState) -> Result<bool> {
        // Test injection deliberately sits *inside* this function, ahead of the
        // real comparison: hardwiring the body to `Ok(true)` — the mutation that
        // used to pass the whole suite — then kills the retry tests too.
        #[cfg(test)]
        if st.force_stale > 0 {
            st.force_stale -= 1;
            return Ok(false);
        }
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
                // Would-block: this fd never acquired anything, so there is
                // nothing to `LOCK_UN`. Just close it — dropping the last fd of
                // an open file description releases any lock anyway, and the
                // extra `flock` would be a wasted syscall on *every* failed poll
                // of a contended acquire.
                st.file = None;
                return Ok(false);
            }
            if self.fd_matches_path(st)? {
                st.stale_warned = false;
                return Ok(true);
            }
            // Locked a file that a releaser already unlinked; drop it and retry
            // a fresh open (which re-creates the path).
            self.discard_fd(st);
        }
        // Giving up is reported as would-block, which the blocking callers
        // retry forever and the `try_*` callers surface as "busy". That is the
        // intended behaviour — the condition is transient by construction — but
        // a path that is *permanently* stale would otherwise spin with nothing
        // in the log.
        //
        // Latched, because this function is `poll_acquire`'s attempt closure:
        // unlatched, a permanently stale path emits a warn per backoff round,
        // ~10/s per addr, for the life of the run. The first one is the signal;
        // the rest are the same fact again.
        if st.stale_warned {
            tracing::debug!(
                path = %self.path.display(),
                retries = STALE_RETRIES,
                "lock file still being unlinked under us"
            );
        } else {
            st.stale_warned = true;
            tracing::warn!(
                path = %self.path.display(),
                retries = STALE_RETRIES,
                "lock file kept being unlinked under us; reporting it busy"
            );
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
    /// exactly then.
    ///
    /// **Truncate first, then write.** Both orders leave the same final bytes and
    /// cost the same two syscalls, but they differ in what a concurrent reader in
    /// another process can observe in between. Writing first leaves the tail of a
    /// longer previous payload in place, so every intermediate state is a *blend*
    /// of the new payload and the old one — and a blend can be indistinguishable
    /// from a complete, well-formed payload. Truncating first makes every
    /// intermediate state a strict prefix of the new payload over an empty file,
    /// which a framed reader can always reject. See `result_lock::read_pid`, whose
    /// whole framing argument rests on this order.
    ///
    /// Positioned (`pwrite`) rather than seek-then-write: it saves the `lseek`,
    /// and no reader of this fd depends on the offset today — `pwrite` keeps it
    /// that way, since the same open file description is shared by every
    /// in-process guard and a seek here would move it under all of them.
    ///
    /// Both syscalls name the lock file in their context: the caller holds only
    /// a guard, so with several addrs in flight an `ENOSPC`/`EIO` here is
    /// otherwise unattributable to a lock.
    fn write_contents(&self, bytes: &[u8]) -> Result<()> {
        let st = self.fd.lock();
        let f = st
            .file
            .as_ref()
            .context("write_contents requires a held lock (fd open)")?;
        f.set_len(0)
            .with_context(|| format!("emptying lock file {}", self.path.display()))?;
        f.write_all_at(bytes, 0)
            .with_context(|| format!("writing contents of lock file {}", self.path.display()))?;
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
        //
        // The *order* here is the invariant, and no test can pin it: unlink after
        // `LOCK_UN` reintroduces the two-holders race the module doc describes,
        // yet leaves the same end state, so every assertion still passes. Only
        // this comment stands between the two.
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

    /// `(dev, ino)` of the currently held fd, or `None` when none is open.
    /// Tests assert against this rather than calling
    /// [`fd_matches_path`](Self::fd_matches_path) — asserting a guard with the
    /// guard is circular, and would survive hardwiring it to `Ok(true)`.
    #[cfg(test)]
    fn held_ids(&self) -> Option<(u64, u64)> {
        let st = self.fd.lock();
        let meta = st
            .file
            .as_ref()?
            .metadata()
            .expect("fstat-ing held lock fd");
        Some((meta.dev(), meta.ino()))
    }

    /// Make the next `n` inode checks report "stale". See
    /// [`FdState::force_stale`].
    #[cfg(test)]
    fn inject_stale(&self, n: usize) {
        self.fd.lock().force_stale = n;
    }

    /// Whether the exhaustion warning is currently latched. The latch decides
    /// `warn!` vs `debug!` only, so a test cannot observe it through behaviour
    /// without installing a subscriber; asserting the state machine directly
    /// keeps the reset-on-success arm from being dead code nobody would notice.
    #[cfg(test)]
    fn stale_warned(&self) -> bool {
        self.fd.lock().stale_warned
    }

    /// Injections not yet consumed — how many of the forced-stale iterations the
    /// acquire actually performed.
    #[cfg(test)]
    fn pending_stale(&self) -> usize {
        self.fd.lock().force_stale
    }

    /// Current offset of the shared open file description. Exists so tests can
    /// pin the invariant [`write_contents`](Self::write_contents) documents:
    /// stamping contents must not move the offset every in-process guard shares.
    #[cfg(test)]
    fn stream_pos(&self) -> u64 {
        let st = self.fd.lock();
        let mut f = st
            .file
            .as_ref()
            .expect("fd must be open to read its offset");
        std::io::Seek::stream_position(&mut f).expect("stream_position on lock fd")
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

    /// Whether *some* open file description currently holds an exclusive lock on
    /// `path`. Probe-only: it never creates the file, never waits, and leaves no
    /// lock behind.
    ///
    /// The probe is a shared `flock(LOCK_SH | LOCK_NB)` on a fresh fd —
    /// `EWOULDBLOCK` means an exclusive holder exists, success means none did at
    /// that instant. It answers for the *lock*, not for a process: it is immune
    /// to pid reuse, to a zombie whose pid still answers `kill(pid, 0)`, and to a
    /// holder owned by another user.
    ///
    /// A missing file is "not held". Not because an unlinked inode cannot carry
    /// a lock — this module is built on the fact that it can, which is what
    /// [`fd_matches_path`](FLockState::fd_matches_path) exists to catch — but
    /// because [`release_write`](FLockState::release_write) unlinks *before*
    /// `LOCK_UN`: no path means any holder is already mid-release, or never
    /// created the file. Either way there is nobody worth naming.
    ///
    /// On a local filesystem `flock` locks belong to the open file description,
    /// not the process, so a lock this same process holds through some other fd
    /// correctly reads as held. (Over NFS, Linux emulates `flock` with
    /// per-process `fcntl` locks and a self-probe would read as unheld; macOS
    /// does not emulate. Cross-process exclusion on NFS is already outside what
    /// this module promises — this only notes that the probe inherits that.)
    ///
    /// Inherently a snapshot: the holder may release the moment after the probe.
    /// It is a diagnostic, never an admission decision — use [`try_lock`] for
    /// that, which acquires or does not.
    ///
    /// [`try_lock`]: Lock::try_lock
    pub fn is_path_held(path: impl AsRef<Path>) -> Result<bool> {
        let path = path.as_ref();
        let f = match OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("opening lock file {} to probe", path.display()));
            }
        };
        let held = !flock_nb(f.as_raw_fd(), libc::LOCK_SH)
            .with_context(|| format!("probing lock file {}", path.display()))?;
        if !held {
            // We took the shared lock to learn nobody held it; release it
            // explicitly rather than leaning on the `close` below, matching
            // `discard_fd`'s idiom. A transient `LOCK_SH` here can make a
            // concurrent `LOCK_EX|LOCK_NB` spuriously report busy, which costs
            // the caller one backoff round — bounded, and once per wait.
            // SAFETY: `f` owns a valid open fd for the duration of this call.
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        }
        Ok(held)
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
    use hcore::hasync::StdCancellationToken;
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
    async fn cross_instance_shared_read_is_blocked_by_a_writer() {
        // The mirror of `cross_instance_contention`, which only ever contends
        // EX-against-SH. This is the SH-against-EX direction: it is the only
        // thing that drives `try_lock_fresh`'s would-block early return down the
        // *read* path, where leaving the failed fd open would keep a phantom
        // reader alive for the lifetime of the instance.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let a = FRWLock::new(&path);
        let b = FRWLock::new(&path);

        let held = a.try_write().unwrap().expect("free lock acquires");
        assert!(b.try_read().unwrap().is_none(), "SH blocked by other EX");
        let (readers, writer, fd_open) = b.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "a refused read must leave no fd behind"
        );

        drop(held);
        assert!(b.try_read().unwrap().is_some(), "SH ok after EX released");
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
    async fn write_contents_targets_the_held_inode_not_the_path() {
        // The guard writes through its open file description. Proven by swapping
        // a decoy inode in at the path: a write that re-resolved the path would
        // land on the decoy. Both halves are asserted — that the bytes reached
        // the held inode, and that the decoy was left alone — so a
        // `write_contents` that wrote nothing at all cannot pass.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        let g = l.lock(&ct()).await.unwrap();
        // A second handle on the *held* inode, opened before the unlink, so what
        // lands there stays readable once the path names a different file.
        let held_inode = std::fs::File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"decoy").unwrap();

        g.write_contents(b"held").unwrap();

        let mut buf = [0u8; 4];
        held_inode
            .read_exact_at(&mut buf, 0)
            .expect("payload landed on the held inode");
        assert_eq!(&buf, b"held", "write_contents must write the held inode");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"decoy",
            "write_contents must not re-open the lock path"
        );
        drop(g);
    }

    #[tokio::test]
    async fn write_contents_leaves_the_shared_file_offset_untouched() {
        // The open file description is shared by every in-process guard on this
        // lock, so stamping contents must be positioned (`pwrite`) rather than
        // seek-then-write: a `rewind` + `write_all` leaves the offset at the
        // payload length, silently relocating any other guard's next read.
        let dir = tempfile::tempdir().expect("tempdir");
        let l = FLock::new(dir.path().join("lock"));

        let g = l.lock(&ct()).await.unwrap();
        assert_eq!(l.state.stream_pos(), 0, "a fresh fd starts at offset 0");

        g.write_contents(b"42").unwrap();
        assert_eq!(
            l.state.stream_pos(),
            0,
            "write_contents must not move the shared file offset"
        );
        drop(g);
    }

    #[test]
    fn write_contents_error_names_the_lock_file() {
        // Callers reach `write_contents` holding only a guard, so they have no
        // path of their own to log. The error must carry the lock file's
        // identity or an ENOSPC/EIO with several addrs in flight cannot be
        // attributed to a lock.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        std::fs::write(&path, b"x").unwrap();

        let state = FLockState::new(path.clone());
        // A read-only file description fails on the first syscall that mutates
        // the file, which beats filling a disk to reach the same branch. Which
        // of the two it is depends on their order, so the assertion pins the
        // identity rather than the wording — both carry the path.
        state.fd.lock().file = Some(std::fs::File::open(&path).unwrap());

        let err = state
            .write_contents(b"42")
            .expect_err("a read-only description cannot be modified");
        assert!(
            format!("{err}").contains(&path.display().to_string()),
            "error must name the lock file, got: {err}"
        );
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
    async fn stale_locked_inode_is_discarded_and_the_live_file_reacquired() {
        // The tail of the delete-on-unlock race: a releaser unlinked the file
        // between our `open` and our `flock`, so the lock we just took is on a
        // dead inode while a different one sits at the path.
        //
        // The window is between `ensure_open` and `flock_nb` and nothing
        // in-process can schedule it — but `ensure_open` hands back an
        // already-open `st.file` untouched, so seeding one reaches the same
        // state. This is the *real* check: no injection, actual inode
        // comparison. (Its predecessor swapped the inode before `lock()`, so the
        // fd was opened after the swap, the first iteration matched, and the
        // retry arm never ran.)
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        let dead = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let dead_ids = {
            let m = dead.metadata().unwrap();
            (m.dev(), m.ino())
        };
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"live").unwrap();
        l.state.fd.lock().file = Some(dead);

        let g = l.lock(&ct()).await.unwrap();

        let live = std::fs::metadata(&path).unwrap();
        assert_ne!(
            l.state.held_ids(),
            Some(dead_ids),
            "the dead inode must not survive the acquire"
        );
        assert_eq!(
            l.state.held_ids(),
            Some((live.dev(), live.ino())),
            "the retry must end up holding the file at the path"
        );
        drop(g);
    }

    #[tokio::test]
    async fn stale_retries_are_exhausted_rather_than_looping_forever() {
        // Pins `STALE_RETRIES` from both sides. One fewer stale iteration than
        // the budget still acquires; the full budget gives up — and gives up as
        // *would-block*, so the caller's backoff retries rather than erroring.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");

        let survives = FLock::new(&path);
        survives.state.inject_stale(STALE_RETRIES - 1);
        let g = survives
            .try_lock()
            .unwrap()
            .expect("one fewer than the budget still acquires");
        assert_eq!(survives.state.pending_stale(), 0, "all retries consumed");
        assert!(
            !survives.state.stale_warned(),
            "an acquire that succeeded never exhausted anything"
        );
        drop(g);

        let exhausts = FLock::new(&path);
        exhausts.state.inject_stale(STALE_RETRIES);
        assert!(
            exhausts.try_lock().unwrap().is_none(),
            "the full budget gives up"
        );
        assert_eq!(exhausts.state.pending_stale(), 0, "all retries consumed");
        assert!(
            exhausts.state.stale_warned(),
            "exhaustion latches, so the next round logs at debug instead of warn"
        );
        let (readers, writer, fd_open) = exhausts.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "giving up must leave no lock and no fd behind"
        );

        // Would-block, not failure: once the condition clears the same instance
        // acquires — and that clears the latch, so a *later* bout of staleness
        // is reported afresh rather than silently downgraded for the rest of the
        // process's life.
        let g = exhausts
            .try_lock()
            .unwrap()
            .expect("acquires once it clears");
        assert!(
            !exhausts.state.stale_warned(),
            "a successful acquire re-arms the warning"
        );
        drop(g);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_staleness_spins_cancellably_without_stranding_the_fd() {
        // Exhaustion reports would-block, and `poll_acquire` retries would-block
        // forever. That is the decision, not an oversight — but it must stay
        // interruptible, and each of the STALE_RETRIES fds per round must be
        // closed rather than accumulated.
        let dir = tempfile::tempdir().expect("tempdir");
        let l = FLock::new(dir.path().join("lock"));
        l.state.inject_stale(usize::MAX);

        let token = ct();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token2.cancel();
        });

        let err = tokio::time::timeout(Duration::from_secs(5), l.lock(&token))
            .await
            .expect("a permanently stale path must not hang")
            .expect_err("cancellation is the only way out");
        assert!(hplugin::error::is_cancelled(&err));

        let (readers, writer, fd_open) = l.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "clean after cancel"
        );
    }

    #[tokio::test]
    async fn is_path_held_answers_for_the_lock_not_for_a_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let l = FLock::new(&path);

        assert!(
            !FLock::is_path_held(&path).unwrap(),
            "no file yet: nothing can hold it"
        );

        // A file left behind by a holder that died: present, but its flock went
        // with the process. This is the case a pid stamped in the file cannot
        // answer — the pid may since have been reused by a live process.
        std::fs::write(&path, b"stamp\n").unwrap();
        assert!(
            !FLock::is_path_held(&path).unwrap(),
            "an unheld file is not held, whatever it contains"
        );

        // `flock` is per open file description, so our own guard is visible
        // through the probe's separate fd.
        let g = l.lock(&ct()).await.unwrap();
        assert!(
            FLock::is_path_held(&path).unwrap(),
            "held while a guard lives"
        );

        // The probe must not have taken a lock of its own, or the guard below
        // could not be re-acquired after release.
        drop(g);
        assert!(
            !FLock::is_path_held(&path).unwrap(),
            "released (and unlinked) on drop"
        );
        l.try_lock()
            .unwrap()
            .expect("probing must leave no lock behind");
    }

    #[tokio::test]
    async fn is_path_held_sees_a_shared_reader_as_unheld() {
        // The probe asks "is an exclusive holder present", which is what the
        // gateway lock is. A shared reader coexists with the probe's own
        // LOCK_SH, so it reads as unheld — correct for the gateway, and the
        // reason this is on `FLock` rather than shared with `FRWLock`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lock");
        let rw = FRWLock::new(&path);

        let r = rw.read(&ct()).await.unwrap();
        assert!(!FLock::is_path_held(&path).unwrap());
        drop(r);
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
        assert!(hplugin::error::is_cancelled(&err));

        let (readers, writer, fd_open) = b.state.counts();
        assert_eq!(
            (readers, writer, fd_open),
            (0, false, false),
            "clean after cancel"
        );
    }
}
