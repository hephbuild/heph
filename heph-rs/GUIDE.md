# Heph Build System - User Guide

This guide will help you get started with Heph, understand its concepts, and use it effectively for your projects.

## Table of Contents

1. [Getting Started](#getting-started)
2. [Core Concepts](#core-concepts)
3. [BUILD Files](#build-files)
4. [Configuration](#configuration)
5. [Workflow API](#workflow-api)
6. [Caching](#caching)
7. [Observability](#observability)
8. [Advanced Usage](#advanced-usage)
9. [Best Practices](#best-practices)

## Getting Started

### Installation

Add Heph to your Rust project:

```bash
cargo add heph
```

Or build the CLI from source:

```bash
git clone https://github.com/hephbuild/heph
cd heph/heph-rs
cargo build --release -p heph-cli
```

### Your First Build

Create a simple Rust project with Heph:

```rust
// main.rs
use heph::workflow::WorkflowBuilder;

fn main() -> heph::Result<()> {
    let workflow = WorkflowBuilder::new(".")
        .jobs(4)
        .cache_enabled(true)
        .build()?;

    let stats = workflow.build_target("//main:app")?;
    println!("Built in {}ms", stats.total_duration_ms);

    Ok(())
}
```

## Core Concepts

### Targets

A **target** is a buildable unit in Heph. Targets are defined in BUILD files and have:

- **Name**: Unique identifier within a package
- **Sources**: Input files
- **Dependencies**: Other targets this depends on
- **Outputs**: Generated artifacts

Target references use the format `//package:target` or `:target` (for local targets).

### Packages

A **package** is a directory containing a BUILD file. Packages organize related targets.

```
my_project/
├── BUILD           # Package: //
├── src/
│   └── BUILD       # Package: //src
└── lib/
    └── BUILD       # Package: //lib
```

### Dependencies

Targets can depend on other targets:

```python
target(
    name = "app",
    deps = [
        "//lib:common",  # Absolute reference
        ":helpers",      # Local reference
    ]
)
```

### Build Graph

Heph builds a **directed acyclic graph (DAG)** of dependencies:

```
//app:main
    ├─> //lib:common
    │       └─> //lib:utils
    └─> //app:helpers
            └─> //lib:common
```

Heph detects cycles and prevents circular dependencies.

## BUILD Files

BUILD files use Starlark syntax (Python-like) to define build targets.

### Basic Syntax

```python
# Define package
package(name = "my_package")

# Simple target
target(
    name = "binary",
    srcs = ["main.rs"],
    outs = ["binary"],
)

# Target with dependencies
target(
    name = "lib",
    srcs = ["lib.rs"],
    deps = ["//common:utils"],
    outs = ["libmy_lib.rlib"],
)
```

### Glob Patterns

Use `glob()` to match multiple files:

```python
target(
    name = "all_tests",
    srcs = glob(["tests/**/*_test.rs"]),
    deps = [":lib"],
)
```

### Visibility

Control which packages can depend on a target:

```python
target(
    name = "internal",
    srcs = ["internal.rs"],
    visibility = ["//my_package/..."],  # Only this package
)

target(
    name = "public",
    srcs = ["public.rs"],
    visibility = ["//..."],  # All packages
)
```

### Complete Example

```python
package(name = "server")

# Main binary
target(
    name = "server",
    srcs = ["src/main.rs"],
    deps = [
        ":handlers",
        ":database",
        "//lib:common",
    ],
    outs = ["server"],
)

# HTTP handlers
target(
    name = "handlers",
    srcs = glob(["src/handlers/**/*.rs"]),
    deps = [
        ":database",
        "//lib:http",
    ],
    outs = ["libhandlers.rlib"],
)

# Database layer
target(
    name = "database",
    srcs = ["src/db.rs"],
    deps = ["//lib:postgres"],
    outs = ["libdatabase.rlib"],
)

# Integration tests
target(
    name = "integration_tests",
    srcs = glob(["tests/**/*.rs"]),
    deps = [
        ":server",
        "//lib:test_utils",
    ],
    test = True,
)
```

## Configuration

### TOML Configuration

Create `heph.toml` in your project root:

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

plugin_dirs = ["./plugins"]
```

### Programmatic Configuration

```rust
use heph::config::HephConfig;

let mut config = HephConfig::new(".");

// Build settings
config.jobs = 16;
config.output_dir = "build".into();

// Cache settings
config.cache.enabled = true;
config.cache.lru_capacity = 2000;

// Observability settings
config.observability.log_level = "debug".to_string();
config.observability.tracing_enabled = true;

// Save to file
config.save("heph.toml")?;
```

### Environment Variables

Override configuration with environment variables:

- `HEPH_JOBS`: Number of parallel jobs
- `HEPH_CACHE_DIR`: Cache directory
- `RUST_LOG`: Log level (overrides config)

## Workflow API

### Building Targets

```rust
use heph::workflow::WorkflowBuilder;

let workflow = WorkflowBuilder::new(".")
    .jobs(8)
    .cache_enabled(true)
    .build()?;

// Single target
let stats = workflow.build_target("//app:main")?;

// Multiple targets
let targets = vec![
    "//app:main".to_string(),
    "//lib:common".to_string(),
];
let stats = workflow.build_targets(&targets)?;
```

### Build Statistics

```rust
let stats = workflow.build_target("//app:main")?;

println!("Total targets:     {}", stats.total_targets);
println!("Successful:        {}", stats.successful_targets);
println!("Failed:            {}", stats.failed_targets);
println!("Cached:            {}", stats.cached_targets);
println!("Duration:          {}ms", stats.total_duration_ms);
println!("Success rate:      {:.1}%", stats.success_rate() * 100.0);
println!("Cache hit rate:    {:.1}%", stats.cache_hit_rate() * 100.0);
```

### Build Context

Access low-level components:

```rust
let context = workflow.context();

// Configuration
let jobs = context.config().jobs;

// Cache operations
if context.is_cached("some-key") {
    println!("Cache hit!");
}

// Event recording
context.record_build("//app:main", 1500, true);
context.record_cache_event(true, "key");

// BUILD file parsing
let build_file = context.parse_build_file("BUILD".into())?;
```

## Caching

Heph uses content-addressable caching to skip rebuilding unchanged targets.

### How It Works

1. **Hash inputs**: Source files and dependencies are hashed (SHA-256)
2. **Check cache**: If hash exists in cache, skip build
3. **Build & store**: Otherwise, build and cache the output
4. **Reuse**: Future builds with same inputs use cached output

### Cache Keys

```rust
use heph::cache::CacheKey;

// From bytes
let key = CacheKey::from_bytes(b"source content");

// From multiple inputs
let key = CacheKey::from_inputs(&[
    b"file1.rs",
    b"file2.rs",
]);

// Use in cache
cache.set(&key, output_data)?;
let cached = cache.get(&key)?;
```

### Cache Management

```rust
let cache = context.cache().unwrap();
let cache_lock = cache.lock().unwrap();

// Check existence
if cache_lock.exists(&key)? {
    println!("Cache hit");
}

// Get statistics
let stats = cache_lock.stats();
println!("Hits: {}", stats.hits);
println!("Misses: {}", stats.misses);
println!("Size: {} bytes", stats.total_size);
```

### Cache Configuration

```toml
[cache]
enabled = true
max_size_bytes = 10737418240  # 10 GB
lru_capacity = 1000            # LRU entries
```

## Observability

Heph includes built-in tracing and metrics.

### Tracing

Structured logging with spans:

```rust
use heph::observability::Observability;

let mut obs = Observability::default();
obs.initialize()?;

// Record events
obs.record_build("//app:main", 1500, true);
obs.record_cache_event(true, "cache-key");
```

### Metrics

Track build performance:

```rust
let metrics = obs.metrics().lock().unwrap();

println!("Total builds:      {}", metrics.total_builds());
println!("Successful:        {}", metrics.successful_builds());
println!("Failed:            {}", metrics.failed_builds());
println!("Cache hits:        {}", metrics.cache_hits());
println!("Cache misses:      {}", metrics.cache_misses());
println!("Cache hit rate:    {:.1}%", metrics.cache_hit_rate() * 100.0);
println!("Success rate:      {:.1}%", metrics.success_rate() * 100.0);
```

### OpenTelemetry

Export telemetry to OTLP-compatible backends:

```toml
[observability]
otlp_endpoint = "http://localhost:4317"
```

## Advanced Usage

### Custom Plugins

Extend Heph with plugins:

```rust
use heph::plugins::{Plugin, PluginRequest, PluginResponse};

struct MyPlugin;

impl Plugin for MyPlugin {
    fn execute(&self, req: PluginRequest) -> PluginResponse {
        // Custom build logic
        PluginResponse::success(outputs)
    }
}
```

### Parallel Execution

Control parallelism:

```rust
let workflow = WorkflowBuilder::new(".")
    .jobs(num_cpus::get())  // Use all CPUs
    .build()?;
```

### Error Handling

```rust
use heph::HephError;

match workflow.build_target("//app:main") {
    Ok(stats) => println!("Success"),
    Err(HephError::ParseError(e)) => eprintln!("Parse error: {}", e),
    Err(HephError::CacheError(e)) => eprintln!("Cache error: {}", e),
    Err(HephError::BuildError(e)) => eprintln!("Build error: {}", e),
    Err(e) => eprintln!("Unexpected error: {}", e),
}
```

## Best Practices

### 1. Organize Code into Packages

```
project/
├── BUILD
├── src/
│   └── BUILD
├── lib/
│   └── BUILD
└── tests/
    └── BUILD
```

### 2. Use Descriptive Target Names

```python
# Good
target(name = "user_service")
target(name = "integration_tests")

# Bad
target(name = "svc")
target(name = "test")
```

### 3. Minimize Dependencies

Only depend on what you actually need:

```python
# Good
deps = ["//lib:database"]

# Bad (too broad)
deps = ["//lib:all"]
```

### 4. Enable Caching

Always enable caching for faster incremental builds:

```toml
[cache]
enabled = true
```

### 5. Use Glob Carefully

Be specific to avoid unnecessary rebuilds:

```python
# Good
srcs = glob(["src/**/*.rs"])

# Bad (too broad, includes build outputs)
srcs = glob(["**/*"])
```

### 6. Set Appropriate Visibility

```python
# Internal implementation
target(
    name = "internal_utils",
    visibility = ["//my_package/..."],
)

# Public API
target(
    name = "public_api",
    visibility = ["//..."],
)
```

### 7. Monitor Build Performance

```rust
let stats = workflow.build_targets(&targets)?;

if stats.cache_hit_rate() < 0.5 {
    println!("Warning: Low cache hit rate");
}

if stats.total_duration_ms > 60000 {
    println!("Warning: Build took over 1 minute");
}
```

### 8. Use Structured Logging

```toml
[observability]
tracing_enabled = true
log_level = "info"
```

## Next Steps

- Explore [examples](crates/heph/examples/)
- Read API documentation: `cargo doc --open`
- Check [migration status](migration/STATUS.md)
- Report issues on GitHub

## Help & Support

- Documentation: `cargo doc -p heph --open`
- Examples: `cargo run -p heph --example simple_build`
- Issues: https://github.com/hephbuild/heph/issues

---

Happy building with Heph! 🚀
