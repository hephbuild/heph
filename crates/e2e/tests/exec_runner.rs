#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `runner =` selection and its effect on the cache key.
//!
//! Design: `docs/EXEC_RUNNERS.md`. The property under test throughout is:
//! a runner reference is a *target* reference, so the environment reaches
//! `hashin` through the ordinary dependency mechanism and not through a new
//! hash component.

mod common;

use common::Workspace;
use heph::htaddr::parse_addr;

async fn hashin(ws: &Workspace, addr: &str) -> anyhow::Result<String> {
    let rs = ws.engine.new_state();
    let meta = ws.engine.clone().meta(rs, &parse_addr(addr)?).await?;
    Ok(meta.hashin)
}

/// Two runner targets describing different environments must give their
/// consumers different keys. This is the whole point of runner-as-dependency:
/// swap the environment, and every artifact built in it is re-keyed.
#[tokio::test]
async fn runner_identity_reaches_the_cache_key() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "run",
        r#"
target(name = "envA", driver = "bash", run = "echo A > $OUT", out = "env.json")
target(name = "envB", driver = "bash", run = "echo B > $OUT", out = "env.json")
target(name = "plain", driver = "bash", run = "echo x > $OUT", out = "o")
target(name = "under_a", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//run:envA")
target(name = "under_b", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//run:envB")
"#,
    );

    let plain = hashin(&ws, "//run:plain").await?;
    let a = hashin(&ws, "//run:under_a").await?;
    let b = hashin(&ws, "//run:under_b").await?;

    assert_ne!(a, b, "different runners must give different keys");
    assert_ne!(a, plain, "a runner must change the key at all");
    Ok(())
}

/// The compatibility promise: a target with no runner hashes exactly as it did
/// before exec runners existed, so shipping this invalidates nothing. `local`
/// is the *absence* of a contribution, not a named one.
#[tokio::test]
async fn no_runner_and_explicit_local_hash_identically() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "loc",
        r#"
target(name = "a", driver = "bash", run = "echo x > $OUT", out = "o")
target(name = "b", driver = "bash", run = "echo x > $OUT", out = "o", runner = None)
"#,
    );

    assert_eq!(
        hashin(&ws, "//loc:a").await?,
        hashin(&ws, "//loc:b").await?,
        "`runner = None` must be indistinguishable from not authoring one",
    );
    Ok(())
}

/// The runner is a *hash-only* input: its bytes must never be materialized into
/// a consumer's sandbox. Otherwise every target under a runner pays a symlink, a
/// list file and an `SRC_*` entry it never asked for — and an in-sandbox glob
/// starts matching a file that appeared for reasons the BUILD file cannot see.
#[tokio::test]
async fn runner_artifact_is_not_materialized_into_the_sandbox() -> anyhow::Result<()> {
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_recording_runner(std::sync::Arc::clone(&opens), ("X", "y"));
    ws.write_build_file(
        "mat",
        r#"
target(name = "env", driver = "bash", run = "echo SECRET > $OUT", out = "env.json")
target(
    name = "consumer",
    driver = "bash",
    # Everything the sandbox contains, plus the routing vars a dep would set.
    run = "ls -A . > $OUT; echo \"SRC=[${SRC:-}] LIST=[${LIST_SRC:-}]\" >> $OUT",
    out = "o",
    runner = "//mat:env",
)
"#,
    );

    let res = ws.run("//mat:consumer").await?;
    let out = common::artifact_string(&res);
    assert!(
        !out.contains("env.json"),
        "runner artifact leaked into the sandbox: {out:?}"
    );
    assert!(
        out.contains("SRC=[] LIST=[]"),
        "runner must not be wired into SRC_/LIST_ routing: {out:?}"
    );
    Ok(())
}

/// A runner target that produces nothing contributes NO bytes to `hashin` —
/// `hashin` folds input *hashouts*, and a zero-output target has none. Two
/// different such runners would give their consumers byte-identical keys, and
/// an artifact built in one environment would be served for the other. Caught
/// as a typed failure rather than silently.
#[tokio::test]
async fn zero_output_runner_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "empty",
        r#"
target(name = "env", driver = "bash", run = "true")
target(name = "consumer", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//empty:env")
"#,
    );

    let msg = match ws.run("//empty:consumer").await {
        Ok(_) => panic!("a runner with no outputs must not resolve"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("produces no output artifacts"),
        "error must name the actual problem, got: {msg}"
    );
    Ok(())
}

/// A bare name is rejected rather than guessed at. `driver = "bash"` sits right
/// next to `runner =`, so the two look symmetric and are not: a runner names a
/// target, because the environment it describes has to reach the cache key.
#[tokio::test]
async fn bare_runner_name_is_rejected() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "bare",
        r#"
target(name = "c", driver = "bash", run = "echo x > $OUT", out = "o", runner = "devenv")
"#,
    );

    let msg = match ws.run("//bare:c").await {
        Ok(_) => panic!("a bare runner name must be rejected"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("target address"),
        "error must say what a runner is, got: {msg}"
    );
    Ok(())
}

/// The workspace default applies to targets that did not author one — which the workspace default
/// calls the expected way to use this ("the whole repo builds under devenv").
#[tokio::test]
async fn workspace_default_applies_and_can_be_opted_out_of() -> anyhow::Result<()> {
    let ws = Workspace::with_default_runner("//def:env");
    ws.write_build_file(
        "def",
        r#"
target(name = "env", driver = "bash", run = "echo A > $OUT", out = "env.json")
target(name = "inherits", driver = "bash", run = "echo x > $OUT", out = "o")
target(name = "opted_out", driver = "bash", run = "echo x > $OUT", out = "o", runner = None)
"#,
    );

    let plain = Workspace::new();
    plain.write_build_file(
        "def",
        r#"
target(name = "env", driver = "bash", run = "echo A > $OUT", out = "env.json")
target(name = "inherits", driver = "bash", run = "echo x > $OUT", out = "o")
target(name = "opted_out", driver = "bash", run = "echo x > $OUT", out = "o", runner = None)
"#,
    );

    let inherits_under_default = hashin(&ws, "//def:inherits").await?;
    let inherits_no_default = hashin(&plain, "//def:inherits").await?;
    assert_ne!(
        inherits_under_default, inherits_no_default,
        "setting defaultRunner must re-key targets that inherit it",
    );

    // `runner = None` is not merely "no runner authored" — it must survive a
    // workspace default. Without that there is no way to keep a bootstrap
    // target out of an environment that does not exist yet.
    assert_eq!(
        hashin(&ws, "//def:opted_out").await?,
        hashin(&plain, "//def:opted_out").await?,
        "`runner = None` must opt out of the workspace default",
    );
    Ok(())
}

/// The runner target itself must not inherit the workspace default, or it
/// becomes its own dependency. The cycle checker would catch that, but it would
/// report a graph problem for what is really a config one — so it is excluded
/// up front and the common case never reaches the checker.
#[tokio::test]
async fn runner_target_does_not_inherit_the_default() -> anyhow::Result<()> {
    let ws = Workspace::with_default_runner("//self:env");
    ws.write_build_file(
        "self",
        r#"
target(name = "env", driver = "bash", run = "echo A > $OUT", out = "env.json")
target(name = "user", driver = "bash", run = "echo x > $OUT", out = "o")
"#,
    );

    // Resolves rather than cycling.
    let res = ws.run("//self:user").await?;
    assert!(!common::artifact_string(&res).is_empty());
    Ok(())
}

/// The session actually reaches the process: a variable the runner supplies is
/// visible to the target, and one the target sets itself is *not* overwritten
/// by the runner's value for the same name.
#[tokio::test]
async fn the_session_environment_reaches_the_target() -> anyhow::Result<()> {
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_recording_runner(
        std::sync::Arc::clone(&opens),
        ("FROM_RUNNER", "runner_value"),
    );
    ws.write_build_file(
        "sess",
        r#"
target(name = "env", driver = "bash", run = "echo ARTIFACT_BODY > $OUT", out = "env.json")
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"$FROM_RUNNER|$HEPH_TEST_RUNNER_ARTIFACT|$MINE\" > $OUT",
    out = "o",
    env = {"MINE": "target_value"},
    runner = "//sess:env",
)
"#,
    );

    let res = ws.run("//sess:consumer").await?;
    let out = common::artifact_string(&res);
    assert!(
        out.contains("runner_value"),
        "runner env did not reach the target: {out:?}"
    );
    // The runner derived this from the runner target's artifact, not from thin
    // air — which is what keeps `open` a pure parse of hashed content.
    assert!(
        out.contains("ARTIFACT_BODY"),
        "runner did not see the runner target's artifact: {out:?}"
    );
    assert!(
        out.contains("target_value"),
        "the target's own env must survive: {out:?}"
    );
    Ok(())
}

/// "Spawn the shell once and have multiple targets run within that context" —
/// the requirement the whole session abstraction exists for. Asserted with a
/// counter, because nothing else can show it.
#[tokio::test]
async fn one_open_serves_many_targets() -> anyhow::Result<()> {
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_recording_runner(std::sync::Arc::clone(&opens), ("V", "1"));
    ws.write_build_file(
        "many",
        r#"
target(name = "env", driver = "bash", run = "echo E > $OUT", out = "env.json")
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "o", runner = "//many:env")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "o", runner = "//many:env")
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "o", runner = "//many:env")
"#,
    );

    for t in ["//many:a", "//many:b", "//many:c"] {
        ws.run(t).await?;
    }

    assert_eq!(
        opens.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "three targets sharing one environment must open it once",
    );
    Ok(())
}

/// Two runner targets whose artifacts are byte-identical are the *same*
/// environment, so they share a session. Keying by content rather than by addr
/// is what makes that true — and it is the same property that lets a renamed
/// runner target keep its cache.
#[tokio::test]
async fn identical_artifacts_share_one_session() -> anyhow::Result<()> {
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_recording_runner(std::sync::Arc::clone(&opens), ("V", "1"));
    ws.write_build_file(
        "same",
        r#"
target(name = "env1", driver = "bash", run = "echo SAME > $OUT", out = "env.json")
target(name = "env2", driver = "bash", run = "echo SAME > $OUT", out = "env.json")
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "o", runner = "//same:env1")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "o", runner = "//same:env2")
"#,
    );

    ws.run("//same:a").await?;
    ws.run("//same:b").await?;

    assert_eq!(
        opens.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "byte-identical runner artifacts describe one environment and must share a session",
    );
    Ok(())
}

/// A session's `PATH` **replaces** the driver's, it does not sit under it.
///
/// This is the PATH-replacement rule, and it is the one that decides whether a runner
/// means anything: with the driver's default appended, a tool missing from the
/// environment silently falls through to the host — the exact ambient
/// dependency a runner exists to remove — and does so under a cache key that
/// asserts the runner's environment. Caught end to end, because it was
/// documented and then not implemented.
#[tokio::test]
async fn session_path_replaces_the_drivers_default() -> anyhow::Result<()> {
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_recording_runner_path(
        std::sync::Arc::clone(&opens),
        ("V", "1"),
        // Real enough to find `bash`, marked enough to prove which PATH won.
        // Deliberately NOT `/usr/bin`, which is in the driver's default.
        Some("/runner/only/bin:/bin"),
    );
    ws.write_build_file(
        "p",
        r#"
target(name = "env", driver = "bash", run = "echo E > $OUT", out = "env.json")
target(
    name = "shows_path",
    driver = "bash",
    run = "echo \"$PATH\" > $OUT",
    out = "o",
    runner = "//p:env",
)
"#,
    );

    let res = ws.run("//p:shows_path").await?;
    let path = common::artifact_string(&res);

    assert!(
        path.contains("/runner/only/bin"),
        "the runner's PATH must be in force: {path:?}"
    );
    // The driver's default must not be appended behind it.
    assert!(
        !path.contains("/usr/bin"),
        "the driver's default PATH must not survive under a runner: {path:?}"
    );
    // The sandbox `bin/` prepend still wins over everything — a hermetic
    // `tools =` dep must never be shadowed by an ambient one.
    Ok(())
}

/// A session's teardown runs on shutdown, and exactly once.
///
/// Without it, a `Wrap` session's `docker run -d` container — or a devenv shell
/// — survives every build, and survives Ctrl-C in particular, which is the case
/// it matters in.
///
/// Exercised through the **explicit** shutdown rather than by dropping the
/// engine, because that is the path production takes and the only one that is
/// guaranteed to run: `Drop` fires only if the last `Arc<Engine>` is released,
/// and heph's hard-abort path calls `std::process::exit`, which runs no
/// destructors at all. An earlier version of this test relied on drop; it
/// passed on darwin and failed on linux/arm64, where something still held the
/// `Arc` — silence being exactly the failure mode teardown must not have.
#[tokio::test]
async fn a_sessions_teardown_runs_on_shutdown_exactly_once() -> anyhow::Result<()> {
    let torn = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let opens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ws = Workspace::with_teardown_runner(
        std::sync::Arc::clone(&opens),
        std::sync::Arc::clone(&torn),
    );
    ws.write_build_file(
        "td",
        r#"
target(name = "env", driver = "bash", run = "echo E > $OUT", out = "env.json")
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "o", runner = "//td:env")
"#,
    );
    ws.run("//td:a").await?;
    assert_eq!(
        torn.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "teardown must not run while the session is still in use",
    );

    ws.engine.shutdown_exec_sessions();
    assert_eq!(
        torn.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the session must be torn down on shutdown",
    );

    // Idempotent: a `Drop` backstop after an explicit shutdown must not run a
    // container's `docker rm` — or a shell's kill — a second time.
    ws.engine.shutdown_exec_sessions();
    assert_eq!(
        torn.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "teardown must not run twice",
    );
    Ok(())
}
