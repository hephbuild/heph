# Rust Code Quality

- Run `cargo clippy -- -D warnings` and `cargo fmt` before committing. Clippy warnings are treated as errors.
- Use `anyhow::Result` for fallible functions at the application layer; define typed errors (like `TargetNotFoundError`) only when callers need to match on the error type.
- Prefer `async_trait` for async trait methods. Async closures passed to the engine's `Memoizer` must return `WrappedError`-wrapped results.
- All new `Provider` and `Driver` implementations must be registered via `Engine::register_provider` / `Engine::register_driver` — the engine owns the registry.
- Avoid `#[allow(unused_*)]` attributes in committed code.
- Always think about performance — allocation and CPU cycles are expensive. Use iterators instead of `Vec` where possible, avoid unnecessary allocations.

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