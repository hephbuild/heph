#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `runner =` selection and its effect on the cache key.
//!
//! Design: `docs/EXEC_RUNNERS.md`. The property under test throughout is §4.3's:
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
    let ws = Workspace::new();
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

/// The workspace default applies to targets that did not author one — which §6
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
