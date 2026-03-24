//! Multi-target build example
//!
//! This example demonstrates building multiple targets in a single workflow.

use heph::workflow::WorkflowBuilder;

fn main() -> heph::Result<()> {
    println!("Heph Multi-Target Build Example");
    println!("================================\n");

    let temp_dir = std::env::temp_dir().join("heph-multi-target");
    std::fs::create_dir_all(&temp_dir)?;

    // Create workflow
    let workflow = WorkflowBuilder::new(&temp_dir)
        .jobs(8)
        .cache_enabled(true)
        .log_level("info")
        .build()?;

    // Define multiple targets to build
    let targets = vec![
        "//pkg1:lib".to_string(),
        "//pkg2:bin".to_string(),
        "//pkg3:test".to_string(),
        "//pkg4:docs".to_string(),
    ];

    println!("Building {} targets:", targets.len());
    for target in &targets {
        println!("  - {}", target);
    }
    println!();

    // Build all targets
    let stats = workflow.build_targets(&targets)?;

    println!("Build Summary:");
    println!("==============");
    println!("Total targets:      {}", stats.total_targets);
    println!("Successful builds:  {}", stats.successful_targets);
    println!("Failed builds:      {}", stats.failed_targets);
    println!("Cached builds:      {}", stats.cached_targets);
    println!("Total duration:     {}ms", stats.total_duration_ms);
    println!();

    println!("Metrics:");
    println!("  Success rate:     {:.1}%", stats.success_rate() * 100.0);
    println!("  Cache hit rate:   {:.1}%", stats.cache_hit_rate() * 100.0);
    println!();

    if stats.successful_targets == stats.total_targets {
        println!("✓ All builds succeeded!");
    } else {
        println!("⚠ Some builds failed");
    }

    // Clean up
    std::fs::remove_dir_all(&temp_dir)?;

    Ok(())
}
