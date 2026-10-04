#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::{fixture, make_workspace, require_go};
use futures::TryStreamExt;
use heph::htaddr::Addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::collections::BTreeSet;

// The go provider reports each target's labels from `list`, and a label
// selector trusts that listing to decide membership without resolving the
// spec. So every listed label set must be exactly what `get` returns.
//
// `label(x) || !label(x)` matches every candidate on its listing, and a `query`
// still resolves each listed match — where the engine compares the listed set
// against the resolved spec and fails the walk on any difference. Between them
// the fixtures carry lint/format (a golangci config), a binary, internal tests
// and external (`_test` package) tests, so every name in the go provider's
// label table is covered.
#[tokio::test]
async fn go_list_labels_equal_spec_labels() -> anyhow::Result<()> {
    require_go!();
    let any = Matcher::Or(vec![
        Matcher::Label("go-build".to_string()),
        Matcher::Not(Box::new(Matcher::Label("go-build".to_string()))),
    ]);
    let mut names = BTreeSet::new();
    for name in ["with_dep", "race", "xtest"] {
        let ws = make_workspace(fixture(name)?)?;
        let addrs: Vec<Addr> = ws
            .engine
            .clone()
            .query(
                ws.engine.new_state(),
                &any,
                heph::engine::Discovery::Complete,
            )
            .try_collect()
            .await?;
        names.extend(addrs.iter().map(|a| a.name.clone()));
    }

    // Not vacuous: every family the go provider lists was resolved and checked.
    for want in [
        "_golist",
        "build_lib",
        "build",
        "lint",
        "lint-check",
        "format",
        "format-check",
        "build_test",
        "test",
        "test_race",
        "build_xtest",
        "xtest",
        "xtest_race",
    ] {
        assert!(names.contains(want), "{want} never resolved: {names:?}");
    }
    Ok(())
}

// Go lists a bare `build` in every package and declines it in a library, where
// a BUILD file may define its own. Registered first, go's empty label set must
// not stand in for the BUILD target's: the walk treats an addr two providers
// list differently as unknown and lets the spec decide.
#[tokio::test]
async fn buildfile_target_shadowing_a_go_listing_is_selected_by_its_own_labels()
-> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    std::fs::write(
        dir.path().join("lib").join("BUILD"),
        r#"target(name = "build", driver = "bash", run = "echo hi > $OUT", out = "o.txt", labels = ["ci"])"#,
    )?;
    let ws = common::make_workspace_go_first(dir, true)?;

    let addrs: Vec<Addr> = ws
        .engine
        .clone()
        .query(
            ws.engine.new_state(),
            &Matcher::Label("ci".to_string()),
            heph::engine::Discovery::Complete,
        )
        .try_collect()
        .await?;
    let formatted: Vec<String> = addrs.iter().map(Addr::format).collect();
    assert_eq!(formatted, vec!["//lib:build"]);

    let batch = ws
        .engine
        .clone()
        .result(
            ws.engine.new_state(),
            &Matcher::Label("ci".to_string()),
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await?;
    assert_eq!(batch.ok.len(), 1, "{:?}", batch.errors);
    Ok(())
}

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
        .labels(
            rs,
            &Matcher::PackagePrefix(PkgBuf::from("")),
            heph::engine::Discovery::Complete,
        )
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
