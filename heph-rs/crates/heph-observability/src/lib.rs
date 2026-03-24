//! Observability and telemetry for Heph
//!
//! This module provides OpenTelemetry integration, structured logging with tracing,
//! and metrics collection for monitoring Heph build system performance.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, info, span, Level};

pub mod metrics;
pub mod telemetry;

#[derive(Error, Debug)]
pub enum ObservabilityError {
    #[error("Telemetry initialization failed: {0}")]
    InitializationFailed(String),

    #[error("Metrics error: {0}")]
    MetricsError(String),

    #[error("Span error: {0}")]
    SpanError(String),
}

pub type Result<T> = std::result::Result<T, ObservabilityError>;

/// Observability configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Enable tracing
    pub tracing_enabled: bool,

    /// Enable metrics collection
    pub metrics_enabled: bool,

    /// OTLP endpoint for exporting telemetry
    pub otlp_endpoint: Option<String>,

    /// Service name for telemetry
    pub service_name: String,

    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            tracing_enabled: true,
            metrics_enabled: true,
            otlp_endpoint: None,
            service_name: "heph".to_string(),
            log_level: "info".to_string(),
        }
    }
}

/// Observability system for Heph
pub struct Observability {
    config: ObservabilityConfig,
    metrics: Arc<Mutex<metrics::Metrics>>,
    initialized: bool,
}

impl Observability {
    /// Create a new observability system
    pub fn new(config: ObservabilityConfig) -> Self {
        Self {
            config,
            metrics: Arc::new(Mutex::new(metrics::Metrics::default())),
            initialized: false,
        }
    }

    /// Initialize observability (tracing, metrics, telemetry)
    pub fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }

        // Initialize tracing subscriber
        if self.config.tracing_enabled {
            self.init_tracing()?;
        }

        // Initialize metrics
        if self.config.metrics_enabled {
            debug!("Metrics collection enabled");
        }

        // Initialize OpenTelemetry if endpoint is configured
        if let Some(endpoint) = &self.config.otlp_endpoint {
            info!(endpoint = %endpoint, "Initializing OpenTelemetry");
            // OTLP initialization would go here
            // For now, we just log it
        }

        self.initialized = true;
        info!(service = %self.config.service_name, "Observability initialized");

        Ok(())
    }

    /// Initialize tracing subscriber
    fn init_tracing(&self) -> Result<()> {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&self.config.log_level))
            .map_err(|e| ObservabilityError::InitializationFailed(e.to_string()))?;

        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .map_err(|e| ObservabilityError::InitializationFailed(e.to_string()))?;

        Ok(())
    }

    /// Get metrics handle
    pub fn metrics(&self) -> Arc<Mutex<metrics::Metrics>> {
        self.metrics.clone()
    }

    /// Record a build event
    pub fn record_build(&self, target: &str, duration_ms: u64, success: bool) {
        let span = span!(Level::INFO, "build", target = %target);
        let _enter = span.enter();

        info!(
            target = %target,
            duration_ms = duration_ms,
            success = success,
            "Build completed"
        );

        let metrics = self.metrics.lock().unwrap();
        metrics.record_build(duration_ms, success);
    }

    /// Record a cache event
    pub fn record_cache_event(&self, hit: bool, key: &str) {
        let span = span!(Level::DEBUG, "cache", key = %key);
        let _enter = span.enter();

        if hit {
            debug!(key = %key, "Cache hit");
        } else {
            debug!(key = %key, "Cache miss");
        }

        let metrics = self.metrics.lock().unwrap();
        if hit {
            metrics.record_cache_hit();
        } else {
            metrics.record_cache_miss();
        }
    }

    /// Shutdown observability system
    pub fn shutdown(&self) {
        info!("Shutting down observability");
        // Cleanup would go here
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new(ObservabilityConfig::default())
    }
}

/// Helper macro for creating instrumented spans
#[macro_export]
macro_rules! instrument_fn {
    ($name:expr) => {
        tracing::span!(tracing::Level::INFO, $name)
    };
}

/// Helper macro for recording errors
#[macro_export]
macro_rules! record_error {
    ($span:expr, $error:expr) => {
        tracing::error!(error = %$error, "Operation failed");
        $span.record("error", &tracing::field::display($error));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observability_creation() {
        let obs = Observability::default();
        assert!(!obs.initialized);
        assert!(obs.config.tracing_enabled);
        assert!(obs.config.metrics_enabled);
    }

    #[test]
    fn test_observability_config() {
        let config = ObservabilityConfig {
            tracing_enabled: true,
            metrics_enabled: true,
            otlp_endpoint: Some("http://localhost:4317".to_string()),
            service_name: "test-service".to_string(),
            log_level: "debug".to_string(),
        };

        let obs = Observability::new(config.clone());
        assert_eq!(obs.config.service_name, "test-service");
        assert_eq!(obs.config.log_level, "debug");
        assert_eq!(
            obs.config.otlp_endpoint,
            Some("http://localhost:4317".to_string())
        );
    }

    #[test]
    fn test_record_build() {
        let obs = Observability::default();
        obs.record_build("//foo:bar", 1500, true);
        obs.record_build("//baz:qux", 2000, false);

        let metrics = obs.metrics.lock().unwrap();
        assert_eq!(metrics.total_builds(), 2);
        assert_eq!(metrics.successful_builds(), 1);
    }

    #[test]
    fn test_record_cache_event() {
        let obs = Observability::default();
        obs.record_cache_event(true, "key1");
        obs.record_cache_event(false, "key2");
        obs.record_cache_event(true, "key3");

        let metrics = obs.metrics.lock().unwrap();
        assert_eq!(metrics.cache_hits(), 2);
        assert_eq!(metrics.cache_misses(), 1);
    }

    #[test]
    fn test_metrics_access() {
        let obs = Observability::default();
        let metrics = obs.metrics();

        {
            let m = metrics.lock().unwrap();
            m.record_build(100, true);
        }

        let m = metrics.lock().unwrap();
        assert_eq!(m.total_builds(), 1);
    }
}
