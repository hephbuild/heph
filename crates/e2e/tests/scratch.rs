#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! End-to-end coverage for the `scratch` declaration driver.
//!
//! These go through the real `Engine` — provider, BUILD-file evaluation, driver
//! registry, `get_def` — rather than calling the driver directly, because what is
//! being tested is the *wiring*: that `driver = "scratch"` resolves at all, that
//! its config reaches the driver, and that a bad declaration fails where a BUILD
//! author will see it.
//!
//! The storage, mounting and locking a declaration eventually drives are not here;
//! a declaration is inert on its own. See `docs/SCRATCH.md`.

mod common;

use common::Workspace;

/// `EResult` has no `Debug`, so `expect_err` will not compile. Unwrap the error
/// side explicitly instead.
fn expect_err<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

/// The driver is registered and a declaration resolves. Without this, everything
/// downstream fails with "driver not found" and no test says why.
#[tokio::test]
async fn a_scratch_declaration_resolves_through_the_engine() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"
target(
    name = "gocache",
    driver = "scratch",
    path = ".cache/go-build",
    env = "GOCACHE",
    access = "shared",
    platform = "os_arch",
    version = "go1.23",
    remote = True,
)
"#,
    );

    let spec = ws.get_spec("//build:gocache").await?;
    assert_eq!(spec.driver, "scratch");
    Ok(())
}

/// A declaration produces no artifacts, so resolving one yields an empty result
/// rather than an error. That is what makes it safe to reference from the graph.
#[tokio::test]
async fn a_declaration_produces_no_artifacts() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "cache")"#,
    );

    let result = ws.run("//build:c").await?;
    assert!(
        result.artifacts.is_empty(),
        "a scratch declaration must produce nothing, got {} artifacts",
        result.artifacts.len()
    );
    Ok(())
}

/// The whole point of declaring it as a target: two packages can each have a
/// `gocache` without agreeing on a naming convention, because the addr is the
/// identity.
#[tokio::test]
async fn two_packages_can_declare_the_same_name() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "go",
        r#"target(name = "cache", driver = "scratch", path = ".cache/go")"#,
    );
    ws.write_build_file(
        "rust",
        r#"target(name = "cache", driver = "scratch", path = ".cache/rust")"#,
    );

    let go = ws.get_spec("//go:cache").await?;
    let rust = ws.get_spec("//rust:cache").await?;
    assert_eq!(go.driver, "scratch");
    assert_eq!(rust.driver, "scratch");
    assert_ne!(go.addr.format(), rust.addr.format());
    Ok(())
}

/// `path` is what a consumer mounts, so a missing one is not a defaultable
/// omission — it is an incomplete declaration, and the error must land at parse
/// time in the package that wrote it.
#[tokio::test]
async fn a_declaration_without_a_path_fails_at_parse() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", version = "1")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "a scratch without `path` must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("path"), "error must name the field: {msg}");
    Ok(())
}

/// A mount is a symlink out of the sandbox. An absolute path would let a BUILD
/// file place it anywhere on the machine, so it is rejected at the declaration
/// rather than at each use.
#[tokio::test]
async fn an_absolute_path_is_rejected_at_the_declaration() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "/var/tmp/cache")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "an absolute scratch path must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("relative"), "error must explain why: {msg}");
    // The addr belongs in the message: a workspace with many declarations needs
    // to know which one is wrong.
    assert!(
        msg.contains("//build:c"),
        "error must name the target: {msg}"
    );
    Ok(())
}

/// An unknown `access` is a typo, not a request for new behaviour. Saying what the
/// two options *mean* is the difference between a one-line fix and a docs hunt.
#[tokio::test]
async fn an_unknown_access_names_the_valid_options() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", path = "cache", access = "concurrent")"#,
    );

    let err = expect_err(
        ws.run("//build:c").await,
        "an unknown access must not resolve",
    );
    let msg = format!("{err:#}");
    assert!(msg.contains("exclusive") && msg.contains("shared"), "{msg}");
    Ok(())
}
