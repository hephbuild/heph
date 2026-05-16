# Testing

- **Every feature and bug fix must have a test.** No exceptions. Tests freeze behavior — if it's not tested, it will regress.
- For bugs: write the failing test first, then fix the code.
- For features: tests define the contract. Write them alongside the implementation, not after.
- Do not write absurd tests that assert something that was just set before — test business logic.
- Do not add tests that assert something is not there during refactoring.

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