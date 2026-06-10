//! Per-request telemetry counters.
//!
//! A lock-free aggregator threaded through `RequestState`. The engine bumps it
//! from the same chokepoint that emits build events ([`RequestState::emit`])
//! plus one explicit artifact-record call per resolved target. The reporting
//! command reads a [`TelemetrySnapshot`] at the end of a run and ships it to
//! PostHog. Collection is always cheap (relaxed atomics); the opt-out only gates
//! whether the snapshot is *sent*, never whether it is gathered.
//!
//! [`RequestState::emit`]: crate::engine::request_state::RequestState::emit

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::event::BuildEventKind;

/// Counters accumulated over the lifetime of the process. A single `static`
/// instance backs the free functions in the parent module; the engine bumps it
/// from `RequestState::emit` and `result_addr`, and the CLI reads a snapshot
/// once, at exit. Scalar tallies are lock-free atomics; per-artifact sizes are
/// gathered under a short-held `Mutex` (one append per resolved target) so the
/// CLI can pre-compute distribution stats (max, p99).
#[derive(Debug, Default)]
pub struct TelemetryCollector {
    /// Targets that passed through `result_addr` — one per non-memoized resolve.
    targets: AtomicU64,
    local_cache_hits: AtomicU64,
    local_cache_misses: AtomicU64,
    /// Total output artifacts across every resolved target (incl. those whose
    /// size is unknown).
    artifacts: AtomicU64,
    /// Summed known `byte_size()` of those artifacts.
    artifact_bytes: AtomicU64,
    /// Largest single known artifact `byte_size()` seen.
    max_artifact_bytes: AtomicU64,
    /// Total target count of a whole-graph (`//...`) query, when one ran. `0`
    /// means no whole-graph query was issued this process.
    graph_size: AtomicU64,
    /// Every known per-artifact `byte_size()`, for percentile pre-computation.
    artifact_sizes: Mutex<Vec<u64>>,
}

impl TelemetryCollector {
    /// `const` so a single instance can back a `static` with no initializer.
    pub const fn new() -> Self {
        Self {
            targets: AtomicU64::new(0),
            local_cache_hits: AtomicU64::new(0),
            local_cache_misses: AtomicU64::new(0),
            artifacts: AtomicU64::new(0),
            artifact_bytes: AtomicU64::new(0),
            max_artifact_bytes: AtomicU64::new(0),
            graph_size: AtomicU64::new(0),
            artifact_sizes: Mutex::new(Vec::new()),
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
            _ => {}
        }
    }

    /// Record one resolved target's artifacts: `count` is the total artifact
    /// count (including those whose size is unknown); `sizes` are the known
    /// per-artifact `byte_size()`s. Artifact size lives on the result, not the
    /// event stream, so this is recorded explicitly where `result_addr` produces
    /// the `EResult`. The `sizes` retention feeds the CLI's percentile stats.
    pub fn record_artifacts(&self, count: u64, sizes: &[u64]) {
        self.artifacts.fetch_add(count, Ordering::Relaxed);
        let sum: u64 = sizes.iter().sum();
        self.artifact_bytes.fetch_add(sum, Ordering::Relaxed);
        if let Some(&max) = sizes.iter().max() {
            self.max_artifact_bytes.fetch_max(max, Ordering::Relaxed);
        }
        if !sizes.is_empty()
            && let Ok(mut v) = self.artifact_sizes.lock()
        {
            v.extend_from_slice(sizes);
        }
    }

    /// Record the total target count of a whole-graph (`//...`) query. Keeps the
    /// largest seen value, so repeated/nested whole-graph queries don't shrink it.
    pub fn record_graph_size(&self, total: u64) {
        self.graph_size.fetch_max(total, Ordering::Relaxed);
    }

    /// Read the accumulated counters at this instant. Computes the artifact-size
    /// 99th percentile (nearest-rank) from the gathered sizes — done once, at
    /// exit, so the sort cost is paid off the hot path.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let sizes = self
            .artifact_sizes
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        let sized_artifacts = sizes.len() as u64;
        TelemetrySnapshot {
            targets: self.targets.load(Ordering::Relaxed),
            local_cache_hits: self.local_cache_hits.load(Ordering::Relaxed),
            local_cache_misses: self.local_cache_misses.load(Ordering::Relaxed),
            artifacts: self.artifacts.load(Ordering::Relaxed),
            artifact_bytes: self.artifact_bytes.load(Ordering::Relaxed),
            max_artifact_bytes: self.max_artifact_bytes.load(Ordering::Relaxed),
            p99_artifact_bytes: percentile_99(sizes),
            sized_artifacts,
            graph_size: self.graph_size.load(Ordering::Relaxed),
        }
    }
}

/// Nearest-rank 99th percentile of `sizes`. Consumes the vec (sorts in place).
/// Returns 0 for an empty input.
fn percentile_99(mut sizes: Vec<u64>) -> u64 {
    if sizes.is_empty() {
        return 0;
    }
    sizes.sort_unstable();
    // Nearest-rank: rank = ceil(99 * n / 100) via integer ceil-div, clamped to
    // [1, n]; index = rank - 1. Integer math avoids any float-cast rounding.
    let n = sizes.len();
    let rank = (99 * n).div_ceil(100).clamp(1, n);
    sizes.get(rank - 1).copied().unwrap_or(0)
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
    pub p99_artifact_bytes: u64,
    /// How many artifacts had a known size (the population behind the size
    /// stats); `0` means max/p99 are not meaningful.
    pub sized_artifacts: u64,
    pub graph_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(s.max_artifact_bytes, 200);
        assert_eq!(s.sized_artifacts, 4);
    }

    #[test]
    fn percentile_99_nearest_rank() {
        assert_eq!(percentile_99(vec![]), 0);
        assert_eq!(percentile_99(vec![42]), 42);
        // 100 values 1..=100: ceil(0.99*100)=99 → 99th smallest = 99.
        let sizes: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_99(sizes), 99);
        // Unsorted input must still work.
        assert_eq!(percentile_99(vec![9, 1, 5, 3, 7]), 9);
    }
}
