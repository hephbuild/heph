//! The sandbox permit: a bound on how many build sandboxes exist on disk at
//! once, counting the ones still waiting for the reclaim lane to remove them.
//!
//! Resource admission (`-j`, `resources`) caps how many targets *run* at once.
//! It does not cap how many finished sandboxes are queued for removal: a
//! finished run hands its sandbox to the reclaim lane fire-and-forget
//! (`sandbox_cleaner`), and a build of many short targets — thousands of
//! sandboxed `go list` runs in a cold Go `validate` — finishes sandboxes faster
//! than they are removed. Nothing bounded that backlog, and it filled the
//! disk.
//!
//! A permit is taken by a managed driver's execute before its sandbox
//! directory exists and travels with the sandbox's
//! [`SandboxTeardown`](crate::engine::sandbox_cleaner::SandboxTeardown) into
//! the reclaim job, which drops it once the removal has returned. So at any
//! moment the live and pending-removal sandboxes together number at most the
//! capacity `S` — plus the credential producers in flight, which run exempt
//! (see [`SandboxPermitPolicy`]).
//!
//! Outside the bound, deliberately: a failed target's sandbox kept for its
//! diagnostic (`leave_for_diagnostics` drops the permit at once; there is at
//! most one such tree per failed addr, and the next run of that addr removes
//! it), invocation sandboxes (`invoke_sandbox`), non-managed drivers and cache
//! hits, which create no sandbox.
//!
//! Nothing about a permit reaches a hash, or a run's argv, env or files: it
//! decides only *when* a run starts.
//!
//! Releases happen on the reclaim lane's OS threads, so a waiter is woken by a
//! cross-thread wake into the runtime; `hcore::blocking`'s module docs give the
//! measurement this trusts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Whether an execute takes a sandbox permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPermitPolicy {
    /// Every execute but one: a managed driver's run waits for a permit.
    Bounded,
    /// A credential producer run from inside a consumer's run
    /// (`material_from_producer`). The consumer already holds a permit and its
    /// sandbox; were the producer to wait for one, a pool exhausted by such
    /// consumers would never free one. The bound becomes `S` + the producers
    /// in flight.
    Exempt,
}

/// The engine's pool of sandbox permits. Cheap to clone; clones share the pool.
#[derive(Clone)]
pub struct SandboxPermits {
    inner: Arc<Inner>,
}

struct Inner {
    sem: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    /// Executes parked in [`SandboxPermits::acquire`].
    waiting: AtomicUsize,
    /// Permits whose sandbox has been handed to the reclaim lane.
    pending_removal: AtomicUsize,
    /// How long a reclaim job sleeps before its removal, in milliseconds: a
    /// test stands in a slow disk with it.
    #[cfg(any(test, feature = "test-support"))]
    removal_delay_ms: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SandboxPermits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.gauge();
        f.debug_struct("SandboxPermits")
            .field("capacity", &g.capacity)
            .field("held", &g.held)
            .field("waiting", &g.waiting)
            .field("pending_removal", &g.pending_removal)
            .finish()
    }
}

/// The pool's state at one instant, for the diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxPermitGauge {
    pub capacity: usize,
    /// Permits out: live sandboxes, sandboxes pending removal, and runs that
    /// hold one while they wait for admission.
    pub held: usize,
    /// Runs waiting for a permit.
    pub waiting: usize,
    /// Sandboxes handed to the reclaim lane and not yet removed.
    pub pending_removal: usize,
}

impl SandboxPermits {
    /// A pool of `capacity` permits; 0 is raised to 1, since a pool of none
    /// would park every managed run forever.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Inner {
                sem: Arc::new(tokio::sync::Semaphore::new(capacity)),
                capacity,
                waiting: AtomicUsize::new(0),
                pending_removal: AtomicUsize::new(0),
                #[cfg(any(test, feature = "test-support"))]
                removal_delay_ms: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn gauge(&self) -> SandboxPermitGauge {
        let available = self.inner.sem.available_permits();
        SandboxPermitGauge {
            capacity: self.inner.capacity,
            held: self.inner.capacity.saturating_sub(available),
            waiting: self.inner.waiting.load(Ordering::Relaxed),
            pending_removal: self.inner.pending_removal.load(Ordering::Relaxed),
        }
    }

    /// Make every reclaim job holding one of this pool's permits sleep `delay`
    /// before its removal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_removal_delay(&self, delay: std::time::Duration) {
        self.inner.removal_delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Take a permit for `addr`, parking until one is free. Returns
    /// [`CancelledError`](hplugin::error::CancelledError) as soon as `ctoken`
    /// is cancelled, so a cancelled request never waits on the reclaim lane.
    /// Dropping the future mid-wait leaves the queue (tokio's semaphore).
    pub(crate) async fn acquire(
        &self,
        addr: &hmodel::htaddr::Addr,
        ctoken: &hcore::hasync::StdCancellationToken,
    ) -> anyhow::Result<SandboxPermit> {
        use hcore::hasync::Cancellable;
        let sem = Arc::clone(&self.inner.sem);
        let permit = match sem.try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::Closed) => anyhow::bail!("sandbox permits closed"),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                let _waiting = Waiting::enter(&self.inner);
                tracing::debug!(
                    addr = %addr,
                    capacity = self.inner.capacity,
                    pending_removal = self.inner.pending_removal.load(Ordering::Relaxed),
                    "waiting for sandbox cleanup: every sandbox permit is held"
                );
                tokio::select! {
                    () = ctoken.cancelled() => {
                        return Err(anyhow::Error::new(hplugin::error::CancelledError));
                    }
                    permit = Arc::clone(&self.inner.sem).acquire_owned() => {
                        permit.map_err(|_closed| anyhow::anyhow!("sandbox permits closed"))?
                    }
                }
            }
        };
        Ok(SandboxPermit {
            _permit: permit,
            inner: Arc::clone(&self.inner),
            pending_removal: false,
        })
    }
}

/// Counts one parked acquire for as long as it lives, so a cancelled or
/// dropped wait leaves the count exactly as a granted one does.
struct Waiting<'a>(&'a Inner);

impl<'a> Waiting<'a> {
    fn enter(inner: &'a Inner) -> Self {
        inner.waiting.fetch_add(1, Ordering::Relaxed);
        Self(inner)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One sandbox's place in the pool. Released on drop.
pub struct SandboxPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    inner: Arc<Inner>,
    pending_removal: bool,
}

impl std::fmt::Debug for SandboxPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxPermit")
            .field("pending_removal", &self.pending_removal)
            .finish()
    }
}

impl SandboxPermit {
    /// Wrap `job`, the removal of this permit's sandbox, so the permit is
    /// released when the removal returns — whether it removed, declined
    /// (a successor owns the path), failed or panicked — and is counted as
    /// pending removal until then.
    pub(crate) fn release_after(
        mut self,
        job: crate::engine::sandbox_cleaner::SandboxCleanupJob,
    ) -> crate::engine::sandbox_cleaner::SandboxCleanupJob {
        self.pending_removal = true;
        self.inner.pending_removal.fetch_add(1, Ordering::Relaxed);
        Box::new(move || {
            #[cfg(any(test, feature = "test-support"))]
            {
                let ms = self.inner.removal_delay_ms.load(Ordering::Relaxed);
                if ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
            }
            let removed = job();
            drop(self);
            removed
        })
    }
}

impl Drop for SandboxPermit {
    fn drop(&mut self) {
        if self.pending_removal {
            self.inner.pending_removal.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn addr() -> hmodel::htaddr::Addr {
        hmodel::htaddr::parse_addr("//p:t").expect("addr")
    }

    #[tokio::test]
    async fn a_permit_is_returned_when_it_drops() {
        let pool = SandboxPermits::new(1);
        let token = hcore::hasync::StdCancellationToken::new();
        let permit = pool.acquire(&addr(), &token).await.expect("first");
        assert_eq!(pool.gauge().held, 1);
        drop(permit);
        assert_eq!(pool.gauge().held, 0);
        drop(pool.acquire(&addr(), &token).await.expect("again"));
    }

    #[tokio::test]
    async fn a_full_pool_parks_until_a_release() {
        let pool = SandboxPermits::new(1);
        let token = hcore::hasync::StdCancellationToken::new();
        let first = pool.acquire(&addr(), &token).await.expect("first");
        let waiter = tokio::spawn({
            let pool = pool.clone();
            let token = token.clone();
            async move { pool.acquire(&addr(), &token).await.map(drop) }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pool.gauge().waiting != 1 {
            assert!(std::time::Instant::now() < deadline, "never parked");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!waiter.is_finished());
        drop(first);
        tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("granted on release")
            .expect("join")
            .expect("acquire");
        assert_eq!(pool.gauge().waiting, 0);
        assert_eq!(pool.gauge().held, 0);
    }

    #[tokio::test]
    async fn a_cancelled_wait_returns_cancelled_and_leaves_the_queue() {
        let pool = SandboxPermits::new(1);
        let token = hcore::hasync::StdCancellationToken::new();
        let first = pool.acquire(&addr(), &token).await.expect("first");
        let waiter = tokio::spawn({
            let pool = pool.clone();
            let token = token.clone();
            async move { pool.acquire(&addr(), &token).await.map(drop) }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pool.gauge().waiting != 1 {
            assert!(std::time::Instant::now() < deadline, "never parked");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        token.cancel();
        let err = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("a cancelled wait must not hang")
            .expect("join")
            .expect_err("cancelled");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert_eq!(pool.gauge().waiting, 0);
        // The cancelled waiter took nothing: the permit goes back whole.
        drop(first);
        assert_eq!(pool.gauge().held, 0);
    }

    #[test]
    fn a_reclaim_job_releases_its_permit_after_the_removal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let pool = SandboxPermits::new(1);
        let token = hcore::hasync::StdCancellationToken::new();
        let permit = rt.block_on(pool.acquire(&addr(), &token)).expect("acquire");
        let seen = Arc::new(std::sync::Mutex::new(None));
        let job = permit.release_after(Box::new({
            let pool = pool.clone();
            let seen = Arc::clone(&seen);
            move || {
                // Inside the removal the permit is still out, and counted as
                // pending removal.
                *seen.lock().expect("lock") = Some(pool.gauge());
                Ok(())
            }
        }));
        assert_eq!(pool.gauge().pending_removal, 1);
        job().expect("job");
        let inside = seen.lock().expect("lock").expect("ran");
        assert_eq!((inside.held, inside.pending_removal), (1, 1));
        assert_eq!((pool.gauge().held, pool.gauge().pending_removal), (0, 0));
    }

    #[test]
    fn a_panicking_or_unrun_reclaim_job_still_releases() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let pool = SandboxPermits::new(2);
        let token = hcore::hasync::StdCancellationToken::new();
        let a = rt.block_on(pool.acquire(&addr(), &token)).expect("a");
        let b = rt.block_on(pool.acquire(&addr(), &token)).expect("b");
        let panics = a.release_after(Box::new(|| panic!("removal panicked")));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(panics));
        assert!(outcome.is_err());
        // A job dropped unrun (a closed lane) releases too.
        drop(b.release_after(Box::new(|| Ok(()))));
        assert_eq!((pool.gauge().held, pool.gauge().pending_removal), (0, 0));
    }

    #[test]
    fn a_pool_of_zero_is_a_pool_of_one() {
        assert_eq!(SandboxPermits::new(0).capacity(), 1);
    }
}
