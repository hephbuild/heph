//! A local mac shell packs debug info into a `.dSYM` per binary
//! (`CARGO_PROFILE_DEV_SPLIT_DEBUGINFO`, set by `enterShell` in `devenv.nix`).
//! Cargo's macOS default leaves each binary's object files in `target/`, and
//! every edit of a crate adds a new set that nothing deletes.
//!
//! This holds where the variable may reach (macOS, outside CI) and that cargo
//! applies it to test binaries, which build under `[profile.test]` and are
//! where the objects piled up.

use std::path::Path;
use std::process::Command;

const VAR: &str = "CARGO_PROFILE_DEV_SPLIT_DEBUGINFO";

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn devenv() -> String {
    std::fs::read_to_string(root().join("devenv.nix")).expect("read devenv.nix")
}

/// The value `enterShell` exports, checked to sit in its macOS-only part and
/// behind the CI guard.
fn exported_value(devenv: &str) -> String {
    let export = format!("export {VAR}=");
    assert_eq!(
        devenv.matches(export.as_str()).count(),
        1,
        "{VAR} must be exported exactly once, from enterShell"
    );
    let (_, shell) = devenv
        .split_once("enterShell = ")
        .expect("devenv.nix has an enterShell");
    let (_, darwin) = shell
        .split_once("lib.optionalString pkgs.stdenv.isDarwin ''")
        .expect("enterShell has a macOS-only part");
    let (darwin, _) = darwin.split_once("'';").expect("enterShell is closed");
    let (_, guarded) = darwin
        .split_once("if [ -z \"''${CI:-}\" ]; then")
        .unwrap_or_else(|| panic!("{VAR} is exported outside the CI guard, so CI gets it"));
    let (guarded, _) = guarded
        .split_once("\n    fi")
        .expect("the CI guard is closed");
    guarded
        .lines()
        .find_map(|l| l.trim().strip_prefix(export.as_str()))
        .unwrap_or_else(|| {
            panic!("{VAR} is not exported behind the CI guard in enterShell's macOS-only part")
        })
        .to_string()
}

#[test]
fn packed_is_exported_only_in_local_mac_shells() {
    assert_eq!(exported_value(&devenv()), "packed");
}

/// `split_debuginfo` of every compiled unit in `cargo test -p core`'s unit
/// graph. Reading the graph compiles nothing; it is unstable, so the channel
/// override unlocks it for this one cargo call.
fn split_debuginfo_of_test_units(value: &str) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .current_dir(root())
        .args(["test", "-p", "core", "--no-run", "--offline", "--locked"])
        .args(["--unit-graph", "-Z", "unstable-options"])
        .env("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS", "nightly")
        .env(VAR, value)
        .output()
        .expect("run cargo --unit-graph");
    assert!(
        out.status.success(),
        "cargo --unit-graph: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let graph: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the unit graph is JSON");
    graph["units"]
        .as_array()
        .expect("the unit graph lists units")
        .iter()
        .filter(|u| u["mode"] != "run-custom-build")
        .map(|u| {
            u["profile"]["split_debuginfo"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

#[test]
fn cargo_applies_the_shell_value_to_test_binaries() {
    let value = exported_value(&devenv());
    let units = split_debuginfo_of_test_units(&value);
    assert!(!units.is_empty(), "cargo test -p core compiles no unit");
    let mut wrong: Vec<&String> = units.iter().filter(|u| **u != value).collect();
    wrong.sort_unstable();
    wrong.dedup();
    assert!(
        wrong.is_empty(),
        "with {VAR}={value}, cargo test resolves split-debuginfo to {wrong:?}: \
         [profile.test] no longer takes it from the dev profile, so test binaries \
         leave their object files in target/ again"
    );
}
