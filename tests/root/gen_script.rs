//! `gen` regenerates the checkout it is run from.
//!
//! A devenv shell's `$DEVENV_ROOT` is wherever the shell was started. An
//! agent in a second worktree runs `gen` from a shell started in the main
//! checkout, so a `gen` anchored at `$DEVENV_ROOT` deleted and regenerated the
//! main checkout's `gen/`, under whatever was building there, and left the
//! worktree without one. `gen` now works on the cargo workspace around the
//! current directory, as `target-dir .` does, and refuses one that is not a
//! heph checkout rather than `rm -rf` its `gen/`.
//!
//! The scripts' own text from `devenv.nix` runs under `bash`, with `buf`
//! stubbed: what is checked is where they write, not what `buf` generates.

use crate::target_dir_script::{devenv, scripts};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `name`'s body from `devenv.nix`, as the shell sees it.
fn script(name: &str) -> String {
    let body = scripts(&devenv())
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, body)| body)
        .unwrap_or_else(|| panic!("devenv.nix defines {name}"));
    // Nix unescapes `''${` to `${` before the shell sees it.
    body.replace("''${", "${")
}

fn write_exe(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/usr/bin/env bash\n{body}")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// An empty cargo workspace in `dir`, a heph checkout when `heph` is set
/// (its `buf.gen.yaml` and `proto/Cargo.toml.tpl`), with a stale `gen/`.
fn workspace(dir: &Path, heph: bool) -> PathBuf {
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n")
        .expect("write Cargo.toml");
    if heph {
        std::fs::write(dir.join("buf.gen.yaml"), "version: v2\n").expect("write buf.gen.yaml");
        std::fs::create_dir_all(dir.join("proto")).expect("mkdir proto");
        std::fs::write(dir.join("proto/Cargo.toml.tpl"), "[package]\n").expect("write tpl");
    }
    std::fs::create_dir_all(dir.join("gen")).expect("mkdir gen");
    std::fs::write(dir.join("gen/stale"), "").expect("write stale");
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir sub");
    dir.canonicalize().expect("canonicalize")
}

/// Run `gen` from `cwd`, in a shell started in `root`. `gen-proto` is its own
/// `devenv.nix` text; `buf` records the directory it generated into.
fn run_gen(root: &Path, cwd: &Path) -> Output {
    let bin = tempfile::tempdir().expect("bin tempdir");
    write_exe(&bin.path().join("gen-proto"), &script("gen-proto"));
    write_exe(
        &bin.path().join("buf"),
        r#"mkdir -p gen/proto/src && printf %s "$PWD" > gen/proto/src/where"#,
    );
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg("-c")
        .arg(script("gen"))
        .current_dir(cwd)
        .env("DEVENV_ROOT", root)
        .env("PATH", path)
        .output()
        .expect("run bash")
}

#[test]
fn gen_regenerates_the_checkout_it_is_run_from_not_the_shells() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let root = workspace(a.path(), true);
    let worktree = workspace(b.path(), true);

    // From a subdirectory, as an agent in a crate of its worktree would.
    let out = run_gen(&root, &worktree.join("sub"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "gen failed: {stderr}");

    assert!(
        root.join("gen/stale").exists(),
        "the shell's checkout was left alone"
    );
    assert!(
        !worktree.join("gen/stale").exists(),
        "the worktree's gen/ was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("gen/proto/src/where")).expect("buf ran"),
        worktree.display().to_string(),
        "buf generated at the worktree's root"
    );
    assert!(
        worktree.join("gen/proto/Cargo.toml").exists(),
        "the crate manifest was copied in"
    );
    assert!(
        stderr.contains(&worktree.join("gen").display().to_string()),
        "gen says which gen/ it regenerated: {stderr}"
    );
}

#[test]
fn gen_refuses_a_directory_that_is_not_a_heph_checkout() {
    let a = tempfile::tempdir().expect("tempdir");
    let b = tempfile::tempdir().expect("tempdir");
    let c = tempfile::tempdir().expect("tempdir");
    let root = workspace(a.path(), true);
    let other = workspace(b.path(), false);

    // Another cargo project: its own `gen/` is not heph's to delete.
    let out = run_gen(&root, &other);
    assert!(!out.status.success(), "gen ran in a non-heph workspace");
    assert!(other.join("gen/stale").exists(), "its gen/ was kept");

    // No cargo workspace at all: an error, not a fallback to the shell's.
    let out = run_gen(&root, &c.path().canonicalize().expect("canonicalize"));
    assert!(!out.status.success(), "gen ran outside any workspace");

    assert!(
        root.join("gen/stale").exists(),
        "the shell's checkout was left alone"
    );
}
