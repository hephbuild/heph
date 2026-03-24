//! Metrics collection and reporting

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Build metrics
#[derive(Debug, Default)]
pub struct Metrics {
    // Build metrics
    total_builds: AtomicU64,
    successful_builds: AtomicU64,
    failed_builds: AtomicU64,
    total_build_time_ms: AtomicU64,

    // Cache metrics
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,

    // Start time for uptime calculation
    start_time: Option<Instant>,
}

impl Metrics {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            start_time: Some(Instant::now()),
            ..Default::default()
        }
    }

    /// Record a build
    pub fn record_build(&self, duration_ms: u64, success: bool) {
        self.total_builds.fetch_add(1, Ordering::Relaxed);
        self.total_build_time_ms
            .fetch_add(duration_ms, Ordering::Relaxed);

        if success {
            self.successful_builds.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_builds.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a cache hit
    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cache miss
    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Get total builds
    pub fn total_builds(&self) -> u64 {
        self.total_builds.load(Ordering::Relaxed)
    }

    /// Get successful builds
    pub fn successful_builds(&self) -> u64 {
        self.successful_builds.load(Ordering::Relaxed)
    }

    /// Get failed builds
    pub fn failed_builds(&self) -> u64 {
        self.failed_builds.load(Ordering::Relaxed)
    }

    /// Get total build time in milliseconds
    pub fn total_build_time_ms(&self) -> u64 {
        self.total_build_time_ms.load(Ordering::Relaxed)
    }

    /// Get average build time in milliseconds
    pub fn average_build_time_ms(&self) -> u64 {
        let total = self.total_builds();
        if total == 0 {
            0
        } else {
            self.total_build_time_ms() / total
        }
    }

    /// Get cache hits
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(Ordering::Relaxed)
    }

    /// Get cache misses
    pub fn cache_misses(&self) -> u64 {
        self.cache_misses.load(Ordering::Relaxed)
    }

    /// Get cache hit rate (0.0 to 1.0)
    pub fn cache_hit_rate(&self) -> f64 {
        let hits = self.cache_hits();
        let misses = self.cache_misses();
        let total = hits + misses;

        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    /// Get success rate (0.0 to 1.0)
    pub fn success_rate(&self) -> f64 {
        let total = self.total_builds();
        if total == 0 {
            0.0
        } else {
            self.successful_builds() as f64 / total as f64
        }
    }

    /// Get uptime duration
    pub fn uptime(&self) -> Option<Duration> {
        self.start_time.map(|start| start.elapsed())
    }

    /// Export metrics as a snapshot
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            total_builds: self.total_builds(),
            successful_builds: self.successful_builds(),
            failed_builds: self.failed_builds(),
            total_build_time_ms: self.total_build_time_ms(),
            average_build_time_ms: self.average_build_time_ms(),
            cache_hits: self.cache_hits(),
            cache_misses: self.cache_misses(),
            cache_hit_rate: self.cache_hit_rate(),
            success_rate: self.success_rate(),
            uptime_secs: self.uptime().map(|d| d.as_secs()).unwrap_or(0),
        }
    }

    /// Reset all metrics
    pub fn reset(&self) {
        self.total_builds.store(0, Ordering::Relaxed);
        self.successful_builds.store(0, Ordering::Relaxed);
        self.failed_builds.store(0, Ordering::Relaxed);
        self.total_build_time_ms.store(0, Ordering::Relaxed);
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_misses.store(0, Ordering::Relaxed);
    }
}

/// Snapshot of metrics at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub total_builds: u64,
    pub successful_builds: u64,
    pub failed_builds: u64,
    pub total_build_time_ms: u64,
    pub average_build_time_ms: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_hit_rate: f64,
    pub success_rate: f64,
    pub uptime_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_creation() {
        let metrics = Metrics::new();
        assert_eq!(metrics.total_builds(), 0);
        assert_eq!(metrics.successful_builds(), 0);
        assert_eq!(metrics.failed_builds(), 0);
        assert!(metrics.uptime().is_some());
    }

    #[test]
    fn test_record_build() {
        let metrics = Metrics::new();

        metrics.record_build(100, true);
        metrics.record_build(200, true);
        metrics.record_build(150, false);

        assert_eq!(metrics.total_builds(), 3);
        assert_eq!(metrics.successful_builds(), 2);
        assert_eq!(metrics.failed_builds(), 1);
        assert_eq!(metrics.total_build_time_ms(), 450);
        assert_eq!(metrics.average_build_time_ms(), 150);
    }

    #[test]
    fn test_cache_metrics() {
        let metrics = Metrics::new();

        metrics.record_cache_hit();
        metrics.record_cache_hit();
        metrics.record_cache_miss();

        assert_eq!(metrics.cache_hits(), 2);
        assert_eq!(metrics.cache_misses(), 1);
        assert_eq!(metrics.cache_hit_rate(), 2.0 / 3.0);
    }

    #[test]
    fn test_success_rate() {
        let metrics = Metrics::new();

        metrics.record_build(100, true);
        metrics.record_build(100, true);
        metrics.record_build(100, true);
        metrics.record_build(100, false);

        assert_eq!(metrics.success_rate(), 0.75);
    }

    #[test]
    fn test_average_build_time() {
        let metrics = Metrics::new();

        metrics.record_build(100, true);
        metrics.record_build(200, true);
        metrics.record_build(300, true);

        assert_eq!(metrics.average_build_time_ms(), 200);
    }

    #[test]
    fn test_metrics_snapshot() {
        let metrics = Metrics::new();

        metrics.record_build(150, true);
        metrics.record_cache_hit();
        metrics.record_cache_miss();

        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.total_builds, 1);
        assert_eq!(snapshot.successful_builds, 1);
        assert_eq!(snapshot.cache_hits, 1);
        assert_eq!(snapshot.cache_misses, 1);
        assert_eq!(snapshot.cache_hit_rate, 0.5);
    }

    #[test]
    fn test_metrics_reset() {
        let metrics = Metrics::new();

        metrics.record_build(100, true);
        metrics.record_cache_hit();

        assert_eq!(metrics.total_builds(), 1);
        assert_eq!(metrics.cache_hits(), 1);

        metrics.reset();

        assert_eq!(metrics.total_builds(), 0);
        assert_eq!(metrics.cache_hits(), 0);
    }

    #[test]
    fn test_zero_division_safety() {
        let metrics = Metrics::new();

        assert_eq!(metrics.average_build_time_ms(), 0);
        assert_eq!(metrics.cache_hit_rate(), 0.0);
        assert_eq!(metrics.success_rate(), 0.0);
    }
}
