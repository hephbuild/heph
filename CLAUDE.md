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
cargo test <test_name>               # run a single test by name

tst                                  # run all tests
lint                                 # lint
fix                                  # format & apply lint fixes
gen                                  # regenerate protobuf bindings (runs buf generate)
```

The `gen` script is a devenv-provided alias, assume its present. It must be run at the beginning of all sessions, or after any `.proto` file changes before building.

## Code Quality

- Run `cargo clippy -- -D warnings` and `cargo fmt` before committing. Clippy warnings are treated as errors.
- Use `anyhow::Result` for fallible functions at the application layer; define typed errors (like `TargetNotFoundError`) only when callers need to match on the error type.
- Prefer `async_trait` for async trait methods. Async closures passed to the engine's `Memoizer` must return `WrappedError`-wrapped results.
- All new `Provider` and `Driver` implementations must be registered via `Engine::register_provider` / `Engine::register_driver` — the engine owns the registry.
- Avoid `#[allow(unused_*)]` attributes in committed code. The `dev.sh` script sets these flags only for the interactive dev workflow.
- Always think about performance, this is a high-performance system, allocation and CPU cycles are expensive (use iterators instead of `Vec` where possible, avoid unnecessary allocations).

### Error handling

Every fallible call must attach context so errors are traceable end-to-end:

```rust
// always chain context
foo().context("loading foo config")?;
bar(path).with_context(|| format!("reading {path}"))?;
```

- Use `.context("…")` for static messages.
- Use `.with_context(|| format!("…"))` when the message includes runtime values.
- Never return a bare `?` on a call that can fail with a cryptic error (e.g. IO, parse, subprocess).
- Context messages should describe *what* the code was trying to do, not just re-state the error.

### Rust principles

- Prefer borrowing over cloning. Clone only when ownership transfer is genuinely required.
- Use `&str` / `&[T]` in function signatures unless ownership is needed.
- Avoid `unwrap()` and `expect()` in non-test code. Propagate errors with `?`.
- Prefer `impl Trait` in function arguments over generics where it simplifies signatures.
- Keep types small and composable. Avoid fat structs that accumulate unrelated fields.
- Derive `Debug` on all public types. Derive `Clone` only when callers need it.
- Use `Arc<T>` for shared ownership across async tasks; avoid `Mutex` unless truly needed (prefer message passing or lock-free structures).
- Mark functions `#[inline]` only when profiling justifies it — not preemptively.
- Use `enclose::enclose!` when passing closures into `spawn_blocking`, `spawn`, or combinators that need cloned captures. Prefer `enclose!((expr => alias, var) move || { … })` over manual pre-closure `let x = x.clone()` bindings. Convert `&str` to `String` manually before `enclose!` since `Clone` cannot change the type.

## Target model

Targets in heph are **isolated** and **side-effect-free**:

- **Isolation** — each target runs inside a sandbox. It sees only its declared inputs; no ambient filesystem access, no implicit dependencies.
- **No side effects** — targets must not write outside their declared output paths. The engine treats all outputs as pure functions of the inputs.
- **Automatic hashing** — the engine computes a content hash of all declared inputs before execution. If the hash matches a cached result, execution is skipped entirely. New code must declare inputs/outputs faithfully or caching will silently break.
- **Reproducibility** — same inputs must always produce same outputs. Embed no timestamps, random seeds, or host-specific paths in outputs.

When implementing a new `Driver`:
1. Declare every file, env var, and tool version the target reads as an input.
2. Declare every file the target writes as an output.
3. Do not read or write anything outside those declarations — the sandbox enforces this, but correctness depends on complete declarations.

## Testing

- **Every feature and bug fix must have a test.** No exceptions. Tests freeze behavior — if it's not tested, it will regress.
- For bugs: write the failing test first, then fix the code.
- For features: tests define the contract. Write them alongside the implementation, not after.
- Do not write absurd tests that assert something that was just set before, test business logic
- Do not add tests that assert taht something is not there during refactoring

### Test isolation

Tests that touch the filesystem must use a unique temporary directory scoped to that test — never `/tmp` directly, and never a shared path that bleeds across parallel runs.

Use `tempfile::TempDir` (already in workspace deps):

```rust
let dir = tempfile::tempdir().expect("tempdir");
let path = dir.path();
// dir is dropped (and deleted) at end of scope
```

Rules:
- Never hardcode `/tmp/something` — parallel tests collide.
- Never reuse a path between tests — leftover state causes false passes.
- `TempDir` must be held alive for the full duration of the test (assign to a `let` binding, not a temporary).
- If a test spawns subprocesses or async tasks that write files, ensure `TempDir` outlives them.

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

Input hash is computed before `execute()`. Cache hit skips `Driver::run` entirely.

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
