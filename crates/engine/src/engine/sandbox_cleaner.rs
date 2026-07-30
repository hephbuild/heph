//! Fire-and-forget background cleanup on dedicated OS threads.
//!
//! Posting to a queue is a non-blocking `crossbeam_channel::send`. A
//! long-lived worker thread drains that queue and runs each job
//! inline. No tokio waker is involved (avoids the macOS cross-thread
//! waker bug — see the hazard note in `hproc::proc_exec`), and no tokio worker
//! is parked for the cleanup (avoids the `block_in_place` concurrency
//! regression measured in `PERFORMANCE.md` suggestion #0).
//!
//! Each job is an opaque `FnOnce` so the layer that built the sandbox
//! also owns the knowledge of how to tear it down. The FUSE bridge
//! rms its upper-side dir directly (bypassing the live mount); the OS
//! bridge rms the plain sandbox dir. The cleaner doesn't branch.
//!
//! ## Two lanes, one job class each
//!
//! There are two classes of background job and they must not share a
//! queue. [`Lane::Reclaim`] is disk reclamation — a `remove_dir_all` of
//! 5k–50k inodes for a finished Go sandbox, one job per executed target —
//! and it is what keeps a build from running the disk out from under
//! itself. [`Lane::Bookkeeping`] is cache housekeeping: today exactly one
//! job, the batched post-write `cache.history` trim that
//! `RequestStateData`'s drop submits, dominated by sqlite round-trips and
//! reclaiming comparatively little per target.
//!
//! What makes them incompatible on one queue is *when* the bookkeeping job
//! arrives. It is submitted at request-state drop — precisely the moment
//! the reclaim backlog is deepest, since every sandbox the run finished
//! with is still queued for removal. On a shared FIFO the trim batch lands
//! behind all of them, so `cache.history` is only enforced after the last
//! 5k–50k-inode removal completes, and process exit — gated on
//! `bg_pending`, which both lanes feed — pays the rmdir drain *plus* the
//! trim rather than the max of the two. Separate threads make the two
//! costs concurrent, and keep a stalled sqlite round-trip from costing
//! anything but bookkeeping latency.
//!
//! The split also keeps the classes separated structurally. Before the
//! deferred-trim work the trim was enqueued *per target, mid-run, ahead of
//! that same target's rmdir* (the rmdir rides a `defer!` firing at the end
//! of the same scope), which put every removal behind a sqlite wait exactly
//! when execution peaked. That specific shape is gone, but nothing except
//! this lane boundary stops the next per-target bookkeeping job from
//! recreating it.
//!
//! Lanes stay dedicated OS threads rather than moving onto
//! `hcore::blocking`: that pool is sized for *bounded* jobs and already
//! carries the cache writes (tar/gzip/borsh), so queued sqlite waits
//! would contend with the very work they are bookkeeping for. Neither lane
//! may move onto a tokio runtime — see the waker note above.
//!
//! Ordering: callers must invoke `enqueue` only *after* any read of the
//! sandbox completes (per `project_sandbox_cleanup_ordering.md`). Within a
//! lane, jobs run in FIFO order on that lane's one thread. There is no
//! ordering guarantee *between* lanes — that is the point.
//!
//! Both lanes decrement the same per-request `bg_pending` counter, so the
//! shutdown path that blocks on it drains both.
use crossbeam_channel::{Sender, unbounded};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{io, thread};

/// `remove_dir_all` that recovers from `PermissionDenied`. The Go module
/// cache (and other read-only tooling) leaves `0555` directories behind;
/// the kernel refuses to unlink their children until the dir is writable.
/// On the first permission failure we recursively `chmod 0777` every
/// directory under `dir` and retry the removal once.
///
/// Borrowed from the Go toolchain's `modfetch.MakeDirsReadWrite`:
/// https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go
// The permission-recovering `remove_dir_all` now lives in `heph-core` so the
// driver-support crate can share it; re-exported here for existing callers.
pub use hcore::fsutil::remove_dir_all;

/// One cleanup unit. Returning an `io::Result` lets the cleaner thread
/// emit a uniform log line on failure (filtering out `NotFound`, which
/// is common when retries collide).
pub type SandboxCleanupJob = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;

/// Per-request count of cleanup jobs enqueued but not yet finished (queued +
/// in-flight). Lives in the request state (see `RequestStateData::bg_pending`)
/// and is carried alongside each job so the global cleaner thread can decrement
/// the right request's counter once the job has run. The shutdown path keeps
/// the TUI open — and the process alive — until this drains to zero, so we never
/// exit out from under an in-progress rmdir.
pub type PendingCounter = Arc<AtomicUsize>;

/// Queue entry: a failure `label`, the job, and the request counter to
/// decrement when it completes.
type Job = (String, SandboxCleanupJob, PendingCounter);

/// Which worker thread a job runs on.
///
/// The two classes have very different cost profiles and one must never be able
/// to queue behind the other; see the module docs for why. Every job picks its
/// lane explicitly so a third class cannot be added by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Disk reclamation — `remove_dir_all` of a finished sandbox. Latency here
    /// is the build's peak disk footprint and its post-build serial tail.
    Reclaim,
    /// Cache bookkeeping — the post-write GC trim. Blocks on sqlite; reclaims
    /// little. Delaying it is cheap, so it never shares a queue with `Reclaim`.
    Bookkeeping,
}

/// Per-sandbox-path generations, serializing destructive jobs against the
/// execute that owns the path now.
///
/// The hazard: a sandbox-removal job is detached the moment it is queued — the
/// pre-run stale-remove runs on `hcore::blocking`'s pool and its awaiter can be
/// cancelled (cancel-on-abandonment makes that routine, and the production
/// wedge was parked at exactly that await), and the post-run cleanup runs
/// fire-and-forget on the cleaner thread. A later execute of the same target
/// recreates the very directory such a straggler is about to delete, and a
/// `remove_dir_all` landing on a live sandbox silently eats freshly written
/// outputs.
///
/// The scheme: **claim at queue time, check at walk time.** Every destructive
/// job is queued carrying the generation its execute [`claim`]ed, and when the
/// job finally runs it re-reads the path's current generation and declines if
/// the path has moved on. This holds in *every* ordering — queue order, pool
/// order, and lock-acquisition order are all irrelevant, because the claim
/// happens synchronously on the execute's own path before the successor can
/// exist, and the check happens under the gate at the last moment:
///
/// * A stale job that starts after the successor's claim sees `current !=
///   claimed` and declines. Declining is always safe: whatever it would have
///   removed was already removed (or is about to be) by the successor's own
///   [`remove_stale`].
/// * A job that passed its check is walking under the `walk` lock, so the
///   successor's own [`remove_stale`] queues behind it — and everything the
///   in-flight walk touches predates the successor's creation, because
///   creation only happens after the successor's awaited [`remove_stale`]
///   returns.
/// * A cancelled execute with no successor keeps the current claim, so its own
///   queued jobs still run and reclaim its mess.
///
/// Two locks per path, and the split is load-bearing: [`claim`] takes only the
/// `generation` mutex (an increment — microseconds even while a walk is in
/// flight), so it is safe to call inline on a tokio worker at queue time.
/// Walks hold the `walk` mutex for their whole duration and take `generation`
/// only for the staleness check; walk holders are dedicated OS threads (the
/// blocking pool, the cleaner thread), never tokio workers. Lock order is
/// `walk` then `generation`, and nothing takes them in the other order.
///
/// Executes for one addr are serialized (in-process by the per-addr result
/// lock, cross-process by the flock), so the generation cannot advance under a
/// *live* run — only past a cancelled or completed one.
///
/// Not covered: a straggler from *another process*. The flock serializes the
/// executes themselves but not their detached jobs, and this registry is
/// process-local. That exposure predates cancel-on-abandonment (any process
/// exit with queued jobs had it) and is bounded by process lifetime — queued
/// jobs die with the process that queued them.
pub mod generation {
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    /// One path's gate. `walk` serializes destructive walks; `generation` is
    /// the claim counter, held only for increments and comparisons so
    /// [`claim`](super::generation::claim) never blocks behind a walk.
    #[derive(Default)]
    struct PathGate {
        walk: Mutex<()>,
        generation: Mutex<u64>,
    }

    /// Outer registry lock is held only to fetch/insert the per-path gate —
    /// never across a walk, so unrelated sandboxes do not serialize each other.
    ///
    /// Never pruned, deliberately: one entry per distinct sandbox path (≈ per
    /// executed target addr) for the life of the process, ~a hundred bytes
    /// each. Pruning would need proof that no queued job still references the
    /// path, which is exactly the bookkeeping this module exists to avoid.
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Arc<PathGate>>>> = OnceLock::new();

    fn gate_for(dir: &Path) -> Arc<PathGate> {
        let mut gates = GATES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Arc::clone(gates.entry(dir.to_path_buf()).or_default())
    }

    /// A panicking job poisons only its own path's locks; both protect plain
    /// values that are always consistent.
    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claim `dir` for a new execute. Advances the generation and returns it;
    /// every removal job this execute queues carries the returned value. No
    /// filesystem work and no walk lock — safe to call inline on the async
    /// path at queue time, which is what makes the claim ordered *before* any
    /// successor can exist.
    pub fn claim(dir: &Path) -> u64 {
        let gate = gate_for(dir);
        let mut generation = lock(&gate.generation);
        *generation += 1;
        *generation
    }

    /// Run `f` under `dir`'s walk lock iff the path still belongs to the
    /// execute that claimed `claimed`; a superseded job declines with `Ok(())`.
    fn run_if_current(
        dir: &Path,
        claimed: u64,
        what: &str,
        f: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let gate = gate_for(dir);
        let _walk = lock(&gate.walk);
        let current = *lock(&gate.generation);
        if current != claimed {
            tracing::debug!(
                dir = %dir.display(),
                claimed,
                current,
                "skipping {what} superseded by a newer execute",
            );
            return Ok(());
        }
        f()
    }

    /// The pre-run stale removal, run on the blocking pool with the claim its
    /// execute took at queue time. Declines if a successor has reclaimed the
    /// path — a queued job whose awaiter was cancelled must never delete the
    /// successor's live sandbox.
    pub fn remove_stale(dir: &Path, claimed: u64) -> io::Result<()> {
        run_if_current(
            dir,
            claimed,
            "stale sandbox removal",
            || match super::remove_dir_all(dir) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            },
        )
    }

    /// Wrap a bridge-owned cleanup job so it runs only if `dir` still belongs
    /// to the execute that claimed `claimed`.
    pub fn guarded(
        dir: PathBuf,
        claimed: u64,
        job: super::SandboxCleanupJob,
    ) -> super::SandboxCleanupJob {
        Box::new(move || run_if_current(&dir, claimed, "sandbox cleanup", job))
    }

    #[cfg(test)]
    pub(super) fn current_generation(dir: &Path) -> u64 {
        *lock(&gate_for(dir).generation)
    }
}

/// Owns a claimed sandbox's teardown from claim to resolution, so that
/// **every** exit makes exactly one teardown decision — and the right one.
///
/// Armed in `Engine::execute` the moment the path is [`generation::claim`]ed,
/// carried back to `execute_and_cache_inner`. Three exits, three behaviours:
///
/// * [`complete`](Self::complete) — **success** (the run and its cache write
///   finished, or the cache write failed with an error value): the
///   bridge-owned cleanup job is enqueued, guarded by this run's generation.
///   The bridge job knows the real layout (plain dir vs FUSE upper side).
/// * [`leave_for_diagnostics`](Self::leave_for_diagnostics) — **the target
///   failed** (an `Err` is propagating out of the execute): the sandbox is
///   left on disk, untouched, deliberately. The failure diagnostic reads the
///   process's last log lines *lazily* from the on-disk log when it renders —
///   a failed target's sandbox surviving until that target's next run is
///   pre-existing, documented behaviour, and reclaiming it here turns the
///   diagnostic into a race against its own cleanup (output present or absent
///   depending on cleaner-lane load). The next execute's
///   [`generation::remove_stale`] reclaims it, exactly as it always has. This
///   is retention by design, not a leak: at most one tree per failed addr,
///   the same bound as before teardown ownership existed.
/// * `Drop` without either — **cancellation or abandonment only** (the future
///   was dropped or unwound without resolving): a generation-checked
///   [`generation::remove_stale`] reclaim is enqueued. If a successor already
///   reclaimed the path it declines; otherwise it removes this run's
///   half-written tree. Without this, mass fail-fast (the wedge run had 1,135
///   and 1,949 cancellations) leaves one abandoned sandbox tree per cancelled
///   execute with nothing to collect them — `gc` has no sandbox sweep, and
///   unlike a failure, a cancelled target has no failure diagnostic that
///   needs the tree.
///
/// The drop path deliberately enqueues the reclaim rather than the bridge job:
/// the bridge job's ordering contract (slot guards deregistered first) is
/// only guaranteed on the completion path, while the reclaim touches nothing
/// but the logical directory — the same operation the next execute's
/// `remove_stale` would perform, just sooner.
pub struct SandboxTeardown {
    dir: PathBuf,
    generation: u64,
    pending: PendingCounter,
    /// The bridge-owned cleanup closure, handed over once the run responded.
    job: Option<SandboxCleanupJob>,
    armed: bool,
}

/// Hand-written because the bridge's cleanup closure is not `Debug`; the
/// closure is reported as present/absent, which is the part worth seeing in a
/// log. Written out rather than skipped so the type still satisfies the
/// derive-`Debug`-on-public-types rule.
impl std::fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("dir", &self.dir)
            .field("generation", &self.generation)
            .field("armed", &self.armed)
            .field("job", &self.job.is_some())
            .finish()
    }
}

impl SandboxTeardown {
    /// Take ownership of tearing down `dir`, which the caller has just
    /// [`generation::claim`]ed as `generation`.
    pub fn arm(dir: PathBuf, generation: u64, pending: PendingCounter) -> Self {
        Self {
            dir,
            generation,
            pending,
            job: None,
            armed: true,
        }
    }

    /// Hand over the bridge's cleanup closure once the run has responded.
    pub fn set_job(&mut self, job: Option<SandboxCleanupJob>) {
        self.job = job;
    }

    /// The run completed (with a success or an error value): enqueue the
    /// bridge cleanup, generation-guarded. Call only after every reader of the
    /// sandbox is done — `cache_locally` reads from it.
    pub fn complete(mut self, label: String) {
        self.armed = false;
        if let Some(job) = self.job.take() {
            enqueue(
                Lane::Reclaim,
                label,
                generation::guarded(self.dir.clone(), self.generation, job),
                Arc::clone(&self.pending),
            );
        }
    }

    /// The target failed: leave the sandbox on disk for the failure
    /// diagnostic, which reads the process's log tail lazily from it. Nothing
    /// is enqueued — not the bridge job, not a reclaim — and the tree survives
    /// until this target's next run removes it, the documented pre-teardown
    /// behaviour. See the type docs for why this must never be folded into
    /// `Drop`: a drop-time reclaim here made the diagnostic race its own
    /// cleanup, surfacing a failed target's exit status with its output
    /// already deleted.
    pub fn leave_for_diagnostics(mut self) {
        self.armed = false;
        self.job = None;
    }
}

impl Drop for SandboxTeardown {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (dir, generation) = (self.dir.clone(), self.generation);
        let label = format!("reclaim {}", dir.display());
        enqueue(
            Lane::Reclaim,
            label,
            Box::new(move || generation::remove_stale(&dir, generation)),
            Arc::clone(&self.pending),
        );
    }
}

impl Lane {
    /// This lane's queue.
    ///
    /// One `OnceLock` per lane rather than one for both: a lane is only spawned
    /// when something is actually enqueued onto it (a run with no cacheable
    /// target never starts the bookkeeping thread), and a spawn failure on one
    /// lane cannot take the other's already-created sender down with it.
    fn cell(self) -> &'static OnceLock<Sender<Job>> {
        static RECLAIM: OnceLock<Sender<Job>> = OnceLock::new();
        static BOOKKEEPING: OnceLock<Sender<Job>> = OnceLock::new();
        match self {
            Lane::Reclaim => &RECLAIM,
            Lane::Bookkeeping => &BOOKKEEPING,
        }
    }

    /// The OS thread name this lane's worker carries.
    ///
    /// The reclaim lane keeps the historical name — it is what
    /// `RCA_MACOS_WAKER.md` refers to and what shows up in existing stack dumps.
    /// It is 20 bytes, so Linux's 16-byte `PR_SET_NAME` limit has always
    /// truncated it to `heph-sandbox-cl` in `/proc/*/comm`, `gdb` and `perf`;
    /// `heph-cache-gc` fits. Rust's own `Thread::name` reports the full string on
    /// every target either way.
    fn thread_name(self) -> &'static str {
        match self {
            Lane::Reclaim => "heph-sandbox-cleaner",
            Lane::Bookkeeping => "heph-cache-gc",
        }
    }
}

/// Spawn one lane's worker thread and return its queue.
fn spawn_lane(lane: Lane) -> io::Result<Sender<Job>> {
    let name = lane.thread_name();
    let (tx, rx) = unbounded::<Job>();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for (label, job, pending) in rx.iter() {
                // catch_unwind so a panicking job doesn't kill the
                // long-lived cleaner thread and silently drop every
                // subsequent cleanup for the process lifetime.
                let outcome = catch_unwind(AssertUnwindSafe(job));
                // Decrement after the job runs (not on dequeue) so the
                // counter only hits zero once the work is genuinely done.
                pending.fetch_sub(1, Ordering::AcqRel);
                match outcome {
                    Ok(Ok(())) => (),
                    Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => (),
                    // `?lane` here and in `enqueue`, never the thread name: one
                    // `lane` field with one value space, or a log query for
                    // `lane=Reclaim` silently misses every failure line.
                    Ok(Err(err)) => {
                        tracing::error!(
                            error = %err,
                            label = %label,
                            ?lane,
                            "background cleanup job failed",
                        );
                    }
                    Err(_) => {
                        tracing::error!(
                            label = %label,
                            ?lane,
                            "background cleanup job panicked",
                        );
                    }
                }
            }
        })
        // Name the lane in the error: `sender` resolves both lanes through one
        // expression, so the panic location alone doesn't say which one failed.
        .map_err(|e| io::Error::new(e.kind(), format!("spawn {name} thread: {e}")))?;
    Ok(tx)
}

fn sender(lane: Lane) -> &'static Sender<Job> {
    // Same stance as `hcore::blocking`: a process that cannot spawn its worker
    // threads has nothing to fall back to.
    lane.cell()
        .get_or_init(|| spawn_lane(lane).expect("spawn background cleanup lane"))
}

/// Enqueue a cleanup job onto `lane` for asynchronous execution. `label` is used
/// only for log lines emitted if the job fails. `pending` is the request's
/// in-flight counter, bumped here and dropped back by the lane's worker once the
/// job has run — both lanes share one counter, so a shutdown blocking on it
/// drains both. Non-blocking.
pub fn enqueue(lane: Lane, label: String, job: SandboxCleanupJob, pending: PendingCounter) {
    // Resolve the queue *before* counting: `sender` spawns the lane's thread on
    // first use and panics if that fails. Bumping first would leave `bg_pending`
    // permanently above zero, and the shutdown path only breaks when it reaches
    // zero — a failed spawn would hang the run instead of ending it.
    let tx = sender(lane);
    // Count before sending so the counter can never observe an enqueued job as
    // already drained. The worker decrements once the job has run.
    //
    // Deliberately *not* split into a reserve-now / submit-later pair. The
    // counter gates process exit through an untimed loop in both TUI backends,
    // so a slot taken before the work is submitted is only ever released by
    // whatever was supposed to submit it — and a `RequestStateData` pinned by an
    // abandoned memoizer cell (a live hazard, see `hmemoizer::cell`'s retained
    // future and the `fail_fast` fanout that drops in-flight awaiters) would
    // then turn a silent leak into a process that never exits. A slot exists
    // only for work already in this queue.
    pending.fetch_add(1, Ordering::AcqRel);
    if let Err(err) = tx.send((label, job, Arc::clone(&pending))) {
        pending.fetch_sub(1, Ordering::AcqRel);
        tracing::error!(error = %err, ?lane, "background cleanup lane closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn wait_for(flag: &AtomicBool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !flag.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    fn counter() -> PendingCounter {
        Arc::new(AtomicUsize::new(0))
    }

    /// Opens a gate on drop, so a failing assertion cannot leave a lane's worker
    /// thread parked for the rest of the test binary.
    struct OpenOnDrop(Arc<AtomicBool>);

    impl Drop for OpenOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A job that occupies its lane's thread until `gate` opens, raising
    /// `started` as soon as it is genuinely running rather than merely queued.
    fn gated_job(started: Arc<AtomicBool>, gate: Arc<AtomicBool>) -> SandboxCleanupJob {
        Box::new(move || {
            started.store(true, Ordering::SeqCst);
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        })
    }

    const ALL_LANES: [Lane; 2] = [Lane::Reclaim, Lane::Bookkeeping];

    /// The lane threads are process-global, so a test that deliberately parks one
    /// would make any *other* test waiting on that lane look like a lane-split
    /// failure. Every test that parks a lane holds this first, so at most one
    /// does at a time.
    static PARKED: Mutex<()> = Mutex::new(());

    fn park_lock() -> std::sync::MutexGuard<'static, ()> {
        PARKED.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn enqueue_runs_job_on_every_lane() {
        for lane in ALL_LANES {
            let ran = Arc::new(AtomicBool::new(false));
            let ran_clone = Arc::clone(&ran);
            enqueue(
                lane,
                format!("enqueue_runs_job_on_every_lane {lane:?}"),
                Box::new(move || {
                    ran_clone.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                counter(),
            );
            assert!(
                wait_for(&ran, Duration::from_secs(5)),
                "job did not run on {lane:?}"
            );
        }
    }

    #[test]
    fn enqueue_removes_tempdir_via_closure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested");
        std::fs::create_dir_all(target.join("a/b")).expect("mkdir");
        std::fs::write(target.join("a/b/file"), b"x").expect("write file");
        assert!(target.exists());
        let target_for_job = target.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);
        enqueue(
            Lane::Reclaim,
            "enqueue_removes_tempdir_via_closure".to_string(),
            Box::new(move || {
                let res = std::fs::remove_dir_all(&target_for_job);
                done_clone.store(true, Ordering::SeqCst);
                res
            }),
            counter(),
        );
        assert!(
            wait_for(&done, Duration::from_secs(5)),
            "job did not run within 5s"
        );
        assert!(!target.exists(), "cleanup closure did not remove target");
    }

    #[test]
    fn enqueue_swallows_notfound() {
        // No assertion on log output (tracing is global); the
        // important behavior is that a lane's worker thread doesn't die
        // when a job returns NotFound. We follow up with another job
        // that must run on the same thread.
        for lane in ALL_LANES {
            enqueue(
                lane,
                format!("enqueue_swallows_notfound_first {lane:?}"),
                Box::new(|| Err(io::Error::from(io::ErrorKind::NotFound))),
                counter(),
            );
            let ran = Arc::new(AtomicBool::new(false));
            let ran_clone = Arc::clone(&ran);
            enqueue(
                lane,
                format!("enqueue_swallows_notfound_followup {lane:?}"),
                Box::new(move || {
                    ran_clone.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                counter(),
            );
            assert!(
                wait_for(&ran, Duration::from_secs(5)),
                "{lane:?} stopped processing after NotFound"
            );
        }
    }

    #[test]
    fn pending_counter_drains_to_zero_after_job_runs() {
        // A blocked job holds the request counter at 1 until released, then drops
        // it back to 0 once the lane finishes it. This is the signal the
        // shutdown path waits on to keep the TUI/process alive during drain.
        let _parked = park_lock();
        let pending = counter();
        let gate = Arc::new(AtomicBool::new(false));
        let _release = OpenOnDrop(Arc::clone(&gate));
        let started = Arc::new(AtomicBool::new(false));
        enqueue(
            Lane::Reclaim,
            "pending_counter_drains_to_zero_after_job_runs".to_string(),
            gated_job(Arc::clone(&started), Arc::clone(&gate)),
            Arc::clone(&pending),
        );
        assert_eq!(
            pending.load(Ordering::Acquire),
            1,
            "counter must rise while job is in flight"
        );
        gate.store(true, Ordering::SeqCst);
        assert!(
            wait_for(&started, Duration::from_secs(5)),
            "job did not run"
        );
        // Spin until the worker's post-job decrement lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "counter did not drain to zero");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// P6.2: neither lane may be gated by the other, in either direction.
    ///
    /// The direction that matters in production is a blocked bookkeeping job
    /// delaying a reclaim: it mirrors the execute path's real ordering — the
    /// post-write GC trim is enqueued first, because the sandbox rmdir rides a
    /// `defer!` that only fires at the end of the same scope — and the trim blocks
    /// on a sqlite round-trip. With both classes on one FIFO thread the rmdir
    /// cannot start until that round-trip returns; with a lane per class it runs
    /// immediately. The mirror direction is asserted too so a future collapse onto
    /// *either* single queue is caught.
    #[test]
    fn a_blocked_lane_does_not_delay_the_other() {
        for (blocked, observed) in [
            (Lane::Bookkeeping, Lane::Reclaim),
            (Lane::Reclaim, Lane::Bookkeeping),
        ] {
            let _parked = park_lock();
            let gate = Arc::new(AtomicBool::new(false));
            // Opens the gate even if an assertion below unwinds, so a failing run
            // does not wedge a lane for the rest of the test binary.
            let _release = OpenOnDrop(Arc::clone(&gate));

            let started = Arc::new(AtomicBool::new(false));
            enqueue(
                blocked,
                format!("a_blocked_lane_does_not_delay_the_other/blocked {blocked:?}"),
                gated_job(Arc::clone(&started), Arc::clone(&gate)),
                counter(),
            );
            // Only enqueue onto the observed lane once the blocked job genuinely
            // owns a worker thread. Otherwise a shared queue could drain the second
            // job first by luck and the test would pass against a single queue.
            assert!(
                wait_for(&started, Duration::from_secs(5)),
                "{blocked:?} job never started"
            );

            let ran = Arc::new(AtomicBool::new(false));
            let ran_job = Arc::clone(&ran);
            enqueue(
                observed,
                format!("a_blocked_lane_does_not_delay_the_other/observed {observed:?}"),
                Box::new(move || {
                    ran_job.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                counter(),
            );
            // Generous on purpose. On a genuinely shared queue `ran` can never
            // fire until `_release` drops, so this budget bounds only the failing
            // case; it costs nothing when passing. A tight budget would instead
            // turn ordinary CI load — the lanes are process-global and every other
            // test in this binary feeds them — into a "the lanes share a queue"
            // verdict that sends the reader to the wrong file.
            assert!(
                wait_for(&ran, Duration::from_secs(30)),
                "{observed:?} did not run while {blocked:?} was parked — the lanes share a queue"
            );
        }
    }

    /// Shutdown blocks until `bg_pending` reaches zero. Splitting the lanes must
    /// not let a job on either one escape that counter, or the process can exit
    /// out from under an in-progress rmdir or trim.
    #[test]
    fn both_lanes_drain_the_same_pending_counter() {
        let _parked = park_lock();
        let pending = counter();
        let gate = Arc::new(AtomicBool::new(false));
        let _release = OpenOnDrop(Arc::clone(&gate));

        let started = ALL_LANES.map(|lane| {
            let started = Arc::new(AtomicBool::new(false));
            enqueue(
                lane,
                format!("both_lanes_drain_the_same_pending_counter {lane:?}"),
                gated_job(Arc::clone(&started), Arc::clone(&gate)),
                Arc::clone(&pending),
            );
            started
        });
        // Both jobs must genuinely be running, not merely queued, or "in flight"
        // below would be an assertion about the enqueue side alone.
        for (lane, started) in ALL_LANES.iter().zip(&started) {
            assert!(
                wait_for(started, Duration::from_secs(30)),
                "{lane:?} job never started"
            );
        }
        assert_eq!(
            pending.load(Ordering::Acquire),
            ALL_LANES.len(),
            "every lane must count into the request's single bg_pending"
        );

        gate.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(
                Instant::now() < deadline,
                "bg_pending did not drain: shutdown would exit before cleanup finished"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(unix)]
    #[test]
    fn remove_dir_all_recovers_from_readonly_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sandbox");
        let inner = root.join("ro");
        std::fs::create_dir_all(&inner).expect("mkdir");
        std::fs::write(inner.join("file"), b"x").expect("write file");
        // 0555 dir: kernel refuses to unlink children → plain remove_dir_all
        // fails with PermissionDenied.
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

        assert!(
            std::fs::remove_dir_all(&root).is_err(),
            "precondition: plain removal must fail on read-only dir"
        );

        remove_dir_all(&root).expect("chmod-retry removal must succeed");
        assert!(!root.exists(), "directory should be gone");
    }

    /// Round-2 review's red test: the *pre-run* removal of a superseded
    /// execute must decline too — an earlier version generation-guarded only
    /// the post-run cleanup.
    ///
    /// The hazard: execute N queues its pre-run removal on the blocking pool,
    /// then N is cancelled at that very await — the production wedge's phase.
    /// The job is `'static` and runs regardless, whenever the pool gets to it.
    /// Execute N+1 for the same addr meanwhile reclaims the path, creates the
    /// sandbox, and starts writing. When N's job finally runs — the pool was
    /// backlogged, or its thread was descheduled between dequeue and the gate
    /// (std `Mutex` is not FIFO, so "queued behind" has no order guarantee) —
    /// it must see the stale claim and decline, not advance the generation and
    /// remove the successor's live sandbox mid-run. Hence the claim/walk
    /// split: [`generation::claim`] at queue time on the async path, and the
    /// queued walk re-checks at run time.
    #[test]
    fn a_superseded_pre_run_removal_declines_instead_of_deleting_the_new_sandbox() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("mkdir");

        // Execute N's pre-run removal job, constructed exactly as
        // `execute.rs` queues it, at the moment N queues it: the claim is
        // taken inline, the walk is deferred. N is then cancelled; the job
        // stays queued on the pool.
        let claim_n = generation::claim(&sandbox);
        let n_job = {
            let sandbox = sandbox.clone();
            move || generation::remove_stale(&sandbox, claim_n)
        };

        // Execute N+1 reclaims the path and starts writing.
        let claim_n1 = generation::claim(&sandbox);
        assert!(claim_n1 > claim_n, "claims must advance");
        generation::remove_stale(&sandbox, claim_n1).expect("N+1 stale removal");
        std::fs::create_dir_all(&sandbox).expect("recreate N+1");
        std::fs::write(sandbox.join("fresh-output"), b"x").expect("write");

        // The pool finally runs N's job.
        n_job().expect("a superseded pre-run removal reports success without acting");

        assert!(
            sandbox.join("fresh-output").exists(),
            "a superseded execute's pre-run removal must not delete the \
             successor's live sandbox"
        );
    }

    /// A cleanup job from a superseded execute must not touch the path.
    ///
    /// The exact hazard: execute N is cancelled after its cleanup job is
    /// queued (or the job simply has not run yet); execute N+1 reclaims the
    /// path, removes stale bytes, and starts writing. When N's job finally
    /// runs, the generation has moved on and the job must decline — running it
    /// would delete N+1's freshly written outputs.
    #[test]
    fn a_superseded_cleanup_job_declines_instead_of_deleting_the_new_sandbox() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");

        // Execute N claims the path and queues its cleanup with generation N.
        std::fs::create_dir_all(&sandbox).expect("mkdir N");
        let gen_n = generation::claim(&sandbox);
        generation::remove_stale(&sandbox, gen_n).expect("N stale removal");
        std::fs::create_dir_all(&sandbox).expect("recreate N");
        let stale_job = generation::guarded(
            sandbox.clone(),
            gen_n,
            Box::new({
                let sandbox = sandbox.clone();
                move || std::fs::remove_dir_all(&sandbox)
            }),
        );

        // Execute N+1 reclaims the path and writes fresh output.
        let gen_n1 = generation::claim(&sandbox);
        assert!(gen_n1 > gen_n, "generations must advance");
        generation::remove_stale(&sandbox, gen_n1).expect("N+1 stale removal");
        std::fs::create_dir_all(&sandbox).expect("recreate N+1");
        std::fs::write(sandbox.join("fresh-output"), b"x").expect("write");

        // N's straggler runs now — after the reclaim.
        stale_job().expect("a superseded job reports success without acting");
        assert!(
            sandbox.join("fresh-output").exists(),
            "a superseded cleanup job must not delete the newer execute's sandbox"
        );

        // The current generation's own job still cleans up.
        let live_job = generation::guarded(
            sandbox.clone(),
            gen_n1,
            Box::new({
                let sandbox = sandbox.clone();
                move || std::fs::remove_dir_all(&sandbox)
            }),
        );
        live_job().expect("current-generation cleanup runs");
        assert!(
            !sandbox.exists(),
            "the owning generation's cleanup must run"
        );
    }

    /// `remove_stale` queues behind a cleanup walk already in progress on the
    /// same path, instead of racing it.
    #[test]
    fn remove_stale_waits_for_an_in_flight_cleanup_walk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("mkdir");
        let generation_now = generation::claim(&sandbox);
        generation::remove_stale(&sandbox, generation_now).expect("stale removal");
        std::fs::create_dir_all(&sandbox).expect("recreate");

        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let walk = generation::guarded(
            sandbox.clone(),
            generation_now,
            Box::new({
                let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
                let sandbox = sandbox.clone();
                move || {
                    entered.store(true, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    std::fs::remove_dir_all(&sandbox)
                }
            }),
        );
        let walker = std::thread::spawn(walk);
        assert!(
            wait_for(&entered, Duration::from_secs(2)),
            "cleanup walk did not start"
        );

        // The successor claims immediately (inline, never blocked by the
        // walk) but its own removal must queue behind the in-flight walk.
        let claim_next = generation::claim(&sandbox);
        assert!(claim_next > generation_now);
        let reclaimed = Arc::new(AtomicBool::new(false));
        let reclaimer = std::thread::spawn({
            let (sandbox, reclaimed) = (sandbox.clone(), Arc::clone(&reclaimed));
            move || {
                generation::remove_stale(&sandbox, claim_next).expect("reclaim");
                reclaimed.store(true, Ordering::SeqCst);
            }
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !reclaimed.load(Ordering::SeqCst),
            "remove_stale must wait for the in-flight walk, not race it"
        );

        release.store(true, Ordering::SeqCst);
        walker.join().expect("walker").expect("walk result");
        reclaimer.join().expect("reclaimer");
        assert!(
            wait_for(&reclaimed, Duration::from_secs(2)),
            "remove_stale must proceed once the walk finishes"
        );
    }

    /// The gate registry is per-path: one sandbox's walk must not serialize an
    /// unrelated sandbox's reclaim.
    #[test]
    fn generations_are_independent_per_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let gen_a = generation::claim(&a);
        let before_b = generation::current_generation(&b);
        assert_eq!(
            generation::current_generation(&a),
            gen_a,
            "a claim must advance its own path"
        );
        assert_eq!(
            generation::current_generation(&b),
            before_b,
            "and leave other paths untouched"
        );
    }

    /// Spin until `pending` drains to zero — the cleaner ran everything queued.
    fn wait_drained(pending: &PendingCounter, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while pending.load(Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "cleaner did not drain in time");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The three-way contract, failure leg: a *failed* execute leaves its
    /// sandbox on disk, untouched — bridge job dropped unused, no reclaim
    /// queued.
    ///
    /// The failure diagnostic reads the process's last log lines lazily from
    /// the on-disk log when it renders. Folding failure into the drop-reclaim
    /// path made that a race against the cleaner lane: with siblings running
    /// the reclaim landed late and the tail survived; alone (and on the CI
    /// runners) it landed first and a failing target reported its exit status
    /// with no output (`test_failure_surfaces_process_log_tail`, e2e). The
    /// deterministic proof here is the pending counter: it was never bumped,
    /// so nothing was queued and nothing can delete the tree later.
    #[test]
    fn a_failed_execute_leaves_its_sandbox_for_diagnostics() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("mkdir");
        std::fs::write(sandbox.join("log.txt"), b"distinctive-marker-line\n").expect("write");

        let pending = counter();
        let claim = generation::claim(&sandbox);
        let mut teardown = SandboxTeardown::arm(sandbox.clone(), claim, Arc::clone(&pending));
        // Even a bridge job already handed over must be dropped unused — the
        // failure decision outranks it.
        let bridge_ran = Arc::new(AtomicBool::new(false));
        teardown.set_job(Some(Box::new({
            let (bridge_ran, sandbox) = (Arc::clone(&bridge_ran), sandbox.clone());
            move || {
                bridge_ran.store(true, Ordering::SeqCst);
                std::fs::remove_dir_all(&sandbox)
            }
        })));

        teardown.leave_for_diagnostics();

        assert_eq!(
            pending.load(Ordering::Acquire),
            0,
            "a failed target's teardown must enqueue nothing — neither the \
             bridge job nor a reclaim"
        );
        assert!(
            !bridge_ran.load(Ordering::SeqCst),
            "the bridge job must be dropped unused on the failure path"
        );
        assert!(
            sandbox.join("log.txt").exists(),
            "a failed target's sandbox (and its log) must survive for the \
             failure diagnostic"
        );
    }

    /// A cancelled execute's teardown reclaims its half-written sandbox.
    ///
    /// This is the disk-leak fix: under mass fail-fast (the wedge run had
    /// 1,135 and 1,949 *cancellations*) every cancelled execute drops its
    /// `SandboxTeardown`, and each drop must queue a reclaim — `gc` has no
    /// sandbox sweep, so nothing else would ever collect these trees. A
    /// *failed* execute is the deliberate exception: see
    /// `a_failed_execute_leaves_its_sandbox_for_diagnostics`.
    #[test]
    fn a_dropped_teardown_reclaims_its_sandbox() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("mkdir");
        std::fs::write(sandbox.join("half-written"), b"x").expect("write");

        let pending = counter();
        let claim = generation::claim(&sandbox);
        let teardown = SandboxTeardown::arm(sandbox.clone(), claim, Arc::clone(&pending));

        // Cancelled mid-run: dropped without `complete`, no bridge job yet.
        drop(teardown);

        wait_drained(&pending, Duration::from_secs(2));
        assert!(
            !sandbox.exists(),
            "a dropped teardown with a current claim must reclaim the sandbox"
        );
    }

    /// A dropped teardown whose path was reclaimed by a successor declines.
    ///
    /// Pairs with `a_dropped_teardown_reclaims_its_sandbox` and must not be
    /// deleted without it: on its own this would also pass if `Drop` enqueued
    /// *nothing at all* (pending stays 0 and the drain returns immediately).
    /// The sibling is what proves the enqueue happens, so together they pin
    /// "enqueues, and declines when superseded".
    #[test]
    fn a_superseded_teardown_reclaim_declines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");

        let pending = counter();
        let claim_n = generation::claim(&sandbox);
        let teardown = SandboxTeardown::arm(sandbox.clone(), claim_n, Arc::clone(&pending));

        // The successor reclaims and writes before N's drop-reclaim runs.
        let claim_n1 = generation::claim(&sandbox);
        generation::remove_stale(&sandbox, claim_n1).expect("successor stale removal");
        std::fs::create_dir_all(&sandbox).expect("recreate");
        std::fs::write(sandbox.join("fresh-output"), b"x").expect("write");

        drop(teardown);

        wait_drained(&pending, Duration::from_secs(2));
        assert!(
            sandbox.join("fresh-output").exists(),
            "a superseded teardown's reclaim must not touch the successor's sandbox"
        );
    }

    /// `complete` enqueues the bridge job (generation-guarded) exactly once —
    /// consuming the teardown, so no reclaim can follow.
    #[test]
    fn a_completed_teardown_enqueues_the_bridge_job_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).expect("mkdir");

        let pending = counter();
        let claim = generation::claim(&sandbox);
        let mut teardown = SandboxTeardown::arm(sandbox.clone(), claim, Arc::clone(&pending));

        let ran = Arc::new(AtomicBool::new(false));
        teardown.set_job(Some(Box::new({
            let (ran, sandbox) = (Arc::clone(&ran), sandbox.clone());
            move || {
                ran.store(true, Ordering::SeqCst);
                std::fs::remove_dir_all(&sandbox)
            }
        })));
        teardown.complete("test".to_string());

        assert!(
            wait_for(&ran, Duration::from_secs(2)),
            "the bridge job must run on completion"
        );
        wait_drained(&pending, Duration::from_secs(2));
        assert!(
            !sandbox.exists(),
            "the bridge job's removal must have applied"
        );
    }

    /// Completing with no bridge job enqueues nothing — the status quo for
    /// drivers that hand back no cleanup closure.
    #[test]
    fn a_completed_teardown_without_a_job_enqueues_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sandbox = tmp.path().join("sandbox");
        let pending = counter();
        let claim = generation::claim(&sandbox);
        let teardown = SandboxTeardown::arm(sandbox.clone(), claim, Arc::clone(&pending));
        teardown.complete("test".to_string());
        assert_eq!(
            pending.load(Ordering::Acquire),
            0,
            "no job, no queue entry — and no reclaim either, complete consumed it"
        );
    }

    #[test]
    fn enqueue_survives_panicking_job() {
        // Same shape as the NotFound case: a panic shouldn't kill a lane's
        // thread; subsequent jobs still run.
        for lane in ALL_LANES {
            let pending = counter();
            enqueue(
                lane,
                format!("enqueue_survives_panicking_job_panicker {lane:?}"),
                Box::new(|| panic!("boom")),
                Arc::clone(&pending),
            );
            let ran = Arc::new(AtomicBool::new(false));
            let ran_clone = Arc::clone(&ran);
            enqueue(
                lane,
                format!("enqueue_survives_panicking_job_followup {lane:?}"),
                Box::new(move || {
                    ran_clone.store(true, Ordering::SeqCst);
                    Ok(())
                }),
                Arc::clone(&pending),
            );
            assert!(
                wait_for(&ran, Duration::from_secs(5)),
                "{lane:?} stopped processing after panic"
            );
            // A panicking job must still decrement the counter (catch_unwind path).
            let deadline = Instant::now() + Duration::from_secs(5);
            while pending.load(Ordering::Acquire) > 0 {
                assert!(Instant::now() < deadline, "counter leaked after panic");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
