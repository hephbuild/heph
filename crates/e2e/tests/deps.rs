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
