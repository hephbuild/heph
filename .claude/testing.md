# Testing

- **Every feature and bug fix must have a test.** No exceptions. Tests freeze behavior — if it's not tested, it will regress.
- For bugs: write the failing test first, then fix the code.
- For features: tests define the contract. Write them alongside the implementation, not after.
- Do not write absurd tests that assert something that was just set before — test business logic.
- Do not add tests that assert something is not there during refactoring.

## Where the test goes

Default to the closest-in unit that can prove the behavior. Reach outward only when the inner layer structurally cannot.

| Where | What | Run with |
|---|---|---|
| `#[cfg(test)]` in the crate | pure logic, parsing, a single type's contract | `cargo test <name>` |
| `crates/e2e`, `crates/plugingo-e2e`, `crates/pluginjs-e2e` | engine semantics end to end — providers, drivers, caching, hashing, the graph. Links `heph` in-process and drives the real `Engine`. | `cargo test <name>` |
| `crates/bin-e2e` | **only** what has no in-process form. Spawns the release binary; links no workspace crate. | `e2e` |

### The `crates/bin-e2e` bar

A test belongs there only if an in-process test *cannot* cover it — not merely if it would be more realistic there. It is slow (release build), runs on three platforms in CI, and gates `release`. Keep it light.

Qualifying seams:

- **Dynamic loading** — `dlopen` of a real cdylib, ABI negotiation across the plugin seam, manifest/checksum resolution. In-process tests construct the plugin directly through generics and never cross the seam.
- **The terminal** — the interactive TUI engages only when stderr is a tty, so a linked test always takes the CI line backend and passes vacuously. Assert on vt100-parsed cells, never on raw escape bytes. Terminal restore on exit is the highest-value assertion: a TUI that leaves the alternate screen behind wrecks the user's shell while every non-PTY test stays green.
- **The process** — exit codes, signal handling, and whether the shipped binary launches at all (dyld/glibc resolution, weak-linked libfuse, the macOS libiconv rewrite). These only fail at `execve`.

Not qualifying: engine semantics, cache correctness, provider or driver logic, anything about the graph. Those go in `crates/e2e`, where they run in seconds.

Fixtures there use the harness in `crates/bin-e2e/tests/common/mod.rs` (`Dist`, `Workspace`, `write_manifest`) — a temp workspace with its own `HOME`, self-update and telemetry disabled. Locate artifacts through `Dist`; never hardcode a path into `target/`.

## Load harnesses

`crates/e2e/tests/cache_load.rs` is an instrument, not a test: it measures whether the tokio
runtime keeps making progress under cache load, by heartbeating a 1ms sleep and recording the
gaps. It exists because the failure it looks for — a worker blocked on a sync cache call — burns
no CPU, so a profiler shows nothing while the whole build appears to hang.

It is `#[ignore]`d and must stay that way. `tst` runs on every push; this writes gigabytes and
reports numbers a human reads.

```bash
HEPH_LT_TARGETS=100 HEPH_LT_SIZE_KB=1024 HEPH_LT_WORKERS=10 \
  cargo test --release -p e2e --test cache_load -- --ignored --nocapture --test-threads=1
```

Read the module docs before trusting a number from it. Two traps, both hit in practice: comparing
two commits without **interleaving** the runs invents differences that disappear once the runs
alternate, and `stall_ms` reads like continuous starvation when the median gap is healthy — use
`over_10ms` and `max_us`.

Anything asserting behaviour rather than measuring it is a test and belongs above.

## Test Isolation

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