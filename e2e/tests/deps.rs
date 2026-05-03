mod common;

use std::collections::HashMap;
use common::Workspace;
use rheph::pluginstatictarget::Target;

fn target(addr: &str, run: &str, out: Option<&str>) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: out.map(|s| s.to_string()),
        deps: HashMap::new(),
        labels: vec![],
    }
}

fn target_with_deps(addr: &str, run: &str, out: Option<&str>, deps: HashMap<String, Vec<String>>) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: out.map(|s| s.to_string()),
        deps,
        labels: vec![],
    }
}

fn deps(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
    pairs.iter().map(|(k, v)| (k.to_string(), vec![v.to_string()])).collect()
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
    let ws = Workspace::with_static(vec![
        target("//vars:t", "echo $OUT > $OUT", Some("out.txt")),
    ])?;

    let result = ws.run("//vars:t").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("out.txt"), "OUT should contain declared filename, got: {content:?}");
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
    let ws = Workspace::with_static(vec![
        target("//missing:t", "echo $SRC_GHOST > $OUT", Some("out.txt")),
    ])?;

    let err = ws.run("//missing:t").await;
    assert!(err.is_err(), "expected failure when referencing undeclared $SRC_GHOST");
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
