//! Per-request telemetry counters.
//!
//! A lock-free aggregator threaded through `RequestState`. The engine bumps it
//! from the same chokepoint that emits build events ([`RequestState::emit`])
//! plus one explicit artifact-record call per resolved target. The reporting
//! command reads a [`TelemetrySnapshot`] at the end of a run and ships it to
//! PostHog. Collection is always cheap (relaxed atomics); the opt-out only gates
//! whether the snapshot is *sent*, never whether it is gathered.
//!
//! Artifact sizes feed a fixed power-of-two **histogram** (65 atomic buckets,
//! constant memory regardless of artifact count) rather than a growing list, so
//! the p99 is approximate (within a factor of 2) but the collector never holds
//! more than a few hundred bytes even for a build with millions of artifacts.
//!
//! [`RequestState::emit`]: RequestState::emit

use std::sync::atomic::{AtomicU64, Ordering};

use hcore::events::BuildEventKind;

/// One bucket per possible bit-length of a `u64` (0..=64). Bucket `i` (`i >= 1`)
/// holds values in `[2^(i-1), 2^i - 1]`; bucket `0` holds value `0`.
const HISTOGRAM_BUCKETS: usize = 65;

/// Histogram bucket for `v`: its bit length (`0` for `v == 0`). Bucket `i >= 1`
/// covers `[2^(i-1), 2^i - 1]`.
fn bucket_index(v: u64) -> usize {
    (u64::BITS - v.leading_zeros()) as usize
}

/// Inclusive upper edge of bucket `i`, reported as the percentile estimate
/// (conservative — never undercounts a large tail). Saturates for the top bucket
/// so `1 << 64` never overflows.
fn bucket_upper(i: usize) -> u64 {
    match i {
        0 => 0,
        _ => (1u64 << i).wrapping_sub(1),
    }
}

/// Fixed-memory, lock-free power-of-two histogram. Records in O(1), reports an
/// approximate (within a factor of 2) percentile. ~520 bytes regardless of how
/// many values are recorded — bounds memory on builds with millions of samples.
#[derive(Debug)]
struct AtomicHistogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
}

impl AtomicHistogram {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; HISTOGRAM_BUCKETS],
        }
    }

    fn record(&self, v: u64) {
        if let Some(b) = self.buckets.get(bucket_index(v)) {
            b.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Approximate nearest-rank 99th percentile: walk buckets low→high until the
    /// cumulative count reaches the 99th-rank threshold, report that bucket's
    /// upper edge. `0` for an empty histogram.
    fn percentile_99(&self) -> u64 {
        let counts: [u64; HISTOGRAM_BUCKETS] =
            std::array::from_fn(|i| self.buckets.get(i).map_or(0, |b| b.load(Ordering::Relaxed)));
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        // Nearest-rank: rank = ceil(99 * total / 100), integer ceil-div.
        let rank = (99u128 * total as u128).div_ceil(100) as u64;
        let mut acc = 0u64;
        for (i, &c) in counts.iter().enumerate() {
            acc = acc.saturating_add(c);
            if acc >= rank {
                return bucket_upper(i);
            }
        }
        bucket_upper(HISTOGRAM_BUCKETS - 1)
    }
}

/// Counters accumulated over the lifetime of the process. A single `static`
/// instance backs the free functions in the parent module; the engine bumps it
/// from `RequestState::emit` and `result_addr`, and the CLI reads a snapshot
/// once, at exit. Everything is lock-free atomics — including the artifact-size
/// histogram, which is fixed-size, so memory never grows with the build.
#[derive(Debug)]
pub struct TelemetryCollector {
    /// Targets that passed through `result_addr` — one per non-memoized resolve.
    targets: AtomicU64,
    local_cache_hits: AtomicU64,
    local_cache_misses: AtomicU64,
    /// Total output artifacts across every resolved target (incl. those whose
    /// size is unknown).
    artifacts: AtomicU64,
    /// Count of artifacts with a known size (the population behind the size stats).
    sized_artifacts: AtomicU64,
    /// Summed known `byte_size()` of those artifacts.
    artifact_bytes: AtomicU64,
    /// Largest single known artifact `byte_size()` seen (exact).
    max_artifact_bytes: AtomicU64,
    /// Power-of-two histogram of known artifact sizes, for an approximate p99.
    size_histogram: AtomicHistogram,
    /// Targets that actually executed (cache misses) this process.
    executes: AtomicU64,
    /// Power-of-two histogram of per-target execute wall time (ms), for p99.
    execute_ms_histogram: AtomicHistogram,
    /// Total target count of a whole-graph (`//...`) query, when one ran. `0`
    /// means no whole-graph query was issued this process.
    graph_size: AtomicU64,
    /// Remote (shared) cache outcomes, folded from the event stream.
    remote_cache_hits: AtomicU64,
    remote_cache_misses: AtomicU64,
    /// Pushes to the remote cache (one per `RemoteCacheWriteStart`).
    remote_cache_writes: AtomicU64,
    /// Approval-gated targets that asked for a decision. The rest of the
    /// approval counters partition the resolved subset of these.
    approvals_requested: AtomicU64,
    /// Granted vs denied; their sum is at most `approvals_requested` (the
    /// shortfall was cancelled/errored before a decision landed).
    approvals_granted: AtomicU64,
    approvals_denied: AtomicU64,
    /// Interactive sandbox shell sessions actually entered (`--shell`).
    shell_sessions: AtomicU64,
    /// Per-execute sandbox backend the bridge chose: a FUSE mount vs the
    /// unpack-copy ("os") path.
    sandbox_fuse: AtomicU64,
    sandbox_os: AtomicU64,
    /// Local-cache blobs that crossed the spill threshold and moved from the
    /// primary store onto the FS blob store.
    cache_spills: AtomicU64,
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryCollector {
    /// `const` so a single instance can back a `static` with no initializer.
    pub const fn new() -> Self {
        Self {
            targets: AtomicU64::new(0),
            local_cache_hits: AtomicU64::new(0),
            local_cache_misses: AtomicU64::new(0),
            artifacts: AtomicU64::new(0),
            sized_artifacts: AtomicU64::new(0),
            artifact_bytes: AtomicU64::new(0),
            max_artifact_bytes: AtomicU64::new(0),
            size_histogram: AtomicHistogram::new(),
            executes: AtomicU64::new(0),
            execute_ms_histogram: AtomicHistogram::new(),
            graph_size: AtomicU64::new(0),
            remote_cache_hits: AtomicU64::new(0),
            remote_cache_misses: AtomicU64::new(0),
            remote_cache_writes: AtomicU64::new(0),
            approvals_requested: AtomicU64::new(0),
            approvals_granted: AtomicU64::new(0),
            approvals_denied: AtomicU64::new(0),
            shell_sessions: AtomicU64::new(0),
            sandbox_fuse: AtomicU64::new(0),
            sandbox_os: AtomicU64::new(0),
            cache_spills: AtomicU64::new(0),
        }
    }

    /// Fold one build event into the counters. Called from `RequestState::emit`,
    /// the single point every progress event flows through, so the cache and
    /// target tallies stay in lockstep with what the renderer sees.
    pub fn observe_event(&self, kind: &BuildEventKind) {
        match kind {
            BuildEventKind::ResultStart { .. } => {
                self.targets.fetch_add(1, Ordering::Relaxed);
            }
            BuildEventKind::LocalCacheHit { .. } => {
                self.local_cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            BuildEventKind::LocalCacheMiss { .. } => {
                self.local_cache_misses.fetch_add(1, Ordering::Relaxed);
            }
            BuildEventKind::RemoteCacheHit { .. } => {
                self.remote_cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            BuildEventKind::RemoteCacheMiss { .. } => {
                self.remote_cache_misses.fetch_add(1, Ordering::Relaxed);
            }
            BuildEventKind::RemoteCacheWriteStart { .. } => {
                self.remote_cache_writes.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Record one resolved target's artifacts: `count` is the total artifact
    /// count (including those whose size is unknown); `sizes` are the known
    /// per-artifact `byte_size()`s. Artifact size lives on the result, not the
    /// event stream, so this is recorded explicitly where `result_addr` produces
    /// the `EResult`. Sizes fold into the fixed histogram — constant memory.
    pub fn record_artifacts(&self, count: u64, sizes: &[u64]) {
        self.artifacts.fetch_add(count, Ordering::Relaxed);
        if sizes.is_empty() {
            return;
        }
        self.sized_artifacts
            .fetch_add(sizes.len() as u64, Ordering::Relaxed);
        let sum: u64 = sizes.iter().sum();
        self.artifact_bytes.fetch_add(sum, Ordering::Relaxed);
        if let Some(&max) = sizes.iter().max() {
            self.max_artifact_bytes.fetch_max(max, Ordering::Relaxed);
        }
        for &size in sizes {
            self.size_histogram.record(size);
        }
    }

    /// Record one target's execute wall time (ms). Called only on a real
    /// execution (cache miss), so the population is exactly the executed targets.
    pub fn record_execute_ms(&self, ms: u64) {
        self.executes.fetch_add(1, Ordering::Relaxed);
        self.execute_ms_histogram.record(ms);
    }

    /// Record the total target count of a whole-graph (`//...`) query. Keeps the
    /// largest seen value, so repeated/nested whole-graph queries don't shrink it.
    pub fn record_graph_size(&self, total: u64) {
        self.graph_size.fetch_max(total, Ordering::Relaxed);
    }

    /// Record that an approval-gated target asked the user for a decision.
    /// Bumped before the prompt so a cancelled/errored gate is still counted as
    /// requested even though no decision follows.
    pub fn record_approval_requested(&self) {
        self.approvals_requested.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the resolved outcome of an approval prompt. Not called when the
    /// gate is cancelled or errors before a decision.
    pub fn record_approval_decision(&self, granted: bool) {
        let c = if granted {
            &self.approvals_granted
        } else {
            &self.approvals_denied
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an interactive sandbox shell session was actually entered.
    pub fn record_shell_session(&self) {
        self.shell_sessions.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one execute's chosen sandbox backend: `fuse` true for a FUSE
    /// mount, false for the unpack-copy ("os") path.
    pub fn record_sandbox(&self, fuse: bool) {
        let c = if fuse {
            &self.sandbox_fuse
        } else {
            &self.sandbox_os
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that one local-cache blob spilled from the primary store to the
    /// FS blob store (crossed the spill threshold).
    pub fn record_cache_spill(&self) {
        self.cache_spills.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the accumulated counters at this instant.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            targets: self.targets.load(Ordering::Relaxed),
            local_cache_hits: self.local_cache_hits.load(Ordering::Relaxed),
            local_cache_misses: self.local_cache_misses.load(Ordering::Relaxed),
            artifacts: self.artifacts.load(Ordering::Relaxed),
            artifact_bytes: self.artifact_bytes.load(Ordering::Relaxed),
            max_artifact_bytes: self.max_artifact_bytes.load(Ordering::Relaxed),
            p99_artifact_bytes: self.size_histogram.percentile_99(),
            sized_artifacts: self.sized_artifacts.load(Ordering::Relaxed),
            executes: self.executes.load(Ordering::Relaxed),
            p99_execute_ms: self.execute_ms_histogram.percentile_99(),
            graph_size: self.graph_size.load(Ordering::Relaxed),
            remote_cache_hits: self.remote_cache_hits.load(Ordering::Relaxed),
            remote_cache_misses: self.remote_cache_misses.load(Ordering::Relaxed),
            remote_cache_writes: self.remote_cache_writes.load(Ordering::Relaxed),
            approvals_requested: self.approvals_requested.load(Ordering::Relaxed),
            approvals_granted: self.approvals_granted.load(Ordering::Relaxed),
            approvals_denied: self.approvals_denied.load(Ordering::Relaxed),
            shell_sessions: self.shell_sessions.load(Ordering::Relaxed),
            sandbox_fuse: self.sandbox_fuse.load(Ordering::Relaxed),
            sandbox_os: self.sandbox_os.load(Ordering::Relaxed),
            cache_spills: self.cache_spills.load(Ordering::Relaxed),
        }
    }
}

/// Immutable read of the counters, taken once at the end of a run.
#[derive(Debug, Clone, Copy)]
pub struct TelemetrySnapshot {
    pub targets: u64,
    pub local_cache_hits: u64,
    pub local_cache_misses: u64,
    pub artifacts: u64,
    pub artifact_bytes: u64,
    pub max_artifact_bytes: u64,
    /// Approximate (power-of-two-bucketed) 99th percentile artifact size.
    pub p99_artifact_bytes: u64,
    /// How many artifacts had a known size (the population behind the size
    /// stats); `0` means max/p99 are not meaningful.
    pub sized_artifacts: u64,
    /// Targets that actually executed (cache misses); population behind the
    /// execute p99. `0` means `p99_execute_ms` is not meaningful.
    pub executes: u64,
    /// Approximate 99th percentile per-target execute wall time (ms).
    pub p99_execute_ms: u64,
    pub graph_size: u64,
    pub remote_cache_hits: u64,
    pub remote_cache_misses: u64,
    pub remote_cache_writes: u64,
    pub approvals_requested: u64,
    pub approvals_granted: u64,
    pub approvals_denied: u64,
    pub shell_sessions: u64,
    pub sandbox_fuse: u64,
    pub sandbox_os: u64,
    pub cache_spills: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_execute_ms_counts_and_p99() {
        let c = TelemetryCollector::new();
        // 99 fast executes (~4ms → bucket 3, [4,7]) + 1 slow (4096ms).
        for _ in 0..99 {
            c.record_execute_ms(4);
        }
        c.record_execute_ms(4096);
        let s = c.snapshot();
        assert_eq!(s.executes, 100);
        // p99 lands in the fast bucket (upper edge 7), not the slow tail.
        assert_eq!(s.p99_execute_ms, 7);

        // No executes → p99 is 0.
        assert_eq!(TelemetryCollector::new().snapshot().p99_execute_ms, 0);
    }

    #[test]
    fn observe_event_tallies_targets_and_cache() {
        let c = TelemetryCollector::new();
        let addr = || "//pkg:a".to_string();
        c.observe_event(&BuildEventKind::ResultStart { addr: addr() });
        c.observe_event(&BuildEventKind::ResultStart { addr: addr() });
        c.observe_event(&BuildEventKind::LocalCacheHit { addr: addr() });
        c.observe_event(&BuildEventKind::LocalCacheMiss { addr: addr() });
        c.observe_event(&BuildEventKind::LocalCacheMiss { addr: addr() });
        // Unrelated events must not move any counter.
        c.observe_event(&BuildEventKind::ResultEnd {
            addr: addr(),
            error: None,
        });

        let s = c.snapshot();
        assert_eq!(s.targets, 2);
        assert_eq!(s.local_cache_hits, 1);
        assert_eq!(s.local_cache_misses, 2);
    }

    #[test]
    fn observe_event_tallies_remote_cache() {
        let c = TelemetryCollector::new();
        let addr = || "//pkg:a".to_string();
        c.observe_event(&BuildEventKind::RemoteCacheHit { addr: addr() });
        c.observe_event(&BuildEventKind::RemoteCacheMiss { addr: addr() });
        c.observe_event(&BuildEventKind::RemoteCacheMiss { addr: addr() });
        c.observe_event(&BuildEventKind::RemoteCacheWriteStart { addr: addr() });
        // The paired end event must not double-count the write.
        c.observe_event(&BuildEventKind::RemoteCacheWriteEnd {
            addr: addr(),
            error: None,
        });
        let s = c.snapshot();
        assert_eq!(s.remote_cache_hits, 1);
        assert_eq!(s.remote_cache_misses, 2);
        assert_eq!(s.remote_cache_writes, 1);
    }

    #[test]
    fn approval_shell_sandbox_spill_counters() {
        let c = TelemetryCollector::new();
        c.record_approval_requested();
        c.record_approval_requested();
        c.record_approval_decision(true);
        c.record_approval_decision(false);
        c.record_shell_session();
        c.record_sandbox(true);
        c.record_sandbox(false);
        c.record_sandbox(false);
        c.record_cache_spill();
        let s = c.snapshot();
        assert_eq!(s.approvals_requested, 2);
        assert_eq!(s.approvals_granted, 1);
        assert_eq!(s.approvals_denied, 1);
        assert_eq!(s.shell_sessions, 1);
        assert_eq!(s.sandbox_fuse, 1);
        assert_eq!(s.sandbox_os, 2);
        assert_eq!(s.cache_spills, 1);
    }

    #[test]
    fn record_artifacts_accumulates_counts_and_sizes() {
        let c = TelemetryCollector::new();
        // 2 artifacts, one size unknown (only one size reported).
        c.record_artifacts(2, &[100]);
        c.record_artifacts(3, &[50, 200, 10]);
        let s = c.snapshot();
        assert_eq!(
            s.artifacts, 5,
            "total count includes unknown-size artifacts"
        );
        assert_eq!(s.artifact_bytes, 360);
        assert_eq!(s.max_artifact_bytes, 200, "max is exact");
        assert_eq!(s.sized_artifacts, 4);
    }

    #[test]
    fn bucket_index_boundaries() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(1), 1); // [1,1]
        assert_eq!(bucket_index(2), 2); // [2,3]
        assert_eq!(bucket_index(3), 2);
        assert_eq!(bucket_index(4), 3); // [4,7]
        assert_eq!(bucket_index(255), 8); // [128,255]
        assert_eq!(bucket_index(256), 9); // [256,511]
        assert_eq!(bucket_index(u64::MAX), 64);
    }

    #[test]
    fn percentile_99_approximate_upper_edge() {
        let c = TelemetryCollector::new();
        // 99 small artifacts (size 1 → bucket 1, upper edge 1) and one big one
        // (size 1<<20 → bucket 21). The 99th-rank value is small.
        let small = vec![1u64; 99];
        c.record_artifacts(99, &small);
        c.record_artifacts(1, &[1 << 20]);
        let s = c.snapshot();
        // p99 lands in the small bucket (upper edge 1), not the big one.
        assert_eq!(s.p99_artifact_bytes, 1);
        assert_eq!(s.max_artifact_bytes, 1 << 20, "max still catches the tail");

        // Empty histogram → 0.
        assert_eq!(TelemetryCollector::new().snapshot().p99_artifact_bytes, 0);
    }

    #[test]
    fn percentile_99_within_factor_of_two() {
        let c = TelemetryCollector::new();
        // All sizes 1000 → bucket 10 ([512,1023], upper edge 1023). True p99 is
        // 1000; the estimate (1023) is within a factor of 2.
        c.record_artifacts(100, &vec![1000u64; 100]);
        let p99 = c.snapshot().p99_artifact_bytes;
        assert!(
            (1000..=2000).contains(&p99),
            "p99 estimate {p99} off by >2x"
        );
    }
}
