//! Cache garbage collection.
//!
//! Two entry points, both keyed per-addr and serialized through
//! [`ResultLock`](crate::engine::result_lock::ResultLock) so GC never deletes a
//! revision another request/process is reading or rebuilding:
//!
//! - [`Engine::gc_all`] — the `heph gc` sweep. For every `(addr, hashin)` group
//!   in the cache: if the target no longer resolves (`get_spec` →
//!   `TargetNotFoundError`) every revision is dropped; otherwise the target is
//!   trimmed to its `cache.history` newest revisions.
//! - [`Engine::try_trim_after_write`] — the post-write trim. Non-blocking: it
//!   trims the just-written target only if its lock is free, and never deletes
//!   the revision that was just written. Deferred to the end of the request
//!   that wrote the revision (see `RequestState::defer_trim`) — a request holds
//!   a read on every addr it resolved, so a trim run inline could never take
//!   the write lock. The whole request's batch is drained by
//!   [`Engine::run_trim_batch_with_delay`], which retries the contended subset.
//!
//! Revision recency comes from each group's `manifest-v1.borsh`
//! (`created_at_nanos`); the full artifact name list to delete comes from the
//! same manifest.

use crate::engine::Engine;
use crate::engine::error::TargetNotFoundError;
use crate::engine::local_cache::{Existence, MANIFEST_V1};
use crate::engine::request_state::RequestState;
use crate::engine::result_lock::ResultWriteGuard;
use anyhow::{Context, Result};
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::{Addr, parse_addr};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

/// How long a [`Engine::run_trim_batch_with_delay`] waits before its one delayed
/// retry, when an immediate re-probe of the contended subset has already failed.
///
/// **This is a hedge, not a synchronization edge, and it is best-effort.** The
/// batch is submitted by `RequestStateData`'s drop, which flushes the blocking
/// pool's backstop registrations (`hcore::blocking::flush_backstop`) —
/// releasing, per its own contract, the `Waker`s it was holding. Those `Waker`s
/// are what reach the addr's riding cache read: one is an `Arc<hmemoizer::Cell>`
/// whose memoized `mem_locked_result` value *is* that read guard.
///
/// For that shape the flush **is** the release edge, and a synchronous one: the
/// flush drops its last `Arc<Cell>` inline, on the flushing thread, before it
/// returns. What is not guaranteed is that it is the *last* owner. Another live
/// `Arc` — an in-flight remote upload, a task mid-poll on a runtime worker —
/// keeps the read alive, and then the release lands whenever that owner
/// finishes, unordered with respect to the `try_write` this batch is about to
/// attempt. A trim can still be lost, and a lost trim leaves a revision on disk
/// until the next write's trim or the next `heph gc`.
///
/// One owner that used to belong on that list no longer does: an abandoned
/// memoizer cell, retained after its last awaiter was dropped by a `fail_fast`
/// fanout, pinned its riding read indefinitely. `hmemoizer` now evicts a key and
/// drops its in-flight future when the last holder goes (#241), which disarms
/// the backstop registration with it. So the population this hedge is covering
/// is smaller than it was, and it should reach the delayed attempt below less
/// often — which costs nothing extra, because the free re-probe is what absorbs
/// the difference.
///
/// So the delay is sized against the cases the wait can actually recover: a
/// woken task that needs a scheduler hop to reach its next poll, and the tail of
/// a concurrent backstop tick finishing its own wake loop. Both are typically
/// microseconds to low milliseconds — usually shorter still, which is why the
/// batch re-probes *before* sleeping and skips the delay entirely when the flush
/// already did the job — and long-tailed under load, so single-digit
/// milliseconds would be too tight for the tail. The ceiling is that the wait is
/// charged to the cleaner thread that gates process exit, *after* the user has
/// their output, ahead of every sandbox rmdir still queued behind it — so it has
/// to stay well under the ~100ms at which an exit stall becomes noticeable.
///
/// Related to, but deliberately not derived from,
/// `hcore::blocking::WAKE_BACKSTOP` (250ms): that tick is the upper bound on how
/// late a *missed* wake can still arrive, so a delay at or above it would cover
/// strictly more — at ten times the exit cost, on every contended run, to buy a
/// case the flush is already designed to prevent. 25ms is the cost decision, not
/// an independence claim. Raising it towards the tick is the knob if enforcement
/// is ever measured to be losing.
pub(crate) const TRIM_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Serialises the tests that arm a backstop registration and then assert on
/// *which* of a batch's flushes released it.
///
/// `hcore::blocking`'s pending list is process-wide and `flush_backstop` takes
/// all of it, so any other test in this binary that drops a request state with
/// deferred trims can wake — and therefore advance — a registration this one is
/// still counting. Held by every test here that cares, and by the request-state
/// test that measures the batch's wait.
///
/// It cannot cover the flushes that happen inside unrelated `result.rs` tests,
/// so a foreign wake landing inside a microsecond-wide window remains possible.
/// It would make an assertion on the *phase* fail, never a reclamation
/// assertion; the residual is documented rather than engineered away, because
/// the alternative is a lock every request teardown in the crate must take.
#[cfg(test)]
pub(crate) fn backstop_exclusive() -> std::sync::MutexGuard<'static, ()> {
    static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Poisoning is ignored: the guard protects no invariant of its own, and one
    // failing test must not cascade into every later one.
    EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one [`Engine::try_trim_after_write`] attempt did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TrimOutcome {
    /// Nothing further is owed for this target: either it was already within
    /// budget (no lock taken) or the lock was free and the trim ran.
    Settled { removed: usize, bytes: u64 },
    /// `try_write` reported the lock held by another request or process. The
    /// **only** outcome worth retrying — a holder exists, and it may let go.
    Contended,
    /// Something else went wrong and was logged at the site: without the lock,
    /// the pre-count or the `existence` probe that corrects it; an `Err` from
    /// `try_write` itself (a vanished or unwritable lock dir, `EMFILE`,
    /// `ENOLCK`); or, with the lock held, the barrier, the enumerate, or the
    /// trim.
    ///
    /// Never retried. Not because these faults are permanent — `EMFILE` and
    /// `ENOLCK` are exactly the transient ones, and fd exhaustion at the end of a
    /// large build is precisely when a batch runs — but because the retry's only
    /// mechanism is *waiting for a lock holder to let go*, and an `Err` is not
    /// evidence that a holder exists. Waiting buys nothing here, and reporting an
    /// IO fault as contention would send an operator looking for a concurrent
    /// `heph` that is not there.
    Failed,
}

impl TrimOutcome {
    /// Settled having deleted nothing — the within-budget early return.
    pub(crate) const SETTLED_NOTHING: Self = Self::Settled {
        removed: 0,
        bytes: 0,
    };
}

/// What one batch of deferred post-write trims did.
///
/// Logged by the caller. A silent drain is how `cache.history` went unenforced
/// for so long, and these are the states a bare success count collapses into one.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TrimBatchReport {
    /// Targets in the batch.
    pub batch: usize,
    /// Targets contended after the first pass — the size of the subset the batch
    /// re-probed. Not a count of passes.
    pub retried: usize,
    /// Targets still contended after the immediate re-probe, i.e. the subset the
    /// batch actually paid [`TRIM_RETRY_DELAY`] for. Zero here with `retried`
    /// non-zero is the good case: the flush released the guards synchronously and
    /// no delay was charged.
    pub delayed: usize,
    /// Targets still contended after the final attempt. Their `cache.history`
    /// was not enforced by this run.
    pub still_contended: usize,
    /// Targets whose trim failed outright (see [`TrimOutcome::Failed`]), summed
    /// over every pass.
    pub failed: usize,
    /// Revisions actually reclaimed, and their bytes.
    pub removed: usize,
    pub bytes: u64,
    /// Backstop registrations handed back by this batch's flushes, summed.
    ///
    /// The one number that says whether the hedge is *doing* anything: with it at
    /// zero, `still_contended` means the release edge is somewhere this batch
    /// cannot reach, and the delay is being paid for nothing. `flush_backstop`
    /// returns it for exactly this reason. It counts every registration in the
    /// process-wide list, not only this request's.
    pub flushed: usize,
}

/// Outcome of a [`Engine::gc_all`] sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcStats {
    /// Targets whose spec no longer resolves; all their revisions were removed.
    pub orphan_targets_removed: usize,
    /// Cache revisions deleted (orphan drops + history trims).
    pub revisions_removed: usize,
    /// Cache revisions retained on live targets.
    pub revisions_kept: usize,
    /// Total bytes freed (summed from the manifests of deleted revisions).
    pub bytes_removed: u64,
    /// Targets that could not be processed (resolve/delete failed). GC logs each
    /// and keeps going — a single bad target never aborts the sweep.
    pub errored: usize,
    /// Rows pruned from the shared filesystem-walk cache (stale past the TTL or
    /// orphaned because their path no longer exists).
    pub fswalk_rows_removed: usize,
    /// Staged read-only input entries (`<home>/stage/`) reclaimed because their
    /// content hash is no longer referenced by any surviving manifest.
    pub stage_entries_removed: usize,
}

/// Per-target result of a GC pass, accumulated into [`GcStats`].
#[derive(Debug, Default)]
struct TargetOutcome {
    removed: usize,
    kept: usize,
    bytes: u64,
    orphan: bool,
}

/// What phase 1 decided for a target; applied under its lock in phase 2.
#[derive(Debug, Clone, Copy)]
enum Decision {
    /// Spec no longer resolves — drop every revision.
    Orphan,
    /// Live target — keep its `history` newest revisions.
    Trim(u32),
    /// Resolution failed — leave the target untouched (counted as errored).
    Skip,
}

impl Engine {
    /// Delete one cache revision: every artifact named in its manifest, then the
    /// manifest itself. Returns the bytes freed (Σ manifest artifact sizes).
    /// Best-effort if the manifest is missing (still removes the manifest key in
    /// case of a partial write; reports 0 bytes).
    fn gc_entry(&self, addr: &Addr, hashin: &str) -> Result<u64> {
        let mut bytes = 0u64;
        if let Some(manifest) = self.read_manifest(addr, hashin)? {
            for a in &manifest.artifacts {
                // A manifest can name blobs that were never downloaded (a
                // revision mirrored from a remote materializes lazily), so only
                // count bytes actually reclaimed.
                let present = self
                    .local_cache
                    .exists(addr, hashin, &a.name)
                    .with_context(|| format!("probe cached artifact {} for {addr}", a.name))?;
                self.local_cache
                    .delete(addr, hashin, &a.name)
                    .with_context(|| format!("delete cached artifact {} for {addr}", a.name))?;
                if present {
                    bytes = bytes.saturating_add(a.size);
                }
            }
        }
        self.local_cache
            .delete(addr, hashin, MANIFEST_V1)
            .with_context(|| format!("delete manifest for {addr} {hashin}"))?;
        Ok(bytes)
    }

    /// Clear all staged read-only inputs under `<home>/stage/`. Delegates to
    /// [`hdriver_support::stage::clear_stage`] — the staging mechanism and its
    /// teardown live together in `driver-support`. Returns
    /// `(entries_removed, bytes_freed)`.
    fn gc_stage(&self) -> (usize, u64) {
        hdriver_support::stage::clear_stage(&self.home.join("stage"))
    }

    /// Trim `addr`'s revisions to the `keep` newest (by `created_at_nanos`),
    /// deleting the rest. `protect` is never deleted regardless of age. Returns
    /// `(removed, kept, bytes_freed)`.
    ///
    /// Takes `_guard` as proof the caller holds `addr`'s write lock: deleting a
    /// revision out from under a concurrent reader/builder would corrupt its
    /// result. The guard is held by the caller (not acquired here) so the lock
    /// also covers the `get_spec`/enumerate that decided what to trim — closing
    /// the window where a racing build could write a fresh revision between the
    /// decision and the delete.
    fn trim_addr_history(
        &self,
        _guard: &ResultWriteGuard,
        addr: &Addr,
        hashins: &[String],
        keep: u32,
        protect: Option<&str>,
    ) -> Result<(usize, usize, u64)> {
        let mut with_ts: Vec<(&str, i64)> = Vec::with_capacity(hashins.len());
        for hashin in hashins {
            // A revision whose manifest is unreadable sorts oldest (ts 0) so it
            // is the first to be reclaimed.
            let ts = self
                .read_manifest(addr, hashin)?
                .map(|m| m.created_at_nanos)
                .unwrap_or(0);
            with_ts.push((hashin.as_str(), ts));
        }
        // Newest first.
        with_ts.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));

        let keep = keep as usize;
        let mut removed = 0;
        let mut kept = 0;
        let mut bytes = 0u64;
        for (i, (hashin, _)) in with_ts.iter().enumerate() {
            if i < keep || protect == Some(*hashin) {
                kept += 1;
                continue;
            }
            bytes = bytes.saturating_add(
                self.gc_entry(addr, hashin)
                    .with_context(|| format!("trim revision {hashin} of {addr}"))?,
            );
            removed += 1;
        }
        Ok((removed, kept, bytes))
    }

    /// The target's cached revisions, logged under `stage` when the read fails.
    ///
    /// `None` rather than the error: every caller here turns a failure into
    /// [`TrimOutcome::Failed`] after logging, and threading an `anyhow::Error`
    /// through only to drop it invites a caller to propagate one instead — this is
    /// a fire-and-forget background lane.
    fn revisions(&self, addr: &Addr, stage: &'static str) -> Option<Vec<String>> {
        self.local_cache
            .list_target_entries(addr)
            .inspect_err(|e| {
                tracing::debug!(error = %format!("{e:#}"), %addr, stage, "post-write gc enumerate");
            })
            .ok()
    }

    /// Synchronous post-write trim: keep the target's `keep` newest revisions,
    /// always preserving `written_hashin`. Runs only if `addr`'s lock is free —
    /// returns immediately (trimming nothing) when contended, never blocking.
    /// Fire-and-forget from the background cleaner; errors are logged, not
    /// propagated.
    ///
    /// Ordered cheapest-first, mirroring [`gc_apply`](Self::gc_apply): a single
    /// *unlocked* revision count decides whether there is anything to do at all,
    /// and only a target genuinely over budget pays for the write lock, the
    /// write barrier and the per-revision manifest reads. The steady state — a
    /// target rebuilt within its `cache.history` budget — is the common case on
    /// every warm run, and it costs one `list_target_entries`, plus at most one
    /// non-blocking `existence` probe, instead of a lock acquire plus one
    /// manifest read per revision.
    ///
    /// The unlocked count is safe precisely because it can only *skip*: it never
    /// deletes, so it needs no lock, and a revision that lands between the count
    /// and the lock is reclaimed by the next write's trim or by `heph gc`. The
    /// authoritative enumeration is re-taken under the write lock below.
    ///
    /// **The one revision that must not be missing from that count is our own.**
    /// `list_target_entries` is a plain `SELECT DISTINCT` with no
    /// [`wait_if_pending`](crate::engine::local_cache_sqlite) — unlike `exists`,
    /// which waits — so the revision this call was handed can still be sitting in
    /// the sqlite writer's queue and be absent here. Then `pre.len() <= keep`
    /// fires on a target that is genuinely over budget, and the trim returns
    /// having *never asked for the lock*: not contention, not an error, nothing to
    /// retry, and indistinguishable in any log from a target that was legitimately
    /// within budget. That is a systematic, every-run undercount by exactly one —
    /// self-inflicted, unlike another writer's revision landing late. Any other
    /// revision arriving late is somebody else's write, and the paragraph above
    /// covers it.
    ///
    /// So the count is *corrected* for our own write before it is trusted — with
    /// [`existence`](LocalCache::existence), which reports the write-behind queue
    /// rather than blocking on it. A queued-or-committed manifest counts as a
    /// revision, and that is all the budget decision needs. Deliberately not
    /// `exists` here: `exists` parks the caller until the sqlite writer has
    /// drained *everything queued ahead of our entry* — at the end of a large
    /// build, the whole remaining write backlog — and this runs on the single FIFO
    /// cleaner thread that also owes every sandbox rmdir and gates process exit.
    /// Paying that before knowing whether there is anything to delete would charge
    /// every within-budget target for a decision it does not need. The wait stays
    /// where it has always been, inside the lock branch, where the enumeration
    /// that actually chooses what to delete has to be ordered against our write.
    ///
    /// Deliberately `try_write` and not a blocking `write`: the lock is a
    /// cross-process `flock`, so a blocking acquire can wait on another `heph`
    /// arbitrarily long — on the single FIFO cleaner thread that also owes every
    /// sandbox rmdir, on the path that gates exit, and with no runtime to await
    /// the async `ResultLock::write` on anyway. A skip is recoverable; an
    /// unbounded stall of all cleanup is not. [`Engine::run_trim_batch_with_delay`] hedges
    /// the skip instead, with one immediate re-probe and one delayed retry.
    pub(crate) fn try_trim_after_write(
        &self,
        addr: &Addr,
        keep: u32,
        written_hashin: &str,
    ) -> TrimOutcome {
        let Some(pre) = self.revisions(addr, "pre-count") else {
            return TrimOutcome::Failed;
        };
        // Correct the count for our own write only when it cannot already see it.
        // When it can, there is nothing to ask about and a warm run pays a `Vec`
        // scan rather than a cache round-trip.
        let count = if pre.iter().any(|h| h == written_hashin) {
            pre.len()
        } else {
            match self
                .local_cache
                .existence(addr, written_hashin, MANIFEST_V1)
            {
                // Queued or committed, the revision exists as far as a budget
                // decision is concerned. `Queued` is dropped without awaiting: it
                // is the answer, not something to wait on.
                Ok(Existence::Queued(_) | Existence::Committed(true)) => pre.len() + 1,
                // Neither queued nor committed: the write we were handed never
                // landed (the writer thread is gone, or it dropped an oversized
                // entry). Say so rather than silently trimming to a count that is
                // missing a revision — the silence is the bug this guards.
                Ok(Existence::Committed(false)) => {
                    tracing::debug!(
                        %addr,
                        hashin = %written_hashin,
                        "post-write gc: the written revision is neither queued nor committed"
                    );
                    pre.len()
                }
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc existence");
                    return TrimOutcome::Failed;
                }
            }
        };
        // Undercount by another writer is safe (see above): a target at exactly
        // `keep` has nothing to delete anyway. `Settled`, not `Contended` — the
        // lock was never asked for, so there is nothing for a retry to win.
        if count as u32 <= keep {
            return TrimOutcome::SETTLED_NOTHING;
        }

        match self.result_lock().try_write(addr) {
            Ok(Some(guard)) => {
                // Barrier, here and not above: the enumeration that decides *what
                // to delete* must observe our write, and this is the one path that
                // deletes. `exists` is the read that waits —
                // `LocalCacheSQLite::exists` calls `wait_if_pending` on this key,
                // `LocalCacheSpill::exists` evaluates its sqlite primary first, and
                // `LocalCacheMem::writer` invalidates rather than populates, so the
                // mem tier cannot answer from a stale resident entry. Each of those
                // three is pinned by its own test; without them this is not a
                // barrier at all, and the failure is silent.
                if let Err(e) = self.local_cache.exists(addr, written_hashin, MANIFEST_V1) {
                    tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc barrier");
                    return TrimOutcome::Failed;
                }
                // Re-listed under the lock: authoritative, and it may have grown
                // since the unlocked pre-count above.
                let Some(hashins) = self.revisions(addr, "locked") else {
                    return TrimOutcome::Failed;
                };
                match self.trim_addr_history(&guard, addr, &hashins, keep, Some(written_hashin)) {
                    Ok((removed, _kept, bytes)) => TrimOutcome::Settled { removed, bytes },
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc trim");
                        TrimOutcome::Failed
                    }
                }
            }
            // Contended — another request or process holds the lock. Skip
            // without blocking, but say so: a silent skip here is exactly how
            // `cache.history` went unenforced for so long. `debug`, not `warn`,
            // because a concurrent build of the same target is ordinary and this
            // fires per addr; the caller retries once, and anything it still
            // cannot take is reclaimed by the next write's trim or by `heph gc`.
            Ok(None) => {
                tracing::debug!(%addr, "post-write gc contended");
                TrimOutcome::Contended
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), %addr, "post-write gc try_write");
                TrimOutcome::Failed
            }
        }
    }

    /// Drain one request's batch of deferred post-write trims.
    ///
    /// Runs on the background cleaner thread, from `DeferredTrims::drop`. Three
    /// bounded attempts per target and **at most one sleep for the whole batch**:
    ///
    /// 1. one pass over the batch;
    /// 2. a flush, then an *immediate* re-probe of whatever came back contended
    ///    — free, and it is the pass that usually wins, because the flush hands
    ///    back the `Waker` that owns the riding read and drops it inline on this
    ///    thread (see [`TRIM_RETRY_DELAY`]);
    /// 3. only if that still loses: sleep once, flush once more, and try that
    ///    subset a final time.
    ///
    /// Never a loop, and never a blocking `write`: either would trade a
    /// recoverable skip for an unbounded stall of every queued sandbox rmdir on
    /// the thread that gates process exit.
    ///
    /// `delay` is [`TRIM_RETRY_DELAY`] in production, carried in by the request
    /// (`DeferredTrims::retry_delay_nanos`) rather than read from the constant
    /// here — so a test can widen the window it has to release a guard in instead
    /// of racing 25ms on a loaded runner. A flaky test here gets quarantined, and
    /// then the retry has no coverage at all.
    pub(crate) fn run_trim_batch_with_delay(
        &self,
        trims: impl IntoIterator<Item = (Addr, u32, String)>,
        delay: Duration,
    ) -> TrimBatchReport {
        let mut report = TrimBatchReport::default();

        // INVARIANT for every `flush_backstop` below: **no result-lock guard is
        // held on this thread across a flush.** A flush wakes arbitrary `Waker`s
        // and hands their registrations back, which can drop an entire abandoned
        // future graph inline, here — including read guards on addrs this batch
        // is about to lock. That is the point. But it also means a flush must
        // never run underneath one of our own guards: `try_trim_after_write`
        // takes the write lock and drops it before returning, so flushing from
        // inside it would let a woken task's drop path meet a lock we hold.
        // Flushes belong here, between attempts, and nowhere else.
        //
        // This first one is not redundant with the drop site's: that one ran on
        // the *dropping* thread, and this batch is dequeued an unbounded time
        // later. Anything armed in that gap still pins whatever its task holds.
        report.flushed += hcore::blocking::flush_backstop();

        // Consumed lazily; the `Vec` is allocated only if something is contended
        // and is *moved* into, never cloned into. Not pre-sized: the expected
        // subset is a straggler or two, and reserving the whole batch would cost
        // the full footprint on every run with a single one.
        let mut contended: Vec<(Addr, u32, String)> = Vec::new();
        for trim in trims {
            report.batch += 1;
            self.attempt_trim(trim, &mut report, &mut contended);
        }
        if contended.is_empty() {
            // The overwhelmingly common case: no retry, and so no sleep. The
            // delay is charged only to a run that actually lost a lock.
            return report;
        }
        report.retried = contended.len();

        // Flush, then re-probe *before* sleeping. `flush_backstop` releases the
        // `Waker`s it takes, and for the shape that matters here — an
        // `Arc<hmemoizer::Cell>` whose memoized value is the addr's riding read —
        // dropping the last one tears the cell down inline, on this thread,
        // before the call returns. When that was the only owner the guard is
        // already gone by now and the delay would be pure exit latency. One
        // non-blocking `flock` per contended addr to find out is a good trade
        // against 25ms.
        report.flushed += hcore::blocking::flush_backstop();
        let mut delayed: Vec<(Addr, u32, String)> = Vec::new();
        for trim in contended {
            self.attempt_trim(trim, &mut report, &mut delayed);
        }
        if delayed.is_empty() {
            return report;
        }
        report.delayed = delayed.len();

        // Somebody else's release edge, then: give it one bounded wait. The
        // flush after the sleep collects anything that finished during it — the
        // registrations taken above are gone, and a waiter still genuinely
        // pending re-armed from the poll that wake provoked.
        std::thread::sleep(delay);
        report.flushed += hcore::blocking::flush_backstop();

        for (addr, keep, hashin) in delayed {
            match self.trim_contained(&addr, keep, &hashin) {
                TrimOutcome::Settled { removed, bytes } => {
                    report.removed += removed;
                    report.bytes = report.bytes.saturating_add(bytes);
                }
                TrimOutcome::Contended => {
                    report.still_contended += 1;
                    // Only worth printing when it is *not* us: `holder_pid`
                    // reports whoever holds the gateway or last held it, and this
                    // process stamps it on its own acquires, so our own pid here
                    // is the uninformative default rather than a finding.
                    let holder = self.result_lock().holder_pid(&addr);
                    tracing::debug!(
                        %addr,
                        other_holder_pid = ?holder.filter(|p| *p != std::process::id()),
                        "post-write gc still contended after retry; history not enforced this run",
                    );
                }
                TrimOutcome::Failed => report.failed += 1,
            }
        }

        report
    }

    /// One attempt, folding a settled or failed outcome into `report` and
    /// moving a contended one onto `again` for the next pass — moved, not
    /// cloned, so a batch that never contends allocates nothing beyond the
    /// (empty) `Vec`.
    fn attempt_trim(
        &self,
        trim: (Addr, u32, String),
        report: &mut TrimBatchReport,
        again: &mut Vec<(Addr, u32, String)>,
    ) {
        match self.trim_contained(&trim.0, trim.1, &trim.2) {
            TrimOutcome::Settled { removed, bytes } => {
                report.removed += removed;
                report.bytes = report.bytes.saturating_add(bytes);
            }
            TrimOutcome::Contended => again.push(trim),
            TrimOutcome::Failed => report.failed += 1,
        }
    }

    /// One trim with its panic contained.
    ///
    /// The cleaner's own `catch_unwind` wraps the whole job, so an uncontained
    /// panic here would discard every trim after it with nothing to say which
    /// one it was. A panicked trim reports [`TrimOutcome::Failed`], never
    /// `Contended` — retrying it would only panic again, and charge the whole
    /// batch the delay for the privilege.
    fn trim_contained(&self, addr: &Addr, keep: u32, hashin: &str) -> TrimOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.try_trim_after_write(addr, keep, hashin)
        })) {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::error!(%addr, "post-write gc trim panicked, continuing");
                TrimOutcome::Failed
            }
        }
    }

    /// Sweep the whole local cache in two phases.
    ///
    /// **Phase 1 (resolve)** decides each target's fate — `Orphan` (spec no
    /// longer resolves), `Trim(history)`, or `Skip` (resolution failed) — under a
    /// *single shared* request state. Resolution executes shared, uncacheable
    /// deps (e.g. `_golist`, `go.mod`); memoizing across the whole sweep runs
    /// each a handful of times instead of once per dependent target — otherwise
    /// GC would re-create the very (tmp-keyed) revisions it is trying to reclaim.
    /// The read guards resolution takes live in that request state and are
    /// released when it drops at the end of phase 1.
    ///
    /// **Phase 2 (apply)** acquires each target's write lock and trims/deletes.
    /// It does *no* resolution and holds no request state, so a per-addr write
    /// lock can never invert against a read lock the sweep still holds (the
    /// deadlock that a one-state sweep would hit). Targets are independent
    /// (distinct addrs → distinct locks) and processed with bounded concurrency.
    ///
    /// A per-target failure is logged and counted (`GcStats::errored`), never
    /// aborts the sweep, and the JoinSet is always fully drained.
    pub async fn gc_all(self: Arc<Self>, rs: Arc<RequestState>) -> Result<GcStats> {
        let decisions = Arc::clone(&self).gc_resolve_decisions(&rs).await;

        // Targets whose resolution failed are left untouched but still counted.
        let mut stats = GcStats {
            errored: decisions
                .iter()
                .filter(|(_, d)| matches!(d, Decision::Skip))
                .count(),
            ..GcStats::default()
        };

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Addr, Result<TargetOutcome>)> = JoinSet::new();
        for (addr, decision) in decisions {
            let (Decision::Orphan | Decision::Trim(_)) = decision else {
                // Skipped target: count already recorded; advance the explored
                // count without taking its lock.
                emit_gc_target_swept(&rs, 0, 0);
                continue;
            };
            while set.len() >= limit {
                Self::drain_one(&mut set, &rs, &mut stats).await;
            }
            let engine = Arc::clone(&self);
            let crs = rs.clone();
            set.spawn(async move {
                let out = engine.gc_apply(crs, &addr, decision).await;
                (addr, out)
            });
        }
        while !set.is_empty() {
            Self::drain_one(&mut set, &rs, &mut stats).await;
        }

        // Prune the shared filesystem-walk cache: drop rows untouched past the
        // TTL and rows whose path no longer exists. Best-effort — a prune failure
        // never fails the artifact GC.
        let walker = self.walker.clone();
        match hcore::blocking::run(move || walker.prune(hwalk::cached_walker::DEFAULT_TTL, true))
            .await
        {
            Ok(n) => stats.fswalk_rows_removed = n,
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "fswalk prune failed"),
        }

        // Clear staged read-only inputs — a pure cache, re-materialized on
        // demand — respecting each entry's advisory lock.
        let engine = Arc::clone(&self);
        let (stage_removed, stage_bytes) = hcore::blocking::run(move || engine.gc_stage()).await;
        stats.stage_entries_removed = stage_removed;
        stats.bytes_removed = stats.bytes_removed.saturating_add(stage_bytes);

        Ok(stats)
    }

    /// Phase 1: resolve every cached target's [`Decision`] under one shared,
    /// memoizing request state, with bounded concurrency. Resolution failures
    /// become [`Decision::Skip`] (logged), never aborting. The shared state — and
    /// every read lock resolution took — is dropped before this returns, so phase
    /// 2's write locks are safe.
    async fn gc_resolve_decisions(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
    ) -> Vec<(Addr, Decision)> {
        let resolve_rs = self.new_state_full(
            false,
            rs.events_sender(),
            rs.bg_pending(),
            Self::DEFAULT_LOG_TAIL_LINES,
            None,
        );
        // Phase 1 resolves, and resolving executes shared cacheable deps — each
        // of which would otherwise record a post-write trim that fires when this
        // state drops, i.e. concurrently with phase 2's write locks. This sweep
        // is the authoritative trim; it does not need a second one racing it.
        resolve_rs.suppress_deferred_trims();
        let targets = match self.local_cache.list_targets() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "gc: listing cache targets failed");
                return Vec::new();
            }
        };

        let limit = self.max_workers.max(1);
        let mut set: JoinSet<(Addr, Decision)> = JoinSet::new();
        let mut decisions: Vec<(Addr, Decision)> = Vec::new();

        for target in targets {
            let addr_key = match target {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "gc: listing targets failed mid-stream, resolving what was seen");
                    break;
                }
            };
            let addr = match parse_addr(&addr_key) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(addr = %addr_key, error = %format!("{e:#}"), "gc: skip unparseable cache addr");
                    continue;
                }
            };
            while set.len() >= limit {
                push_decision(&mut decisions, set.join_next().await);
            }
            let engine = Arc::clone(&self);
            let rrs = resolve_rs.clone();
            set.spawn(async move {
                let d = engine.gc_decide(rrs, &addr).await;
                (addr, d)
            });
        }
        while !set.is_empty() {
            push_decision(&mut decisions, set.join_next().await);
        }

        decisions
        // `resolve_rs` drops here → releases every read lock resolution took.
    }

    /// Resolve one target's [`Decision`]. Never errors: a resolution failure maps
    /// to [`Decision::Skip`] (logged) so the sweep keeps going.
    async fn gc_decide(self: Arc<Self>, rrs: Arc<RequestState>, addr: &Addr) -> Decision {
        match Arc::clone(&self).get_spec(rrs.clone(), addr).await {
            Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => Decision::Orphan,
            Err(e) => {
                tracing::warn!(%addr, error = %format!("{e:#}"), "gc: get_spec failed, skipping target");
                Decision::Skip
            }
            Ok(_) => match self.get_direct_def(rrs, addr).await {
                Ok(def) => Decision::Trim(def.target_def.cache.history),
                Err(e) => {
                    tracing::warn!(%addr, error = %format!("{e:#}"), "gc: get_def failed, skipping target");
                    Decision::Skip
                }
            },
        }
    }

    /// Phase 2: apply a resolved [`Decision`].
    ///
    /// First does a *cheap, unlocked* revision count. A target that is already
    /// within budget — a live target with `count <= history`, or an orphan with
    /// no entries — has nothing to delete, so it is returned without ever taking
    /// the per-addr write lock. That lock's file open/`unlink`/close churn is the
    /// dominant cost of a steady-state sweep (profiled ~31% of gc CPU when
    /// nothing was freed), so skipping it for already-clean targets is the big
    /// lever. The skip is race-safe: skipping deletes nothing, so no lock is
    /// needed; a revision a build adds after the check is reclaimed next sweep.
    ///
    /// Only when work is actually needed is the write lock acquired
    /// (`acquire_with_notice` surfaces a contended lock as a TUI notice) and the
    /// entry set re-listed under it (authoritative — it may have changed since
    /// the unlocked pre-check).
    async fn gc_apply(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        decision: Decision,
    ) -> Result<TargetOutcome> {
        let pre = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("gc: list entries for {addr}"))?;
        let needs_lock = match decision {
            Decision::Orphan => !pre.is_empty(),
            Decision::Trim(history) => pre.len() as u32 > history,
            Decision::Skip => false,
        };
        if !needs_lock {
            // Nothing to delete — retained as-is, no lock taken.
            let kept = if matches!(decision, Decision::Orphan) {
                0
            } else {
                pre.len()
            };
            return Ok(TargetOutcome {
                removed: 0,
                kept,
                bytes: 0,
                orphan: false,
            });
        }

        let guard = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, rs.ctoken()))
            .await?;

        let hashins = self
            .local_cache
            .list_target_entries(addr)
            .with_context(|| format!("gc: list entries for {addr}"))?;

        // Everything below deletes, and `LocalCache::delete` parks the calling
        // thread until the sqlite writer thread commits its batch. `gc_apply` is a
        // plain `tokio::spawn`ed task and phase 2 runs `max_workers` of them at
        // once, so parking here parks that many runtime workers on a batch commit —
        // taking the reactor, the timer wheel and the TUI with them. Onto the
        // blocking pool, where parking a thread is the contract. The write guard
        // moves in with the job so the lock still spans every delete it covers.
        let engine = Arc::clone(&self);
        let addr_owned = addr.clone();
        match decision {
            Decision::Orphan => {
                let removed = hashins.len();
                let bytes = hcore::blocking::run(move || {
                    // Moves the write lock into the job so it spans every delete
                    // below; without the binding the closure would not capture it
                    // and `gc_apply`'s frame would release it early.
                    let _guard = guard;
                    let mut bytes = 0u64;
                    for hashin in &hashins {
                        bytes = bytes.saturating_add(
                            engine
                                .gc_entry(&addr_owned, hashin)
                                .with_context(|| format!("gc: drop orphan {addr_owned}"))?,
                        );
                    }
                    anyhow::Ok(bytes)
                })
                .await?;
                Ok(TargetOutcome {
                    removed,
                    kept: 0,
                    bytes,
                    orphan: true,
                })
            }
            Decision::Trim(history) => {
                let (removed, kept, bytes) = hcore::blocking::run(move || {
                    engine.trim_addr_history(&guard, &addr_owned, &hashins, history, None)
                })
                .await?;
                Ok(TargetOutcome {
                    removed,
                    kept,
                    bytes,
                    orphan: false,
                })
            }
            // Skipped targets never reach phase 2 (filtered in gc_all).
            Decision::Skip => Ok(TargetOutcome::default()),
        }
    }

    /// Await one finished phase-2 task and fold it into `stats`. Per-target errors
    /// (and task panics) are logged and counted, never propagated, so the sweep
    /// continues. Emits one `GcTargetSwept` per target so the TUI's "targets
    /// explored" count advances even for failures.
    async fn drain_one(
        set: &mut JoinSet<(Addr, Result<TargetOutcome>)>,
        rs: &Arc<RequestState>,
        stats: &mut GcStats,
    ) {
        let Some(joined) = set.join_next().await else {
            return;
        };
        match joined {
            Ok((_, Ok(o))) => {
                stats.revisions_removed += o.removed;
                stats.revisions_kept += o.kept;
                stats.bytes_removed = stats.bytes_removed.saturating_add(o.bytes);
                if o.orphan {
                    stats.orphan_targets_removed += 1;
                }
                emit_gc_target_swept(rs, o.removed, o.bytes);
            }
            Ok((addr, Err(e))) => {
                tracing::warn!(%addr, error = %format!("{e:#}"), "gc: target failed, continuing");
                stats.errored += 1;
                emit_gc_target_swept(rs, 0, 0);
            }
            Err(join_err) => {
                // A panicked task must not take down the sweep.
                tracing::warn!(error = %join_err, "gc: target task panicked, continuing");
                stats.errored += 1;
            }
        }
    }
}

/// Fold one resolved decision into the list, tolerating a panicked resolve task
/// (logged, dropped — the target is simply left untouched in phase 2).
fn push_decision(
    decisions: &mut Vec<(Addr, Decision)>,
    joined: Option<std::result::Result<(Addr, Decision), tokio::task::JoinError>>,
) {
    match joined {
        Some(Ok(entry)) => decisions.push(entry),
        Some(Err(join_err)) => {
            tracing::warn!(error = %join_err, "gc: resolve task panicked, skipping target")
        }
        None => {}
    }
}

/// Emit one `GcTargetSwept` so the TUI's "targets explored" count advances.
fn emit_gc_target_swept(rs: &RequestState, revisions_removed: usize, bytes_removed: u64) {
    rs.emit(crate::engine::event::BuildEventKind::GcTargetSwept {
        revisions_removed,
        bytes_removed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::local_cache::{
        EntryWriter, Existence, LocalCache, MANIFEST_V1, Manifest, ManifestArtifact,
        ManifestArtifactContentType, ManifestArtifactEncoding, ManifestArtifactType, PendingWrite,
        SizedReader, TargetStream,
    };
    use crate::engine::local_cache_test_double::ForwardingCache;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_engine() -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        (Arc::new(engine), dir)
    }

    /// Engine whose durable cache spills any blob over `spill` bytes to the FS
    /// store, so GC tests can exercise reclaiming filesystem-spilled blobs
    /// without writing megabytes.
    fn test_engine_spill(spill: u64) -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            spill_threshold_bytes: spill,
            ..Default::default()
        })
        .expect("engine");
        (Arc::new(engine), dir)
    }

    /// A cache that reproduces the one asymmetry the post-write trim depends on,
    /// deterministically.
    ///
    /// On the real sqlite backend `exists` waits for an in-flight write of its key
    /// (`PendingTracker::wait_if_pending`) and `list_target_entries` — a plain
    /// `SELECT DISTINCT hashin` — does not. So a revision whose manifest write is
    /// still queued is *invisible to an enumeration and visible to a barrier*, and
    /// nothing in a CI log distinguishes a trim that skipped because of that from
    /// a trim that skipped because the target really was within budget.
    ///
    /// `queued` names such revisions: each is filtered out of every enumeration
    /// until an `exists` on its manifest clears it, exactly as a real
    /// `wait_if_pending` would, and is reported by `existence` as
    /// [`Existence::Queued`] — the non-blocking answer the budget decision uses.
    /// Everything else is forwarded to a genuine sqlite cache, so the trim under
    /// test is running against the real backend.
    ///
    /// The *asymmetry itself* — that the real `exists` waits and the real
    /// `list_target_entries` does not — is not taken on trust from this double.
    /// It is pinned directly against `LocalCacheSQLite` by
    /// `queued_write_is_invisible_to_list_target_entries_and_awaited_by_exists`,
    /// and against the tiers above it by `LocalCacheMem` /  `LocalCacheSpill`'s
    /// own delegation tests. Without those, a double like this one would be free
    /// to assert a property the backend had stopped having.
    struct QueuedWriteCache {
        inner: Arc<dyn LocalCache>,
        /// `(addr, hashin)` of every revision whose write is "in flight".
        queued: std::sync::Mutex<Vec<(String, String)>>,
        barrier_reads: Arc<AtomicUsize>,
    }

    impl QueuedWriteCache {
        /// Hide `hashin` from enumerations until it is barriered.
        ///
        /// Consumed by *any* `exists` on that manifest — including the one inside
        /// `write_revision` and the one inside `present`. So setup writes must
        /// happen before `queue`, and `present` probes after any `barrier_reads`
        /// assertion, or the test silently stops testing anything.
        fn queue(&self, addr: &Addr, hashin: &str) {
            self.queued
                .lock()
                .expect("queued")
                .push((addr.format(), hashin.to_string()));
        }

        /// Whether `(addr, hashin)` is still in flight. Observes; never clears.
        fn is_queued(&self, addr: &Addr, hashin: &str) -> bool {
            self.queued
                .lock()
                .expect("queued")
                .iter()
                .any(|(a, h)| *a == addr.format() && h == hashin)
        }

        /// Drop `(addr, hashin)` from the in-flight set, the way a completed
        /// write does. Returns whether it was there.
        fn land(&self, addr: &Addr, hashin: &str) -> bool {
            let mut q = self.queued.lock().expect("queued");
            let before = q.len();
            q.retain(|(a, h)| !(*a == addr.format() && h == hashin));
            q.len() != before
        }
    }

    impl LocalCache for QueuedWriteCache {
        fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader> {
            self.inner.reader(addr, hashin, name)
        }
        fn writer(
            &self,
            addr: &Addr,
            hashin: &str,
            name: &str,
        ) -> anyhow::Result<Box<dyn EntryWriter>> {
            self.inner.writer(addr, hashin, name)
        }
        fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool> {
            if name == MANIFEST_V1 {
                self.barrier_reads.fetch_add(1, Ordering::SeqCst);
                // The wait: once this returns, the write has landed and every
                // later enumeration sees it.
                self.land(addr, hashin);
            }
            self.inner.exists(addr, hashin, name)
        }
        fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Existence> {
            // Reports the queue, never waits on it, and never clears it — the
            // whole point of `existence` is that it settles nothing.
            if name == MANIFEST_V1 && self.is_queued(addr, hashin) {
                return Ok(Existence::Queued(PendingWrite::new(std::future::ready(()))));
            }
            self.inner.existence(addr, hashin, name)
        }
        /// Mirrors *this* layer's `exists`, as the trait requires — not
        /// `inner`'s. The bytes really are in the sqlite cache underneath (the
        /// writes are forwarded), so a blind delegation would answer `true` for
        /// a revision this double is pretending is still in flight, and the
        /// asymmetry the double exists to model would only hold for `exists`
        /// and `existence`. Committed-only, so no `land` and no
        /// `barrier_reads`: this settles nothing and waits for nothing.
        fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool> {
            if name == MANIFEST_V1 && self.is_queued(addr, hashin) {
                return Ok(false);
            }
            self.inner.exists_committed(addr, hashin, name)
        }
        fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()> {
            self.inner.delete(addr, hashin, name)
        }
        fn list_targets(&self) -> anyhow::Result<TargetStream> {
            self.inner.list_targets()
        }
        fn list_target_entries(&self, addr: &Addr) -> anyhow::Result<Vec<String>> {
            let mut entries = self.inner.list_target_entries(addr)?;
            let q = self.queued.lock().expect("queued");
            entries.retain(|e| !q.iter().any(|(a, h)| *a == addr.format() && h == e));
            Ok(entries)
        }
        fn seekable_reader(
            &self,
            addr: &Addr,
            hashin: &str,
            name: &str,
        ) -> anyhow::Result<Option<Box<dyn hcore::hartifactcontent::ReadSeek + Send>>> {
            self.inner.seekable_reader(addr, hashin, name)
        }
        fn file_path(&self, addr: &Addr, hashin: &str, name: &str) -> Option<std::path::PathBuf> {
            self.inner.file_path(addr, hashin, name)
        }
    }

    /// Engine whose cache can hide just-written revisions from enumerations.
    /// `cache.barrier_reads` is the manifest-barrier counter.
    fn test_engine_queued() -> (Arc<Engine>, Arc<QueuedWriteCache>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        let cache = Arc::new(QueuedWriteCache {
            inner: Arc::clone(&engine.local_cache),
            queued: std::sync::Mutex::new(Vec::new()),
            barrier_reads: Arc::new(AtomicUsize::new(0)),
        });
        engine.local_cache = Arc::clone(&cache) as Arc<dyn LocalCache>;
        (Arc::new(engine), cache, dir)
    }

    /// Engine whose cache counts manifest barrier/recency reads. Returns the two
    /// counters alongside it.
    fn test_engine_counting() -> (
        Arc<Engine>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        let barrier_reads = Arc::new(AtomicUsize::new(0));
        let manifest_reads = Arc::new(AtomicUsize::new(0));
        // Counts the two cache round-trips the post-write trim must not make
        // when the target is already inside its history budget: the write
        // barrier (`exists` on the manifest key) and the per-revision recency
        // reads (`reader` on the manifest key, one per revision). Everything
        // else is forwarded verbatim to the real backend, so the target under
        // test is a genuine sqlite cache rather than a stub.
        engine.local_cache = Arc::new(
            ForwardingCache::new(Arc::clone(&engine.local_cache))
                .on_reader({
                    let manifest_reads = Arc::clone(&manifest_reads);
                    move |_, _, name| {
                        if name == MANIFEST_V1 {
                            manifest_reads.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
                .on_exists({
                    let barrier_reads = Arc::clone(&barrier_reads);
                    move |_, _, name| {
                        if name == MANIFEST_V1 {
                            barrier_reads.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }),
        );
        (Arc::new(engine), barrier_reads, manifest_reads, dir)
    }

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), BTreeMap::new())
    }

    /// Write a cache revision with a controlled `created_at` and artifact set,
    /// so recency ordering is deterministic (real writes stamp wall-clock time).
    fn write_revision(
        engine: &Engine,
        addr: &Addr,
        hashin: &str,
        created: i64,
        artifacts: &[&str],
    ) {
        for name in artifacts {
            let mut w = engine
                .local_cache
                .writer(addr, hashin, name)
                .expect("writer");
            w.write_all(b"data").expect("write artifact");
            w.commit().expect("commit artifact");
        }
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: created,
            hashin: hashin.to_string(),
            artifacts: artifacts
                .iter()
                .map(|name| ManifestArtifact {
                    hashout: "ho".to_string(),
                    group: String::new(),
                    name: (*name).to_string(),
                    size: 4,
                    r#type: ManifestArtifactType::Output,
                    content_type: ManifestArtifactContentType::Tar,
                    encoding: ManifestArtifactEncoding::None,
                })
                .collect(),
        };
        let mut w = engine
            .local_cache
            .writer(addr, hashin, MANIFEST_V1)
            .expect("manifest writer");
        borsh::to_writer(&mut w, &manifest).expect("write manifest");
        w.commit().expect("commit manifest");
        // Barrier: ensure the write landed before callers enumerate.
        assert!(
            engine
                .local_cache
                .exists(addr, hashin, MANIFEST_V1)
                .expect("exists")
        );
    }

    fn present(engine: &Engine, addr: &Addr, hashin: &str) -> bool {
        engine
            .local_cache
            .exists(addr, hashin, MANIFEST_V1)
            .expect("exists")
    }

    async fn wlock(engine: &Engine, addr: &Addr) -> ResultWriteGuard {
        engine
            .result_lock()
            .write(addr, &StdCancellationToken::new())
            .await
            .expect("write lock")
    }

    #[tokio::test]
    async fn trim_keeps_newest_and_deletes_artifacts() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["out_x.tar"]);
        write_revision(&engine, &a, "h2", 200, &["out_x.tar"]);
        write_revision(&engine, &a, "h3", 300, &["out_x.tar"]);

        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 1, None)
            .expect("trim");

        assert_eq!((removed, kept), (2, 1));
        // Each trimmed revision held one 4-byte artifact (manifest size).
        assert_eq!(bytes, 8);
        assert!(present(&engine, &a, "h3"), "newest revision kept");
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &a, "h2"));
        // The trimmed revision's artifacts are deleted too, not just the manifest.
        assert!(
            !engine
                .local_cache
                .exists(&a, "h1", "out_x.tar")
                .expect("exists")
        );
    }

    #[tokio::test]
    async fn trim_history_two_keeps_two_newest() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        write_revision(&engine, &a, "h3", 300, &["o.tar"]);

        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, _bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 2, None)
            .expect("trim");

        assert_eq!((removed, kept), (1, 2));
        assert!(present(&engine, &a, "h3"));
        assert!(present(&engine, &a, "h2"));
        assert!(!present(&engine, &a, "h1"), "oldest dropped");
    }

    #[tokio::test]
    async fn trim_protects_named_revision() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "old", 100, &["o.tar"]);
        write_revision(&engine, &a, "new", 300, &["o.tar"]);

        // keep=1 would normally drop "old", but protecting it keeps both.
        let guard = wlock(&engine, &a).await;
        let hashins = engine.local_cache.list_target_entries(&a).expect("hashins");
        let (removed, kept, _bytes) = engine
            .trim_addr_history(&guard, &a, &hashins, 1, Some("old"))
            .expect("trim");

        assert_eq!((removed, kept), (0, 2));
        assert!(present(&engine, &a, "new"));
        assert!(present(&engine, &a, "old"), "protected revision survives");
    }

    #[tokio::test]
    async fn try_trim_after_write_trims_when_lock_free() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        assert_eq!(
            engine.try_trim_after_write(&a, 1, "h2"),
            TrimOutcome::Settled {
                removed: 1,
                bytes: 4
            },
            "the outcome carries what it reclaimed, not just that it ran",
        );

        assert!(present(&engine, &a, "h2"), "just-written revision kept");
        assert!(!present(&engine, &a, "h1"), "stale revision trimmed");
    }

    #[tokio::test]
    async fn try_trim_after_write_skips_when_lock_held() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        // Hold the addr's write lock; the non-blocking trim must skip.
        let ctoken = StdCancellationToken::new();
        let _held = engine
            .result_lock()
            .write(&a, &ctoken)
            .await
            .expect("write lock");

        assert_eq!(
            engine.try_trim_after_write(&a, 1, "h2"),
            TrimOutcome::Contended,
            "a held lock is contention, the one outcome worth retrying",
        );

        assert!(
            present(&engine, &a, "h1"),
            "contended lock → nothing trimmed"
        );
        assert!(present(&engine, &a, "h2"));
    }

    /// The no-op case — a target rebuilt while still inside its history budget —
    /// must cost exactly one unlocked enumeration. No write barrier, no manifest
    /// recency reads: those only exist to decide *what* to delete, and there is
    /// nothing to delete.
    #[tokio::test]
    async fn try_trim_after_write_skips_reads_when_within_budget() {
        let (engine, barrier_reads, manifest_reads, _dir) = test_engine_counting();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        // Setup wrote and barriered its own revisions; only the trim is measured.
        barrier_reads.store(0, Ordering::SeqCst);
        manifest_reads.store(0, Ordering::SeqCst);

        // 2 revisions, history 2 → within budget, nothing to trim. `Settled`,
        // not `Contended`: the lock was never asked for, so a retry has nothing
        // to win and must not be charged the delay for it.
        assert_eq!(
            engine.try_trim_after_write(&a, 2, "h2"),
            TrimOutcome::SETTLED_NOTHING,
        );

        assert_eq!(
            barrier_reads.load(Ordering::SeqCst),
            0,
            "within budget: no post-write barrier read"
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            0,
            "within budget: no per-revision manifest read"
        );
        assert!(present(&engine, &a, "h1"), "nothing trimmed");
        assert!(present(&engine, &a, "h2"));
    }

    /// The mirror of the above: over budget, the trim *does* pay for the
    /// per-revision recency reads. Without this the skip assertion could pass on a
    /// trim that had stopped working entirely.
    ///
    /// Still no barrier read: setup barriered its own writes, so the pre-count
    /// already contains `h2` and there is nothing to order against. The case that
    /// *does* barrier is the test below.
    #[tokio::test]
    async fn try_trim_after_write_reads_manifests_when_over_budget() {
        let (engine, barrier_reads, manifest_reads, _dir) = test_engine_counting();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        barrier_reads.store(0, Ordering::SeqCst);
        manifest_reads.store(0, Ordering::SeqCst);

        engine.try_trim_after_write(&a, 1, "h2");

        assert_eq!(
            barrier_reads.load(Ordering::SeqCst),
            1,
            "over budget: the just-written manifest is barriered once, under the lock"
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            3,
            "over budget: one recency read per revision, plus the artifact list \
             of the one revision actually deleted"
        );
        assert!(!present(&engine, &a, "h1"), "stale revision trimmed");
    }

    /// **A pre-count that cannot see its own revision must not be believed.**
    ///
    /// `list_target_entries` is a plain `SELECT DISTINCT` with no
    /// `wait_if_pending` — unlike `exists`, which waits — so the revision this
    /// trim was handed can still be queued in the sqlite writer and absent from
    /// the count. Then `pre.len() <= keep` fires on a target that is genuinely
    /// over budget and the trim returns having never asked for the lock: not
    /// `Contended`, so #222's retry deliberately never revisits it ("the lock was
    /// never asked for, so there is nothing for a retry to win" — true for a
    /// within-budget target, false for a stale count). `cache.history` silently
    /// goes unenforced for the run, and the log is identical to the legitimate
    /// case.
    ///
    /// Constructed, not raced: [`QueuedWriteCache`] hides `h2` from every
    /// enumeration until the manifest barrier runs, which is exactly what the real
    /// backend does while a write sits in the queue. Drop the `existence`
    /// correction and this fails every time rather than one run in N.
    #[tokio::test]
    async fn try_trim_after_write_counts_a_revision_its_pre_count_cannot_see() {
        let (engine, cache, _dir) = test_engine_queued();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        // `h2` is this run's write and its commit is not yet observable to an
        // unbarriered enumeration. Queued *after* the setup writes, whose own
        // `exists` barriers would consume it.
        cache.queue(&a, "h2");
        cache.barrier_reads.store(0, Ordering::SeqCst);

        // Uncorrected the count is `["h1"]`, `1 <= keep`, and the answer is
        // SETTLED_NOTHING with `h1` still on disk.
        assert_eq!(
            engine.try_trim_after_write(&a, 1, "h2"),
            TrimOutcome::Settled {
                removed: 1,
                bytes: 4
            },
            "a queued revision is still a revision: the target is over budget",
        );
        assert_eq!(
            cache.barrier_reads.load(Ordering::SeqCst),
            1,
            "the budget decision used the non-blocking `existence`; the one \
             barrier is the `exists` under the lock"
        );
        assert!(!present(&engine, &a, "h1"), "the stale revision is trimmed");
        assert!(present(&engine, &a, "h2"), "the just-written one is kept");
    }

    /// A revision that is queued but *within* budget must not be trimmed, and
    /// must not park the cleaner thread finding that out.
    ///
    /// This is the case the earlier shape of this fix mispriced: barriering
    /// before the budget decision made every within-budget target wait on the
    /// sqlite writer's whole backlog, on the thread that gates process exit.
    /// `existence` reports the queue instead of blocking on it.
    #[tokio::test]
    async fn try_trim_after_write_counts_a_queued_revision_without_barriering() {
        let (engine, cache, _dir) = test_engine_queued();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        cache.queue(&a, "h2");
        cache.barrier_reads.store(0, Ordering::SeqCst);

        // 2 revisions (one of them invisible to the enumeration), history 2.
        assert_eq!(
            engine.try_trim_after_write(&a, 2, "h2"),
            TrimOutcome::SETTLED_NOTHING,
            "the queued revision counts, so the target is inside its budget",
        );
        assert_eq!(
            cache.barrier_reads.load(Ordering::SeqCst),
            0,
            "within budget: nothing is deleted, so nothing waits on the writer"
        );
        assert!(present(&engine, &a, "h1"), "nothing trimmed");
    }

    /// Only *our* revision is corrected for. Another writer's in-flight write
    /// stays uncounted — that undercount is the pre-existing, non-systematic one
    /// the unlocked count has always tolerated, and correcting it would mean
    /// waiting on a writer this call knows nothing about.
    #[tokio::test]
    async fn try_trim_after_write_corrects_only_for_its_own_write() {
        let (engine, cache, _dir) = test_engine_queued();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        write_revision(&engine, &a, "h3", 300, &["o.tar"]);
        // Ours, and a foreign writer's.
        cache.queue(&a, "h3");
        cache.queue(&a, "h1");
        cache.barrier_reads.store(0, Ordering::SeqCst);

        // Visible: ["h2"]. Ours (h3) is corrected in, h1 is not → count 2 > 1.
        assert_eq!(
            engine.try_trim_after_write(&a, 1, "h3"),
            TrimOutcome::Settled {
                removed: 1,
                bytes: 4
            },
            "corrected for h3 only; h1 stays invisible and is not counted",
        );
        assert!(present(&engine, &a, "h3"), "the written revision is kept");
    }

    /// A write we were handed that never landed is *said out loud* in the count,
    /// not silently trimmed around. `existence` answering `Committed(false)`
    /// means the writer thread dropped it; the count then genuinely has one fewer
    /// revision, and the trim must behave as though it does.
    #[tokio::test]
    async fn try_trim_after_write_does_not_invent_a_revision_that_never_landed() {
        let (engine, cache, _dir) = test_engine_queued();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        // "h2" was never written at all: not in the enumeration, not queued,
        // not committed.
        cache.barrier_reads.store(0, Ordering::SeqCst);

        assert_eq!(
            engine.try_trim_after_write(&a, 1, "h2"),
            TrimOutcome::SETTLED_NOTHING,
            "one real revision against keep=1: nothing to do",
        );
        assert_eq!(
            cache.barrier_reads.load(Ordering::SeqCst),
            0,
            "no lock, so no barrier"
        );
        assert!(present(&engine, &a, "h1"));
    }

    /// Take `addr`'s write lock synchronously, the way the trim itself asks for
    /// it. Synchronous on purpose: these tests model the cleaner thread, which
    /// has no tokio runtime.
    fn hold_write(engine: &Engine, addr: &Addr) -> ResultWriteGuard {
        engine
            .result_lock()
            .try_write(addr)
            .expect("try_write")
            .expect("lock must be free")
    }

    /// Two revisions of `name`, the older one over any `keep = 1` budget.
    fn two_revisions(engine: &Engine, name: &str) -> Addr {
        let a = addr(name);
        write_revision(engine, &a, "h1", 100, &["o.tar"]);
        write_revision(engine, &a, "h2", 200, &["o.tar"]);
        a
    }

    /// **The point of the retry.** A target whose write lock is held when the
    /// batch first reaches it must still be trimmed once the guard lands.
    ///
    /// Constructed, not raced. Batch order is load-bearing: `busy` is probed
    /// first and `sentinel` second, so `sentinel`'s stale revision disappearing
    /// *proves* the first pass already asked for `busy`'s lock and lost it. Only
    /// then is the guard released, inside the (deliberately wide) delay. With no
    /// retry there is no later attempt and `busy` keeps its stale revision.
    ///
    /// That ordering is a property of the `Vec` this test passes, not of
    /// production — `DeferredTrims` hands over an `FxHashMap` whose iteration
    /// order is arbitrary.
    ///
    /// `thread::scope` rather than a detached `spawn`: on an assertion failure
    /// the scope still joins the worker, so it cannot outlive the `TempDir` it is
    /// deleting out of.
    #[test]
    fn trim_batch_retries_the_contended_subset_and_reclaims() {
        let (engine, _dir) = test_engine();
        let busy = two_revisions(&engine, "busy");
        let sentinel = two_revisions(&engine, "sentinel");

        let mut held = Some(hold_write(&engine, &busy));
        let batch = vec![
            (busy.clone(), 1, "h2".to_string()),
            (sentinel.clone(), 1, "h2".to_string()),
        ];

        let report = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                // Wide enough that releasing the guard below is never a race
                // against the clock; the production constant is banded
                // separately.
                engine.run_trim_batch_with_delay(batch, Duration::from_secs(2))
            });

            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while present(&engine, &sentinel, "h1") {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the first pass never reached the sentinel",
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                present(&engine, &busy, "h1"),
                "the first pass must have lost the contended target's lock",
            );

            drop(held.take());
            worker.join().expect("trim batch thread")
        });

        assert!(
            !present(&engine, &busy, "h1"),
            "a later attempt must reclaim the revision the first pass could not",
        );
        assert!(
            present(&engine, &busy, "h2"),
            "the just-written revision is still protected on the retry",
        );
        assert_eq!(
            (report.batch, report.retried, report.still_contended),
            (2, 1, 0),
            "exactly the contended target was retried, and it succeeded: {report:?}",
        );
        assert_eq!(
            report.removed, 2,
            "both stale revisions reclaimed: {report:?}"
        );
    }

    /// The batch lane, with a revision the enumeration cannot see — the shape
    /// `e2e::cache_history_is_enforced_by_the_end_of_the_run` actually runs.
    ///
    /// Every other assertion about the invisible-revision case calls
    /// `try_trim_after_write` directly. This one goes through
    /// `run_trim_batch_with_delay`, which is what `DeferredTrims::drop` submits
    /// and what the flaking test exercises: without the `existence` correction the
    /// batch reports `removed: 0` and the run ends with `cache.history`
    /// unenforced, exactly as CI sees it.
    #[test]
    fn trim_batch_reclaims_a_target_whose_revision_is_still_queued() {
        let (engine, cache, _dir) = test_engine_queued();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        // Queued after the setup writes, whose `exists` barriers would consume it.
        cache.queue(&a, "h2");
        cache.barrier_reads.store(0, Ordering::SeqCst);

        let report = engine.run_trim_batch_with_delay(
            vec![(a.clone(), 1, "h2".to_string())],
            Duration::from_millis(1),
        );

        assert_eq!(
            (
                report.batch,
                report.removed,
                report.still_contended,
                report.failed
            ),
            (1, 1, 0, 0),
            "the batch reclaimed the stale revision on its first pass: {report:?}",
        );
        assert_eq!(
            report.retried, 0,
            "an invisible revision is not contention and must not spend the \
             batch's one delay: {report:?}",
        );
        assert_eq!(
            cache.barrier_reads.load(Ordering::SeqCst),
            1,
            "barriered once, under the lock, on the single attempt that ran"
        );
        assert!(!present(&engine, &a, "h1"), "the stale revision is trimmed");
        assert!(present(&engine, &a, "h2"), "the just-written one is kept");
    }

    /// The retry re-enumerates under the lock it newly acquired rather than
    /// reusing anything decided before the delay, and it still protects the
    /// revision the request wrote.
    ///
    /// `keep = 1` is what makes that second half load-bearing: a revision written
    /// during the delay sorts newest and takes the only budgeted slot, so the
    /// recorded `written_hashin` survives *only* because it is passed as
    /// `protect`. Dropping `protect` deletes a revision the request is still
    /// handing out.
    #[test]
    fn trim_batch_retry_sees_revisions_written_during_the_delay() {
        let (engine, _dir) = test_engine();
        let busy = two_revisions(&engine, "busy");
        let sentinel = two_revisions(&engine, "sentinel");

        let mut held = Some(hold_write(&engine, &busy));
        let batch = vec![
            (busy.clone(), 1, "h2".to_string()),
            (sentinel.clone(), 1, "h2".to_string()),
        ];

        let report = std::thread::scope(|scope| {
            let worker =
                scope.spawn(|| engine.run_trim_batch_with_delay(batch, Duration::from_secs(2)));

            // Same ordered-batch signal as above. Watched through the cache
            // rather than through the lock — asking for the sentinel's lock here
            // would contend the pass under test for it.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while present(&engine, &sentinel, "h1") {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the first pass never reached the sentinel",
                );
                std::thread::sleep(Duration::from_millis(2));
            }

            // A third party lands a newer revision while the batch waits.
            write_revision(&engine, &busy, "h3", 300, &["o.tar"]);
            drop(held.take());
            worker.join().expect("trim batch thread")
        });

        assert_eq!(report.retried, 1, "{report:?}");
        assert_eq!(report.still_contended, 0, "{report:?}");
        assert!(
            present(&engine, &busy, "h3"),
            "a revision written during the delay is enumerated and kept as newest",
        );
        assert!(
            present(&engine, &busy, "h2"),
            "the recorded written revision is protected even though it is no longer newest",
        );
        assert!(
            !present(&engine, &busy, "h1"),
            "the oldest revision is still reclaimed",
        );
    }

    /// A batch that loses nothing must not pay the delay. This is the common
    /// case — every clean run — so the cost has to be zero, not small.
    #[test]
    fn trim_batch_pays_nothing_when_nothing_is_contended() {
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "a");
        let b = two_revisions(&engine, "b");

        // A delay this long is never survivable if the delayed pass is entered.
        let delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let report = engine.run_trim_batch_with_delay(
            vec![
                (a.clone(), 1, "h2".to_string()),
                (b.clone(), 1, "h2".to_string()),
            ],
            delay,
        );

        assert!(
            started.elapsed() < delay,
            "an uncontended batch must not sleep",
        );
        assert_eq!(
            (report.batch, report.retried, report.delayed, report.failed),
            (2, 0, 0, 0),
            "{report:?}",
        );
        assert_eq!((report.removed, report.bytes), (2, 8), "{report:?}");
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &b, "h1"));
    }

    /// An empty batch costs nothing at all. Pinned here rather than relying on
    /// `DeferredTrims::drop`'s own early return, which is a different guard in a
    /// different file.
    #[test]
    fn trim_batch_is_free_when_empty() {
        let (engine, _dir) = test_engine();
        let delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let report = engine.run_trim_batch_with_delay(Vec::new(), delay);
        assert!(started.elapsed() < delay);
        assert_eq!(
            (report.batch, report.retried, report.delayed, report.removed),
            (0, 0, 0, 0),
            "{report:?}",
        );
    }

    /// One delayed attempt, not a loop. A target whose lock is held for the whole
    /// batch is given up on — and the elapsed time proves it was given up after
    /// exactly one wait, which the counts alone cannot: a three-attempt loop
    /// reports identical counts.
    #[test]
    fn trim_batch_gives_up_after_a_single_delay() {
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "a");
        let b = two_revisions(&engine, "b");

        let _ha = hold_write(&engine, &a);
        let _hb = hold_write(&engine, &b);

        let delay = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let report = engine.run_trim_batch_with_delay(
            vec![
                (a.clone(), 1, "h2".to_string()),
                (b.clone(), 1, "h2".to_string()),
            ],
            delay,
        );
        let elapsed = started.elapsed();

        assert_eq!(
            (
                report.batch,
                report.retried,
                report.delayed,
                report.still_contended
            ),
            (2, 2, 2, 2),
            "the counts are per *target*, and both stayed contended throughout: {report:?}",
        );
        assert!(
            elapsed >= delay,
            "the delayed pass must have waited: {elapsed:?}"
        );
        assert!(
            elapsed < delay * 2,
            "a second wait means this is a loop, not one delayed attempt: {elapsed:?}",
        );
        assert!(
            present(&engine, &a, "h1"),
            "nothing trimmed under contention"
        );
        assert!(present(&engine, &b, "h1"));
        assert_eq!(report.removed, 0, "{report:?}");
    }

    /// A trim that fails while *holding* the lock is not contention: waiting
    /// cannot help, and charging the batch a delay for it would turn a permanent
    /// fault into a per-run cost that reads as flakiness.
    #[test]
    fn trim_batch_does_not_retry_a_failure() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        // An unreadable manifest makes `trim_addr_history` fail with the lock
        // held — `Failed`, not `Contended`.
        {
            let mut w = engine
                .local_cache
                .writer(&a, "h1", MANIFEST_V1)
                .expect("writer");
            w.write_all(b"not a valid borsh manifest").expect("write");
            w.commit().expect("commit");
        }
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        let delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let report =
            engine.run_trim_batch_with_delay(vec![(a.clone(), 1, "h2".to_string())], delay);

        assert!(
            started.elapsed() < delay,
            "a failure must not charge the batch the retry delay",
        );
        assert_eq!(
            (report.batch, report.retried, report.failed),
            (1, 0, 1),
            "{report:?}",
        );
    }

    /// The same for a lock that cannot be *asked for* at all. An `Err` from
    /// `try_write` is not evidence that a holder exists, so there is nothing for
    /// a wait to win — the batch must charge no delay and report `failed`, not
    /// `still_contended`, so an operator is not sent looking for a concurrent
    /// `heph` that is not there.
    ///
    /// Provoked by replacing the lock directory with a regular file, so creating
    /// the per-addr lock file fails with `ENOTDIR` — which, unlike a permission
    /// bit, also holds when the suite runs as root.
    #[test]
    fn trim_batch_does_not_retry_an_unaskable_lock() {
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "t");

        let lock_dir = engine.home.join("lock");
        std::fs::remove_dir_all(&lock_dir).expect("remove lock dir");
        std::fs::write(&lock_dir, b"not a directory").expect("write file over lock dir");

        let delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let report =
            engine.run_trim_batch_with_delay(vec![(a.clone(), 1, "h2".to_string())], delay);

        assert!(
            started.elapsed() < delay,
            "an unaskable lock must not charge the batch the retry delay",
        );
        assert_eq!(
            (
                report.batch,
                report.retried,
                report.still_contended,
                report.failed
            ),
            (1, 0, 0, 1),
            "{report:?}",
        );
    }

    /// A waker that releases a held write guard on its `nth` wake, re-arming
    /// itself on every earlier one — the shape `hcore::blocking`'s contract
    /// requires of a waiter that is still pending.
    ///
    /// This is the whole mechanism the batch's flushes exist for, in miniature:
    /// `flush_backstop` hands back the `Waker`s it holds, and dropping one can
    /// release a cache read guard inline on the flushing thread. Choosing which
    /// wake releases is what lets a test name *which* flush it is pinning.
    ///
    /// **Only wakes arriving on `only_from` are counted.** `hcore::blocking`'s
    /// pending list is process-wide, so the backstop tick and every other test in
    /// this binary that tears down a request state can wake this registration
    /// too. An uncounted foreign wake would advance the phase and release the
    /// guard a flush early — which does not break reclamation, but does make an
    /// assertion about *which* flush released it fail for an unrelated reason
    /// (observed, before this filter existed). `flush_backstop` wakes inline on
    /// the flushing thread and the tick has a thread of its own, so the thread id
    /// is exactly the discriminator: a wake from anywhere else re-arms and is
    /// ignored.
    struct ReleasingWaker {
        remaining: std::sync::atomic::AtomicUsize,
        guard: std::sync::Mutex<Option<ResultWriteGuard>>,
        backstop: hcore::blocking::Backstop,
        only_from: std::thread::ThreadId,
    }

    impl ReleasingWaker {
        /// Arm a waker that releases `guard` on the `nth` wake delivered by the
        /// calling thread — which must also be the thread that runs the batch.
        fn arm(guard: ResultWriteGuard, nth: usize) -> Arc<Self> {
            let me = Arc::new(Self {
                remaining: std::sync::atomic::AtomicUsize::new(nth),
                guard: std::sync::Mutex::new(Some(guard)),
                backstop: hcore::blocking::Backstop::new(),
                only_from: std::thread::current().id(),
            });
            Self::rearm(&me);
            me
        }

        fn released(&self) -> bool {
            self.guard.lock().expect("guard lock").is_none()
        }

        fn rearm(me: &Arc<Self>) {
            me.backstop.arm(&futures::task::waker(Arc::clone(me)));
        }
    }

    impl futures::task::ArcWake for ReleasingWaker {
        fn wake_by_ref(me: &Arc<Self>) {
            use std::sync::atomic::Ordering;
            if std::thread::current().id() != me.only_from {
                // Somebody else's flush, or the tick. It took our registration;
                // put it back, and do not advance the phase.
                Self::rearm(me);
                return;
            }
            let before = me
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                    Some(v.saturating_sub(1))
                })
                .unwrap_or(0);
            if before <= 1 {
                // The wait is over: release, and deliberately do not re-arm.
                drop(me.guard.lock().expect("guard lock").take());
            } else {
                Self::rearm(me);
            }
        }
    }

    /// The batch flushes *before* its first pass.
    ///
    /// Not redundant with the flush `DeferredTrims::drop` already does: that one
    /// runs on the dropping thread, and the batch is dequeued an unbounded time
    /// later. A registration armed in that gap still pins whatever its task
    /// holds — here, literally the write guard the trim needs.
    ///
    /// Pinned by `retried == 0`: with the flush the guard is gone before the
    /// first pass, so nothing is ever contended. Remove it and the first pass
    /// loses, and the *second* flush recovers it — same reclamation, different
    /// count.
    #[test]
    fn trim_batch_flushes_before_the_first_pass() {
        let _exclusive = crate::engine::gc::backstop_exclusive();
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "t");

        let waker = ReleasingWaker::arm(hold_write(&engine, &a), 1);
        let report = engine
            .run_trim_batch_with_delay(vec![(a.clone(), 1, "h2".to_string())], Duration::ZERO);

        assert!(waker.released(), "the flush must have woken our waker");
        assert!(report.flushed >= 1, "the batch must flush: {report:?}");
        assert_eq!(
            (report.retried, report.removed),
            (0, 1),
            "the pre-pass flush released the guard before anything was contended: {report:?}",
        );
    }

    /// The batch flushes again before it *re-probes* the contended subset, and
    /// that re-probe is what usually spares the delay.
    ///
    /// Pinned by `delayed == 0`: the guard is released by the second flush, the
    /// immediate re-probe wins, and no wait is charged. Remove that flush and the
    /// re-probe loses, the batch sleeps, and the post-sleep flush recovers it —
    /// same reclamation, but the run paid `TRIM_RETRY_DELAY` for nothing.
    #[test]
    fn trim_batch_reprobes_after_a_flush_before_paying_the_delay() {
        let _exclusive = crate::engine::gc::backstop_exclusive();
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "t");

        // Releases on the *second* wake, so the first flush cannot do it.
        let waker = ReleasingWaker::arm(hold_write(&engine, &a), 2);
        let delay = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let report =
            engine.run_trim_batch_with_delay(vec![(a.clone(), 1, "h2".to_string())], delay);

        assert!(waker.released());
        assert!(
            started.elapsed() < delay,
            "the re-probe must land before any wait is charged",
        );
        assert_eq!(
            (report.retried, report.delayed, report.removed),
            (1, 0, 1),
            "contended once, then reclaimed by the free re-probe: {report:?}",
        );
    }

    /// And the batch flushes once more *after* the wait, before its final
    /// attempt.
    ///
    /// The pre-sleep flush took every registration it found; anything that
    /// re-armed during the wait — a waiter that was genuinely still pending — is
    /// only handed back by a flush on the far side of it. Without this one the
    /// batch sleeps and then asks for a lock whose owner it never woke.
    ///
    /// Pinned by `still_contended == 0` with the guard released on the third
    /// wake, which only the third flush can deliver.
    #[test]
    fn trim_batch_flushes_again_after_the_delay() {
        let _exclusive = crate::engine::gc::backstop_exclusive();
        let (engine, _dir) = test_engine();
        let a = two_revisions(&engine, "t");

        let waker = ReleasingWaker::arm(hold_write(&engine, &a), 3);
        let report = engine.run_trim_batch_with_delay(
            vec![(a.clone(), 1, "h2".to_string())],
            Duration::from_millis(20),
        );

        assert!(waker.released());
        assert_eq!(
            (report.retried, report.delayed, report.still_contended),
            (1, 1, 0),
            "the post-wait flush released the guard for the final attempt: {report:?}",
        );
        assert_eq!(report.removed, 1, "{report:?}");
        assert!(!present(&engine, &a, "h1"));
    }

    /// The delay is exit latency paid by every run whose contention outlives a
    /// flush, so it is held to a band rather than left to drift. Below ~10ms it
    /// cannot cover a scheduler hop on a loaded runner and the wait is theatre;
    /// above ~50ms it is a visible stall after the user already has their output,
    /// on the thread that also owes every queued sandbox rmdir.
    #[test]
    fn trim_retry_delay_stays_in_its_band() {
        assert!(
            (Duration::from_millis(10)..=Duration::from_millis(50)).contains(&TRIM_RETRY_DELAY),
            "TRIM_RETRY_DELAY is {TRIM_RETRY_DELAY:?}; re-justify it before moving it out of band",
        );
    }

    #[tokio::test]
    async fn gc_all_drops_orphan_target_entries() {
        let (engine, _dir) = test_engine();
        // No provider knows this addr → get_spec yields TargetNotFoundError.
        let a = addr("ghost");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        assert_eq!(stats.orphan_targets_removed, 1);
        assert_eq!(stats.revisions_removed, 2);
        assert_eq!(stats.revisions_kept, 0);
        // Two revisions × one 4-byte artifact each.
        assert_eq!(stats.bytes_removed, 8);
        assert!(!present(&engine, &a, "h1"));
        assert!(!present(&engine, &a, "h2"));
    }

    #[tokio::test]
    async fn gc_all_processes_many_targets_concurrently() {
        let (engine, _dir) = test_engine();
        for i in 0..8 {
            let a = addr(&format!("ghost{i}"));
            write_revision(&engine, &a, "h1", 100, &["o.tar"]);
            write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        }

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        // Every target is visited and reclaimed regardless of fan-out order.
        assert_eq!(stats.orphan_targets_removed, 8);
        assert_eq!(stats.revisions_removed, 16);
        assert_eq!(stats.errored, 0);
    }

    #[tokio::test]
    async fn gc_all_keeps_going_when_a_target_fails() {
        let (engine, _dir) = test_engine();
        // Healthy orphan: must still be reclaimed.
        let good = addr("good");
        write_revision(&engine, &good, "h1", 100, &["o.tar"]);

        // Broken orphan: a corrupt manifest makes its deletion fail. The sweep
        // must record the error and carry on, not abort.
        let bad = addr("bad");
        {
            let mut w = engine
                .local_cache
                .writer(&bad, "h1", MANIFEST_V1)
                .expect("writer");
            w.write_all(b"not a valid borsh manifest").expect("write");
            w.commit().expect("commit");
            assert!(
                engine
                    .local_cache
                    .exists(&bad, "h1", MANIFEST_V1)
                    .expect("exists")
            );
        }

        let rs = engine.new_state();
        let stats = Arc::clone(&engine)
            .gc_all(rs)
            .await
            .expect("gc_all completes despite a failing target");

        assert!(
            stats.errored >= 1,
            "expected the bad target counted: {stats:?}"
        );
        assert!(
            !present(&engine, &good, "h1"),
            "healthy orphan reclaimed despite the other target failing"
        );
    }

    /// End-to-end: a revision whose artifact spilled to the FS blob store is
    /// fully reclaimed by an orphan sweep — the large blob is no longer readable
    /// through the cache after GC, proving `delete` reached the FS backend.
    #[tokio::test]
    async fn gc_reclaims_fs_spilled_blob() {
        // Threshold 16B; the artifact below is well over it, so it spills to FS.
        let (engine, _dir) = test_engine_spill(16);
        let a = addr("ghost"); // unknown to any provider → orphan on sweep
        let big = vec![0xABu8; 4096];

        let name = "out_big.tar";
        {
            let mut w = engine.local_cache.writer(&a, "h1", name).expect("writer");
            w.write_all(&big).expect("write blob");
            w.commit().expect("commit blob");
        }
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: a.format(),
            created_at_nanos: 100,
            hashin: "h1".to_string(),
            artifacts: vec![ManifestArtifact {
                hashout: "ho".to_string(),
                group: String::new(),
                name: name.to_string(),
                size: big.len() as u64,
                r#type: ManifestArtifactType::Output,
                content_type: ManifestArtifactContentType::Tar,
                encoding: ManifestArtifactEncoding::None,
            }],
        };
        {
            let mut w = engine
                .local_cache
                .writer(&a, "h1", MANIFEST_V1)
                .expect("manifest writer");
            borsh::to_writer(&mut w, &manifest).expect("write manifest");
            w.commit().expect("commit manifest");
        }
        // Barrier + precondition: both the spilled blob and the manifest must
        // have landed before GC. `gc_all` enumerates targets via `list_targets`
        // on a fresh read connection that does *not* wait on in-flight writes, so
        // without barriering the manifest write the sweep can observe zero
        // targets and reclaim nothing (raced on linux).
        assert!(engine.local_cache.exists(&a, "h1", name).expect("exists"));
        assert!(present(&engine, &a, "h1"), "manifest landed before GC");

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        assert_eq!(stats.orphan_targets_removed, 1);
        assert_eq!(stats.revisions_removed, 1);
        assert_eq!(stats.bytes_removed, big.len() as u64);
        // The FS-spilled blob and its manifest are gone from the cache.
        assert!(!engine.local_cache.exists(&a, "h1", name).expect("exists"));
        assert!(!present(&engine, &a, "h1"));
    }

    /// Create a stage entry `<home>/stage/<group>/<hash>/blob` plus its
    /// `<hash>.ready` witness, returning the entry dir.
    fn stage_entry(engine: &Engine, group: &str, hash: &str) -> std::path::PathBuf {
        let gdir = engine.home.join("stage").join(group);
        let entry = gdir.join(hash);
        std::fs::create_dir_all(&entry).expect("mkdir stage entry");
        std::fs::write(entry.join("blob"), b"staged-bytes").expect("write blob");
        std::fs::write(gdir.join(format!("{hash}.ready")), b"").expect("ready");
        entry
    }

    #[tokio::test]
    async fn gc_all_clears_stage() {
        let (engine, _dir) = test_engine();
        let staged = stage_entry(&engine, "__pkg_ghost", "abcd");

        let rs = engine.new_state();
        let stats = Arc::clone(&engine).gc_all(rs).await.expect("gc_all");

        assert_eq!(stats.stage_entries_removed, 1);
        assert!(!staged.exists(), "stage cleared by sweep");
    }

    #[tokio::test]
    async fn gc_apply_skips_when_within_history() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);

        // 1 revision, history 2 → nothing to trim. The lock is never taken; the
        // revision is retained.
        let rs = engine.new_state();
        let out = Arc::clone(&engine)
            .gc_apply(rs, &a, Decision::Trim(2))
            .await
            .expect("apply");

        assert_eq!((out.removed, out.kept), (0, 1));
        assert!(present(&engine, &a, "h1"), "revision within budget kept");
    }

    #[tokio::test]
    async fn gc_apply_trims_when_over_history() {
        let (engine, _dir) = test_engine();
        let a = addr("t");
        write_revision(&engine, &a, "h1", 100, &["o.tar"]);
        write_revision(&engine, &a, "h2", 200, &["o.tar"]);
        write_revision(&engine, &a, "h3", 300, &["o.tar"]);

        // 3 revisions, history 1 → over budget, lock taken, trims to newest.
        let rs = engine.new_state();
        let out = Arc::clone(&engine)
            .gc_apply(rs, &a, Decision::Trim(1))
            .await
            .expect("apply");

        assert_eq!((out.removed, out.kept), (2, 1));
        assert!(present(&engine, &a, "h3"), "newest kept");
        assert!(!present(&engine, &a, "h1"));
    }
}
