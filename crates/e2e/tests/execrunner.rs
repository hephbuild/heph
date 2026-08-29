//! Exec runners, through the real engine.
//!
//! The unit tests in `plugin-exec` prove the runner reaches `def.inputs` with
//! the right flags. What only the engine can prove is the part that decides
//! whether a build is correct: that the rewrite actually reaches the child,
//! that the runner's hashout reaches the consumer's cache key, and that a
//! broken runner fails by name instead of silently running locally.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::Workspace;

/// A `bash` target whose single output is a `runner.json` with the given body.
///
/// A runner is just a target that emits this file — no plugin, no special
/// driver — which is the property that makes wrap runners adoptable, so the
/// tests exercise it the same way a user would.
fn runner_target(name: &str, json: &str) -> String {
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = """cat > $OUT <<'JSON'
{json}
JSON""",
    out = "runner.json",
)
"#
    )
}

/// The rewrite must reach the child: a wrap runner's `env` shows up in the
/// target's environment, and its `prefix` in the program actually executed.
#[tokio::test]
async fn wrap_runner_rewrites_the_child() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "fp-1", "runner": "wrap",
 "config": {"prefix": ["/usr/bin/env", "XR_PREFIX=yes"], "env": {"XR_MARK": "wrapped"}}}"#,
    );
    build.push_str(
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"$XR_MARK/$XR_PREFIX\" > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let result = ws.run("//xr:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.contains("wrapped/yes"),
        "the wrap runner's env and argv prefix must both reach the child; got {content:?}"
    );
    Ok(())
}

/// The runner is a hash dep: hashed, so it keys the cache — and *not* runtime,
/// so nothing about it is materialized where the target can see it.
#[tokio::test]
async fn runner_json_never_enters_the_sandbox() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "fp-1", "runner": "wrap", "config": {}}"#,
    );
    build.push_str(
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "find . -name runner.json | wc -l | tr -d ' ' > $OUT",
    out = "count.txt",
    runner = "//xr:runner",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let result = ws.run("//xr:consumer").await?;
    let content = common::artifact_string(&result);
    assert!(
        content.trim() == "0",
        "runner.json must not be materialized into the sandbox; found {content:?}"
    );
    Ok(())
}

/// The whole cache-correctness argument in one test. The consumer keys on the
/// runner target's hashout, so changing what the runner *provides* — here, the
/// environment it injects — must produce a different result rather than
/// serving the one built under the old environment.
#[tokio::test]
async fn changing_the_runner_rekeys_its_consumers() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let consumer = r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"$XR_MARK\" > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#;

    let mut first = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "fp-1", "runner": "wrap",
 "config": {"env": {"XR_MARK": "one"}}}"#,
    );
    first.push_str(consumer);
    ws.write_build_file("xr", &first);
    let before = common::artifact_string(&*ws.run("//xr:consumer").await?);
    assert!(before.contains("one"), "got {before:?}");

    // A second engine over the same on-disk cache — what the next `heph`
    // invocation sees. Without it the spec is memoized and the rewritten BUILD
    // file is never read.
    let mut second = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "fp-2", "runner": "wrap",
 "config": {"env": {"XR_MARK": "two"}}}"#,
    );
    second.push_str(consumer);
    ws.write_build_file("xr", &second);

    let engine = ws.reopen()?;
    let addr = heph::htaddr::parse_addr("//xr:consumer")?;
    let rs = engine.new_state();
    let result = engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await?;
    let after = common::artifact_string(&result);
    // Free the riding cache read before the workspace drops, in the order
    // `heph run`'s own locals unwind.
    drop(result);
    drop(rs);
    assert!(
        after.contains("two"),
        "the consumer must rebuild under the new runner rather than serving the \
         artifact built under the old one; got {after:?}"
    );
    Ok(())
}

/// A typo in the runner name must name the unknown runner and list the known
/// ones, not fail somewhere deep in execution with a generic error.
#[tokio::test]
async fn an_unknown_runner_name_is_diagnosable() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "fp-1", "runner": "devnev", "config": {}}"#,
    );
    build.push_str(
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let err = match ws.run("//xr:consumer").await {
        Ok(_) => panic!("an unknown runner name must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("devnev"),
        "must name the unknown runner: {err}"
    );
    assert!(err.contains("wrap"), "must list the known runners: {err}");
    Ok(())
}

/// The fingerprint is what makes a runner's hashout move when its environment
/// changes. A runner that omits it is a cache-poisoning foot-gun, so it is
/// refused rather than defaulted.
#[tokio::test]
async fn a_runner_without_a_fingerprint_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "runner",
        r#"{"version": 1, "fingerprint": "", "runner": "wrap", "config": {}}"#,
    );
    build.push_str(
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let err = match ws.run("//xr:consumer").await {
        Ok(_) => panic!("an empty fingerprint must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("fingerprint"), "{err}");
    Ok(())
}

/// A version this heph does not understand must be rejected by name rather
/// than parsed leniently into a different meaning.
#[tokio::test]
async fn an_unknown_runner_json_version_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "runner",
        r#"{"version": 99, "fingerprint": "fp", "runner": "wrap", "config": {}}"#,
    );
    build.push_str(
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let err = match ws.run("//xr:consumer").await {
        Ok(_) => panic!("an unknown runner.json version must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("version 99"), "{err}");
    Ok(())
}

/// A runner target that produces something other than a single `runner.json`
/// must say so by address, not be guessed past.
#[tokio::test]
async fn a_runner_target_producing_no_runner_json_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "xr",
        r#"
target(name = "runner", driver = "bash", run = "echo nope > $OUT", out = "other.txt")
target(
    name = "consumer",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "result.txt",
    runner = "//xr:runner",
)
"#,
    );

    let err = match ws.run("//xr:consumer").await {
        Ok(_) => panic!("a runner with no runner.json must fail the build"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("runner.json"), "{err}");
    assert!(err.contains("//xr:runner"), "must name the runner: {err}");
    Ok(())
}

/// Two targets under two different runners must not share a cache entry, even
/// when their own definitions are identical.
#[tokio::test]
async fn two_runners_produce_two_results() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let mut build = runner_target(
        "one",
        r#"{"version": 1, "fingerprint": "fp-one", "runner": "wrap",
 "config": {"env": {"XR_MARK": "one"}}}"#,
    );
    build.push_str(&runner_target(
        "two",
        r#"{"version": 1, "fingerprint": "fp-two", "runner": "wrap",
 "config": {"env": {"XR_MARK": "two"}}}"#,
    ));
    build.push_str(
        r#"
target(
    name = "a",
    driver = "bash",
    run = "echo \"$XR_MARK\" > $OUT",
    out = "result.txt",
    runner = "//xr:one",
)
target(
    name = "b",
    driver = "bash",
    run = "echo \"$XR_MARK\" > $OUT",
    out = "result.txt",
    runner = "//xr:two",
)
"#,
    );
    ws.write_build_file("xr", &build);

    let a = common::artifact_string(&*ws.run("//xr:a").await?);
    let b = common::artifact_string(&*ws.run("//xr:b").await?);
    assert!(a.contains("one"), "got {a:?}");
    assert!(
        b.contains("two"),
        "identical targets under different runners must not share a cache entry; got {b:?}"
    );
    Ok(())
}

/// A target naming itself as its own runner is a dependency cycle, and must
/// surface as one rather than deadlocking the memoizer.
#[tokio::test]
async fn a_self_referential_runner_is_a_cycle_not_a_hang() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "xr",
        r#"
target(
    name = "loop",
    driver = "bash",
    run = "echo hi > $OUT",
    out = "runner.json",
    runner = "//xr:loop",
)
"#,
    );

    let run = tokio::time::timeout(std::time::Duration::from_secs(60), ws.run("//xr:loop")).await;
    let Ok(res) = run else {
        panic!("a self-referential runner deadlocked instead of reporting a cycle")
    };
    let err = match res {
        Ok(_) => panic!("a self-referential runner must fail"),
        Err(e) => format!("{e:#}"),
    };
    let lower = err.to_lowercase();
    assert!(
        lower.contains("cyclic") || lower.contains("cycle"),
        "must report a dependency cycle; got {err}"
    );
    Ok(())
}
