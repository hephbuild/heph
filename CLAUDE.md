# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Environment

This project uses [devenv](https://devenv.sh) for reproducible development environments. All development should happen inside the devenv shell.

```bash
devenv shell        # enter the dev shell (provides Rust toolchain, buf, protoc plugins)
```

## Commands

```bash
cargo build                          # build
cargo test                           # run all tests
cargo test <test_name>               # run a single test by name
cargo clippy -- -D warnings          # lint
cargo fmt                            # format
gen                                  # regenerate protobuf bindings (runs buf generate)
```

The `gen` script is a devenv-provided alias, assume its present. It must be run at the beginning of all sessions, or after any `.proto` file changes before building.

## Code Quality

- Run `cargo clippy -- -D warnings` and `cargo fmt` before committing. Clippy warnings are treated as errors.
- Use `anyhow::Result` for fallible functions at the application layer; define typed errors (like `TargetNotFoundError`) only when callers need to match on the error type.
- Prefer `async_trait` for async trait methods. Async closures passed to the engine's `Memoizer` must return `WrappedError`-wrapped results.
- All new `Provider` and `Driver` implementations must be registered via `Engine::register_provider` / `Engine::register_driver` — the engine owns the registry.
- Avoid `#[allow(unused_*)]` attributes in committed code. The `dev.sh` script sets these flags only for the interactive dev workflow.
- Always think about performance, this is a high-performance system, allocation and CPU cycles are expensive (for ex: use iterator instead of Vec where it makes sense, try not to allocate where unecessary)

## Architecture

**rheph** is a build/task execution engine with a provider/driver plugin model.

### Core concepts

- **`Addr`** (`src/htaddr/`) — target address in `//package:name` format. The fundamental identifier for any target.
- **`Matcher`** (`src/htmatcher/`) — a composable query predicate (by addr, label, package, prefix, or boolean combinations) used to select targets.
- **`Provider`** (`src/engine/provider.rs`) — a source of target definitions. Implements `list`, `list_packages`, `get`, and `probe`. The `pluginbuildfile` provider discovers packages by walking the filesystem for `BUILD` files and evaluates them as Starlark.
- **`Driver`** (`src/engine/driver.rs`) — executes targets. Given a `TargetSpec` from a provider, a driver `parse`s it into a `TargetDef` (with inputs/outputs/sandbox config), then `run`s it to produce `OutputArtifact`s.
- **`Engine`** (`src/engine/engine.rs`) — holds the provider and driver registries plus a local cache. Entry point for all queries and execution.
- **`RequestState`** (`src/engine/request_state.rs`) — per-request context: cancellation token, and `Memoizer` instances for deduplicating in-flight `result` and `execute` calls. Dropped automatically cleans up the request registry.

### Execution flow

```
Engine::result(addr)
  → Engine::get_spec(addr)    # asks each Provider::get() in order
  → Engine::get_def(addr)     # Driver::parse(TargetSpec) → TargetDef
  → Engine::execute()         # Driver::run(TargetDef) → OutputArtifacts
  → Engine::cache_locally()   # writes to .heph3/cache/ if target.cache == true
```

### Proto / codegen

Protobuf definitions live in `proto/`. Generated Rust code is output to `gen/proto/src/` via the `gen` devenv command. The `gen/proto` crate is a workspace member and is imported as `rheph-proto-gen` with the `proto_full` feature.

### Workspace layout

- `src/engine/` — core engine, provider/driver traits, query, result, caching
- `src/pluginbuildfile/` — filesystem `Provider` that reads Starlark `BUILD` files
- `src/pluginexec/` — `Driver` that executes targets as subprocesses
- `src/htaddr/`, `src/htmatcher/` — address and matcher parsing
- `src/hasync/` — cancellation token abstraction over `tokio`
- `src/hmemoizer/` — async memoizer for deduplicating concurrent requests
- `src/commands/` — CLI subcommands (`run`, `inspect`, `bootstrap`)
- `gen/proto/` — generated protobuf crate
- `proto/` — `.proto` source files