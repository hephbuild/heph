//! Guards that every driver's `parse` carries `resources` onto the def.
//!
//! `resources = {...}` on a `target()` reaches the engine only through
//! `TargetDef.resources`, which each driver's `parse` sets (from its spec's
//! `resources` field or `Resources::from_config`). A driver that builds its
//! def with `..Default::default()` and forgets the field keeps working and
//! keeps passing its own tests, and every declaration on its targets is
//! silently ignored: they all admit at one CPU.
//!
//! The rule: a non-test source file that builds a `ParseResponse` with a
//! `TargetDef { … }` names `resources` in that code. Cheap on purpose: a walk
//! and some string work, no build. Each driver crate's own
//! `declared_resources_reach_the_def…` test checks the value; this checks no
//! driver was left out.

use super::execrunner_gate::{repo_root, rust_files, workspace_members};

/// Files whose `parse` builds a def that deliberately declares nothing.
const EXEMPT: &[(&str, &str)] = &[
    (
        "crates/builtins/src/plugincredential/mod.rs",
        "a credential is obtained through the credential store, not built",
    ),
    (
        "crates/builtins/src/pluginscratch/mod.rs",
        "a scratch names a directory the engine's scratch store manages; it runs no work",
    ),
];

/// The part of `text` before its first `#[cfg(test)]`: the tests of a file
/// build defs of their own, and a mention there is not the driver's.
fn non_test(text: &str) -> &str {
    text.split_once("#[cfg(test)]")
        .map_or(text, |(code, _)| code)
}

/// Whether `code` builds a driver's parse response around a `TargetDef`.
fn builds_a_def(code: &str) -> bool {
    code.contains("ParseResponse {") && code.contains("TargetDef {")
}

/// Whether `code` sets the def's resources (`resources: …` or shorthand).
fn sets_resources(code: &str) -> bool {
    code.lines().any(|line| {
        let code = line.split_once("//").map_or(line, |(code, _)| code).trim();
        code.starts_with("resources:") || code == "resources,"
    })
}

#[test]
fn every_driver_parse_carries_resources() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let members = workspace_members(&manifest);
    assert!(members.len() > 10, "parsed only {} members", members.len());

    let mut drivers = 0usize;
    let mut offenders = Vec::new();
    for member in &members {
        let mut files = Vec::new();
        rust_files(&root.join(member).join("src"), &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let code = non_test(&text);
            if !builds_a_def(code) {
                continue;
            }
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            if EXEMPT.iter().any(|(path, _)| *path == rel) {
                continue;
            }
            drivers += 1;
            if !sets_resources(code) {
                offenders.push(rel);
            }
        }
    }
    // exec, fs, group, template, the go and oci drivers, ...: well over 20.
    assert!(
        drivers > 20,
        "found only {drivers} driver parse sites — the pattern no longer matches how drivers \
         build their defs, and this gate is scanning almost nothing"
    );
    assert!(
        offenders.is_empty(),
        "these build a `TargetDef` in `parse` without setting `resources`, so a \
         `resources = {{...}}` on their targets is silently ignored:\n  {}\n\nAdd a \
         `resources: hplugin::driver::Resources` field to the driver's spec (or call \
         `Resources::from_config`) and set `resources:` on the def. If the driver's targets \
         run no work, add the file to EXEMPT with the reason.",
        offenders.join("\n  ")
    );
}

/// An exemption that stops building a def, or starts declaring resources,
/// leaves the list.
#[test]
fn the_exemptions_are_current() {
    let root = repo_root();
    for (path, why) in EXEMPT {
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("exempt file {path} ({why}): {e}"));
        let code = non_test(&text);
        assert!(
            builds_a_def(code) && !sets_resources(code),
            "{path} is exempt ({why}) but no longer needs to be: drop it from EXEMPT"
        );
    }
}

#[test]
fn sets_resources_reads_fields_not_prose() {
    assert!(sets_resources("    resources: spec.resources,\n"));
    assert!(sets_resources("        resources,\n"));
    assert!(!sets_resources("    // resources: not here\n"));
    assert!(!sets_resources("    let resources_total = 1;\n"));
    assert!(!sets_resources("    ..Default::default()\n"));
}
