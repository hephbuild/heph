mod common;

use common::Workspace;

#[tokio::test]
async fn test_bash_stdout_no_artifacts() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "hello",
        r#"target(name = "greet", driver = "bash", run = "printf 'hi there'")"#,
    );

    let result = ws.run("//hello:greet").await?;
    assert!(
        result.artifacts.is_empty(),
        "stdout-only target should produce no artifacts"
    );
    Ok(())
}

#[tokio::test]
async fn test_bash_out_file_artifact() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "out",
        r#"target(name = "gen", driver = "bash", run = "printf 'generated' > $OUT", out = "result.txt")"#,
    );

    let result = ws.run("//out:gen").await?;
    assert!(!result.artifacts.is_empty(), "no artifacts produced");

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("result.txt")),
        "result.txt not found in artifacts: {paths:?}"
    );

    let content = common::artifact_string(&result);
    assert!(content.contains("generated"), "got: {content:?}");
    Ok(())
}

#[tokio::test]
async fn test_target_not_found() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let err = ws.run("//nonexistent:target").await;
    assert!(err.is_err(), "expected error for missing target");
    Ok(())
}

#[tokio::test]
async fn test_two_targets_same_package() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "multi",
        r#"
target(name = "a", driver = "bash", run = "printf 'AAA' > $OUT", out = "a.txt")
target(name = "b", driver = "bash", run = "printf 'BBB' > $OUT", out = "b.txt")
"#,
    );

    // Read each artifact immediately — the engine writes all tars to a shared temp path
    // so content must be consumed before the next target overwrites it.
    let a = ws.run("//multi:a").await?;
    let a_content = common::artifact_string(&a);

    let b = ws.run("//multi:b").await?;
    let b_content = common::artifact_string(&b);

    assert!(a_content.contains("AAA"), "a: {a_content:?}");
    assert!(b_content.contains("BBB"), "b: {b_content:?}");
    Ok(())
}

#[tokio::test]
async fn test_cached_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "cached",
        r#"target(name = "t", driver = "bash", run = "printf 'cached_ok' > $OUT", out = "out.txt")"#,
    );

    let r1 = ws.run("//cached:t").await?;
    let r2 = ws.run("//cached:t").await?;

    assert!(common::artifact_string(&r1).contains("cached_ok"));
    assert!(common::artifact_string(&r2).contains("cached_ok"));
    Ok(())
}

#[tokio::test]
async fn test_force_run() -> anyhow::Result<()> {
    use heph::engine::{OutputMatcher, ResultOptions};
    use heph::htaddr::parse_addr;

    let ws = Workspace::new();
    ws.write_build_file(
        "force",
        r#"target(name = "t", driver = "bash", run = "printf 'forced' > $OUT", out = "out.txt")"#,
    );

    let addr = parse_addr("//force:t")?;
    let e = ws.engine.clone();
    let rs = e.clone().new_state();
    let result = e
        .result_addr(
            rs,
            &addr,
            OutputMatcher::All,
            &ResultOptions {
                force: true,
                ..Default::default()
            },
        )
        .await?;

    assert!(common::artifact_string(&result).contains("forced"));
    Ok(())
}

#[tokio::test]
async fn test_failing_command_returns_error() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fail",
        r#"target(name = "t", driver = "bash", run = "exit 1")"#,
    );

    let err = ws.run("//fail:t").await;
    assert!(err.is_err(), "expected error from failing command");
    Ok(())
}

#[tokio::test]
async fn test_failure_surfaces_process_log_tail() -> anyhow::Result<()> {
    // The failure diagnostic reads the last log lines lazily from the on-disk
    // log (the sandbox of a failed target survives until its next run), so the
    // captured output must reach the recorded failure end-to-end.
    let ws = Workspace::new();
    ws.write_build_file(
        "fail",
        r#"target(name = "t", driver = "bash", run = "echo distinctive-marker-line; exit 3")"#,
    );

    let err = match ws.run("//fail:t").await {
        Ok(_) => panic!("expected error from failing command"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("distinctive-marker-line"),
        "log tail must surface the process output, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn test_output_selection_filters_to_named_output() -> anyhow::Result<()> {
    // `heph run --output foo` resolves only the requested output group; the
    // sibling output's artifact must not surface. Mirrors the OutputMatcher::Exact
    // path the CLI takes.
    let ws = Workspace::new();
    ws.write_build_file(
        "sel",
        r#"
target(
    name = "t",
    driver = "bash",
    run = "printf 'FOO' > $OUT_FOO; printf 'BAR' > $OUT_BAR",
    out = {"foo": ["foo.txt"], "bar": ["bar.txt"]},
)
"#,
    );

    let only_foo = ws.run_addr_outputs("//sel:t", &["foo"]).await?;
    let paths = common::artifact_paths(&only_foo);
    assert!(
        paths.iter().any(|p| p.ends_with("foo.txt")),
        "foo.txt must be present: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("bar.txt")),
        "bar.txt must be filtered out: {paths:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_output_selection_unknown_name_errors() -> anyhow::Result<()> {
    // An output name the target does not declare must fail loudly, not silently
    // resolve to an empty set.
    let ws = Workspace::new();
    ws.write_build_file(
        "selerr",
        r#"target(name = "t", driver = "bash", run = "printf 'X' > $OUT", out = "x.txt")"#,
    );

    let err = match ws.run_addr_outputs("//selerr:t", &["nope"]).await {
        Ok(_) => panic!("unknown output must error"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("output not found"), "got: {msg}");
    Ok(())
}
