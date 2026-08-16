#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::{fixture, make_workspace, require_go};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;

// `heph i labels` resolves the *spec* of every addr the providers list, so a
// listed addr that no provider can `get` used to abort the whole walk with
// `target not found`. The go provider lists a candidate set on purpose — which
// targets a Go package really has is only known once `go list` has run — and a
// directory holding a `go.mod`/`go.sum` and no `.go` file at all has none of
// them. The walk must skip those and still report the labels it did find.
#[tokio::test]
async fn test_labels_over_a_module_with_no_go_files() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("mod_no_go_files")?;
    let ws = make_workspace(dir)?;
    let rs = ws.engine.new_state();
    // Whole-graph walk — exactly what `heph i labels` with no matcher does.
    let labels = ws
        .engine
        .clone()
        .labels(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
        .await?;

    // Not vacuous: the walk really did resolve specs. `go-build` comes from the
    // root package's `build_lib`, `marker` from the BUILD file next to it.
    assert!(
        labels.contains("go-build"),
        "labels must include the go targets' own label: {labels:?}"
    );
    assert!(
        labels.contains("marker"),
        "labels must include the buildfile target's label: {labels:?}"
    );
    Ok(())
}
