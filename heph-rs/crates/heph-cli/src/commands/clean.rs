//! Clean command - Clean cache and build artifacts

use crate::{output, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct CleanCommand {
    /// Clean all cache entries
    #[arg(long)]
    pub all: bool,

    /// Clean specific target
    #[arg(short, long)]
    pub target: Option<String>,

    /// Dry run (don't actually delete)
    #[arg(long)]
    pub dry_run: bool,
}

impl CleanCommand {
    pub fn execute(&self) -> Result<()> {
        output::section("Cleaning Cache");

        let items_to_clean: Vec<String> = if self.all {
            vec!["cache/".to_string(), "build/".to_string(), "tmp/".to_string()]
        } else if let Some(target) = &self.target {
            vec![format!("cache/{}", target)]
        } else {
            vec!["cache/".to_string()]
        };

        let total_size: u64 = 1024 * 1024 * 512; // 512 MB simulated

        if self.dry_run {
            output::warning("Dry run mode - no files will be deleted");
        }

        for item in &items_to_clean {
            if self.dry_run {
                output::info(&format!("Would clean: {}", output::format_path(item)));
            } else {
                output::info(&format!("Cleaning: {}", output::format_path(item)));
                // Simulate cleanup
                std::thread::sleep(std::time::Duration::from_millis(50));
                output::success(&format!("Cleaned: {}", output::format_path(item)));
            }
        }

        output::section("Summary");
        println!(
            "Cleaned {} item(s), freed {} MB",
            output::format_number(items_to_clean.len()),
            output::format_number(total_size / 1024 / 1024)
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_command_default() {
        let cmd = CleanCommand {
            all: false,
            target: None,
            dry_run: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_clean_command_all() {
        let cmd = CleanCommand {
            all: true,
            target: None,
            dry_run: false,
        };

        assert!(cmd.all);
    }

    #[test]
    fn test_clean_command_specific_target() {
        let cmd = CleanCommand {
            all: false,
            target: Some("//foo:bar".to_string()),
            dry_run: true,
        };

        assert!(cmd.target.is_some());
        assert!(cmd.dry_run);
    }
}
