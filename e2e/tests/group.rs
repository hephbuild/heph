mod common;

use common::Workspace;

// Group's deps' artifacts all appear in consumer's $SRC_<GROUP>
#[tokio::test]
async fn test_group_artifacts_merged_into_consumer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "grp",
        r#"
target(name = "t1", driver = "bash", run = "echo t1_value > $OUT", out = "t1.txt")
target(name = "t2", driver = "bash", run = "echo t2_value > $OUT", out = "t2.txt")
target(name = "g", driver = "group", deps = ["//grp:t1", "//grp:t2"])
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC_G > $OUT",
    out = "result.txt",
    deps = {"g": ["//grp:g"]},
)
"#,
    );

    let result = ws.run("//grp:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("t1_value"),
        "missing t1_value, got: {content:?}"
    );
    assert!(
        content.contains("t2_value"),
        "missing t2_value, got: {content:?}"
    );
    Ok(())
}

// result_addr on a group directly returns merged artifacts from all deps
#[tokio::test]
async fn test_result_addr_on_group_returns_merged_artifacts() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "grpres",
        r#"
target(name = "a", driver = "bash", run = "echo aaa > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "echo bbb > $OUT", out = "b.txt")
target(name = "g", driver = "group", deps = ["//grpres:a", "//grpres:b"])
"#,
    );

    let result = ws.run("//grpres:g").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("aaa"), "missing aaa, got: {content:?}");
    assert!(content.contains("bbb"), "missing bbb, got: {content:?}");
    Ok(())
}

// Nested groups: group of groups — consumer sees all leaf artifacts
#[tokio::test]
async fn test_nested_groups_fully_expanded() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "nested",
        r#"
target(name = "l1", driver = "bash", run = "echo leaf1 > $OUT", out = "l1.txt")
target(name = "l2", driver = "bash", run = "echo leaf2 > $OUT", out = "l2.txt")
target(name = "l3", driver = "bash", run = "echo leaf3 > $OUT", out = "l3.txt")
target(name = "inner", driver = "group", deps = ["//nested:l1", "//nested:l2"])
target(name = "outer", driver = "group", deps = ["//nested:inner", "//nested:l3"])
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC_OUTER > $OUT",
    out = "result.txt",
    deps = {"outer": ["//nested:outer"]},
)
"#,
    );

    let result = ws.run("//nested:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(content.contains("leaf1"), "missing leaf1, got: {content:?}");
    assert!(content.contains("leaf2"), "missing leaf2, got: {content:?}");
    assert!(content.contains("leaf3"), "missing leaf3, got: {content:?}");
    Ok(())
}

// Group propagates its deps' transitive sandboxes to consumers
#[tokio::test]
async fn test_group_propagates_transitive_sandboxes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "transg",
        r#"
target(name = "tool", driver = "bash", run = "echo tool_value > $OUT", out = "tool.txt")
target(
    name = "lib",
    driver = "bash",
    run = "echo lib_value > $OUT",
    out = "lib.txt",
    transitive = {"deps": {"tool": ["//transg:tool"]}},
)
target(name = "g", driver = "group", deps = ["//transg:lib"])
target(
    name = "consumer",
    driver = "bash",
    run = "printf '%s %s' \"$(cat $SRC_LIB)\" \"$(cat $SRC_TOOL)\" > $OUT",
    out = "result.txt",
    deps = {"lib": ["//transg:g"]},
)
"#,
    );

    let result = ws.run("//transg:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("lib_value"),
        "missing lib_value, got: {content:?}"
    );
    assert!(
        content.contains("tool_value"),
        "missing transitive tool_value, got: {content:?}"
    );
    Ok(())
}

// Empty group (no deps) produces empty result, does not error
#[tokio::test]
async fn test_empty_group_ok() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "emptyg",
        r#"
target(name = "g", driver = "group", deps = [])
"#,
    );

    let result = ws.run("//emptyg:g").await?;
    assert!(
        result.artifacts.is_empty(),
        "expected no artifacts from empty group"
    );
    Ok(())
}

// Group does not create a cache entry for itself
#[tokio::test]
async fn test_group_not_cached() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "nocache",
        r#"
target(name = "t", driver = "bash", run = "echo val > $OUT", out = "t.txt")
target(name = "g", driver = "group", deps = ["//nocache:t"])
"#,
    );

    let spec = ws.get_spec("//nocache:g").await?;
    let rs = ws.engine.new_state();
    let def = ws
        .engine
        .clone()
        .get_def(rs, &heph::htaddr::parse_addr("//nocache:g")?)
        .await?;
    assert!(!def.target_def.cache.enabled, "group must not be cached");
    assert!(def.target_def.transparent, "group must be transparent");
    drop(spec);
    Ok(())
}
