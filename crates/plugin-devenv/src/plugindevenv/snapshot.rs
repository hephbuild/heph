//! The environment snapshot: what `devenv print-dev-env --json` becomes.
//!
//! This file is the whole hermeticity argument for the `devenv` runner, so the
//! rules are here rather than spread through the driver.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bump to invalidate every snapshot when the filtering rules change.
///
/// Not optional. The runner half is not a target, so it has no `TargetDef.hash`
/// of its own — without a version folded into the *driver's* def hash, dropping
/// a variable from the filter (a correctness fix) would leave every existing
/// artifact keyed as though the old environment were still in force. The same
/// reason `NixDef.system` and `EXEC_DEF_FORMAT_VERSION` exist.
/// v2: session-mode preludes gained `export -f`, without which a function was
/// defined in a shell the target never runs in.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 2;

/// The canonicalized environment. **This artifact _is_ the description** — the
/// runner half does nothing but parse it, so everything the environment depends
/// on is content the cache key already covers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub format_version: u32,
    /// Sorted for a byte-stable artifact: an unsorted map would re-key every
    /// consumer on a whim of iteration order.
    pub env: BTreeMap<String, String>,
    /// Names only, for diagnostics. A target whose `run` calls one of these gets
    /// "…is a devenv shell function, not a binary" instead of a misleading
    /// "not found in PATH" — which is the difference between a dead end and a
    /// recoverable one.
    pub shell_functions: Vec<String>,
    /// PATH entries dropped for being outside `/nix/store`, so the spawn
    /// failure can say what was removed and why.
    pub dropped_path_entries: Vec<String>,
    /// Variables dropped for naming a machine-local path.
    pub dropped_vars: Vec<String>,
    /// The shell functions' definitions, as a bash snippet.
    ///
    /// Empty unless the target asked for `mode = "session"`. Carried in the
    /// artifact rather than re-derived at `open` for the same reason as
    /// everything else here: `open` runs after `hashin` and not at all on a
    /// cached build, so a definition discovered there would be unhashed input.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shell_prelude: String,
}

/// Paths that make a snapshot machine-specific. A value mentioning any of them
/// would make the artifact differ between two checkouts of the same commit — so
/// the same lockfiles would produce different cache keys on a laptop and in CI,
/// and on two worktrees of one machine.
#[derive(Debug, Clone)]
pub struct LocalPaths {
    pub tree_root: String,
    pub home: String,
    pub tmpdir: String,
}

/// Names that are never environment regardless of value: they describe *this
/// invocation* rather than the environment it produced.
const ALWAYS_DROP: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TEMPDIR",
    "HOME",
    "USER",
    "LOGNAME",
    "TERM",
    "IN_NIX_SHELL",
];

/// Prefixes that are never environment. `DEVENV_CMDLINE` is the sharpest: it is
/// the literal command line used to enter the shell, so without this the
/// snapshot would differ between `devenv shell` and `devenv shell -- cmd`.
const ALWAYS_DROP_PREFIX: &[&str] = &["DEVENV_", "NIX_BUILD_", "__"];

fn is_store_path(p: &str) -> bool {
    p.starts_with("/nix/store/")
}

/// Build a snapshot from `devenv print-dev-env --json`'s decoded variables.
///
/// Three filters, each closing a hole measured in a real devenv shell:
///
/// 1. **Only `exported` variables.** Shell-local `var`/`array` entries are not
///    environment; passing them would be inventing an environment nobody has.
/// 2. **Nothing naming a machine-local path.** Measured in this repo's own
///    shell, `DEVENV_ROOT`/`DEVENV_DOTFILE`/`DEVENV_STATE` carry the absolute
///    checkout path and `NIX_PROFILES` carries `$HOME` — so without this, two
///    worktrees, or a laptop and CI, produce different keys for every target
///    under the runner. Over-hashing, and it switches the remote cache off in
///    practice.
/// 3. **`PATH` is store-only.** Measured: 38 of 107 entries were outside
///    `/nix/store` (`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, …).
///    Those are mutable directories, so two machines with identical PATH
///    *strings* but different `/opt/homebrew/bin/protoc` produce identical
///    snapshot bytes, identical `hashin`, and different artifacts — shared
///    through the remote cache. Under-hashing, and the worse of the two.
///    Dropping them also restores the layer-2 rule (`docs/EXEC_RUNNERS.md`
///    §4.4): a tool missing from the environment now fails loudly instead of
///    silently falling through to the host.
pub fn build(
    variables: &BTreeMap<String, Variable>,
    shell_functions: Vec<String>,
    local: &LocalPaths,
) -> Snapshot {
    build_with_prelude(variables, shell_functions, local, String::new())
}

pub fn build_with_prelude(
    variables: &BTreeMap<String, Variable>,
    shell_functions: Vec<String>,
    local: &LocalPaths,
    shell_prelude: String,
) -> Snapshot {
    let mut env = BTreeMap::new();
    let mut dropped_vars = Vec::new();
    let mut dropped_path_entries = Vec::new();

    for (name, var) in variables {
        if var.r#type != "exported" {
            continue;
        }
        if ALWAYS_DROP.contains(&name.as_str())
            || ALWAYS_DROP_PREFIX.iter().any(|p| name.starts_with(p))
        {
            continue;
        }

        if name == "PATH" {
            let mut kept = Vec::new();
            for entry in var.value.split(':').filter(|e| !e.is_empty()) {
                if is_store_path(entry) {
                    kept.push(entry.to_string());
                } else {
                    dropped_path_entries.push(entry.to_string());
                }
            }
            if !kept.is_empty() {
                env.insert("PATH".to_string(), kept.join(":"));
            }
            continue;
        }

        if mentions_local_path(&var.value, local) {
            dropped_vars.push(name.clone());
            continue;
        }
        env.insert(name.clone(), var.value.clone());
    }

    dropped_vars.sort();
    dropped_path_entries.sort();
    dropped_path_entries.dedup();
    let mut shell_functions = shell_functions;
    shell_functions.sort();

    Snapshot {
        format_version: SNAPSHOT_FORMAT_VERSION,
        env,
        shell_functions,
        dropped_path_entries,
        dropped_vars,
        shell_prelude,
    }
}

fn mentions_local_path(value: &str, local: &LocalPaths) -> bool {
    [&local.tree_root, &local.home, &local.tmpdir]
        .into_iter()
        .any(|p| !p.is_empty() && value.contains(p.as_str()))
}

/// One entry of `print-dev-env --json`'s `variables` map.
#[derive(Debug, Clone, Deserialize)]
pub struct Variable {
    pub r#type: String,
    /// `serde_json::Value` in the wire format for `array` entries; only
    /// `exported` ones are used and those are always strings.
    #[serde(default, deserialize_with = "string_or_empty")]
    pub value: String,
}

fn string_or_empty<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        _ => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> LocalPaths {
        LocalPaths {
            tree_root: "/Users/alice/repo".to_string(),
            home: "/Users/alice".to_string(),
            tmpdir: "/tmp/x".to_string(),
        }
    }

    fn vars(pairs: &[(&str, &str, &str)]) -> BTreeMap<String, Variable> {
        pairs
            .iter()
            .map(|(n, t, v)| {
                (
                    n.to_string(),
                    Variable {
                        r#type: t.to_string(),
                        value: v.to_string(),
                    },
                )
            })
            .collect()
    }

    /// The under-hashing hole: a mutable host directory on PATH means two
    /// machines can have identical snapshot bytes and different tools.
    #[test]
    fn path_keeps_only_store_entries() {
        let s = build(
            &vars(&[(
                "PATH",
                "exported",
                "/nix/store/aaa/bin:/opt/homebrew/bin:/usr/bin:/nix/store/bbb/bin",
            )]),
            vec![],
            &local(),
        );
        assert_eq!(
            s.env.get("PATH").map(String::as_str),
            Some("/nix/store/aaa/bin:/nix/store/bbb/bin")
        );
        assert_eq!(
            s.dropped_path_entries,
            vec!["/opt/homebrew/bin", "/usr/bin"]
        );
    }

    /// The over-hashing hole. Measured in this repo: `DEVENV_ROOT` is the
    /// absolute checkout path, so without this two worktrees of one commit key
    /// every target differently.
    #[test]
    fn variables_naming_machine_local_paths_are_dropped() {
        let s = build(
            &vars(&[
                ("DEVENV_ROOT", "exported", "/Users/alice/repo"),
                ("NIX_PROFILES", "exported", "/nix/var/nix /Users/alice/.nix"),
                ("CC", "exported", "/nix/store/ccc/bin/clang"),
            ]),
            vec![],
            &local(),
        );
        assert!(s.env.contains_key("CC"));
        assert!(!s.env.contains_key("NIX_PROFILES"));
        // DEVENV_* is dropped by name before the value is even looked at.
        assert!(!s.env.contains_key("DEVENV_ROOT"));
        assert_eq!(s.dropped_vars, vec!["NIX_PROFILES"]);
    }

    /// The property the whole filter exists for: same lockfiles, different
    /// checkout, byte-identical snapshot.
    #[test]
    fn two_checkouts_produce_identical_snapshots() {
        let mk = |root: &str, home: &str| {
            build(
                &vars(&[
                    ("DEVENV_ROOT", "exported", root),
                    ("NIX_PROFILES", "exported", &format!("{home}/.nix-profile")),
                    ("CC", "exported", "/nix/store/ccc/bin/clang"),
                    ("PATH", "exported", "/nix/store/aaa/bin:/usr/bin"),
                ]),
                vec!["fmt-all".to_string()],
                &LocalPaths {
                    tree_root: root.to_string(),
                    home: home.to_string(),
                    tmpdir: String::new(),
                },
            )
        };
        assert_eq!(
            mk("/Users/alice/repo", "/Users/alice"),
            mk("/home/runner/work/repo/repo", "/home/runner"),
        );
    }

    #[test]
    fn only_exported_variables_are_environment() {
        let s = build(
            &vars(&[
                ("A", "exported", "1"),
                ("B", "var", "2"),
                ("C", "array", "3"),
                ("D", "unknown", "4"),
            ]),
            vec![],
            &local(),
        );
        assert_eq!(s.env.keys().collect::<Vec<_>>(), vec!["A"]);
    }

    /// `DEVENV_CMDLINE` is the literal command used to enter the shell, so
    /// without the prefix rule the snapshot differs between `devenv shell` and
    /// `devenv shell -- cmd` on the same machine.
    #[test]
    fn devenv_bookkeeping_never_reaches_the_environment() {
        let s = build(
            &vars(&[
                ("DEVENV_CMDLINE", "exported", "shell -- claude"),
                ("__structuredAttrs", "exported", "1"),
                ("NIX_BUILD_CORES", "exported", "10"),
            ]),
            vec![],
            &local(),
        );
        assert!(s.env.is_empty(), "{:?}", s.env);
    }

    #[test]
    fn shell_function_names_are_recorded_sorted() {
        let s = build(&vars(&[]), vec!["z".into(), "a".into()], &local());
        assert_eq!(s.shell_functions, vec!["a", "z"]);
    }
}
