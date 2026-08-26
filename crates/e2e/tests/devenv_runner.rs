#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The `devenv_runner` driver against a **real** `devenv`.
//!
//! The unit tests prove the pieces — the env-0 parse, the fingerprint's
//! sensitivity and stability, the mode parse. What they cannot prove is that
//! `devenv shell -- env -0` produces what the driver assumes, that the captured
//! environment actually reaches a target, or that the fingerprint is stable
//! across two independent evaluations of the same environment. That last one is
//! the quiet catastrophe of the whole feature: if it drifts, every consumer in
//! the workspace full-misses forever, nothing errors, and nobody traces it back
//! here.
//!
//! # Opt-in, not auto-skip
//!
//! Unlike the docker suite, this one does not run just because the tool is
//! present. A devenv environment nix has never evaluated costs **~2m40s**; the
//! same environment a second time costs ~4s, because nix's eval cache is keyed
//! on content. CI runners are ephemeral, so the honest expectation there is the
//! cold number, on every push, on three platforms.
//!
//! So it is gated on `HEPH_E2E_DEVENV=1` and skips loudly otherwise. Run it
//! when touching the devenv driver:
//!
//! ```sh
//! HEPH_E2E_DEVENV=1 cargo test -p e2e --test devenv_runner
//! ```

mod common;

use hplugin_devenv::plugindevenv;
use htestkit::WorkspaceBuilder;

/// The environment every fixture evaluates.
///
/// Deliberately identical across tests: nix caches an evaluation by content, so
/// one environment shared between them costs one evaluation rather than one
/// each. `env.*` is the cheapest thing devenv can be asked to contribute — no
/// packages means no store paths to realise.
const DEVENV_NIX: &str = r#"{ pkgs, ... }: {
  env.XR_DEVENV_MARKER = "hello-from-devenv";
}
"#;

const DEVENV_YAML: &str = r#"inputs:
  nixpkgs:
    url: github:cachix/devenv-nixpkgs/rolling
"#;

/// Skip unless explicitly opted in — see the module header for why this is not
/// a capability probe.
macro_rules! skip_unless_opted_in {
    () => {
        if std::env::var_os("HEPH_E2E_DEVENV").is_none() {
            eprintln!(
                "skipping: set HEPH_E2E_DEVENV=1 to run the devenv suite. It evaluates a real \
                 devenv environment, which costs ~2m40s the first time any machine sees it."
            );
            return Ok(());
        }
        if which_devenv().is_none() {
            eprintln!("skipping: HEPH_E2E_DEVENV is set but no `devenv` is on PATH");
            return Ok(());
        }
    };
}

fn which_devenv() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("devenv"))
        .find(|p| p.is_file())
}

fn workspace() -> htestkit::Workspace {
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(heph::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_managed_driver(Box::new(heph::pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(plugindevenv::Driver::new()))
        .build()
        .expect("build workspace")
}

/// Write the devenv files plus a `devenv_runner` target over them.
fn write_env(ws: &htestkit::Workspace, mode: &str, extra: &str) {
    ws.write_file("env/devenv.nix", DEVENV_NIX);
    ws.write_file("env/devenv.yaml", DEVENV_YAML);
    ws.write_build_file(
        "env",
        &format!(
            r#"
target(
    name = "runner",
    driver = "devenv_runner",
    mode = "{mode}",
    deps = [glob("devenv.*")],
)
{extra}
"#
        ),
    );
}

/// Wrap mode, end to end: the environment devenv describes reaches the target.
///
/// The marker is set by `devenv.nix` and by nothing else, so a target that sees
/// it can only have got it through the capture.
#[tokio::test]
async fn wrap_mode_puts_the_devenv_environment_in_the_target() -> anyhow::Result<()> {
    skip_unless_opted_in!();
    let ws = workspace();
    write_env(
        &ws,
        "wrap",
        r#"
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"marker=$XR_DEVENV_MARKER\" > $OUT",
    out = "result.txt",
    runner = "//env:runner",
)
"#,
    );

    let got = common::artifact_string(&*ws.run("//env:consumer").await?);
    assert!(
        got.contains("marker=hello-from-devenv"),
        "the devenv environment must reach the target; got {got:?}"
    );
    Ok(())
}

/// **The one that matters.** Two independent evaluations of the same
/// environment must produce byte-identical output.
///
/// If one ambient value leaks into the capture — a temp path, a pid, a
/// timestamp — the runner's hashout moves on every build and every consumer in
/// the workspace misses forever. Nothing fails; the build is just always cold,
/// and the cause is nowhere near the symptom.
///
/// Two separate workspaces, so the tree paths differ too: a capture that
/// embedded its own sandbox path would pass a same-directory check and fail
/// this one.
#[tokio::test]
async fn the_runner_output_is_identical_across_two_workspaces() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let mut outputs = Vec::new();
    for _ in 0..2 {
        let ws = workspace();
        write_env(&ws, "wrap", "");
        outputs.push(common::artifact_string(&*ws.run("//env:runner").await?));
    }

    let (first, second) = (&outputs[0], &outputs[1]);
    assert_eq!(
        first, second,
        "two evaluations of one environment must produce identical bytes — otherwise the \
         runner's hashout moves every build and every consumer full-misses forever"
    );
    assert!(
        first.contains("\"fingerprint\""),
        "the runner must declare a fingerprint; got {first}"
    );
    assert!(
        first.contains("hello-from-devenv"),
        "wrap mode must carry the captured environment; got {first}"
    );
    Ok(())
}

/// Session mode emits a `session` config whose launch enters devenv, and a
/// fingerprint derived the same way wrap's is — it pays one evaluation it does
/// not strictly need so that the fingerprint describes the environment rather
/// than the paperwork.
#[tokio::test]
async fn session_mode_emits_a_launch_and_the_same_fingerprint() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let wrap_ws = workspace();
    write_env(&wrap_ws, "wrap", "");
    let wrap = common::artifact_string(&*wrap_ws.run("//env:runner").await?);

    let session_ws = workspace();
    write_env(&session_ws, "session", "");
    let session = common::artifact_string(&*session_ws.run("//env:runner").await?);

    assert!(session.contains("\"runner\": \"session\""), "got {session}");
    assert!(session.contains("\"launch\""), "got {session}");
    assert!(
        session.contains("shell"),
        "the launch must enter the devenv shell; got {session}"
    );

    // Both modes derive the fingerprint from the resolved environment, so the
    // same environment fingerprints the same either way. The *configs* differ,
    // so the hashouts still differ — which is right: a target's output can
    // depend on process ancestry, so the two modes must not share cache
    // entries.
    let fp = |doc: &str| {
        doc.lines()
            .find(|l| l.contains("\"fingerprint\""))
            .map(str::trim)
            .map(str::to_string)
            .expect("a fingerprint line")
    };
    assert_eq!(fp(&wrap), fp(&session));
    assert_ne!(
        wrap, session,
        "the two modes must not emit identical bytes, or they would share a cache entry"
    );
    Ok(())
}
