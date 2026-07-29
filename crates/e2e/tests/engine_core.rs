#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

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

/// `cache.history` must be enforced *by the run that broke the budget*, not
/// left for the next `heph gc`.
///
/// Three runs of one target, each with a different script, write three cache
/// revisions; the default history budget is 1, so by the end of the third run
/// the two older revisions owe us their disk back. Observed through `gc_all`:
/// a post-run sweep that still finds revisions to reclaim is proof the in-run
/// trim never happened.
///
/// Each run gets a fresh engine over the same root — the next `heph run`'s view
/// of the same on-disk cache. That is not incidental: an in-process re-run
/// replays the memoized spec, so the rewritten BUILD file would produce the
/// same `hashin` and no second revision, and the test could not fail.
///
/// The run is driven through `result_addr` rather than `Workspace::run` so the
/// test owns the two things that decide the outcome, exactly as `heph run`
/// does: the result is released before the request state, and the run is not
/// over until the background queue drains.
#[tokio::test]
async fn cache_history_is_enforced_by_the_end_of_the_run() -> anyhow::Result<()> {
    use heph::engine::{Engine, OutputMatcher, ResultOptions};
    use heph::htaddr::parse_addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let ws = Workspace::new();
    let addr = parse_addr("//hist:t")?;

    for v in ["v1", "vv22", "vvv333"] {
        // A different script is a different def hash, hence a different
        // `hashin` — a genuinely new revision rather than a re-hit.
        ws.write_build_file(
            "hist",
            &format!(
                r#"target(name = "t", driver = "bash", run = "printf '{v}' > $OUT", out = "out.txt")"#
            ),
        );

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
        // Precondition: each run really did rebuild. If this ever reports a
        // stale value the runs are sharing a revision and the assertions below
        // become vacuous.
        assert_eq!(
            common::artifact_string(&result),
            v,
            "each run must produce its own revision"
        );
        // Release the artifacts, then the request — the order `heph run`'s
        // locals unwind in, and the order that frees the addr's riding read.
        drop(result);
        drop(rs);

        // `bg` is the counter the CLI blocks on before exiting; the run's
        // background cache work is not finished until it reads zero.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while bg.load(Ordering::Acquire) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "background work never drained"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        drop(engine);

        // Assert per run, not once at the end. A single check after the last run
        // cannot tell "every run enforced its budget" from "only the last one
        // did" — run 3's trim protects its own revision and deletes both older
        // ones, so an end-only assertion passes even when two of the three trims
        // silently lost their lock.
        //
        // `gc_all` is the oracle because it reports what a sweep still finds to
        // reclaim; anything above zero is a trim that did not happen. It is also
        // destructive, which is what makes it a *tight* check — it leaves the
        // cache in the state the trim should already have produced, so the next
        // iteration starts clean and any failure is attributable to one run.
        let sweeper = ws.reopen()?;
        let stats = sweeper.clone().gc_all(sweeper.new_state()).await?;
        assert_eq!(
            stats.revisions_removed, 0,
            "after the `{v}` run a sweep still found revisions to reclaim, so \
             that run's post-write trim never ran: {stats:?}"
        );
        assert_eq!(
            stats.revisions_kept, 1,
            "exactly the newest revision survives the `{v}` run: {stats:?}"
        );
    }
    Ok(())
}
