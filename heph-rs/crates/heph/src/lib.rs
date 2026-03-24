//! Heph - High-level build system integration
//!
//! This crate provides a unified API for the Heph build system, integrating
//! all core components into a cohesive workflow.
//!
//! # Quick Start
//!
//! ```no_run
//! use heph::workflow::WorkflowBuilder;
//!
//! # fn main() -> heph::Result<()> {
//! // Create a build workflow
//! let workflow = WorkflowBuilder::new(".")
//!     .jobs(8)
//!     .cache_enabled(true)
//!     .log_level("info")
//!     .build()?;
//!
//! // Build a target
//! let stats = workflow.build_target("//my_package:binary")?;
//! println!("Built {} targets in {}ms",
//!     stats.total_targets,
//!     stats.total_duration_ms);
//! # Ok(())
//! # }
//! ```
//!
//! # Features
//!
//! - **Unified Configuration**: Single TOML file for all build settings
//! - **Build Workflows**: High-level API for building targets
//! - **Caching**: Content-addressable caching for incremental builds
//! - **Observability**: Built-in tracing and metrics
//! - **Plugin System**: Extensible via plugins
//!
//! # Architecture
//!
//! The Heph build system consists of several crates:
//!
//! - [`heph`] (this crate) - Integration layer
//! - [`heph-cache`](cache) - Content-addressable caching
//! - [`heph-dag`](dag) - Dependency graph
//! - [`heph-engine`](engine) - Parallel build execution
//! - [`heph-starlark`](starlark) - BUILD file parsing
//! - [`heph-observability`](observability) - Tracing and metrics
//!
//! # Examples
//!
//! ## Simple Build
//!
//! ```no_run
//! use heph::workflow::WorkflowBuilder;
//!
//! # fn main() -> heph::Result<()> {
//! let workflow = WorkflowBuilder::new(".").build()?;
//! let stats = workflow.build_target("//app:main")?;
//! println!("Success rate: {:.1}%", stats.success_rate() * 100.0);
//! # Ok(())
//! # }
//! ```
//!
//! ## Configuration
//!
//! ```no_run
//! use heph::config::HephConfig;
//!
//! # fn main() -> heph::Result<()> {
//! // Load from file
//! let config = HephConfig::from_file("heph.toml")?;
//!
//! // Or create programmatically
//! let mut config = HephConfig::new(".");
//! config.jobs = 16;
//! config.save("heph.toml")?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Multi-target Build
//!
//! ```no_run
//! use heph::workflow::WorkflowBuilder;
//!
//! # fn main() -> heph::Result<()> {
//! let workflow = WorkflowBuilder::new(".").build()?;
//!
//! let targets = vec![
//!     "//pkg1:lib".to_string(),
//!     "//pkg2:bin".to_string(),
//! ];
//!
//! let stats = workflow.build_targets(&targets)?;
//! println!("Cache hit rate: {:.1}%", stats.cache_hit_rate() * 100.0);
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-export core components
pub use heph_cache as cache;
pub use heph_dag as dag;
pub use heph_engine as engine;
pub use heph_fs as fs;
pub use heph_kv as kv;
pub use heph_observability as observability;
pub use heph_pipe as pipe;
pub use heph_plugins as plugins;
pub use heph_starlark as starlark;
pub use heph_tref as tref;
pub use heph_uuid as uuid;

pub mod config;
pub mod context;
pub mod workflow;

#[derive(Error, Debug)]
pub enum HephError {
    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Build error: {0}")]
    BuildError(String),

    #[error("Target not found: {0}")]
    TargetNotFound(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Cache error: {0}")]
    CacheError(#[from] cache::CacheError),

    #[error("DAG error: {0}")]
    DagError(#[from] dag::DagError),

    #[error("Engine error: {0}")]
    EngineError(#[from] engine::EngineError),

    #[error("FS error: {0}")]
    FsError(#[from] fs::FsError),

    #[error("Starlark error: {0}")]
    StarlarkError(#[from] starlark::StarlarkError),

    #[error("Plugin error: {0}")]
    PluginError(#[from] plugins::PluginError),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

impl From<tref::ParseError> for HephError {
    fn from(e: tref::ParseError) -> Self {
        HephError::ParseError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, HephError>;

/// Build statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStats {
    pub total_targets: usize,
    pub successful_targets: usize,
    pub failed_targets: usize,
    pub cached_targets: usize,
    pub total_duration_ms: u64,
}

impl BuildStats {
    pub fn new() -> Self {
        Self {
            total_targets: 0,
            successful_targets: 0,
            failed_targets: 0,
            cached_targets: 0,
            total_duration_ms: 0,
        }
    }

    pub fn success_rate(&self) -> f64 {
        if self.total_targets == 0 {
            0.0
        } else {
            self.successful_targets as f64 / self.total_targets as f64
        }
    }

    pub fn cache_hit_rate(&self) -> f64 {
        if self.total_targets == 0 {
            0.0
        } else {
            self.cached_targets as f64 / self.total_targets as f64
        }
    }
}

impl Default for BuildStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_stats_creation() {
        let stats = BuildStats::new();
        assert_eq!(stats.total_targets, 0);
        assert_eq!(stats.successful_targets, 0);
        assert_eq!(stats.failed_targets, 0);
        assert_eq!(stats.cached_targets, 0);
    }

    #[test]
    fn test_build_stats_success_rate() {
        let mut stats = BuildStats::new();
        stats.total_targets = 10;
        stats.successful_targets = 7;
        assert_eq!(stats.success_rate(), 0.7);
    }

    #[test]
    fn test_build_stats_cache_hit_rate() {
        let mut stats = BuildStats::new();
        stats.total_targets = 10;
        stats.cached_targets = 3;
        assert_eq!(stats.cache_hit_rate(), 0.3);
    }

    #[test]
    fn test_build_stats_zero_division() {
        let stats = BuildStats::new();
        assert_eq!(stats.success_rate(), 0.0);
        assert_eq!(stats.cache_hit_rate(), 0.0);
    }
}
