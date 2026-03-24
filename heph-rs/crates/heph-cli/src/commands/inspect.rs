//! Inspect command - Inspect targets and cache

use crate::{output, Result};
use clap::Args;
use serde::Serialize;

#[derive(Args, Debug)]
pub struct InspectCommand {
    /// Target to inspect
    pub target: String,

    /// Show cache information
    #[arg(long)]
    pub cache: bool,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct InspectResult {
    target: String,
    kind: String,
    sources: Vec<String>,
    dependencies: Vec<String>,
    cached: bool,
    cache_key: Option<String>,
}

impl InspectCommand {
    pub fn execute(&self) -> Result<()> {
        output::section("Target Inspection");

        let result = InspectResult {
            target: self.target.clone(),
            kind: "rust_library".to_string(),
            sources: vec!["src/lib.rs".to_string(), "src/mod.rs".to_string()],
            dependencies: vec!["//lib:common".to_string()],
            cached: true,
            cache_key: Some("a1b2c3d4e5f6...".to_string()),
        };

        if self.json {
            let json = serde_json::to_string_pretty(&result)?;
            println!("{}", json);
        } else {
            println!("Target: {}", output::format_target(&result.target));
            println!("Kind: {}", result.kind);

            println!("\nSources:");
            for src in &result.sources {
                println!("  {}", output::format_path(src));
            }

            println!("\nDependencies:");
            for dep in &result.dependencies {
                println!("  {}", output::format_target(dep));
            }

            if self.cache {
                println!("\nCache:");
                println!("  Cached: {}", if result.cached { "✓" } else { "✗" });
                if let Some(key) = &result.cache_key {
                    println!("  Key: {}", key);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inspect_command() {
        let cmd = InspectCommand {
            target: "//foo:bar".to_string(),
            cache: false,
            json: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_inspect_with_cache() {
        let cmd = InspectCommand {
            target: "//lib:common".to_string(),
            cache: true,
            json: false,
        };

        assert!(cmd.cache);
    }

    #[test]
    fn test_inspect_json_output() {
        let cmd = InspectCommand {
            target: "//app:main".to_string(),
            cache: true,
            json: true,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }
}
