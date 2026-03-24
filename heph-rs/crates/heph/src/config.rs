//! Unified configuration for Heph build system

use crate::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Heph configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HephConfig {
    /// Root directory of the build
    pub root_dir: PathBuf,

    /// Build output directory
    pub output_dir: PathBuf,

    /// Cache directory
    pub cache_dir: PathBuf,

    /// Number of parallel jobs (0 = auto-detect)
    pub jobs: usize,

    /// Cache configuration
    pub cache: CacheConfig,

    /// Observability configuration
    pub observability: ObservabilityConfig,

    /// Plugin directories
    pub plugin_dirs: Vec<PathBuf>,
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Enable caching
    pub enabled: bool,

    /// Maximum cache size in bytes (0 = unlimited)
    pub max_size_bytes: u64,

    /// LRU cache capacity (number of entries)
    pub lru_capacity: usize,
}

/// Observability configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Enable tracing
    pub tracing_enabled: bool,

    /// Enable metrics collection
    pub metrics_enabled: bool,

    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,

    /// OTLP endpoint (optional)
    pub otlp_endpoint: Option<String>,
}

impl HephConfig {
    /// Create a new configuration with defaults
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        let root_dir = root_dir.into();
        Self {
            output_dir: root_dir.join("heph-out"),
            cache_dir: root_dir.join(".heph-cache"),
            root_dir,
            jobs: num_cpus::get(),
            cache: CacheConfig::default(),
            observability: ObservabilityConfig::default(),
            plugin_dirs: vec![],
        }
    }

    /// Load configuration from TOML file
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: HephConfig = toml::from_str(&contents)
            .map_err(|e| crate::HephError::ConfigError(e.to_string()))?;
        Ok(config)
    }

    /// Save configuration to TOML file
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let contents = toml::to_string_pretty(self)
            .map_err(|e| crate::HephError::ConfigError(e.to_string()))?;
        fs::write(path, contents)?;
        Ok(())
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        if !self.root_dir.exists() {
            return Err(crate::HephError::ConfigError(format!(
                "Root directory does not exist: {}",
                self.root_dir.display()
            )));
        }

        if self.jobs == 0 {
            return Err(crate::HephError::ConfigError(
                "Jobs must be greater than 0".to_string(),
            ));
        }

        Ok(())
    }

    /// Ensure all required directories exist
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.output_dir)?;
        fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }
}

impl Default for HephConfig {
    fn default() -> Self {
        Self::new(".")
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size_bytes: 10 * 1024 * 1024 * 1024, // 10 GB
            lru_capacity: 1000,
        }
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            tracing_enabled: true,
            metrics_enabled: true,
            log_level: "info".to_string(),
            otlp_endpoint: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_creation() {
        let config = HephConfig::new("/tmp/test");
        assert_eq!(config.root_dir, PathBuf::from("/tmp/test"));
        assert_eq!(config.output_dir, PathBuf::from("/tmp/test/heph-out"));
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/test/.heph-cache"));
        assert!(config.jobs > 0);
    }

    #[test]
    fn test_config_default() {
        let config = HephConfig::default();
        assert_eq!(config.root_dir, PathBuf::from("."));
        assert!(config.cache.enabled);
        assert!(config.observability.tracing_enabled);
    }

    #[test]
    fn test_cache_config_default() {
        let config = CacheConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_size_bytes, 10 * 1024 * 1024 * 1024);
        assert_eq!(config.lru_capacity, 1000);
    }

    #[test]
    fn test_observability_config_default() {
        let config = ObservabilityConfig::default();
        assert!(config.tracing_enabled);
        assert!(config.metrics_enabled);
        assert_eq!(config.log_level, "info");
        assert!(config.otlp_endpoint.is_none());
    }

    #[test]
    fn test_config_save_load() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("heph.toml");

        let mut config = HephConfig::new(temp_dir.path());
        config.jobs = 8;
        config.cache.lru_capacity = 500;

        config.save(&config_path).unwrap();
        let loaded = HephConfig::from_file(&config_path).unwrap();

        assert_eq!(loaded.jobs, 8);
        assert_eq!(loaded.cache.lru_capacity, 500);
    }

    #[test]
    fn test_config_validate() {
        let temp_dir = TempDir::new().unwrap();
        let config = HephConfig::new(temp_dir.path());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_invalid_root() {
        let config = HephConfig::new("/nonexistent/path/that/does/not/exist");
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_ensure_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let config = HephConfig::new(temp_dir.path());

        assert!(config.ensure_dirs().is_ok());
        assert!(config.output_dir.exists());
        assert!(config.cache_dir.exists());
    }
}
