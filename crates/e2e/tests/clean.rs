#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]
//! `heph tool clean` against a real cache, written by a real run.
//!
//! The unit tests in `engine::clean` plant revisions directly; these drive the
//! whole path — build a target, let its cache write land, then evict it and
//! observe that the eviction is real.
//!
//! The oracle throughout is `clean`'s own `revisions_removed` on a *second*
//! call, plus `gc_all`'s view of what is left. Both are destructive, which is
//! what makes them tight: each leaves the cache in the state the previous step
//! should already have produced, so a failure is attributable to one step.

mod common;

use common::Workspace;
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build `addr` through a *fresh* engine over `ws`'s root — the next `heph run`'s
/// view of the same on-disk cache — and don't return until the run's background
/// cache work has drained.
///
/// Both halves matter. A fresh engine because an in-process re-run replays the
/// memoized spec, so a rewritten BUILD file would resolve to the same `hashin`
/// and write no new revision. The drain because the cache write is *not* part of
/// `result_addr`'s future: `bg` is the same counter the CLI blocks on before
/// exiting, and cleaning before it reads zero would race the write this test is
/// trying to evict.
async fn run_and_settle(ws: &Workspace, addr_str: &str) -> anyhow::Result<String> {
    let addr = parse_addr(addr_str)?;
    let engine = ws.reopen()?;
    let bg: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let rs = engine.new_state_full(
        true,
        None,
        Arc::clone(&bg),
        Engine::DEFAULT_LOG_TAIL_LINES,
        None,
    );
    let result = engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    let out = common::artifact_string(&result);
    // The order `heph run`'s locals unwind in, and the order that frees the
    // addr's riding cache read.
    drop(result);
    drop(rs);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while bg.load(Ordering::Acquire) > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "background work never drained"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    Ok(out)
}

/// `heph tool clean <selection>` over a fresh engine, as a separate invocation would.
async fn clean(ws: &Workspace, m: &Matcher) -> anyhow::Result<heph::engine::CleanStats> {
    let engine = ws.reopen()?;
    engine.clone().clean(engine.new_state(), m).await
}

#[tokio::test]
async fn clean_evicts_a_run_s_cache_entry_and_the_next_run_repopulates_it() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        r#"target(name = "t", driver = "bash", run = "printf 'first' > $OUT", out = "out.txt")"#,
    );
    assert_eq!(run_and_settle(&ws, "//c:t").await?, "first");

    let addr = Matcher::Addr(parse_addr("//c:t")?);
    let stats = clean(&ws, &addr).await?;
    assert_eq!(stats.targets_cleaned, 1, "{stats:?}");
    assert_eq!(stats.revisions_removed, 1, "{stats:?}");
    assert_eq!(stats.errored, 0, "{stats:?}");
    assert!(
        stats.bytes_removed > 0,
        "a real artifact was freed: {stats:?}"
    );

    // The eviction is real, not just reported: a second clean finds nothing.
    let again = clean(&ws, &addr).await?;
    assert_eq!(again.revisions_removed, 0, "{again:?}");
    assert_eq!(again.targets_cleaned, 0, "{again:?}");

    // And the target still builds — to the same bytes, since nothing about its
    // inputs changed — writing a fresh revision, which is only possible if the
    // cache really was empty.
    assert_eq!(run_and_settle(&ws, "//c:t").await?, "first");
    let refilled = clean(&ws, &addr).await?;
    assert_eq!(
        refilled.revisions_removed, 1,
        "the rebuild wrote a new revision: {refilled:?}"
    );
    Ok(())
}

#[tokio::test]
async fn clean_removes_the_revision_gc_deliberately_keeps() -> anyhow::Result<()> {
    // The whole reason `clean` exists next to `gc`: gc trims a live target to its
    // `cache.history` newest revisions and keeps them, so it can never evict the
    // entry you actually want gone.
    let ws = Workspace::new();
    ws.write_build_file(
        "keepme",
        r#"target(name = "t", driver = "bash", run = "printf 'x' > $OUT", out = "out.txt")"#,
    );
    run_and_settle(&ws, "//keepme:t").await?;

    let sweeper = ws.reopen()?;
    let gc = sweeper.clone().gc_all(sweeper.new_state()).await?;
    assert_eq!(gc.revisions_removed, 0, "gc reclaims nothing here: {gc:?}");
    assert_eq!(gc.revisions_kept, 1, "gc keeps the newest revision: {gc:?}");

    let stats = clean(&ws, &Matcher::Addr(parse_addr("//keepme:t")?)).await?;
    assert_eq!(
        stats.revisions_removed, 1,
        "clean takes what gc kept: {stats:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_package_matcher_cleans_its_subtree_and_nothing_else() -> anyhow::Result<()> {
    let ws = Workspace::new();
    for pkg in ["app/one", "app/two", "lib"] {
        ws.write_build_file(
            pkg,
            r#"target(name = "t", driver = "bash", run = "printf 'y' > $OUT", out = "out.txt")"#,
        );
    }
    for pkg in ["app/one", "app/two", "lib"] {
        run_and_settle(&ws, &format!("//{pkg}:t")).await?;
    }

    let stats = clean(&ws, &Matcher::PackagePrefix(PkgBuf::from("app"))).await?;
    assert_eq!(stats.targets_cleaned, 2, "{stats:?}");
    assert_eq!(stats.revisions_removed, 2, "{stats:?}");

    // `//lib:t` was not selected, so its entry is still there to be cleaned.
    let rest = clean(&ws, &Matcher::PackagePrefix(PkgBuf::from(""))).await?;
    assert_eq!(
        rest.revisions_removed, 1,
        "exactly the unselected target's entry survived: {rest:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_label_selection_cleans_what_run_would_have_selected() -> anyhow::Result<()> {
    // `heph tool clean test //...` — the form that cannot be answered from cache keys,
    // because a label set only exists after the target is resolved. It must select
    // the same targets `heph run test //...` would.
    let ws = Workspace::new();
    ws.write_build_file(
        "lbl",
        r#"
target(name = "tested", driver = "bash", run = "printf 'a' > $OUT", out = "out.txt", labels = ["test"])
target(name = "plain", driver = "bash", run = "printf 'b' > $OUT", out = "out.txt")
"#,
    );
    run_and_settle(&ws, "//lbl:tested").await?;
    run_and_settle(&ws, "//lbl:plain").await?;

    let by_label = Matcher::And(vec![
        Matcher::Label("test".to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ]);
    let stats = clean(&ws, &by_label).await?;
    assert_eq!(stats.targets_cleaned, 1, "only the labelled one: {stats:?}");
    assert_eq!(stats.revisions_removed, 1, "{stats:?}");

    // `//lbl:plain` carries no `test` label, so its entry is still there.
    let rest = clean(&ws, &Matcher::PackagePrefix(PkgBuf::from(""))).await?;
    assert_eq!(
        rest.revisions_removed, 1,
        "exactly the unlabelled target's entry survived: {rest:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_label_selection_cannot_reach_an_entry_the_graph_no_longer_defines() -> anyhow::Result<()>
{
    // The cost of asking a question only a definition can answer: with the BUILD
    // file gone there is no label set to test, so the entry is unreachable by
    // label — and reachable by the addr-only forms, which is the documented way
    // out. Pinned because it is the one place the two paths genuinely differ.
    let ws = Workspace::new();
    ws.write_build_file(
        "orphan",
        r#"target(name = "t", driver = "bash", run = "printf 'c' > $OUT", out = "out.txt", labels = ["test"])"#,
    );
    run_and_settle(&ws, "//orphan:t").await?;
    std::fs::remove_file(ws.dir.path().join("orphan/BUILD"))?;

    let by_label = Matcher::And(vec![
        Matcher::Label("test".to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ]);
    let missed = clean(&ws, &by_label).await?;
    assert_eq!(missed, heph::engine::CleanStats::default(), "{missed:?}");

    let got = clean(&ws, &Matcher::PackagePrefix(PkgBuf::from("orphan"))).await?;
    assert_eq!(got.revisions_removed, 1, "{got:?}");
    Ok(())
}

#[tokio::test]
async fn clean_evicts_an_entry_whose_target_no_longer_exists() -> anyhow::Result<()> {
    // `clean` resolves nothing, so a cached entry outlives its BUILD file and is
    // still cleanable — which is exactly the state a user reaches by deleting or
    // renaming a target and then wanting its cache gone.
    let ws = Workspace::new();
    ws.write_build_file(
        "gone",
        r#"target(name = "t", driver = "bash", run = "printf 'z' > $OUT", out = "out.txt")"#,
    );
    run_and_settle(&ws, "//gone:t").await?;

    std::fs::remove_file(ws.dir.path().join("gone/BUILD"))?;

    let stats = clean(&ws, &Matcher::PackagePrefix(PkgBuf::from("gone"))).await?;
    assert_eq!(stats.targets_cleaned, 1, "{stats:?}");
    assert_eq!(stats.revisions_removed, 1, "{stats:?}");
    assert_eq!(
        stats.errored, 0,
        "no resolution, so nothing to fail: {stats:?}"
    );
    Ok(())
}
