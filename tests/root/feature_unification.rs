//! Local shells resolve features once for the whole workspace (`enterShell` in
//! `devenv.nix`), so every `-p` selection shares one build of each crate
//! instead of growing `target/` by a copy per selection.
//!
//! It rides on a nightly-only cargo setting unlocked by a channel override, so
//! a toolchain bump can silently turn it off. Then it only *warns*, and every
//! worktree goes back to accumulating copies with nothing else failing. This
//! asks cargo itself, with the variables `devenv.nix` exports.
//!
//! It also holds the two places the setting must not reach: CI (which builds
//! the release artifacts) and `scripts.e2e`'s release build (which stands in
//! for CI's).

use std::path::Path;
use std::process::Command;

const VARS: [&str; 3] = [
    "__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS",
    "CARGO_UNSTABLE_FEATURE_UNIFICATION",
    "CARGO_RESOLVER_FEATURE_UNIFICATION",
];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn devenv() -> String {
    std::fs::read_to_string(root().join("devenv.nix")).expect("read devenv.nix")
}

/// The `enterShell` block that exports the variables, from its CI guard to
/// its `fi`.
fn guarded_block(devenv: &str) -> &str {
    let guard = "if [ -z \"''${CI:-}\" ]; then";
    let (_, after) = devenv
        .split_once(guard)
        .expect("enterShell exports the feature-unification variables behind a CI guard");
    after
        .split_once("\n    fi")
        .expect("the CI guard is closed")
        .0
}

/// `(name, value)` of each variable the guarded block exports.
fn exported(devenv: &str) -> Vec<(String, String)> {
    let block = guarded_block(devenv);
    VARS.iter()
        .map(|var| {
            let prefix = format!("export {var}=");
            let value = block
                .lines()
                .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
                .unwrap_or_else(|| panic!("{var} is not exported behind the CI guard"));
            ((*var).to_string(), value.to_string())
        })
        .collect()
}

#[test]
fn every_variable_is_exported_only_outside_ci() {
    let devenv = devenv();
    let vars = exported(&devenv);
    assert_eq!(vars.len(), VARS.len());
    let outside = devenv.replace(guarded_block(&devenv), "");
    for var in VARS {
        assert!(
            !outside.contains(&format!("export {var}="))
                && !outside.contains(&format!("env.{var}")),
            "{var} is set outside the CI guard, so CI's release build would get it"
        );
    }
}

#[test]
fn the_e2e_release_build_drops_every_variable() {
    let devenv = devenv();
    let (_, e2e) = devenv
        .split_once("scripts.e2e.exec = ''")
        .expect("scripts.e2e exists");
    let (e2e, _) = e2e.split_once("'';").expect("scripts.e2e is closed");
    let (before_build, _) = e2e
        .split_once("cargo build --release")
        .expect("scripts.e2e builds the release artifacts");
    // The `env -u …` prefix sits on the lines just above the build.
    let (_, lead) = before_build
        .rsplit_once("env -u")
        .expect("the release build has an `env -u` prefix");
    let lead = format!("env -u{lead}");
    for var in VARS {
        assert!(
            lead.contains(&format!("-u {var}")),
            "scripts.e2e's release build keeps {var}, so it no longer builds what CI builds"
        );
    }
}

/// `lint` and `fix` lint the fuse-sandbox=off arm with `--workspace
/// --no-default-features` minus the members that turn it back on. Under
/// workspace-wide unification the excluded members still count, so the arm is
/// unreachable and `lint` fails in every fresh local shell.
#[test]
fn the_feature_off_passes_drop_every_variable() {
    let devenv = devenv();
    for script in ["lint", "fix"] {
        let (_, body) = devenv
            .split_once(&format!("scripts.{script}.exec = ''"))
            .unwrap_or_else(|| panic!("scripts.{script} exists"));
        let (body, _) = body
            .split_once("'';")
            .unwrap_or_else(|| panic!("scripts.{script} is closed"));
        let first = body
            .lines()
            .position(|l| {
                let l = l.trim();
                !l.starts_with('#') && !l.starts_with("echo") && l.contains("--no-default-features")
            })
            .unwrap_or_else(|| panic!("scripts.{script} has a --no-default-features pass"));
        let unset = body
            .lines()
            .take(first)
            .find(|l| l.trim().starts_with("unset "))
            .unwrap_or_else(|| {
                panic!("scripts.{script} runs its --no-default-features pass with features unified")
            });
        for var in VARS {
            assert!(
                unset.split_whitespace().any(|w| w == var),
                "scripts.{script}'s --no-default-features pass keeps {var}"
            );
        }
    }
}

/// `tokio`'s resolved features under `cargo tree <selection>`, with `env`.
fn tokio_features(selection: &[&str], env: &[(String, String)]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root())
        .args([
            "tree",
            "--offline",
            "--locked",
            "-e",
            "features",
            "-i",
            "tokio",
        ])
        .args(["--prefix", "none", "--format", "{p} {f}"])
        .args(selection);
    for var in VARS {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    let out = cmd.output().expect("run cargo tree");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cargo tree {selection:?}: {stderr}");
    assert!(
        !stderr.contains("ignoring"),
        "cargo ignored a setting, so feature unification is off: {stderr}"
    );
    stdout
        .lines()
        .find(|l| l.starts_with("tokio v"))
        .map(|l| {
            let mut f: Vec<&str> = l.rsplit(' ').next().unwrap_or("").split(',').collect();
            f.sort_unstable();
            f.join(",")
        })
        .unwrap_or_else(|| panic!("tokio is not in cargo tree {selection:?}: {stdout}"))
}

#[test]
fn cargo_resolves_a_narrow_selection_like_the_whole_workspace() {
    let env = exported(&devenv());
    let workspace = tokio_features(&["--workspace"], &env);
    for selection in [["-p", "core"], ["-p", "e2e"]] {
        assert_eq!(
            tokio_features(&selection, &env),
            workspace,
            "{selection:?} resolves tokio unlike --workspace: cargo no longer honors \
             the feature-unification variables in devenv.nix"
        );
    }
    // Without the variables the selections must differ, or this test proves
    // nothing about them.
    assert_ne!(
        tokio_features(&["-p", "core"], &[]),
        tokio_features(&["--workspace"], &[]),
        "-p core already resolves tokio like --workspace without the variables; \
         pick a selection that differs"
    );
}
