#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

// Exercises the `.hephconfig`-driven `default_tools` mechanism end to end:
// a `bash` target with no `tools = [...]` of its own resolves `cat` purely
// because it's in the driver's configured `default_tools`, and a tool that
// isn't in that list (and isn't declared) fails clearly. Deliberately not
// using `common::Workspace` (its shared harness registers bare
// `Driver::new_exec`/`new_bash`, which never populate `default_tools` and
// don't register `hostbin` — see `plugin-exec`'s `Driver` doc comment on
// `legacy_ambient_path_fallback` for why) — this test builds its own engine
// with `hostbin` registered and an explicit, deterministic `default_tools`
// list, rather than depending on the shape of the built-in curated default.

use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginhostbin;
use htestkit::WorkspaceBuilder;

fn default_tools_opts(names: &[&str]) -> std::collections::BTreeMap<String, serde_yaml::Value> {
    let mut opts = std::collections::BTreeMap::new();
    opts.insert(
        "default_tools".to_string(),
        serde_yaml::Value::Sequence(
            names
                .iter()
                .map(|n| serde_yaml::Value::String((*n).to_string()))
                .collect(),
        ),
    );
    opts
}

fn build_ws(default_tools: &[&str]) -> anyhow::Result<htestkit::Workspace> {
    let bash_driver = pluginexec::Driver::from_options_bash(&default_tools_opts(default_tools))?;
    WorkspaceBuilder::new()?
        .with_provider(|init| Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf())))
        .with_provider(|_| Box::new(pluginhostbin::Provider))
        .with_driver(Box::new(pluginhostbin::Driver))
        .with_managed_driver(Box::new(bash_driver))
        .build()
}

/// The stock driver a real `.hephconfig` `builtin: bash` (no options) would
/// register — the curated `DEFAULT_TOOLS` list, not a test-controlled
/// override. Proves the shipped default list, not just an explicit stand-in
/// for it, actually resolves something real end to end.
fn build_ws_with_curated_defaults() -> anyhow::Result<htestkit::Workspace> {
    let bash_driver = pluginexec::Driver::from_options_bash(&std::collections::BTreeMap::new())?;
    WorkspaceBuilder::new()?
        .with_provider(|init| Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf())))
        .with_provider(|_| Box::new(pluginhostbin::Provider))
        .with_driver(Box::new(pluginhostbin::Driver))
        .with_managed_driver(Box::new(bash_driver))
        .build()
}

#[tokio::test]
async fn test_default_tool_resolves_without_explicit_tools_dep() -> anyhow::Result<()> {
    let ws = build_ws(&["bash", "cat"])?;
    ws.write_build_file(
        "defaulttools",
        r#"
target(name = "src", driver = "bash", run = "echo hi > $OUT", out = "src.txt")
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC > $OUT",
    out = "result.txt",
    deps = {"": ["//defaulttools:src"]},
)
"#,
    );

    let result = ws.run("//defaulttools:consumer").await?;
    let content = htestkit::artifact_string(&result);
    assert!(content.contains("hi"), "missing 'hi', got: {content:?}");
    Ok(())
}

#[tokio::test]
async fn test_tool_outside_default_list_fails_clearly() -> anyhow::Result<()> {
    // Only `bash` itself is a default tool — `gzip` is neither in
    // `default_tools` nor declared via `tools = [...]`, so it must not be
    // silently reachable the way an unconstrained ambient PATH would allow.
    let ws = build_ws(&["bash"])?;
    ws.write_build_file(
        "defaulttoolsmissing",
        r#"
target(name = "consumer", driver = "bash", run = "gzip --version > $OUT", out = "result.txt")
"#,
    );

    let err = ws
        .run("//defaulttoolsmissing:consumer")
        .await
        .err()
        .expect("gzip must not be reachable");
    let msg = err.to_string();
    assert!(msg.contains("gzip"), "missing tool name: {msg}");
    Ok(())
}

// A target's own explicit `tools = [...]` naming the same `//@heph/bin:<x>`
// addr the driver's `default_tools` also injects must not conflict: both are
// the same underlying file, so `$TOOL` (from the explicit dep) and the bare
// name on PATH (from the default) both resolve. This exercises the `linked`
// dedup shared between the `tool_group_inputs` and `default_tool_inputs`
// symlink loops in `run_inner`.
#[tokio::test]
async fn test_own_tools_dep_overlapping_default_tool_name() -> anyhow::Result<()> {
    let ws = build_ws(&["bash", "cat"])?;
    ws.write_build_file(
        "defaulttoolsoverlap",
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "test -x \"$TOOL\" && cat $TOOL > $OUT",
    out = "result.txt",
    tools = {"": ["//@heph/bin:cat"]},
)
"#,
    );

    let result = ws.run("//defaulttoolsoverlap:consumer").await?;
    let content = htestkit::artifact_string(&result);
    // $TOOL points at the symlinked `bin/cat`; catting it dumps the (binary)
    // executable — just confirm the run succeeded and produced non-empty
    // output, proving both the explicit dep and the bare-name PATH lookup
    // resolved without erroring.
    assert!(!content.is_empty(), "expected non-empty output");
    Ok(())
}

#[tokio::test]
async fn test_curated_default_tools_resolve_cat() -> anyhow::Result<()> {
    let ws = build_ws_with_curated_defaults()?;
    ws.write_build_file(
        "defaulttoolscurated",
        r#"
target(name = "src", driver = "bash", run = "echo hi > $OUT", out = "src.txt")
target(
    name = "consumer",
    driver = "bash",
    run = "cat $SRC > $OUT",
    out = "result.txt",
    deps = {"": ["//defaulttoolscurated:src"]},
)
"#,
    );

    let result = ws.run("//defaulttoolscurated:consumer").await?;
    let content = htestkit::artifact_string(&result);
    assert!(content.contains("hi"), "missing 'hi', got: {content:?}");
    Ok(())
}
