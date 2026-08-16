//! Real end-to-end coverage for `plugin-js`: a real `Engine`, in-process,
//! against a real npm-installed workspace on disk — the js-plugin analog of
//! `crates/plugingo-e2e`. Unlike Go, `plugin-js` has no hermetic toolchain
//! (`js_test`/`js_typecheck`/`js_bundle`/`js_lint` all resolve their tool
//! from `<workspace_root>/node_modules/.bin/<tool>`, a disclosed
//! non-hermetic escape hatch — see `crates/plugin-js/src/pluginjs/driver_test.rs`'s
//! module doc), so every test here runs a real `npm install` first
//! (`common::npm_fixture`) and needs `npm` on `PATH` (`devenv.nix` provides
//! `pkgs.nodejs_24`; `require_npm!` skips outside the devenv shell, and
//! hard-fails in CI rather than silently passing an empty suite).

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::require_npm;

/// The actual pipeline this whole test proves, end to end, with nothing
/// faked: `Provider::list`/`get` discover `math.test.ts` from a bare
/// `package.json` with no BUILD file at all, `build_test_closure` resolves
/// its `./math` import via heph's own import graph (no BUILD-declared dep),
/// the real `vitest` binary `npm install` just placed at
/// `node_modules/.bin/vitest` runs inside the sandbox, and its JSON reporter
/// output is what decides success.
#[tokio::test]
async fn js_test_passes_a_real_vitest_run() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_vitest")?;
    let ws = common::make_workspace(dir)?;

    let result = ws.run("//:js_test@file=math.test.ts").await?;
    // `js_test` declares no output artifacts (see module docs on
    // `driver_test.rs`) — reaching `Ok` at all, with an empty artifact set,
    // *is* the success signal.
    assert!(
        result.artifacts.is_empty(),
        "js_test produces no output artifacts by design"
    );
    Ok(())
}

/// Mirror of the passing case: a real, genuine vitest assertion failure
/// (`expect(1 + 1).toBe(3)`) must surface as a driver failure with the
/// runner's own diagnostic in it — not a bare "failed" and not a false
/// pass. Asserting on the test's own description (`"fails on purpose"`)
/// rather than vitest's exact diff formatting keeps this robust across
/// vitest versions/reporters while still proving the *real* runner's own
/// output reached the caller, not a canned message.
#[tokio::test]
async fn js_test_fails_and_surfaces_the_real_vitest_output() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_vitest")?;
    let ws = common::make_workspace(dir)?;

    let err = ws
        .run("//:js_test@file=broken.test.ts")
        .await
        .err()
        .expect("a genuine assertion failure must fail the target");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("fails on purpose"),
        "expected the real vitest failure output (naming the failing test), got: {msg}"
    );
    Ok(())
}

/// The whole caching claim this driver's `cache: CacheConfig::on(true)`
/// makes, proven against a real input hash computed from real files on
/// disk — not just the unit-tested `Hash for JsTestDef` impl in isolation.
/// `js_test` has no output artifacts to diff between runs (see the passing
/// test above), so the only way to observe a cache hit is the engine's own
/// `BuildEvent` stream (`common::run_with_events`): the first run must
/// actually execute vitest (`ExecuteStart`), the second, unchanged run must
/// be a `LocalCacheHit` and must NOT execute vitest again.
#[tokio::test]
async fn js_test_result_is_cached_across_unchanged_runs() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_vitest")?;
    let ws = common::make_workspace(dir)?;
    let addr = "//:js_test@file=math.test.ts";

    let (first, first_events) = common::run_with_events(&ws, addr).await?;
    first.expect("first run must pass");
    assert!(
        common::execute_started(&first_events, addr),
        "the first run must actually execute vitest, not somehow start cached"
    );
    assert!(
        !common::is_local_cache_hit(&first_events, addr),
        "the first run cannot be a cache hit — nothing has run yet"
    );

    let (second, second_events) = common::run_with_events(&ws, addr).await?;
    second.expect("second, unchanged run must also pass");
    assert!(
        common::is_local_cache_hit(&second_events, addr),
        "an unchanged input hash must be a cache hit"
    );
    assert!(
        !common::execute_started(&second_events, addr),
        "a cache hit must not re-execute vitest"
    );
    Ok(())
}

/// Editing the test file's own first-party import (`math.ts`, not the test
/// file itself) must invalidate the cache — proving `build_test_closure`
/// really declared it as a hashed `Input`, not just resolved it for staging.
/// A cache key that missed this would silently serve a stale pass after the
/// source it exercises changed underneath it.
#[tokio::test]
async fn js_test_cache_is_invalidated_when_an_imported_file_changes() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_vitest")?;
    let ws = common::make_workspace(dir)?;
    let addr = "//:js_test@file=math.test.ts";

    let (first, _) = common::run_with_events(&ws, addr).await?;
    first.expect("first run must pass");

    // `add` now returns the wrong answer — the test file itself is
    // untouched, only the file it imports changed.
    ws.write_file(
        "math.ts",
        "export function add(a: number, b: number): number {\n  return a + b + 1;\n}\n",
    );

    let (second, second_events) = common::run_with_events(&ws, addr).await?;
    assert!(
        !common::is_local_cache_hit(&second_events, addr),
        "editing math.ts must invalidate js_test's cache, not replay the stale pass"
    );
    let err = second
        .err()
        .expect("the now-broken add() must fail the real assertion");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("adds two numbers"),
        "expected the real vitest failure naming the now-broken test, got: {msg}"
    );
    Ok(())
}

/// `js_lint` end to end: a real `node_modules/.bin/oxlint` (installed by the
/// same real `npm install` `simple_vitest`'s tests rely on) linting a real,
/// clean first-party source file. Reported live bug: `heph r //pkg:js_lint`
/// failed with oxlint's own `No files found to lint` — the driver invoked a
/// real oxlint binary but handed it either no source-file args, or args that
/// didn't resolve to any file actually staged in the sandbox. This proves
/// the whole pipeline — `Provider::list`/`get` discover the package, declare
/// its first-party sources as Inputs, the sandbox stages them, and
/// `JsLintDriver::run` builds oxlint's argv from what was actually staged —
/// produces a real, successful lint of a real file, not a false "0 files"
/// pass or an unwarranted failure.
#[tokio::test]
async fn js_lint_passes_on_a_real_clean_package() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_oxlint")?;
    let ws = common::make_workspace(dir)?;

    let result = ws.run("//:js_lint").await?;
    assert!(
        result.artifacts.is_empty(),
        "js_lint produces no output artifacts by design"
    );
    Ok(())
}

/// Mirror of the passing case: a real oxlint violation (`no-debugger`, one of
/// its default-enabled correctness rules) must fail the target and surface
/// oxlint's own diagnostic — proving oxlint actually saw and linted the
/// staged file, not the "silently 0 files, exit 0" failure mode that would
/// make this reported bug invisible (an older oxlint accepts bogus/missing
/// path args as "nothing to lint" and exits 0; a newer one hard-fails with
/// `No files found to lint`; either way a real violation must be caught).
#[tokio::test]
async fn js_lint_fails_and_surfaces_the_real_oxlint_violation() -> anyhow::Result<()> {
    require_npm!();
    let dir = common::npm_fixture("simple_oxlint_violation")?;
    let ws = common::make_workspace(dir)?;

    let err = ws
        .run("//:js_lint")
        .await
        .err()
        .expect("a real oxlint violation must fail the target");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no-debugger") || msg.contains("debugger"),
        "expected the real oxlint failure naming the violating rule, got: {msg}"
    );
    Ok(())
}
