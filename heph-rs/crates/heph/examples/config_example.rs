//! Configuration example
//!
//! This example demonstrates how to create, save, and load
//! Heph configuration files.

use heph::config::HephConfig;

fn main() -> heph::Result<()> {
    println!("Heph Configuration Example");
    println!("==========================\n");

    // Create a temporary directory for this example
    let temp_dir = std::env::temp_dir().join("heph-config-example");
    std::fs::create_dir_all(&temp_dir)?;

    let config_path = temp_dir.join("heph.toml");

    // Create a custom configuration
    let mut config = HephConfig::new(&temp_dir);
    config.jobs = 8;
    config.cache.lru_capacity = 500;
    config.cache.max_size_bytes = 5 * 1024 * 1024 * 1024; // 5 GB
    config.observability.log_level = "debug".to_string();
    config.observability.otlp_endpoint = Some("http://localhost:4317".to_string());

    println!("Created configuration:");
    println!("  Root dir: {}", config.root_dir.display());
    println!("  Jobs: {}", config.jobs);
    println!("  Cache enabled: {}", config.cache.enabled);
    println!("  Cache capacity: {}", config.cache.lru_capacity);
    println!("  Log level: {}", config.observability.log_level);
    println!();

    // Save configuration to file
    println!("Saving configuration to: {}", config_path.display());
    config.save(&config_path)?;
    println!("Configuration saved successfully!\n");

    // Load configuration from file
    println!("Loading configuration from file...");
    let loaded_config = HephConfig::from_file(&config_path)?;

    println!("Loaded configuration:");
    println!("  Root dir: {}", loaded_config.root_dir.display());
    println!("  Jobs: {}", loaded_config.jobs);
    println!("  Cache enabled: {}", loaded_config.cache.enabled);
    println!("  Cache capacity: {}", loaded_config.cache.lru_capacity);
    println!("  Log level: {}", loaded_config.observability.log_level);
    println!();

    // Verify values match
    assert_eq!(config.jobs, loaded_config.jobs);
    assert_eq!(config.cache.lru_capacity, loaded_config.cache.lru_capacity);
    println!("✓ Configuration loaded successfully!");

    // Show the TOML content
    println!("\nGenerated TOML:");
    println!("{}", std::fs::read_to_string(&config_path)?);

    // Clean up
    std::fs::remove_dir_all(&temp_dir)?;

    Ok(())
}
