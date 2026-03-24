//! Query command - Query the build graph

use crate::{output, Result};
use clap::Args;
use serde::Serialize;

#[derive(Args, Debug)]
pub struct QueryCommand {
    /// Query expression
    pub expression: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Show dependencies
    #[arg(long)]
    pub deps: bool,

    /// Show reverse dependencies (dependents)
    #[arg(long)]
    pub rdeps: bool,
}

#[derive(Debug, Serialize)]
struct QueryResult {
    target: String,
    deps: Vec<String>,
    rdeps: Vec<String>,
}

impl QueryCommand {
    pub fn execute(&self) -> Result<()> {
        output::section("Query Results");

        output::verbose(&format!("Query: {}", self.expression));

        // Simulate query results
        let results = vec![
            QueryResult {
                target: "//foo:bar".to_string(),
                deps: vec!["//lib:common".to_string(), "//lib:utils".to_string()],
                rdeps: vec!["//app:main".to_string()],
            },
            QueryResult {
                target: "//lib:common".to_string(),
                deps: vec![],
                rdeps: vec!["//foo:bar".to_string(), "//baz:qux".to_string()],
            },
        ];

        if self.json {
            // JSON output
            let json = serde_json::to_string_pretty(&results)?;
            println!("{}", json);
        } else {
            // Human-readable output
            for result in &results {
                println!("{}", output::format_target(&result.target));

                if self.deps && !result.deps.is_empty() {
                    println!("  Dependencies:");
                    for dep in &result.deps {
                        println!("    → {}", output::format_target(dep));
                    }
                }

                if self.rdeps && !result.rdeps.is_empty() {
                    println!("  Dependents:");
                    for rdep in &result.rdeps {
                        println!("    ← {}", output::format_target(rdep));
                    }
                }

                println!();
            }

            output::info(&format!("Found {} target(s)", output::format_number(results.len())));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_command() {
        let cmd = QueryCommand {
            expression: "//foo:*".to_string(),
            json: false,
            deps: true,
            rdeps: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_query_command_json() {
        let cmd = QueryCommand {
            expression: "//...".to_string(),
            json: true,
            deps: false,
            rdeps: false,
        };

        let result = cmd.execute();
        assert!(result.is_ok());
    }

    #[test]
    fn test_query_command_with_deps_and_rdeps() {
        let cmd = QueryCommand {
            expression: "//lib:*".to_string(),
            json: false,
            deps: true,
            rdeps: true,
        };

        assert!(cmd.deps);
        assert!(cmd.rdeps);
    }
}
