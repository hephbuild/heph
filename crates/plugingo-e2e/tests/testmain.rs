#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the generated `testmain.go` running against the
//! **pinned** Go SDK.
//!
//! The generated runner has two shapes (see `plugingo::gen_testmain`). Without a
//! `TestMain` it ends in `os.Exit(m.Run())` — plain API, safe across releases.
//! *With* one it calls `TestMain(m)` and then digs the status out by reflection:
//!
//! ```go
//! os.Exit(int(reflect.ValueOf(m).Elem().FieldByName("exitCode").Int()))
//! ```
//!
//! `exitCode` is an **unexported field of `testing.M`** — not API, and nothing
//! promises it survives a Go release. `FieldByName` on a name that no longer
//! exists returns the zero `Value`, whose `Int()` panics; a rename to a field
//! that happens to exist would report the wrong status. Either way the failure
//! arrives on a Go upgrade, not on a heph change.
//!
//! Nothing else covers it. Every other test in this suite that *runs* a test
//! binary uses the host `go` (whatever `pkgs.go` currently is), which lags the
//! version the provider pins — so the pinned SDK's `testing.M` layout was never
//! exercised — and no other fixture in the tree declares a `TestMain` at all.
//!
//! Both directions are asserted in one test on purpose. A reflection read that
//! silently yielded 0 would make every Go test pass, including the broken ones —
//! the same shape as a race detector that never fires, and worse than not having
//! the feature. Sharing one workspace also keeps this to a single hermetic std
//! build: staging the pinned SDK per workspace is the expensive part (see
//! `common::make_workspace_hermetic`), so the two runs deliberately do not get a
//! fixture each.

mod common;

use common::{fixture, make_workspace_hermetic, require_go};

/// A `TestMain` package under the pinned SDK: passing tests exit 0, failing
/// tests exit non-zero — both routed through the `exitCode` reflection read.
#[tokio::test]
async fn testmain_exit_code_survives_the_pinned_toolchain() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("testmain")?;
    let ws = make_workspace_hermetic(dir)?;

    // Exit 0 must reach heph as success. A panic in the reflection read (the
    // field renamed out from under us) also lands here, as a failed target.
    ws.run("//pass:test@v=host").await?;

    // …and a failing Go test must fail the target. This is the half that catches
    // a reflection read that quietly returns 0: without it, `//pass:test`
    // passing proves nothing — a runner that always exits 0 passes it too.
    let err = ws
        .run("//fail:test@v=host")
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    // Specific on purpose. `err` being merely non-empty proves nothing — a
    // missing target, a broken fixture or a failed compile all land here too.
    // These two assertions pin the mechanism:
    //   - the `--- FAIL:` line says the test binary ran and the Go test failed,
    //     so we reached the runner rather than dying before it;
    //   - `exit status: 1` is the value that came back out through the
    //     `exitCode` field. A reflection read that silently yielded 0 turns this
    //     into a *pass*, which is the failure mode worth catching.
    assert!(
        err.contains("--- FAIL: TestSumIsDeliberatelyWrong"),
        "expected the Go test's own failure in the error, got: {err}"
    );
    assert!(
        err.contains("exit status: 1"),
        "the failing test's exit code must survive the `exitCode` reflection \
         read — a read yielding 0 would report every broken package green. \
         Got: {err}"
    );
    Ok(())
}
