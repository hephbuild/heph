//! OpenTelemetry telemetry integration

use crate::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, span, Level};

/// Telemetry configuration for OpenTelemetry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// OTLP endpoint (e.g., "http://localhost:4317")
    pub endpoint: String,

    /// Service name
    pub service_name: String,

    /// Service version
    pub service_version: String,

    /// Enable traces
    pub traces_enabled: bool,

    /// Enable metrics
    pub metrics_enabled: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4317".to_string(),
            service_name: "heph".to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            traces_enabled: true,
            metrics_enabled: true,
        }
    }
}

/// Telemetry system using OpenTelemetry
pub struct Telemetry {
    config: TelemetryConfig,
    initialized: bool,
}

impl Telemetry {
    /// Create a new telemetry system
    pub fn new(config: TelemetryConfig) -> Self {
        Self {
            config,
            initialized: false,
        }
    }

    /// Initialize OpenTelemetry
    pub fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }

        info!(
            endpoint = %self.config.endpoint,
            service = %self.config.service_name,
            "Initializing OpenTelemetry"
        );

        // In a real implementation, we would initialize the OTLP exporter here
        // For now, we just mark as initialized
        self.initialized = true;

        Ok(())
    }

    /// Create a span for a build operation
    pub fn build_span(&self, target: &str) -> BuildSpan {
        let span = span!(Level::INFO, "build", target = %target);
        BuildSpan { span }
    }

    /// Create a span for a cache operation
    pub fn cache_span(&self, operation: &str, key: &str) -> CacheSpan {
        let span = span!(Level::DEBUG, "cache", operation = %operation, key = %key);
        CacheSpan { span }
    }

    /// Shutdown telemetry
    pub fn shutdown(&self) {
        info!("Shutting down telemetry");
        // Cleanup would go here
    }

    /// Check if telemetry is initialized
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new(TelemetryConfig::default())
    }
}

/// A traced build span
pub struct BuildSpan {
    span: tracing::Span,
}

impl BuildSpan {
    /// Record build success
    pub fn record_success(&self, duration_ms: u64) {
        let _enter = self.span.enter();
        tracing::info!(duration_ms = duration_ms, success = true, "Build completed");
    }

    /// Record build failure
    pub fn record_failure(&self, error: &str) {
        let _enter = self.span.enter();
        tracing::error!(error = %error, success = false, "Build failed");
    }
}

/// A traced cache span
pub struct CacheSpan {
    span: tracing::Span,
}

impl CacheSpan {
    /// Record cache hit
    pub fn record_hit(&self) {
        let _enter = self.span.enter();
        tracing::debug!(hit = true, "Cache hit");
    }

    /// Record cache miss
    pub fn record_miss(&self) {
        let _enter = self.span.enter();
        tracing::debug!(hit = false, "Cache miss");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_creation() {
        let telemetry = Telemetry::default();
        assert!(!telemetry.is_initialized());
        assert_eq!(telemetry.config.service_name, "heph");
    }

    #[test]
    fn test_telemetry_initialization() {
        let mut telemetry = Telemetry::default();
        assert!(telemetry.initialize().is_ok());
        assert!(telemetry.is_initialized());

        // Second initialization should succeed (no-op)
        assert!(telemetry.initialize().is_ok());
    }

    #[test]
    fn test_telemetry_config() {
        let config = TelemetryConfig {
            endpoint: "http://custom:4317".to_string(),
            service_name: "test-service".to_string(),
            service_version: "1.0.0".to_string(),
            traces_enabled: true,
            metrics_enabled: false,
        };

        let telemetry = Telemetry::new(config.clone());
        assert_eq!(telemetry.config.endpoint, "http://custom:4317");
        assert_eq!(telemetry.config.service_name, "test-service");
        assert!(!telemetry.config.metrics_enabled);
    }

    #[test]
    fn test_build_span_creation() {
        let telemetry = Telemetry::default();
        let span = telemetry.build_span("//foo:bar");

        span.record_success(1500);
        // No panic means success
    }

    #[test]
    fn test_cache_span_creation() {
        let telemetry = Telemetry::default();
        let span = telemetry.cache_span("get", "key123");

        span.record_hit();
        span.record_miss();
        // No panic means success
    }
}
