#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::{artifact_paths, fixture, make_workspace, require_go};
use futures::TryStreamExt;
use heph::htaddr::Addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::collections::BTreeSet;

/// The race fixture, with a `pre_run` test state under `//racy` only: its test
/// runners switch to `bash`, `//clean`'s stay `exec`.
fn race_with_pre_run() -> anyhow::Result<tempfile::TempDir> {
    let dir = fixture("race")?;
    std::fs::write(
        dir.path().join("racy").join("BUILD"),
        r#"provider_state(provider = "go", test = {"pre_run": ["true"]})"#,
    )?;
    Ok(dir)
}

// The go provider reports each target's labels, driver and `has_codegen` from
// `list`, and the walks trust a listed No without resolving the spec. So every
// known listed fact must be exactly what `get` and `parse` return — which is
// `heph validate`'s listed-fact check, run here over fixtures that between them
// carry lint/format (a golangci config), a binary, internal tests, external
// (`_test` package) tests and a state chain that varies by package (`pre_run`
// under `//racy`), so every name in the go provider's table is compared, every
// variant included.
#[tokio::test]
async fn go_listed_facts_equal_spec_and_def() -> anyhow::Result<()> {
    require_go!();
    let all = Matcher::PackagePrefix(PkgBuf::from(""));
    let mut names = BTreeSet::new();
    for (name, dir) in [
        ("with_dep", fixture("with_dep")?),
        ("race", race_with_pre_run()?),
        ("xtest", fixture("xtest")?),
    ] {
        let ws = make_workspace(dir)?;
        let mismatches = ws
            .engine
            .clone()
            .listed_fact_mismatches(
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
            if heph::engine::query::skip_unresolvable(&addr, spec)?.is_some() {
                names.insert(addr.name.clone());
            }
        }
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

/// The addrs `m` selects, and every `_golist` that was built on the way —
/// the observable cost of a go `get`, which resolves through its package's
/// `_golist`.
async fn select_counting_golist(
    ws: &htestkit::Workspace,
    m: &Matcher,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(true, Some(tx));
    let specs: Vec<_> = ws
        .engine
        .clone()
        .query_spec(rs.clone(), m, heph::engine::Discovery::Complete)
        .try_collect()
        .await?;
    drop(rs);
    let mut golists = BTreeSet::new();
    while let Ok(ev) = rx.try_recv() {
        if let heph::engine::event::BuildEventKind::ResultStart { addr, .. } = &ev.kind
            && addr.contains(":_golist")
        {
            golists.insert(addr.clone());
        }
    }
    let mut addrs: Vec<String> = specs.iter().map(|s| s.addr.format()).collect();
    addrs.sort();
    Ok((addrs, golists.into_iter().collect()))
}

// C5, C27: `heph auth status` selects `driver("auth.credential")`. Every go
// entry lists its driver, so none is resolved — no `go list` anywhere — and the
// credential BUILD target is still found.
#[tokio::test]
async fn auth_status_never_gets_go() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("with_dep")?;
    std::fs::write(
        dir.path().join("lib").join("BUILD"),
        r#"target(name = "token", driver = "auth.credential")"#,
    )?;
    let ws = make_workspace(dir)?;
    let m = Matcher::And(vec![
        Matcher::PackagePrefix(PkgBuf::from("")),
        Matcher::Driver("auth.credential".to_string()),
    ]);
    let (addrs, golists) = select_counting_golist(&ws, &m).await?;
    assert_eq!(addrs, vec!["//lib:token"]);
    assert!(golists.is_empty(), "go resolved through {golists:?}");
    Ok(())
}

// C9, C10: every go name but `format`/`lint` lists `has_codegen = false`, so
// `tree_output()` drops them from the listing; `format`/`lint` (unknown) reach
// the def, which decides.
#[tokio::test]
async fn tree_output_to_reads_has_codegen() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(fixture("with_dep")?)?;
    let addrs: Vec<Addr> = ws
        .engine
        .clone()
        .query(
            ws.engine.new_state(),
            &Matcher::TreeOutputTo(PkgBuf::from("")),
            heph::engine::Discovery::Complete,
        )
        .try_collect()
        .await?;
    let names: BTreeSet<String> = addrs.iter().map(|a| a.name.clone()).collect();
    assert!(
        names.iter().all(|n| n == "format" || n == "lint"),
        "only format/lint write a tree: {names:?}"
    );
    assert!(names.contains("format"), "{names:?}");
    Ok(())
}

// C25: a `pre_run` state under `//racy` lists the test runners there as
// `bash`, and `exec` under `//clean`; the specs agree (the validate test above).
#[tokio::test]
async fn go_test_driver_follows_pre_run_state() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(race_with_pre_run()?)?;
    // Confirmed matches only: `query` alone also prints the phantom `test`
    // entries go lists in packages with no tests.
    let listed = |d: &str| {
        let ws = &ws;
        let m = Matcher::Driver(d.to_string());
        async move {
            let (addrs, golists) = select_counting_golist(ws, &m).await?;
            let packages = addrs
                .iter()
                .filter_map(|a| heph::htaddr::parse_addr(a).ok())
                .filter(|a| a.name == "test")
                .map(|a| a.package.as_str().to_string())
                .collect::<BTreeSet<_>>();
            anyhow::Ok((packages, golists))
        }
    };
    let (bash, _) = listed("bash").await?;
    assert_eq!(bash, BTreeSet::from(["racy".to_string()]));
    let (exec, _) = listed("exec").await?;
    assert_eq!(exec, BTreeSet::from(["clean".to_string()]));
    Ok(())
}

// C37: go lists `test` in a package with no test files, with facts. A BUILD
// `query("label(go-test)")` sees a listed Yes, `get` declines it, and it is
// not a dep.
#[tokio::test]
async fn go_phantom_test_yes_confirmed_by_get() -> anyhow::Result<()> {
    require_go!();
    let ws = make_workspace(fixture("with_dep")?)?;
    let q = heph::htaddr::parse_addr(&format!(
        "//{}:q@expr=label(go-test)",
        heph::pluginquery::PACKAGE
    ))?;
    let spec = ws
        .engine
        .clone()
        .get_spec(ws.engine.new_state(), &q)
        .await?;
    let deps = format!("{:?}", spec.config.get("deps"));
    assert!(!deps.contains(":test"), "a phantom test is a dep: {deps}");
    Ok(())
}

// C40 (R4): the buildfile provider never lists `has_codegen` — only the exec
// driver's `parse` knows it — so a BUILD codegen target is never a listed No
// for go's source queries, and still reaches the build under trust.
#[tokio::test]
async fn buildfile_codegen_target_reaches_go_src_under_trust() -> anyhow::Result<()> {
    require_go!();
    let dir = fixture("codegen")?;
    let ws = make_workspace(dir)?;
    std::fs::remove_file(ws.dir.path().join("gen.go")).unwrap_or_default();
    // The workspace's engine trusts listed facts by default.
    let result = ws.run("//:build@v=host").await?;
    assert!(!artifact_paths(&result).is_empty(), "no binary was built");
    Ok(())
}

// Go lists a bare `build` (and every `build@<variant>`) in every package and
// declines them in a library, where a BUILD file may define its own — which the
// buildfile provider then resolves for every one of those addrs. Registered
// first, go's empty label set must not stand in for the BUILD target's: every
// listing of the name is merged, they differ, and the spec decides — exactly
// what a walk that ignores listed facts selects.
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

    let mut selected = Vec::new();
    for trust in [
        heph::engine::listed::ListedFactsTrust::Trust,
        heph::engine::listed::ListedFactsTrust::Ignore,
    ] {
        let addrs: Vec<Addr> = ws
            .engine
            .clone()
            .query(
                ws.engine.new_state_with_listed_facts_trust(trust),
                &Matcher::Label("ci".to_string()),
                heph::engine::Discovery::Complete,
            )
            .try_collect()
            .await?;
        selected.push(addrs.iter().map(Addr::format).collect::<Vec<_>>());
    }
    assert_eq!(selected[0], selected[1], "trust changed the selection");
    assert!(
        selected[0].contains(&"//lib:build".to_string()),
        "{selected:?}"
    );
    assert!(
        selected[0].iter().all(|a| a.starts_with("//lib:build")),
        "{selected:?}"
    );

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
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);
    assert!(!batch.ok.is_empty());

    // Validate reports go's listing all the same: it claims facts about an addr
    // the buildfile provider resolves (D13). Nothing else is reported.
    let mismatches = ws
        .engine
        .clone()
        .listed_fact_mismatches(
            ws.engine.new_state(),
            &Matcher::PackagePrefix(PkgBuf::from("")),
            heph::engine::Discovery::Complete,
        )
        .await?;
    let shown: Vec<String> = mismatches.iter().map(ToString::to_string).collect();
    assert!(
        !shown.is_empty(),
        "go's listing of //lib:build is not reported"
    );
    assert!(
        shown
            .iter()
            .all(|m| m.contains("provider `go` lists //lib:build")
                && m.contains("provider `buildfile` resolves it")),
        "{shown:#?}"
    );
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
