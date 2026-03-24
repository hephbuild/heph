//! Run command - Execute build targets

use crate::{output, CliError, Result};
use clap::Args;
use heph::workflow::WorkflowBuilder;
use std::time::Instant;

#[derive(Args, Debug)]
pub struct RunCommand {
    /// Target references to build
    #[arg(required = true)]
    pub targets: Vec<String>,

    /// Number of parallel jobs
    #[arg(short, long, default_value = "4")]
    pub jobs: usize,

    /// Force rebuild (ignore cache)
    #[arg(short, long)]
    pub force: bool,
}

impl RunCommand {
    pub fn execute(&self) -> Result<()> {
        output::section("Building Targets");

        // Validate targets
        for target in &self.targets {
            if !target.starts_with("//") && !target.starts_with(':') {
                return Err(CliError::InvalidTarget(format!(
                    "Target must start with // or : (got: {})",
                    target
                )));
            }
        }

        output::verbose(&format!("Parallel jobs: {}", self.jobs));
        output::verbose(&format!("Force rebuild: {}", self.force));

        let start_time = Instant::now();

        // Build workflow
        let workflow = WorkflowBuilder::new(".")
            .jobs(self.jobs)
            .cache_enabled(!self.force)
            .build()
            .map_err(|e| CliError::BuildFailed(format!("Failed to create workflow: {}", e)))?;

        // Execute builds
        let mut successful_builds = 0;

        for target in &self.targets {
            output::info(&format!("Building {}", output::format_target(target)));

            match workflow.build_target(target) {
                Ok(stats) => {
                    successful_builds += stats.successful_targets;

                    output::success(&format!(
                        "Built {} ({} targets, {}ms)",
                        output::format_target(target),
                        stats.total_targets,
                        stats.total_duration_ms
                    ));

                    if stats.cached_targets > 0 {
                        output::verbose(&format!(
                            "  Cache hits: {}/{}",
                            stats.cached_targets,
                            stats.total_targets
                        ));
                    }
                }
                Err(e) => {
                    return Err(CliError::BuildFailed(format!(
                        "Failed to build {}: {}",
                        target, e
                    )));
                }
            }
        }

        let elapsed = start_time.elapsed();

        output::section("Build Summary");
        println!(
            "Successfully built {} target(s) in {:.2}s",
            output::format_number(successful_builds),
            elapsed.as_secs_f64()
        );

        if !self.force {
            if let Some(cache) = workflow.context().cache() {
                let cache_lock = cache.lock().map_err(|e| {
                    CliError::CacheError(format!("Failed to lock cache: {}", e))
                })?;
                let cache_stats = cache_lock.stats();

                output::verbose(&format!(
                    "Cache: {} hits, {} misses, {} bytes",
                    cache_stats.hits,
                    cache_stats.misses,
                    cache_stats.total_size
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_command_valid_targets() {
        let cmd = RunCommand {
            targets: vec!["//foo:bar".to_string(), ":baz".to_string()],
            jobs: 4,
            force: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_command_invalid_target() {
        let cmd = RunCommand {
            targets: vec!["invalid".to_string()],
            jobs: 4,
            force: false,
        };

        let result = cmd.execute();
        assert!(result.is_err());
        assert!(matches!(result, Err(CliError::InvalidTarget(_))));
    }

    #[test]
    fn test_run_command_multiple_targets() {
        let cmd = RunCommand {
            targets: vec![
                "//pkg1:target1".to_string(),
                "//pkg2:target2".to_string(),
                ":local".to_string(),
            ],
            jobs: 8,
            force: true,
        };

        assert_eq!(cmd.targets.len(), 3);
        assert_eq!(cmd.jobs, 8);
        assert!(cmd.force);
    }
}
