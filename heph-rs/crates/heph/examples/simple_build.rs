//! Simple build workflow example
//!
//! This example demonstrates how to create a basic build workflow
//! using the Heph integration layer.

use heph::workflow::WorkflowBuilder;

fn main() -> heph::Result<()> {
    println!("Heph Simple Build Example");
    println!("=========================\n");

    // Create a temporary directory for this example
    let temp_dir = std::env::temp_dir().join("heph-example");
    std::fs::create_dir_all(&temp_dir)?;

    // Create a workflow with custom configuration
    let workflow = WorkflowBuilder::new(&temp_dir)
        .jobs(4)
        .cache_enabled(true)
        .log_level("info")
        .build()?;

    println!("Workflow created successfully!");
    println!("Configuration:");
    println!("  Root dir: {}", workflow.context().config().root_dir.display());
    println!("  Jobs: {}", workflow.context().config().jobs);
    println!("  Cache enabled: {}", workflow.context().config().cache.enabled);
    println!();

    // Build a single target
    println!("Building target: //example:hello");
    let stats = workflow.build_target("//example:hello")?;

    println!("\nBuild completed!");
    println!("  Total targets: {}", stats.total_targets);
    println!("  Successful: {}", stats.successful_targets);
    println!("  Failed: {}", stats.failed_targets);
    println!("  Cached: {}", stats.cached_targets);
    println!("  Duration: {}ms", stats.total_duration_ms);
    println!("  Success rate: {:.1}%", stats.success_rate() * 100.0);
    println!("  Cache hit rate: {:.1}%", stats.cache_hit_rate() * 100.0);

    // Clean up
    std::fs::remove_dir_all(&temp_dir)?;

    Ok(())
}
