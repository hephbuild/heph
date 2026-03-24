# Heph - Build System Integration Layer

High-level Rust API for the Heph build system, providing a unified interface to all core components.

## Overview

Heph is a fast, parallel build system with:
- **Content-addressable caching** for incremental builds
- **Parallel execution** across multiple cores
- **BUILD file syntax** using Starlark (Python-like)
- **Observability** with tracing and metrics
- **Plugin system** for extensibility

This crate provides the integration layer that ties all components together into a cohesive API.

## Quick Start

```rust
use heph::workflow::WorkflowBuilder;

fn main() -> heph::Result<()> {
    // Create a build workflow
    let workflow = WorkflowBuilder::new(".")
        .jobs(8)
        .cache_enabled(true)
        .log_level("info")
        .build()?;

    // Build a target
    let stats = workflow.build_target("//my_package:my_target")?;

    println!("Built {} targets in {}ms",
        stats.total_targets,
        stats.total_duration_ms);

    Ok(())
}
```

## Features

### Unified Configuration

Configure the entire build system from a single TOML file:

```toml
root_dir = "."
output_dir = "heph-out"
cache_dir = ".heph-cache"
jobs = 8

[cache]
enabled = true
max_size_bytes = 10737418240  # 10 GB
lru_capacity = 1000

[observability]
tracing_enabled = true
metrics_enabled = true
log_level = "info"
otlp_endpoint = "http://localhost:4317"
```

Load configuration programmatically:

```rust
use heph::config::HephConfig;

// Load from file
let config = HephConfig::from_file("heph.toml")?;

// Or create programmatically
let mut config = HephConfig::new(".");
config.jobs = 16;
config.cache.enabled = true;
config.save("heph.toml")?;
```

### Build Workflows

The `BuildWorkflow` provides high-level build operations:

```rust
// Build a single target
let stats = workflow.build_target("//foo:bar")?;

// Build multiple targets
let targets = vec![
    "//pkg1:lib".to_string(),
    "//pkg2:bin".to_string(),
];
let stats = workflow.build_targets(&targets)?;

// Build with dependencies (coming soon)
let stats = workflow.build_with_deps("//foo:bar")?;

// Query targets (coming soon)
let targets = workflow.query_targets("//...")?;
```

### Build Statistics

Track build performance with `BuildStats`:

```rust
let stats = workflow.build_target("//foo:bar")?;

println!("Total targets: {}", stats.total_targets);
println!("Successful: {}", stats.successful_targets);
println!("Failed: {}", stats.failed_targets);
println!("Cached: {}", stats.cached_targets);
println!("Duration: {}ms", stats.total_duration_ms);
println!("Success rate: {:.1}%", stats.success_rate() * 100.0);
println!("Cache hit rate: {:.1}%", stats.cache_hit_rate() * 100.0);
```

### Build Context

Access low-level components via `BuildContext`:

```rust
let context = workflow.context();

// Access configuration
let config = context.config();
println!("Jobs: {}", config.jobs);

// Check cache
if context.is_cached("some-key") {
    println!("Cache hit!");
}

// Record events
context.record_build("//foo:bar", 1500, true);
context.record_cache_event(true, "cache-key");

// Parse BUILD files
let build_file = context.parse_build_file("BUILD".into())?;
```

## Architecture

The Heph build system consists of multiple crates:

- **heph** (this crate) - Integration layer and high-level API
- **heph-uuid** - Unique identifiers for build artifacts
- **heph-tref** - Target reference parsing (`//pkg:target`)
- **heph-kv** - Key-value store for metadata
- **heph-dag** - Dependency graph with cycle detection
- **heph-pipe** - Async pipeline processing
- **heph-fs** - File system operations
- **heph-engine** - Parallel build execution
- **heph-plugins** - Plugin SDK
- **heph-starlark** - BUILD file parsing (Starlark/Python syntax)
- **heph-cache** - Content-addressable caching
- **heph-observability** - Tracing, metrics, and telemetry
- **heph-cli** - Command-line interface

## Examples

See the [examples directory](examples/) for complete examples:

- [`simple_build.rs`](examples/simple_build.rs) - Basic build workflow
- [`config_example.rs`](examples/config_example.rs) - Configuration management
- [`multi_target.rs`](examples/multi_target.rs) - Building multiple targets

Run examples with:

```bash
cargo run -p heph --example simple_build
cargo run -p heph --example config_example
cargo run -p heph --example multi_target
```

## Error Handling

All operations return `heph::Result<T>` which wraps `HephError`:

```rust
use heph::HephError;

match workflow.build_target("//foo:bar") {
    Ok(stats) => println!("Success: {} targets", stats.total_targets),
    Err(HephError::ParseError(msg)) => eprintln!("Parse error: {}", msg),
    Err(HephError::CacheError(e)) => eprintln!("Cache error: {}", e),
    Err(e) => eprintln!("Error: {}", e),
}
```

## Re-exports

All core components are re-exported for convenience:

```rust
use heph::cache;        // Caching system
use heph::dag;          // Dependency graphs
use heph::engine;       // Build execution
use heph::fs;           // File operations
use heph::kv;           // Key-value store
use heph::observability;// Tracing & metrics
use heph::pipe;         // Pipelines
use heph::plugins;      // Plugin SDK
use heph::starlark;     // BUILD file parsing
use heph::tref;         // Target references
use heph::uuid;         // UUIDs
```

## Testing

Run tests with:

```bash
cargo test -p heph
```

## License

MIT License

## Contributing

Contributions welcome! This is part of the Rust migration of the Heph build system.

## Status

This crate is part of Phase 9 of the Rust migration. Phase 10 (documentation) is in progress.

**Migration Status**: 90% complete (9/10 phases)
