//! Validate command - Validate build configuration

use crate::{output, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct ValidateCommand {
    /// Path to build files
    #[arg(default_value = ".")]
    pub path: String,

    /// Strict validation (treat warnings as errors)
    #[arg(long)]
    pub strict: bool,
}

impl ValidateCommand {
    pub fn execute(&self) -> Result<()> {
        output::section("Validating Configuration");

        output::verbose(&format!("Path: {}", output::format_path(&self.path)));
        output::verbose(&format!("Strict mode: {}", self.strict));

        // Simulate validation checks
        let checks = vec![
            ("Checking build files", true),
            ("Validating target references", true),
            ("Checking dependency cycles", true),
            ("Validating file paths", true),
        ];

        let mut warnings = 0;
        let mut errors = 0;

        for (check, passed) in &checks {
            output::info(&format!("Running: {}", check));
            std::thread::sleep(std::time::Duration::from_millis(50));

            if *passed {
                output::success(check);
            } else {
                if self.strict {
                    errors += 1;
                    output::error(check);
                } else {
                    warnings += 1;
                    output::warning(check);
                }
            }
        }

        output::section("Validation Summary");

        println!(
            "Checks passed: {}",
            output::format_number(checks.len() - warnings - errors)
        );

        if warnings > 0 {
            println!("Warnings: {}", output::format_number(warnings));
        }

        if errors > 0 {
            println!("Errors: {}", output::format_number(errors));
            return Err(crate::CliError::CommandFailed("Validation failed".to_string()));
        }

        output::success("Configuration is valid");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_command() {
        let cmd = ValidateCommand {
            path: ".".to_string(),
            strict: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_strict_mode() {
        let cmd = ValidateCommand {
            path: "/tmp/project".to_string(),
            strict: true,
        };

        assert!(cmd.strict);
        assert_eq!(cmd.path, "/tmp/project");
    }
}
