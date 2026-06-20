mod common;

use common::Workspace;
use heph::htvalue::Value as TargetSpecValue;
use heph::pluginquery::PACKAGE;

// Query by label resolves to group spec with all matching targets as deps
#[tokio::test]
async fn test_query_by_label_spec() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qa",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt", labels = ["lint"])
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
"#,
    );
    ws.write_build_file(
        "qb",
        r#"
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt", labels = ["lint"])
"#,
    );

    let spec = ws
        .get_spec(&format!("//{PACKAGE}:q@expr=label(lint)"))
        .await?;

    assert_eq!(spec.driver, heph::plugingroup::DRIVER_NAME);
    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list, got: {:?}", spec.config.get("deps")),
    };
    assert_eq!(deps.len(), 2, "expected 2 labeled targets, got: {deps:?}");
    let dep_strs: Vec<&str> = deps
        .iter()
        .map(|v| match v {
            TargetSpecValue::String(s) => s.as_str(),
            _ => panic!("expected string dep"),
        })
        .collect();
    assert!(
        dep_strs.contains(&"//qa:a"),
        "missing //qa:a in {dep_strs:?}"
    );
    assert!(
        dep_strs.contains(&"//qb:c"),
        "missing //qb:c in {dep_strs:?}"
    );
    Ok(())
}

// Query by package returns only targets in that exact package
#[tokio::test]
async fn test_query_by_package_spec() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "exact/pkg",
        r#"
target(name = "x", driver = "bash", run = "echo x > $OUT", out = "x.txt")
target(name = "y", driver = "bash", run = "echo y > $OUT", out = "y.txt")
"#,
    );
    ws.write_build_file(
        "other",
        r#"
target(name = "z", driver = "bash", run = "echo z > $OUT", out = "z.txt")
"#,
    );

    let spec = ws
        .get_spec(&format!("//{PACKAGE}:q@expr=package(exact/pkg)"))
        .await?;

    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list"),
    };
    assert_eq!(
        deps.len(),
        2,
        "expected 2 targets from exact/pkg, got: {deps:?}"
    );
    Ok(())
}

// Query by package_prefix returns targets in all matching sub-packages
#[tokio::test]
async fn test_query_by_package_prefix_spec() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "src/alpha",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
"#,
    );
    ws.write_build_file(
        "src/alpha/sub",
        r#"
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
"#,
    );
    ws.write_build_file(
        "src/beta",
        r#"
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt")
"#,
    );

    let spec = ws
        .get_spec(&format!("//{PACKAGE}:q@expr=package_prefix(src/alpha)"))
        .await?;

    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list"),
    };
    assert_eq!(
        deps.len(),
        2,
        "expected a + b from src/alpha prefix, got: {deps:?}"
    );
    Ok(())
}

// Running a query target merges artifacts from all matched targets
#[tokio::test]
async fn test_query_run_merges_artifacts() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qrun",
        r#"
target(name = "p", driver = "bash", run = "echo p_val > $OUT", out = "p.txt", labels = ["collect"])
target(name = "q", driver = "bash", run = "echo q_val > $OUT", out = "q.txt", labels = ["collect"])
target(name = "skip", driver = "bash", run = "echo skip_val > $OUT", out = "skip.txt")
"#,
    );

    let result = ws
        .run(&format!("//{PACKAGE}:q@expr=label(collect)"))
        .await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("p_val"), "missing p_val, got: {content:?}");
    assert!(content.contains("q_val"), "missing q_val, got: {content:?}");
    assert!(
        !content.contains("skip_val"),
        "skip_val should not appear, got: {content:?}"
    );
    Ok(())
}

// Query target used as dep: consumer sees all matched artifacts via $SRC_<KEY>.
// `exclude_provider=__none__` opts out of the auto-injected exclusion of the
// dest's producing provider — required to enumerate sibling buildfile targets.
#[tokio::test]
async fn test_query_as_dep_in_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qdep",
        r#"
target(name = "lib1", driver = "bash", run = "echo lib1_val > $OUT", out = "lib1.txt", labels = ["lib"])
target(name = "lib2", driver = "bash", run = "echo lib2_val > $OUT", out = "lib2.txt", labels = ["lib"])
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC_LIBS > $OUT",
    out = "result.txt",
    deps = {"libs": ["//@heph/query:q@expr=label(lib),exclude_provider=__none__"]},
)
"#,
    );

    let result = ws.run("//qdep:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("lib1_val"),
        "missing lib1_val, got: {content:?}"
    );
    assert!(
        content.contains("lib2_val"),
        "missing lib2_val, got: {content:?}"
    );
    Ok(())
}

// Empty query (no targets match) produces empty group — no error
#[tokio::test]
async fn test_query_no_match_empty_group() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qempty",
        r#"
target(name = "t", driver = "bash", run = "echo x > $OUT", out = "x.txt")
"#,
    );

    let spec = ws
        .get_spec(&format!("//{PACKAGE}:q@expr=label(nonexistent)"))
        .await?;

    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list"),
    };
    assert!(
        deps.is_empty(),
        "expected empty deps for no match, got: {deps:?}"
    );
    Ok(())
}

// AND combination: label + package filters to intersection
#[tokio::test]
async fn test_query_label_and_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "inter/a",
        r#"
target(name = "x", driver = "bash", run = "echo x > $OUT", out = "x.txt", labels = ["tag"])
target(name = "y", driver = "bash", run = "echo y > $OUT", out = "y.txt")
"#,
    );
    ws.write_build_file(
        "inter/b",
        r#"
target(name = "z", driver = "bash", run = "echo z > $OUT", out = "z.txt", labels = ["tag"])
"#,
    );

    let spec = ws
        .get_spec(&format!(
            "//{PACKAGE}:q@expr=\"package(inter/a) && label(tag)\""
        ))
        .await?;

    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list"),
    };
    assert_eq!(
        deps.len(),
        1,
        "expected only inter/a:x (AND filter), got: {deps:?}"
    );
    assert!(
        matches!(&deps[0], TargetSpecValue::String(s) if s == "//inter/a:x"),
        "expected //inter/a:x, got: {:?}",
        deps[0]
    );
    Ok(())
}

// Query by expr arg parses the query language and resolves to matching targets.
#[tokio::test]
async fn test_query_by_expr_spec() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qx",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt", labels = ["lint"])
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt")
"#,
    );
    ws.write_build_file(
        "qx/vendor",
        r#"
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "c.txt", labels = ["lint"])
"#,
    );

    // Every lint target under //qx, excluding the vendored subtree.
    let spec = ws
        .get_spec(&format!(
            "//{PACKAGE}:q@expr=\"//qx/... && label(lint) && !//qx/vendor/...\""
        ))
        .await?;

    let deps = match spec.config.get("deps") {
        Some(TargetSpecValue::List(l)) => l,
        _ => panic!("expected deps list, got: {:?}", spec.config.get("deps")),
    };
    let dep_strs: Vec<&str> = deps
        .iter()
        .map(|v| match v {
            TargetSpecValue::String(s) => s.as_str(),
            _ => panic!("expected string dep"),
        })
        .collect();
    assert_eq!(dep_strs, vec!["//qx:a"], "got: {dep_strs:?}");
    Ok(())
}

// The BUILD-file `query(...)` builtin composes a @heph/query dep that a group
// expands to the matched targets — same shape as `file()`/`glob()`.
#[tokio::test]
async fn test_buildfile_query_builtin_resolves() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "qb",
        r#"
target(name = "a", driver = "bash", run = "echo a_value > $OUT", out = "a.txt", labels = ["lint"])
target(name = "b", driver = "bash", run = "echo b_value > $OUT", out = "b.txt")
"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "g", driver = "group", deps = [query("//qb/... && label(lint)")])
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC_G > $OUT",
    out = "result.txt",
    deps = {"g": ["//app:g"]},
)
"#,
    );

    let result = ws.run("//app:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("a_value"),
        "expected lint target output, got: {content:?}"
    );
    assert!(
        !content.contains("b_value"),
        "unlabeled target must not be selected, got: {content:?}"
    );
    Ok(())
}
