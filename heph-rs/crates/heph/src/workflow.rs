//! Build workflow and execution

use crate::context::BuildContext;
use crate::{tref, BuildStats, Result};
use std::time::Instant;

/// Build workflow orchestrator
pub struct BuildWorkflow {
    context: BuildContext,
}

impl BuildWorkflow {
    /// Create a new workflow
    pub fn new(context: BuildContext) -> Self {
        Self { context }
    }

    /// Build a single target
    pub fn build_target(&self, target_ref: &str) -> Result<BuildStats> {
        let start = Instant::now();
        let mut stats = BuildStats::new();

        // Parse the target reference
        let target = tref::TargetRef::parse(target_ref)?;

        // Check cache first
        let cache_key = target.format();
        if self.context.is_cached(&cache_key) {
            self.context.record_cache_event(true, &cache_key);
            stats.total_targets = 1;
            stats.cached_targets = 1;
            stats.successful_targets = 1;
            stats.total_duration_ms = start.elapsed().as_millis() as u64;
            return Ok(stats);
        }

        self.context.record_cache_event(false, &cache_key);

        // Execute the build (simplified - real implementation would use engine)
        let build_duration = start.elapsed().as_millis() as u64;
        let success = true; // Simplified

        self.context
            .record_build(&target.format(), build_duration, success);

        stats.total_targets = 1;
        stats.successful_targets = if success { 1 } else { 0 };
        stats.failed_targets = if success { 0 } else { 1 };
        stats.total_duration_ms = build_duration;

        Ok(stats)
    }

    /// Build multiple targets
    pub fn build_targets(&self, target_refs: &[String]) -> Result<BuildStats> {
        let start = Instant::now();
        let mut stats = BuildStats::new();

        for target_ref in target_refs {
            let target_stats = self.build_target(target_ref)?;
            stats.total_targets += target_stats.total_targets;
            stats.successful_targets += target_stats.successful_targets;
            stats.failed_targets += target_stats.failed_targets;
            stats.cached_targets += target_stats.cached_targets;
        }

        stats.total_duration_ms = start.elapsed().as_millis() as u64;
        Ok(stats)
    }

    /// Build with dependencies
    pub fn build_with_deps(&self, target_ref: &str) -> Result<BuildStats> {
        let start = Instant::now();
        let mut stats = BuildStats::new();

        // Parse target reference
        let _target = tref::TargetRef::parse(target_ref)?;

        // In a real implementation, we would:
        // 1. Parse the BUILD file to get dependencies
        // 2. Build a DAG of dependencies
        // 3. Execute builds in topological order
        // 4. Use the engine for parallel execution

        // For now, simplified implementation
        stats.total_targets = 1;
        stats.successful_targets = 1;
        stats.total_duration_ms = start.elapsed().as_millis() as u64;

        Ok(stats)
    }

    /// Query targets matching a pattern
    pub fn query_targets(&self, _pattern: &str) -> Result<Vec<String>> {
        // Parse pattern (e.g., "//pkg:*", "//...")
        // In a real implementation, would search BUILD files
        // For now, return empty list
        Ok(vec![])
    }

    /// Get the build context
    pub fn context(&self) -> &BuildContext {
        &self.context
    }
}

/// Builder for creating workflows with custom configuration
pub struct WorkflowBuilder {
    pub config: crate::config::HephConfig,
}

impl WorkflowBuilder {
    /// Create a new workflow builder
    pub fn new(root_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            config: crate::config::HephConfig::new(root_dir),
        }
    }

    /// Set the number of parallel jobs
    pub fn jobs(mut self, jobs: usize) -> Self {
        self.config.jobs = jobs;
        self
    }

    /// Enable or disable caching
    pub fn cache_enabled(mut self, enabled: bool) -> Self {
        self.config.cache.enabled = enabled;
        self
    }

    /// Set the log level
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.config.observability.log_level = level.into();
        self
    }

    /// Build the workflow
    pub fn build(self) -> Result<BuildWorkflow> {
        let context = BuildContext::new(self.config)?;
        Ok(BuildWorkflow::new(context))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(temp_dir: &TempDir) -> crate::config::HephConfig {
        let mut config = crate::config::HephConfig::new(temp_dir.path());
        config.observability.tracing_enabled = false; // Disable tracing in tests
        config
    }

    #[test]
    fn test_workflow_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        let workflow = BuildWorkflow::new(context);

        assert_eq!(workflow.context().config().jobs, num_cpus::get());
    }

    #[test]
    fn test_build_target() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        let workflow = BuildWorkflow::new(context);

        let stats = workflow.build_target("//foo:bar").unwrap();
        assert_eq!(stats.total_targets, 1);
        // Duration is always >= 0 for u64
    }

    #[test]
    fn test_build_multiple_targets() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        let workflow = BuildWorkflow::new(context);

        let targets = vec!["//foo:bar".to_string(), "//baz:qux".to_string()];
        let stats = workflow.build_targets(&targets).unwrap();

        assert_eq!(stats.total_targets, 2);
    }

    #[test]
    fn test_build_with_deps() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        let workflow = BuildWorkflow::new(context);

        let stats = workflow.build_with_deps("//foo:bar").unwrap();
        assert_eq!(stats.total_targets, 1);
    }

    #[test]
    fn test_query_targets() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(&temp_dir);
        config.ensure_dirs().unwrap();

        let context = BuildContext::new(config).unwrap();
        let workflow = BuildWorkflow::new(context);

        let targets = workflow.query_targets("//...").unwrap();
        assert_eq!(targets.len(), 0); // Empty for now
    }

    #[test]
    fn test_workflow_builder() {
        let temp_dir = TempDir::new().unwrap();

        let mut builder = WorkflowBuilder::new(temp_dir.path());
        builder.config.observability.tracing_enabled = false;
        let workflow = builder
            .jobs(4)
            .cache_enabled(true)
            .log_level("debug")
            .build();

        assert!(workflow.is_ok());
        let wf = workflow.unwrap();
        assert_eq!(wf.context().config().jobs, 4);
        assert!(wf.context().config().cache.enabled);
    }

    #[test]
    fn test_workflow_builder_defaults() {
        let temp_dir = TempDir::new().unwrap();

        let mut builder = WorkflowBuilder::new(temp_dir.path());
        builder.config.observability.tracing_enabled = false;
        let workflow = builder.build();

        assert!(workflow.is_ok());
        let wf = workflow.unwrap();
        assert!(wf.context().config().cache.enabled);
    }
}
