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

use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::event::BuildEventKind;

/// Lock-free counters accumulated over the lifetime of the process. A single
/// `static` instance backs the free functions in the parent module; the engine
/// bumps it from `RequestState::emit` and `result_addr`, and the CLI reads a
/// snapshot once, at exit.
#[derive(Debug, Default)]
pub struct TelemetryCollector {
    /// Targets that passed through `result_addr` — one per non-memoized resolve.
    targets: AtomicU64,
    local_cache_hits: AtomicU64,
    local_cache_misses: AtomicU64,
    /// Total output artifacts summed across every resolved target.
    artifacts: AtomicU64,
    /// Summed `byte_size()` of those artifacts (a target's unknown sizes add 0).
    artifact_bytes: AtomicU64,
    /// Total target count of a whole-graph (`//...`) query, when one ran. `0`
    /// means no whole-graph query was issued this process.
    graph_size: AtomicU64,
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
            graph_size: AtomicU64::new(0),
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

    /// Record one resolved target's artifact count and total byte size. Artifact
    /// size lives on the result, not the event stream, so this is recorded
    /// explicitly where `result_addr` produces the `EResult`.
    pub fn record_artifacts(&self, count: u64, bytes: u64) {
        self.artifacts.fetch_add(count, Ordering::Relaxed);
        self.artifact_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record the total target count of a whole-graph (`//...`) query. Keeps the
    /// largest seen value, so repeated/nested whole-graph queries don't shrink it.
    pub fn record_graph_size(&self, total: u64) {
        self.graph_size.fetch_max(total, Ordering::Relaxed);
    }

    /// Read the accumulated counters at this instant.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            targets: self.targets.load(Ordering::Relaxed),
            local_cache_hits: self.local_cache_hits.load(Ordering::Relaxed),
            local_cache_misses: self.local_cache_misses.load(Ordering::Relaxed),
            artifacts: self.artifacts.load(Ordering::Relaxed),
            artifact_bytes: self.artifact_bytes.load(Ordering::Relaxed),
            graph_size: self.graph_size.load(Ordering::Relaxed),
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
    fn record_artifacts_accumulates() {
        let c = TelemetryCollector::new();
        c.record_artifacts(2, 100);
        c.record_artifacts(3, 50);
        let s = c.snapshot();
        assert_eq!(s.artifacts, 5);
        assert_eq!(s.artifact_bytes, 150);
    }
}
