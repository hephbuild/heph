//! Build context for managing build state

use crate::config::HephConfig;
use crate::{cache, observability, starlark, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Build context that manages all components
pub struct BuildContext {
    /// Configuration
    config: HephConfig,

    /// Observability system
    observability: Arc<Mutex<observability::Observability>>,

    /// Cache system
    cache: Option<Arc<Mutex<cache::LocalCache>>>,

    /// Starlark runtime
    starlark_runtime: Arc<Mutex<starlark::StarlarkRuntime>>,
}

impl BuildContext {
    /// Create a new build context
    pub fn new(config: HephConfig) -> Result<Self> {
        // Initialize observability
        let obs_config = observability::ObservabilityConfig {
            tracing_enabled: config.observability.tracing_enabled,
            metrics_enabled: config.observability.metrics_enabled,
            otlp_endpoint: config.observability.otlp_endpoint.clone(),
            service_name: "heph".to_string(),
            log_level: config.observability.log_level.clone(),
        };

        let mut obs = observability::Observability::new(obs_config);
        obs.initialize()
            .map_err(|e| crate::HephError::BuildError(e.to_string()))?;

        // Initialize cache if enabled
        let cache_instance = if config.cache.enabled {
            let cache_db = config.cache_dir.join("cache.db");
            Some(Arc::new(Mutex::new(
                cache::LocalCache::new(cache_db)
                    .map_err(crate::HephError::CacheError)?,
            )))
        } else {
            None
        };

        // Initialize Starlark runtime
        let starlark_runtime = starlark::StarlarkRuntime::new();

        Ok(Self {
            config,
            observability: Arc::new(Mutex::new(obs)),
            cache: cache_instance,
            starlark_runtime: Arc::new(Mutex::new(starlark_runtime)),
        })
    }

    /// Get the configuration
    pub fn config(&self) -> &HephConfig {
        &self.config
    }

    /// Get the observability system
    pub fn observability(&self) -> Arc<Mutex<observability::Observability>> {
        self.observability.clone()
    }

    /// Get the cache system
    pub fn cache(&self) -> Option<Arc<Mutex<cache::LocalCache>>> {
        self.cache.clone()
    }

    /// Get the Starlark runtime
    pub fn starlark_runtime(&self) -> Arc<Mutex<starlark::StarlarkRuntime>> {
        self.starlark_runtime.clone()
    }

    /// Parse a BUILD file
    pub fn parse_build_file(&self, path: PathBuf) -> Result<starlark::buildfile::BuildFile> {
        let parser = starlark::buildfile::BuildFileParser::new();
        let build_file = parser.parse(&path)?;
        Ok(build_file)
    }

    /// Check if a target is cached
    pub fn is_cached(&self, key: &str) -> bool {
        if let Some(cache) = &self.cache {
            let cache_lock = cache.lock().unwrap();
            let cache_key = cache::CacheKey::from_bytes(key.as_bytes());
            cache_lock.exists(&cache_key).unwrap_or(false)
        } else {
            false
        }
    }

    /// Record a build event
    pub fn record_build(&self, target: &str, duration_ms: u64, success: bool) {
        let obs = self.observability.lock().unwrap();
        obs.record_build(target, duration_ms, success);
    }

    /// Record a cache event
    pub fn record_cache_event(&self, hit: bool, key: &str) {
        let obs = self.observability.lock().unwrap();
        obs.record_cache_event(hit, key);
    }

    /// Shutdown the context
    pub fn shutdown(&self) {
        let obs = self.observability.lock().unwrap();
        obs.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(temp_dir: &TempDir) -> HephConfig {
        let mut config = HephConfig::new(temp_dir.path());
        config.observability.tracing_enabled = false; // Disable tracing in tests
        config
    }

    #[test]
    fn test_context_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);

        let context = BuildContext::new(config);
        assert!(context.is_ok());
    }

    #[test]
    fn test_context_with_cache_enabled() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = test_config(&temp_dir);
        config.cache.enabled = true;
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        assert!(context.cache().is_some());
    }

    #[test]
    fn test_context_with_cache_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = test_config(&temp_dir);
        config.cache.enabled = false;

        let context = BuildContext::new(config).unwrap();
        assert!(context.cache().is_none());
    }

    #[test]
    fn test_context_config_access() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        let jobs = config.jobs;

        let context = BuildContext::new(config).unwrap();
        assert_eq!(context.config().jobs, jobs);
    }

    #[test]
    fn test_context_record_build() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);

        let context = BuildContext::new(config).unwrap();
        context.record_build("//foo:bar", 1500, true);
        // No panic means success
    }

    #[test]
    fn test_context_record_cache_event() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);

        let context = BuildContext::new(config).unwrap();
        context.record_cache_event(true, "key123");
        // No panic means success
    }

    #[test]
    fn test_context_is_cached_no_cache() {
        let temp_dir = TempDir::new().unwrap();
        let mut config = test_config(&temp_dir);
        config.cache.enabled = false;

        let context = BuildContext::new(config).unwrap();
        assert!(!context.is_cached("somekey"));
    }
}
