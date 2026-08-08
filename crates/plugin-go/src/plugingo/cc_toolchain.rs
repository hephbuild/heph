//! The C compiler a **race** build needs, and only where it needs one.
//!
//! Go's race detector ships as a prebuilt TSan runtime that has to be reachable
//! from Go code. On darwin that link is pure Go — `runtime/race/race_darwin_*.go`
//! carries the syso-derived symbol information — so a darwin race build stays as
//! hermetic as an ordinary one and never comes near this module. Everywhere else
//! `runtime/race` is a **cgo** package, so building the standard library with
//! `-race` means compiling C, which means a C compiler.
//!
//! Only [`crate::plugingo::target_std::install_spec`] uses it: `go install -race
//! std` is the single step that compiles C. The per-package compiles are pure Go
//! either way, and the link resolves the TSan runtime internally — both were
//! verified to succeed with an empty `PATH` and no compiler in sight.
//!
//! See [`crate::plugingo::factors::cgo_required`] for the platform rule.

use hcore::htvalue::Value;
use std::collections::HashMap;

/// Dep group the C compiler is staged under.
pub const CC_DEP_GROUP: &str = "cc";

/// Default for the `cctool` provider option: the host's C compiler, exposed as a
/// target by the hostbin provider. Mirrors how `gotool = "//@heph/bin:go"`
/// reaches the host toolchain — an explicit, addressable dependency rather than
/// an ambient one, so pointing `cctool` at a hermetic compiler instead is a
/// one-line config change.
pub fn default_addr() -> String {
    "//@heph/bin:cc".to_string()
}

/// `(group, value)` dep entry staging the `cctool` target for a race build that
/// needs cgo. `None` when this build does not — an ordinary build, or a race
/// build on darwin — so no `cctool` target is ever resolved for them.
pub fn cc_dep(cctool_addr: &str, goos: &str, race: bool) -> Option<(String, Value)> {
    if !crate::plugingo::factors::cgo_required(goos, race) {
        return None;
    }
    Some((
        CC_DEP_GROUP.to_string(),
        Value::List(vec![Value::String(cctool_addr.to_string())]),
    ))
}

/// Shell lines pointing `go` at the staged compiler, for a script that already
/// carries the [`cc_dep`] group. Empty when this build needs no compiler.
///
/// `$SRC_CC` is the exec driver's auto-injected path for the `cc` dep group.
pub fn cc_prelude(goos: &str, race: bool) -> Vec<String> {
    if !crate::plugingo::factors::cgo_required(goos, race) {
        return Vec::new();
    }
    vec![
        // Absolute: `go` re-invokes CC from various working directories.
        "CC=\"$(cd \"$(dirname \"$SRC_CC\")\" && pwd)/$(basename \"$SRC_CC\")\"".to_string(),
        "export CC".to_string(),
    ]
}

/// Env names a cgo-enabled race build must see at run time, unhashed.
///
/// `PATH` is unavoidable: the C compiler finds its own subprograms (`as`, `ld`,
/// `cc1`) through it, and with an empty `PATH` cgo fails with
/// `cannot execute 'as'`. Runtime rather than hashed, matching how the plugin
/// already treats a non-hermetic toolchain's `PATH`. Empty when no compiler is
/// needed, so a hermetic build stays PATH-independent.
pub fn cc_runtime_pass_env(goos: &str, race: bool) -> Vec<String> {
    if crate::plugingo::factors::cgo_required(goos, race) {
        vec!["PATH".to_string()]
    } else {
        Vec::new()
    }
}

/// Merge [`cc_runtime_pass_env`] into an existing `runtime_pass_env` config value,
/// preserving whatever the caller already set and de-duplicating.
pub fn merge_runtime_pass_env(config: &mut HashMap<String, Value>, goos: &str, race: bool) {
    let extra = cc_runtime_pass_env(goos, race);
    if extra.is_empty() {
        return;
    }
    let mut names: Vec<String> = match config.get("runtime_pass_env") {
        Some(Value::List(v)) => v
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for name in extra {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    config.insert(
        "runtime_pass_env".to_string(),
        Value::List(names.into_iter().map(Value::String).collect()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cc_for_an_ordinary_build() {
        assert!(cc_dep("//@heph/bin:cc", "linux", false).is_none());
        assert!(cc_prelude("linux", false).is_empty());
        assert!(cc_runtime_pass_env("linux", false).is_empty());
    }

    #[test]
    fn no_cc_for_a_darwin_race_build() {
        // darwin's runtime/race is pure Go + a syso, so a race build there must
        // stay hermetic — no compiler dep, no PATH passthrough.
        assert!(cc_dep("//@heph/bin:cc", "darwin", true).is_none());
        assert!(cc_prelude("darwin", true).is_empty());
        assert!(cc_runtime_pass_env("darwin", true).is_empty());
    }

    #[test]
    fn linux_race_build_stages_the_compiler() {
        let (group, value) = cc_dep("//@heph/bin:cc", "linux", true).expect("cc dep");
        assert_eq!(group, CC_DEP_GROUP);
        assert!(matches!(value, Value::List(v) if v.len() == 1));
        assert!(cc_prelude("linux", true).iter().any(|l| l.contains("CC")));
        assert_eq!(cc_runtime_pass_env("linux", true), vec!["PATH".to_string()]);
    }

    #[test]
    fn merge_runtime_pass_env_keeps_existing_names() {
        let mut config = HashMap::from([(
            "runtime_pass_env".to_string(),
            Value::List(vec![Value::String("HOME".to_string())]),
        )]);
        merge_runtime_pass_env(&mut config, "linux", true);
        let names = match config.get("runtime_pass_env") {
            Some(Value::List(v)) => v.clone(),
            other => panic!("expected list, got {other:?}"),
        };
        assert_eq!(
            names,
            vec![
                Value::String("HOME".to_string()),
                Value::String("PATH".to_string())
            ]
        );
    }

    #[test]
    fn merge_runtime_pass_env_does_not_duplicate() {
        let mut config = HashMap::from([(
            "runtime_pass_env".to_string(),
            Value::List(vec![Value::String("PATH".to_string())]),
        )]);
        merge_runtime_pass_env(&mut config, "linux", true);
        let names = match config.get("runtime_pass_env") {
            Some(Value::List(v)) => v.clone(),
            other => panic!("expected list, got {other:?}"),
        };
        assert_eq!(names, vec![Value::String("PATH".to_string())]);
    }

    #[test]
    fn merge_runtime_pass_env_is_a_noop_without_cgo() {
        let mut config: HashMap<String, Value> = HashMap::new();
        merge_runtime_pass_env(&mut config, "darwin", true);
        assert!(!config.contains_key("runtime_pass_env"));
    }
}
