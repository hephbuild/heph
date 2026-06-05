# Architecture

**heph** is a build/task execution engine with a provider/driver plugin model.

## Core Concepts

- **`Addr`** (`src/htaddr/`) — target address in `//package:name` format. The fundamental identifier for any target.
- **`Matcher`** (`src/htmatcher/`) — a composable query predicate (by addr, label, package, prefix, or boolean combinations) used to select targets.
- **`Provider`** (`src/engine/provider.rs`) — a source of target definitions. Implements `list`, `list_packages`, `get`, and `probe`. The `pluginbuildfile` provider discovers packages by walking the filesystem for `BUILD` files and evaluates them as Starlark.
- **`Driver`** (`src/engine/driver.rs`) — executes targets. Given a `TargetSpec` from a provider, a driver `parse`s it into a `TargetDef` (with inputs/outputs/sandbox config), then `run`s it to produce `OutputArtifact`s.
- **`Engine`** (`src/engine/engine.rs`) — holds the provider and driver registries plus a local cache. Entry point for all queries and execution.
- **`RequestState`** (`src/engine/request_state.rs`) — per-request context: cancellation token, and `Memoizer` instances for deduplicating in-flight `result` and `execute` calls. Dropped automatically cleans up the request registry.

## Execution Flow

```
Engine::result(addr)
  → Engine::get_spec(addr)    # asks each Provider::get() in order
  → Engine::get_def(addr)     # Driver::parse(TargetSpec) → TargetDef
  → Engine::execute()         # Driver::run(TargetDef) → OutputArtifacts
  → Engine::cache_locally()   # writes to .heph3/cache/ if target.cache == true
```

Input hash is computed before `execute()`. Cache hit skips `Driver::run` entirely.

## Target Model

Targets are **isolated** and **side-effect-free**:

- **Isolation** — each target runs inside a sandbox. It sees only its declared inputs; no ambient filesystem access, no implicit dependencies.
- **No side effects** — targets must not write outside their declared output paths.
- **Automatic hashing** — content hash of all declared inputs computed before execution. Hash match = cache hit, skip execution.
- **Reproducibility** — same inputs must always produce same outputs. No timestamps, random seeds, or host-specific paths in outputs.

When implementing a new `Driver`:
1. Declare every file, env var, and tool version the target reads as an input.
2. Declare every file the target writes as an output.
3. Do not read or write anything outside those declarations.

## Proto / Codegen

Protobuf definitions live in `proto/`. Generated Rust code is output to `gen/proto/src/` via the `gen` devenv command. The `gen/proto` crate is a workspace member and is imported as `heph-proto-gen` with the `proto_full` feature.

## Workspace Layout

- `src/engine/` — core engine, provider/driver traits, query, result, caching
- `src/pluginbuildfile/` — filesystem `Provider` that reads Starlark `BUILD` files
- `src/pluginexec/` — `Driver` that executes targets as subprocesses
- `src/htaddr/`, `src/htmatcher/` — address and matcher parsing
- `src/hasync/` — cancellation token abstraction over `tokio`
- `src/hmemoizer/` — async memoizer for deduplicating concurrent requests
- `src/commands/` — CLI subcommands (`run`, `inspect`, `bootstrap`)
- `gen/proto/` — generated protobuf crate
- `proto/` — `.proto` source files