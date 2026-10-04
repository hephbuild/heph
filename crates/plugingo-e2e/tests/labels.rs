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

// The go provider reports each target's labels and driver from `list`, and a
// label selector (or `heph auth`'s walk for `credential`s) trusts that listing
// without resolving the spec. So every listed label set and driver must be
// exactly what `get` returns — which is `heph validate`'s listing check, run
// here over fixtures that between them carry lint/format (a golangci config), a
// binary, internal tests and external (`_test` package) tests, and a package
// whose tests have `pre_run` lines (a `bash` runner rather than `exec`), so
// every name in the go provider's label and driver tables is compared.
#[tokio::test]
async fn go_list_labels_and_drivers_equal_spec() -> anyhow::Result<()> {
    require_go!();
    let all = Matcher::PackagePrefix(PkgBuf::from(""));
    let mut names = BTreeSet::new();
    let mut drivers = BTreeSet::new();
    // The second field is the package, if any, whose tests get `pre_run` lines.
    for (name, pre_run) in [
        ("with_dep", None),
        ("race", None),
        ("race", Some("racy")),
        ("xtest", None),
        ("xtest", Some("lib")),
    ] {
        let dir = fixture(name)?;
        if let Some(pkg) = pre_run {
            std::fs::write(
                dir.path().join(pkg).join("BUILD"),
                r#"provider_state(provider = "go", test = {"pre_run": ["true"]})"#,
            )?;
        }
        let ws = make_workspace(dir)?;
        let mismatches = ws
            .engine
            .clone()
            .listing_mismatches(
                ws.engine.new_state(),
                &all,
                heph::engine::Discovery::Complete,
            )
            .await?;
        assert!(mismatches.is_empty(), "{name}: {mismatches:#?}");

        // Not vacuous: collect the names that really resolve, which are the
        // ones the check compared.
        let addrs: Vec<Addr> = ws
            .engine
            .clone()
            .query(
                ws.engine.new_state(),
                &all,
                heph::engine::Discovery::Complete,
            )
            .try_collect()
            .await?;
        let rs = ws.engine.new_state();
        for addr in addrs {
            let spec = ws.engine.clone().get_spec(rs.clone(), &addr).await;
            if let Some(spec) = heph::engine::query::skip_unresolvable(&addr, spec)? {
                names.insert(addr.name.clone());
                drivers.insert((addr.name.clone(), spec.driver.clone()));
            }
        }
    }

    // Not vacuous: each driver that depends on more than the name — the bare
    // host `build` vs a variant's link, and `pre_run` turning the test runner
    // from `exec` into `bash` — was resolved both ways.
    for (name, driver) in [
        ("build", "group"),
        ("build", "bash"),
        ("xtest", "exec"),
        ("xtest", "bash"),
        ("test_race", "exec"),
        ("test_race", "bash"),
    ] {
        assert!(
            drivers.contains(&(name.to_string(), driver.to_string())),
            "{name} never resolved to `{driver}`: {drivers:?}"
        );
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

    // Nor is go's listing a lie for validate to report: the walk never trusted it.
    let mismatches = ws
        .engine
        .clone()
        .listing_mismatches(
            ws.engine.new_state(),
            &Matcher::PackagePrefix(PkgBuf::from("")),
            heph::engine::Discovery::Complete,
        )
        .await?;
    assert!(mismatches.is_empty(), "{mismatches:#?}");

    // Same for the driver: go lists the bare `build` as its host `group`, the
    // BUILD file's is `bash`. A walk for `bash` targets must not take go's word
    // for it and drop the BUILD target unresolved.
    let bash: Vec<String> = ws
        .engine
        .clone()
        .query_driver(
            ws.engine.new_state(),
            &Matcher::PackagePrefix(PkgBuf::from("lib")),
            "bash",
            heph::engine::Discovery::Complete,
        )
        .map_ok(|s| s.addr.format())
        .try_collect()
        .await?;
    assert!(bash.contains(&"//lib:build".to_string()), "{bash:?}");
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
