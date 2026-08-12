// An integration-test target is only ever compiled as a test, so the exemption
// needs no `cfg(test)` gate.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use anyhow::Context as _;
use common::Workspace;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;

fn out_map(out: Option<&str>) -> HashMap<String, Vec<String>> {
    match out {
        Some(o) => HashMap::from([(String::new(), vec![o.to_string()])]),
        None => HashMap::new(),
    }
}

fn target(addr: &str, run: &str, out: Option<&str>) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: out_map(out),
        codegen: None,
        deps: HashMap::new(),
        labels: vec![],
        ..Default::default()
    }
}

fn target_with_deps(
    addr: &str,
    run: &str,
    out: Option<&str>,
    deps: HashMap<String, Vec<String>>,
) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: out_map(out),
        codegen: None,
        deps,
        labels: vec![],
        ..Default::default()
    }
}

fn deps(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
        .collect()
}

// Basic dep: consumer reads dep output via $SRC_<GROUP>
#[tokio::test]
async fn test_dep_output_propagated_via_env() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target("//deps:d1", "echo hello > $OUT", Some("d1.txt")),
        target_with_deps(
            "//deps:consumer",
            "cat $SRC_D1 > $OUT",
            Some("result.txt"),
            deps(&[("d1", "//deps:d1")]),
        ),
    ])?;

    let result = ws.run("//deps:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("hello"), "got: {content:?}");
    Ok(())
}

// $OUT env var is set to the declared output path
#[tokio::test]
async fn test_out_env_var_set() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![target(
        "//vars:t",
        "echo $OUT > $OUT",
        Some("out.txt"),
    )])?;

    let result = ws.run("//vars:t").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("out.txt"),
        "OUT should contain declared filename, got: {content:?}"
    );
    Ok(())
}

// Multiple deps in different groups each get their own $SRC_<GROUP>
#[tokio::test]
async fn test_multiple_dep_groups_separate_env_vars() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target("//multideps:a", "echo aaa > $OUT", Some("a.txt")),
        target("//multideps:b", "echo bbb > $OUT", Some("b.txt")),
        target_with_deps(
            "//multideps:consumer",
            r#"printf '%s %s' "$(cat $SRC_SRCA)" "$(cat $SRC_SRCB)" > $OUT"#,
            Some("result.txt"),
            deps(&[("srca", "//multideps:a"), ("srcb", "//multideps:b")]),
        ),
    ])?;

    let result = ws.run("//multideps:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("aaa"), "missing aaa, got: {content:?}");
    assert!(content.contains("bbb"), "missing bbb, got: {content:?}");
    Ok(())
}

// Transitive deps: base → mid → top, final output contains content from base
#[tokio::test]
async fn test_transitive_deps_resolved() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target("//trans:base", "echo base_value > $OUT", Some("base.txt")),
        target_with_deps(
            "//trans:mid",
            "echo mid_$(cat $SRC_BASE) > $OUT",
            Some("mid.txt"),
            deps(&[("base", "//trans:base")]),
        ),
        target_with_deps(
            "//trans:top",
            "cat $SRC_MID > $OUT",
            Some("top.txt"),
            deps(&[("mid", "//trans:mid")]),
        ),
    ])?;

    let result = ws.run("//trans:top").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("mid_base_value"), "got: {content:?}");
    Ok(())
}

// $SRC_<GROUP> missing when dep not declared → bash -u mode makes it fail
#[tokio::test]
async fn test_undeclared_src_var_fails() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![target(
        "//missing:t",
        "echo $SRC_GHOST > $OUT",
        Some("out.txt"),
    )])?;

    let err = ws.run("//missing:t").await;
    assert!(
        err.is_err(),
        "expected failure when referencing undeclared $SRC_GHOST"
    );
    Ok(())
}

// Dep across packages
#[tokio::test]
async fn test_cross_package_dep() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target("//lib:data", "echo cross_pkg > $OUT", Some("data.txt")),
        target_with_deps(
            "//app:main",
            "cat $SRC_LIB > $OUT",
            Some("result.txt"),
            deps(&[("lib", "//lib:data")]),
        ),
    ])?;

    let result = ws.run("//app:main").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("cross_pkg"), "got: {content:?}");
    Ok(())
}

// A has transitive deps = B; C depends on A → C sees both $SRC_A and $SRC_B
#[tokio::test]
async fn test_transitive_dep_available_in_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "trans",
        r#"
target(name = "b", driver = "bash", run = "printf b_value > $OUT", out = "b.txt")
target(
    name = "a",
    driver = "bash",
    run = "printf a_value > $OUT",
    out = "a.txt",
    transitive = {"deps": {"b": ["//trans:b"]}},
)
target(
    name = "c",
    driver = "bash",
    run = "printf '%s %s' \"$(cat $SRC_A)\" \"$(cat $SRC_B)\" > $OUT",
    out = "c.txt",
    deps = {"a": ["//trans:a"]},
)
"#,
    );

    let result = ws.run("//trans:c").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("a_value"),
        "missing a_value in transitive output, got: {content:?}"
    );
    assert!(
        content.contains("b_value"),
        "missing b_value from transitive dep, got: {content:?}"
    );
    Ok(())
}

// Transitive dep does not leak when not depending on the intermediary
#[tokio::test]
async fn test_transitive_dep_not_leaked_without_dep() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "noleak",
        r#"
target(name = "b", driver = "bash", run = "printf b_value > $OUT", out = "b.txt")
target(
    name = "a",
    driver = "bash",
    run = "printf a_value > $OUT",
    out = "a.txt",
    transitive = {"deps": {"b": ["//noleak:b"]}},
)
target(
    name = "c",
    driver = "bash",
    run = "echo $SRC_B > $OUT",
    out = "c.txt",
)
"#,
    );

    let err = ws.run("//noleak:c").await;
    assert!(
        err.is_err(),
        "expected failure: $SRC_B should be unset when A is not a dep"
    );
    Ok(())
}

// A dep that declares no transitive sandbox is skipped by the transitive
// collector without disturbing the deps that do declare one.
//
// The collector numbers each merged sandbox by its position in the consumer's
// runtime input list, and that number ends up in the merged tool/dep ids — so a
// skip that renumbered would silently move def hashes and invalidate cached
// artifacts. Here `plain` sorts before `carrier` in the consumer's inputs, so
// `carrier`'s sandbox is *not* at position 0: if skipping `plain` shifted it,
// or dropped it, `$SRC_HIDDEN` would not resolve.
#[tokio::test]
async fn test_transitive_survives_a_preceding_dep_without_one() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "mixed",
        r#"
target(name = "hidden", driver = "bash", run = "printf hidden_value > $OUT", out = "h.txt")
target(name = "plain", driver = "bash", run = "printf plain_value > $OUT", out = "p.txt")
target(
    name = "carrier",
    driver = "bash",
    run = "printf carrier_value > $OUT",
    out = "c.txt",
    transitive = {"deps": {"hidden": ["//mixed:hidden"]}},
)
target(
    name = "consumer",
    driver = "bash",
    run = "printf '%s %s' \"$(cat $SRC_PLAIN)\" \"$(cat $SRC_HIDDEN)\" > $OUT",
    out = "out.txt",
    deps = {"plain": ["//mixed:plain"], "carrier": ["//mixed:carrier"]},
)
"#,
    );

    let result = ws.run("//mixed:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("plain_value"),
        "missing plain_value, got: {content:?}"
    );
    assert!(
        content.contains("hidden_value"),
        "transitive dep behind a non-transitive dep was dropped, got: {content:?}"
    );
    Ok(())
}

/// Build `addr` through a *fresh* engine over `ws`'s root — a second `heph`
/// invocation's view of the same on-disk cache — and wait for the cache write to
/// land. A fresh engine is the point: everything memoized per-engine (the spec,
/// the def, and every `HashMap` built while producing them) is rebuilt, which is
/// what a per-process hash instability needs in order to show.
async fn run_in_a_fresh_engine(ws: &Workspace, addr_str: &str) -> anyhow::Result<()> {
    use heph::engine::{Engine, OutputMatcher, ResultOptions};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let addr = heph::htaddr::parse_addr(addr_str)?;
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
    Ok(())
}

/// Revisions the cache holds for `addr`, read destructively via `clean` — the
/// count of distinct `hashin`s the runs above produced. A stable hash means one
/// revision no matter how many times it ran; one revision per run means the
/// cache never hits and every build is cold.
///
/// The targets under test must raise `cache.history`: it defaults to 1, and the
/// post-write GC enforces it, so a target left at the default keeps exactly one
/// revision whether or not its hash is stable — an oracle that reads "stable"
/// for every input.
async fn revisions(ws: &Workspace, addr_str: &str) -> anyhow::Result<usize> {
    let engine = ws.reopen()?;
    let m = heph::htmatcher::Matcher::Addr(heph::htaddr::parse_addr(addr_str)?);
    Ok(engine
        .clone()
        .clean(engine.new_state(), &m)
        .await?
        .revisions_removed)
}

// A transitive sandbox with more than one dep group must hash the same in every
// process. The group ids are built by enumerating the parsed `{group: [dep]}`
// map, whose iteration order is randomized per `HashMap` instance, and each id
// reaches this consumer's def hash as an `Input::origin_id`. One group is always
// index 0 — hence stable, and hence why every other fixture here misses this.
#[tokio::test]
async fn test_multi_group_transitive_hashes_the_same_every_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "mgt",
        r#"
target(name = "p", driver = "bash", run = "printf p > $OUT", out = "p.txt")
target(name = "q", driver = "bash", run = "printf q > $OUT", out = "q.txt")
target(name = "r", driver = "bash", run = "printf r > $OUT", out = "r.txt")
target(name = "s", driver = "bash", run = "printf s > $OUT", out = "s.txt")
target(
    name = "carrier",
    driver = "bash",
    run = "printf carrier > $OUT",
    out = "c.txt",
    transitive = {"deps": {"gp": ["//mgt:p"], "gq": ["//mgt:q"],
                           "gr": ["//mgt:r"], "gs": ["//mgt:s"]}},
)
target(
    name = "consumer",
    driver = "bash",
    run = "printf consumed > $OUT",
    out = "out.txt",
    cache = {"history": 8},
    deps = {"carrier": ["//mgt:carrier"]},
)
"#,
    );

    for _ in 0..4 {
        run_in_a_fresh_engine(&ws, "//mgt:consumer").await?;
    }
    assert_eq!(
        revisions(&ws, "//mgt:consumer").await?,
        1,
        "consumer's hashin moved between runs: every build is a cold cache"
    );
    Ok(())
}

// The same property for the *consumer* side: the transitive collector numbers
// each merged sandbox by its position in the consumer's input list, and that
// number lands in the merged ids. With more than one dep group, that list is
// assembled by iterating a `HashMap`, so the position — and the hash — is drawn
// from a per-process random order.
#[tokio::test]
async fn test_multi_group_consumer_hashes_the_same_every_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "mgc",
        r#"
target(name = "hidden", driver = "bash", run = "printf hidden > $OUT", out = "h.txt")
target(
    name = "carrier",
    driver = "bash",
    run = "printf carrier > $OUT",
    out = "c.txt",
    transitive = {"deps": {"hidden": ["//mgc:hidden"]}},
)
target(name = "a", driver = "bash", run = "printf a > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "printf b > $OUT", out = "b.txt")
target(name = "c", driver = "bash", run = "printf c > $OUT", out = "cc.txt")
target(
    name = "consumer",
    driver = "bash",
    run = "printf consumed > $OUT",
    out = "out.txt",
    cache = {"history": 8},
    deps = {"ga": ["//mgc:a"], "gb": ["//mgc:b"], "gc": ["//mgc:c"],
            "carrier": ["//mgc:carrier"]},
)
"#,
    );

    for _ in 0..4 {
        run_in_a_fresh_engine(&ws, "//mgc:consumer").await?;
    }
    assert_eq!(
        revisions(&ws, "//mgc:consumer").await?,
        1,
        "consumer's hashin moved between runs: every build is a cold cache"
    );
    Ok(())
}

// Two deps in one transitive group each need their own `origin_id`: it names the
// per-input list file (`list/input_<origin_id>.list`) that the exec driver reads
// back to build `$SRC_<group>`, and the lookup is a `find` on that id. Sharing an
// id makes both deps append to one list file and the first input answer for both,
// so every path lands in `$SRC_G` twice.
#[tokio::test]
async fn test_transitive_group_with_two_deps_lists_each_once() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "dupsrc",
        r#"
target(name = "x", driver = "bash", run = "printf x > $OUT", out = "x.txt")
target(name = "y", driver = "bash", run = "printf y > $OUT", out = "y.txt")
target(
    name = "carrier",
    driver = "bash",
    run = "printf carrier > $OUT",
    out = "c.txt",
    transitive = {"deps": {"g": ["//dupsrc:x", "//dupsrc:y"]}},
)
target(
    name = "consumer",
    driver = "bash",
    run = "printf '%s' \"$SRC_G\" > $OUT",
    out = "out.txt",
    deps = {"carrier": ["//dupsrc:carrier"]},
)
"#,
    );

    let result = ws.run("//dupsrc:consumer").await?;
    let content = common::artifact_string(&result);
    let paths: Vec<&str> = content.split_whitespace().collect();
    let mut uniq: Vec<&str> = paths.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(
        paths.len(),
        uniq.len(),
        "$SRC_G repeats a path, got: {content:?}"
    );
    assert_eq!(paths.len(), 2, "expected x.txt and y.txt, got: {content:?}");
    Ok(())
}

// Direct cycle A → B → A must be rejected
#[tokio::test]
async fn test_direct_cyclic_dep_detected() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target_with_deps("//cycle:a", "echo a", None, deps(&[("b", "//cycle:b")])),
        target_with_deps("//cycle:b", "echo b", None, deps(&[("a", "//cycle:a")])),
    ])?;

    let err = ws.run("//cycle:a").await;
    let e = err.err().expect("expected cyclic dep error");
    let msg = format!("{:#}", e);
    assert!(
        msg.contains("cyclic"),
        "expected 'cyclic' in error, got: {msg}"
    );
    Ok(())
}

// Diamond deps with cache=false should not deadlock when parallelism is constrained.
// B1 and B2 both depend on the same leaf. With the old semaphore placement (acquired
// before resolving deps), B1 and B2 each hold a permit while waiting for the leaf to
// acquire one — deadlock. The fix moves the semaphore inside execute(), after dep
// resolution, so no permit is held while waiting for deps.
#[tokio::test]
async fn test_no_deadlock_diamond_deps() -> anyhow::Result<()> {
    // parallelism=1 → 2 semaphore permits, just enough to trigger the deadlock with 2
    // concurrent mid-nodes each waiting for the leaf.
    let ws = common::Workspace::with_parallelism(1);
    ws.write_build_file(
        "diamond",
        r#"
target(name = "leaf", driver = "bash", run = "echo leaf > $OUT", out = "leaf.txt", cache = False)
target(name = "b1", driver = "bash", run = "cat $SRC_LEAF > $OUT", out = "b1.txt", cache = False, deps = {"leaf": ["//diamond:leaf"]})
target(name = "b2", driver = "bash", run = "cat $SRC_LEAF > $OUT", out = "b2.txt", cache = False, deps = {"leaf": ["//diamond:leaf"]})
target(name = "root", driver = "bash", run = "cat $SRC_B1 $SRC_B2 > $OUT", out = "root.txt", cache = False, deps = {"b1": ["//diamond:b1"], "b2": ["//diamond:b2"]})
"#,
    );

    tokio::time::timeout(std::time::Duration::from_secs(30), ws.run("//diamond:root"))
        .await
        .context("deadlock detected: test timed out after 30s")??;

    Ok(())
}

// Indirect cycle A → B → C → A must be rejected
#[tokio::test]
async fn test_indirect_cyclic_dep_detected() -> anyhow::Result<()> {
    let ws = Workspace::with_static(vec![
        target_with_deps("//icycle:a", "echo a", None, deps(&[("b", "//icycle:b")])),
        target_with_deps("//icycle:b", "echo b", None, deps(&[("c", "//icycle:c")])),
        target_with_deps("//icycle:c", "echo c", None, deps(&[("a", "//icycle:a")])),
    ])?;

    let err = ws.run("//icycle:a").await;
    let e = err.err().expect("expected cyclic dep error");
    let msg = format!("{:#}", e);
    assert!(
        msg.contains("cyclic"),
        "expected 'cyclic' in error, got: {msg}"
    );
    Ok(())
}
