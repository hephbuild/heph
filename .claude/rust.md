# Rust Code Quality

- Run `cargo clippy -- -D warnings` and `cargo fmt` before committing. Clippy warnings are treated as errors.
- Use `anyhow::Result` for fallible functions at the application layer; define typed errors (like `TargetNotFoundError`) only when callers need to match on the error type.
- Prefer `async_trait` for async trait methods. Async closures passed to the engine's `Memoizer` must return `WrappedError`-wrapped results.
- All new `Provider` and `Driver` implementations must be registered via `Engine::register_provider` / `Engine::register_driver` — the engine owns the registry.
- Avoid `#[allow(unused_*)]` attributes in committed code.
- Always think about performance — allocation and CPU cycles are expensive. Use iterators instead of `Vec` where possible, avoid unnecessary allocations.

## Logging

- **All diagnostics go through `tracing`** (`error!`/`warn!`/`info!`/`debug!`/`trace!`), with structured fields rather than a pre-formatted sentence.
- **No `eprintln!` / `println!`** in engine, driver, provider, plugin, or library code. `println!` is only for command output the user asked for, and must respect the TUI's ownership of stdout. `eprintln!` is only for the diag/panic-render paths that run when no subscriber exists (`src/diag.rs`, `src/commands/errors.rs`) and for test skip messages.
- A plugin cdylib statically links its **own** `tracing` whose global subscriber is never set — install the host sink (`hplugin_sdk::stabby::install_log_sink`) or every plugin log is dropped. Never write to stderr to be seen.

## Plugins

- The author surface is the `hplugin_sdk` re-exports (`provider`, `driver`, `eresult`, `hook`). The transport (`stabby` cdylib today; proto/shm/wasm later) is an opt-in cargo feature — do not reach past the SDK into `plugin-abi` / `plugin-stabby` internals or into host binary internals.
- Only ABI-stable types cross the seam. A panic across it is a non-unwinding **abort**, not an error.
- **Assume nothing about the environment.** No tokio reactor (a cdylib's runtime is a separate instance polled by host workers — timers and IO panic), no installed subscriber, no particular cwd, no tool on `$PATH`, no `$HOME`/`$TMPDIR`/`$USER`, no terminal, no globals shared with the host (allocator, env, statics). Whatever the plugin needs is handed to it, not discovered.

## Portability

Features work the same across unix OSes (Linux, macOS). A per-OS difference is sometimes the right answer, but it is **the user's decision** — never the implementation's and never an agent's. When a design would diverge (`#[cfg(target_os = …)]` semantics split, a Linux-only mechanism, a macOS path that degrades, a one-OS-only dependency), stop and put the choice to the user: what differs, on which OS, what each option costs. Uniform semantics reached by different implementations is fine. Silent divergence is a bug.

## Error Handling

Every fallible call must attach context so errors are traceable end-to-end:

```rust
foo().context("loading foo config")?;
bar(path).with_context(|| format!("reading {path}"))?;
```

- Use `.context("…")` for static messages.
- Use `.with_context(|| format!("…"))` when the message includes runtime values.
- Never return a bare `?` on a call that can fail with a cryptic error (e.g. IO, parse, subprocess).
- Context messages should describe *what* the code was trying to do, not just re-state the error.

## Rust Principles

- Prefer borrowing over cloning. Clone only when ownership transfer is genuinely required.
- Use `&str` / `&[T]` in function signatures unless ownership is needed.
- Avoid `unwrap()` and `expect()` in non-test code. Propagate errors with `?`.
- Prefer `impl Trait` in function arguments over generics where it simplifies signatures.
- Keep types small and composable. Avoid fat structs that accumulate unrelated fields.
- Derive `Debug` on all public types. Derive `Clone` only when callers need it.
- Use `Arc<T>` for shared ownership across async tasks; avoid `Mutex` unless truly needed (prefer message passing or lock-free structures).
- Mark functions `#[inline]` only when profiling justifies it — not preemptively.
- Use `enclose::enclose!` when passing closures into `spawn_blocking`, `spawn`, or combinators that need cloned captures. Prefer `enclose!((expr => alias, var) move || { … })` over manual pre-closure `let x = x.clone()` bindings. Convert `&str` to `String` manually before `enclose!` since `Clone` cannot change the type.