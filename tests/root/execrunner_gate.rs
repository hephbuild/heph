//! Guards the exec-runner chokepoint.
//!
//! Every subprocess heph spawns goes through `hexecrunner`, so that a target
//! can be moved into another execution environment by naming a runner, and so
//! that the runner's identity reaches the cache key. A driver that calls
//! `proc_exec::spawn`/`output` directly silently opts out of both: it will keep
//! working, keep passing its own tests, and quietly ignore the runner its
//! target asked for. Nothing else in the tree would notice.
//!
//! That is the whole reason this file exists. The invariant is not "nobody
//! imports `proc_exec`" — drivers still build a `proc_exec::Spec`, which is the
//! type the seam takes — it is "nobody *starts a process* outside the seam".
//!
//! Scoped to `src/` of each workspace member. Test harnesses that exercise
//! `proc_exec` itself are the point of `crates/proc`'s own suite and the root
//! `tests/` files, and are not production spawn sites.
//!
//! Cheap on purpose: a directory walk and some string work, no build.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Members allowed to start a process directly.
///
/// Only the seam. `crates/execrunner` is the one caller that is *supposed* to
/// reach through, and it is where the runner rewrite is applied first.
///
/// `crates/proc` is deliberately absent rather than exempt: it *is*
/// `proc_exec`, so it refers to its own entry points unqualified (`imp::spawn`)
/// and never writes the pattern this gate looks for. If it ever does, that is
/// worth a look rather than an exemption.
const SPAWN_ALLOWED: &[&str] = &["crates/execrunner"];

/// The paths in the root manifest's `workspace.members` array.
///
/// Read from the manifest rather than hardcoded, for the same reason
/// `lint_gate.rs` does it: a copy of the list would stop covering exactly the
/// crates this test exists to catch — the new ones.
fn workspace_members(manifest: &str) -> Vec<String> {
    let after = manifest
        .split_once("members = [")
        .map(|(_, rest)| rest)
        .expect("root Cargo.toml has a workspace.members array");
    let list = after
        .split_once(']')
        .map(|(list, _)| list)
        .expect("workspace.members array is closed");
    list.split(',')
        .map(|entry| entry.trim().trim_matches('"').to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The direct-spawn entry points. `proc_exec::Spec` and the other types are
/// deliberately absent: constructing a spec is what a driver is meant to do.
const DIRECT_SPAWN: &[&str] = &["proc_exec::spawn(", "proc_exec::output("];

#[test]
fn no_workspace_member_spawns_outside_the_exec_runner_seam() {
    let root = repo_root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    let members = workspace_members(&manifest);
    assert!(
        members.len() > 10,
        "parsed only {} workspace members — the manifest layout changed and this \
         gate is now scanning almost nothing",
        members.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for member in &members {
        if SPAWN_ALLOWED.contains(&member.as_str()) {
            continue;
        }
        let src = root.join(member).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                // A line that only mentions the call in prose is not a call.
                let code = line.split_once("//").map_or(line, |(code, _)| code);
                if DIRECT_SPAWN.iter().any(|pat| code.contains(pat)) {
                    let rel = file.strip_prefix(&root).unwrap_or(&file);
                    offenders.push(format!("{}:{}", rel.display(), n + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these spawn a process outside the exec-runner seam, so a target's \
         `runner` would be silently ignored there:\n  {}\n\nUse \
         `hexecrunner::spawn`/`spawn_io`/`output` with a `RunnerRef`. If the \
         call is a host tool the driver invokes for itself rather than the \
         target's command, `RunnerRef::local()` is correct and still goes \
         through the seam.",
        offenders.join("\n  ")
    );
}

/// The allowlist must not rot into a list of crates that merely happen to
/// mention `proc_exec`. Both entries exist for a stated reason; if one stops
/// spawning, it should leave the list.
#[test]
fn the_spawn_allowlist_is_minimal() {
    let root = repo_root();
    for member in SPAWN_ALLOWED {
        let src = root.join(member).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let spawns = files.iter().any(|f| {
            std::fs::read_to_string(f)
                .map(|t| DIRECT_SPAWN.iter().any(|pat| t.contains(pat)))
                .unwrap_or(false)
        });
        assert!(
            spawns,
            "`{member}` is on the direct-spawn allowlist but no longer spawns \
             anything — drop it from SPAWN_ALLOWED so the gate keeps meaning \
             what it says"
        );
    }
}
