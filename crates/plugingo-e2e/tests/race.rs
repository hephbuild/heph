#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `test_race` targets.
//!
//! Everything else about race mode — which flags reach `go tool compile`, what
//! the addresses and cache keys look like, which archives the link pulls in — is
//! asserted in `plugin-go`'s unit tests, where it runs in milliseconds. What can
//! only be proven here is the part no spec assertion reaches: that the binary
//! heph builds is *actually instrumented* and reports a real data race.
//!
//! [`race_detects_a_real_data_race`] is the load-bearing one. A `test_race` that
//! silently dropped `-race` somewhere in the pipeline would still build, still
//! run, and still pass — reporting a clean run for racy code, which is worse
//! than not having the feature. That test fails unless TSan is genuinely live.
//!
//! These build the standard library with `-race` (a separate archive set from
//! the ordinary one), so they are slower than the rest of the suite. Two tests
//! rather than one because a detector that fires on everything is as broken as
//! one that never fires — [`race_passes_on_a_clean_package`] pins the other end.

mod common;

use common::{fixture, make_workspace, require_go};

/// A race build of a correctly-synchronised package passes. Proves the race
/// pipeline — `go install -race std`, the instrumented per-package compiles, the
/// `runtime/race` archive seeded into the link, the non-PIE buildmode on linux —
/// produces a binary that runs and exits clean.
#[tokio::test]
async fn race_passes_on_a_clean_package() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("race")?;
    let ws = make_workspace(dir)?;
    ws.run("//clean:test_race@v=host").await?;
    Ok(())
}

/// The one that proves the instrumentation is real: a package with an unguarded
/// concurrent write fails under `test_race`, with the detector's own report in
/// the output.
///
/// The fixture's assertion is deliberately loose (`want >= 1`), so the test
/// cannot fail for any reason *except* the race detector firing.
#[tokio::test]
async fn race_detects_a_real_data_race() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("race")?;
    let ws = make_workspace(dir)?;
    // Empty when the target *succeeded* — which is itself a failure here, and
    // the assertion below reports it.
    let err = ws
        .run("//racy:test_race@v=host")
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    assert!(
        RACE_MARKERS.iter().any(|m| err.contains(m)),
        "test_race must fail with the race detector's report on a package with a \
         real data race (empty means it wrongly passed); got: {err}"
    );
    Ok(())
}

/// Strings that only a ThreadSanitizer report produces, any one of which proves
/// the binary was instrumented and fired.
///
/// It has to be a set rather than one line, because all we get is the **last 10
/// lines** of the process log (`Engine::DEFAULT_LOG_TAIL_LINES`) and which part
/// of the report lands in that window varies with how deep the goroutine stacks
/// in the final report happen to be — deeper stacks on a CI runner pushed
/// `race detected during execution of test` out of it, failing this test while
/// the feature worked perfectly.
///
/// `==================` is the load-bearing one: it delimits every report, so it
/// is always within the last few lines of a race-failing binary. The other two
/// are friendlier to read when they do survive.
const RACE_MARKERS: &[&str] = &[
    // The report's own delimiter — always at or near the end of the output.
    "==================",
    // The banner, when the report is short enough to fit.
    "DATA RACE",
    // `testing`'s summary, when the race is caught during the test rather than
    // at exit.
    "race detected during execution of test",
];

/// The control for [`race_detects_a_real_data_race`]: the *same* racy package
/// passes under the ordinary `test` target. Without this, that test would also
/// pass if the fixture were simply broken — this pins the failure on the
/// instrumentation, and confirms race mode left the ordinary target alone.
#[tokio::test]
async fn ordinary_test_ignores_the_race() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("race")?;
    let ws = make_workspace(dir)?;
    ws.run("//racy:test@v=host").await?;
    Ok(())
}
