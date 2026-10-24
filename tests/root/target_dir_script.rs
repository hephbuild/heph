//! A devenv script that builds must read its artifacts from the workspace it
//! built.
//!
//! `target-dir` answers for `$DEVENV_ROOT` — wherever the shell was started —
//! because `run-release` and friends are run against *other* projects. A
//! script that has just run `cargo build` built the workspace it was run from
//! instead, and in a second worktree those are different checkouts: `e2e` there
//! built the worktree, then staged the main checkout's stale binaries and
//! tested those, green. So such a script asks `target-dir .`.
//!
//! Cheap on purpose: one file read, string work, and two `cargo
//! locate-project` calls against empty manifests.

use std::path::{Path, PathBuf};
use std::process::Command;

fn devenv() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("devenv.nix");
    std::fs::read_to_string(&path).expect("read devenv.nix")
}

/// Every `scripts.<name>.exec = ''…'';` body in `devenv.nix`, by name.
fn scripts(devenv: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = devenv;
    while let Some((_, after)) = rest.split_once("scripts.") {
        rest = after;
        let Some((name, after)) = after.split_once(".exec = ''") else {
            continue;
        };
        // A name is one identifier; anything else is prose mentioning
        // `scripts.` that happened to precede a later definition.
        if name.contains(|c: char| c.is_whitespace() || c == '=') {
            continue;
        }
        let (body, _) = after.split_once("'';").expect("script body is closed");
        out.push((name.to_string(), body.to_string()));
    }
    out
}

#[test]
fn a_script_that_builds_reads_the_target_dir_it_built() {
    let scripts = scripts(&devenv());
    let mut checked = Vec::new();
    for (name, body) in &scripts {
        if !body.contains("cargo build") || !body.contains("target-dir") {
            continue;
        }
        let calls = body.matches("target-dir").count();
        let anchored = body.matches("target-dir .").count();
        assert_eq!(
            calls, anchored,
            "`{name}` runs `cargo build` in the current workspace but reads \
             artifacts with a bare `target-dir`, which answers for \
             `$DEVENV_ROOT` — another checkout when run from a worktree. Use \
             `target-dir .`."
        );
        checked.push(name.as_str());
    }
    // Not vacuous: the scripts this exists for were found and checked.
    for want in ["e2e", "install-go-plugin", "install-release-build"] {
        assert!(checked.contains(&want), "{want} not checked: {checked:?}");
    }
}

/// An empty cargo workspace in `dir`.
fn workspace(dir: &Path) -> PathBuf {
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n")
        .expect("write Cargo.toml");
    dir.canonicalize().expect("canonicalize")
}

/// Run `target-dir`'s own text from `cwd`, with the shell started in `root`.
fn target_dir(root: &Path, cwd: &Path, args: &[&str]) -> String {
    let devenv = devenv();
    let body = scripts(&devenv)
        .into_iter()
        .find(|(name, _)| name == "target-dir")
        .map(|(_, body)| body)
        .expect("devenv.nix defines target-dir");
    // Nix unescapes `''${` to `${` before the shell sees it.
    let body = body.replace("''${", "${");
    let out = Command::new("bash")
        .arg("-c")
        .arg(body)
        .arg("target-dir")
        .args(args)
        .current_dir(cwd)
        .env("DEVENV_ROOT", root)
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "target-dir failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_string()
}

#[test]
fn target_dir_answers_for_the_shell_root_unless_given_a_dir() {
    let root = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("tempdir");
    let root = workspace(root.path());
    let worktree = workspace(other.path());

    // Bare: the checkout the shell was started in, from anywhere — what
    // `run-release` against another project needs.
    assert_eq!(
        target_dir(&root, &worktree, &[]),
        root.join("target").display().to_string()
    );
    // `.`: the workspace the caller is in — what a script that just built
    // needs.
    assert_eq!(
        target_dir(&root, &worktree, &["."]),
        worktree.join("target").display().to_string()
    );
}
