//! Heph CLI - Command-line interface for the Heph build system
//!
//! This crate provides the CLI interface for Heph, including commands
//! for running builds, querying the build graph, inspecting targets,
//! and managing the cache.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use thiserror::Error;

pub mod commands;
pub mod output;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("Command execution failed: {0}")]
    CommandFailed(String),

    #[error("Target not found: {0}")]
    TargetNotFound(String),

    #[error("Invalid target reference: {0}")]
    InvalidTarget(String),

    #[error("Build failed: {0}")]
    BuildFailed(String),

    #[error("Cache error: {0}")]
    CacheError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CliError>;

/// Heph - A fast, powerful build system
#[derive(Parser, Debug)]
#[command(name = "heph")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Output format (json, plain)
    #[arg(long, value_name = "FORMAT", global = true)]
    pub format: Option<String>,

    /// Working directory
    #[arg(short = 'C', long, value_name = "DIR", global = true)]
    pub directory: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run build targets
    Run(commands::run::RunCommand),

    /// Query the build graph
    Query(commands::query::QueryCommand),

    /// Inspect targets and cache
    Inspect(commands::inspect::InspectCommand),

    /// Clean cache and build artifacts
    Clean(commands::clean::CleanCommand),

    /// Validate build configuration
    Validate(commands::validate::ValidateCommand),

    /// Show system diagnostics
    Doctor(commands::doctor::DoctorCommand),
}

impl Cli {
    /// Execute the CLI command
    pub fn execute(&self) -> Result<()> {
        // Change directory if specified
        if let Some(dir) = &self.directory {
            std::env::set_current_dir(dir)?;
        }

        // Configure output formatting
        output::configure(self.no_color, self.verbose);

        // Execute the command
        match &self.command {
            Commands::Run(cmd) => cmd.execute(),
            Commands::Query(cmd) => cmd.execute(),
            Commands::Inspect(cmd) => cmd.execute(),
            Commands::Clean(cmd) => cmd.execute(),
            Commands::Validate(cmd) => cmd.execute(),
            Commands::Doctor(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        let cli = Cli::parse_from(["heph", "run", "//foo:bar"]);
        assert!(matches!(cli.command, Commands::Run(_)));
        assert!(!cli.verbose);
        assert!(!cli.no_color);
    }

    #[test]
    fn test_cli_verbose() {
        let cli = Cli::parse_from(["heph", "-v", "run", "//foo:bar"]);
        assert!(cli.verbose);
    }

    #[test]
    fn test_cli_no_color() {
        let cli = Cli::parse_from(["heph", "--no-color", "run", "//foo:bar"]);
        assert!(cli.no_color);
    }

    #[test]
    fn test_cli_directory() {
        let cli = Cli::parse_from(["heph", "-C", "/tmp", "run", "//foo:bar"]);
        assert_eq!(cli.directory, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn test_cli_format() {
        let cli = Cli::parse_from(["heph", "--format", "json", "run", "//foo:bar"]);
        assert_eq!(cli.format, Some("json".to_string()));
    }
}
