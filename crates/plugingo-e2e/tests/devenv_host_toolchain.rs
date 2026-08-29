#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]
#![expect(
    unused_macros,
    reason = "`common` carries `require_go!`, which this suite does not use: the point is that \
              the runner's environment supplies Go, not the host"
)]

//! `gotool = "host"` **inside a devenv**, which is the combination that has to
//! agree about which `go` a build used.
//!
//! `host` does not mean "heph's `go`" — it means "the `go` this build's
//! environment provides". With no runner those are the same thing. With one
//! they are not, and reading heph's `PATH` there compiles with the laptop's
//! toolchain inside the named environment, under a cache key that claims the
//! environment's: a silently wrong build rather than a failing one.
//!
//! The assertion is therefore not "it built" — a build can succeed with the
//! wrong compiler. It is that the compiler baked into the binary is the one the
//! runner's environment provides, computed by asking that environment rather
//! than hardcoded, so the test cannot drift from whatever nixpkgs pins.
//!
//! # Opt-in, not auto-skip
//!
//! An environment nix has never evaluated costs minutes, and this one realises
//! a full Go toolchain. CI runners are ephemeral, so there the honest
//! expectation is that cold cost on every push, on three platforms. Gated:
//!
//! ```sh
//! HEPH_E2E_DEVENV=1 cargo test -p plugingo-e2e --test devenv_host_toolchain
//! ```

mod common;

use common::make_workspace_host_under_runner;
use std::process::Command;

/// Whatever Go this environment's nixpkgs carries — deliberately not a pinned
/// attribute like `pkgs.go_1_23`, which the rolling nixpkgs no longer has and
/// which therefore fails to evaluate at all.
///
/// It does not need to be a *specific* version, only one resolved by the
/// environment rather than by heph. Whether it differs from the host's Go is
/// what decides if a given run can tell a right answer from a wrong one, and
/// the test says which case it got instead of assuming.
const DEVENV_NIX: &str = r#"{ pkgs, ... }: {
  packages = [ pkgs.go ];
}
"#;

const DEVENV_YAML: &str = r#"inputs:
  nixpkgs:
    url: github:cachix/devenv-nixpkgs/rolling
"#;

const GO_MOD: &str = "module example.com/devenvhost\n\ngo 1.21\n";

/// Prints the toolchain that compiled it. `runtime.Version()` is baked in by
/// the compiler, so it reports the `go` that ran, not the `go` on the PATH of
/// whoever runs the binary.
const MAIN_GO: &str = r#"package main

import (
	"fmt"
	"runtime"
)

func main() {
	fmt.Println(runtime.Version())
}
"#;

const BUILD: &str = r#"target(
    name = "runner",
    driver = "devenv_runner",
    mode = "wrap",
    root = "env",
    deps = [glob("env/devenv.*")],
)
"#;

macro_rules! skip_unless_opted_in {
    () => {
        if std::env::var_os("HEPH_E2E_DEVENV").is_none() {
            eprintln!(
                "skipping: set HEPH_E2E_DEVENV=1 to run the devenv suite. It evaluates a real \
                 devenv environment and realises a Go toolchain, which costs minutes the first \
                 time any machine sees it."
            );
            return Ok(());
        }
        if which_on_path("devenv").is_none() {
            eprintln!("skipping: HEPH_E2E_DEVENV is set but no `devenv` is on PATH");
            return Ok(());
        }
    };
}

fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `go env GOVERSION` as answered **inside** the environment — the expected
/// answer, asked of the same place the build is supposed to ask.
///
/// `root` is the directory holding `devenv.nix`, which is the fixture's `env/`
/// subdirectory and not its workspace root: `devenv` resolves its environment
/// from the working directory, and pointing it at the wrong one fails in
/// milliseconds in a way that looks just like a broken environment.
fn devenv_goversion(root: &std::path::Path) -> Option<String> {
    let out = Command::new("devenv")
        .args(["shell", "--", "go", "env", "GOVERSION"])
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `go env GOVERSION` on the host, to say whether this run could tell the two
/// apart at all.
fn host_goversion() -> Option<String> {
    let out = Command::new("go")
        .args(["env", "GOVERSION"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn write_fixture(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("env"))?;
    std::fs::write(dir.join("env/devenv.nix"), DEVENV_NIX)?;
    std::fs::write(dir.join("env/devenv.yaml"), DEVENV_YAML)?;
    std::fs::write(dir.join("BUILD"), BUILD)?;
    std::fs::write(dir.join("go.mod"), GO_MOD)?;
    std::fs::create_dir_all(dir.join("cmd"))?;
    std::fs::write(dir.join("cmd/main.go"), MAIN_GO)?;
    Ok(())
}

/// A Go binary built with `gotool = "host"` under a devenv runner is compiled
/// by the **devenv's** Go, not by heph's.
#[tokio::test]
async fn host_toolchain_under_a_devenv_runner_uses_the_devenvs_go() -> anyhow::Result<()> {
    skip_unless_opted_in!();

    let dir = tempfile::tempdir()?;
    write_fixture(dir.path())?;

    // Deliberately not a skip. Past the opt-in gate, an environment that will
    // not evaluate is a broken fixture, and skipping would report it as a pass.
    let expected = devenv_goversion(&dir.path().join("env")).unwrap_or_else(|| {
        panic!(
            "the fixture environment did not answer `go env GOVERSION`. Run \
             `devenv shell -- go env GOVERSION` in a copy of tests/devenv_host_toolchain.rs's \
             fixture's `env/` to see why."
        )
    });

    // Not a hard failure: if the host happens to ship the same Go, the test
    // still asserts the right thing, it just cannot distinguish the two
    // answers. Say so rather than let it read as proof.
    match host_goversion() {
        Some(host) if host == expected => eprintln!(
            "note: the host Go is also {expected}, so this run cannot distinguish the \
             environment's toolchain from heph's"
        ),
        Some(host) => eprintln!("host Go is {host}; the environment provides {expected}"),
        None => eprintln!("note: no host `go`, so a wrong resolution would have failed outright"),
    }

    // `make_workspace_*` takes ownership of the fixture tempdir, so the staged
    // binary needs somewhere of its own that lives to the end of the test.
    let dir_keep = tempfile::tempdir()?;
    let ws = make_workspace_host_under_runner(dir, "//:runner")?;
    let result = ws.run("//cmd:build@v=host").await?;

    // `artifact_paths` yields entry paths *inside* the artifact, not files on
    // disk, so stage the bytes and run those.
    let bytes = common::artifact_bytes(&result);
    assert!(!bytes.is_empty(), "the build must produce a binary");
    let staged = dir_keep.path().join("built-bin");
    std::fs::write(&staged, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }

    let out = Command::new(&staged)
        .output()
        .unwrap_or_else(|e| panic!("run the built binary {staged:?}: {e}"));
    assert!(out.status.success(), "the built binary must run: {out:?}");
    let reported = String::from_utf8_lossy(&out.stdout).trim().to_string();

    assert_eq!(
        reported, expected,
        "the binary must be compiled by the Go the runner's environment provides. Reported \
         {reported}, environment has {expected} — a mismatch means `gotool = \"host\"` resolved \
         `go` from heph's PATH and then ran that host binary inside the environment."
    );
    Ok(())
}
