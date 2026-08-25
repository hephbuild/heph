//! `runner.json` — the file a runner target produces, and the only thing that
//! decides how a target's command is rewritten.
//!
//! A runner is a *target*, because only a target has a hashout and only a
//! hashout reaches a consumer's cache key without inventing a second hash
//! component. Any driver can produce the file; a hand-written `text_file`
//! target is a legitimate runner, which is what keeps this from being a
//! plugin-only feature.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The only `runner.json` schema version this host understands.
///
/// Checked by exact match, not a range. The tree already has one version field
/// that is written on every path and read on none (`RemoteManifest.version`);
/// a version nobody enforces is decoration, and this one guards a file that
/// decides where a build runs.
pub const RUNNER_JSON_VERSION: u32 = 1;

/// The name of the file a runner target must produce.
pub const RUNNER_JSON: &str = "runner.json";

/// A parsed `runner.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    /// Schema version. Must equal [`RUNNER_JSON_VERSION`].
    pub version: u32,

    /// A digest of the environment this runner provides.
    ///
    /// **This is the field the cache correctness of the whole feature rests
    /// on.** A consumer keys on the runner target's *hashout* — the hash of
    /// these bytes. A config that names only a reference (`{"root": "/x",
    /// "profile": "ci"}`) is byte-identical after the environment it points at
    /// changes, so the hashout does not move and every consumer serves results
    /// built in the old environment. Declaring the digest in the file is what
    /// makes the output change when the environment does.
    ///
    /// It must be *derived*, never authored: hash the runner's own declared
    /// inputs, or better, the resolved environment itself. A hand-written
    /// runner with a copy-pasted fingerprint is a cache-poisoning foot-gun, so
    /// the docs give a recipe and `heph inspect` surfaces the value.
    pub fingerprint: String,

    /// Which registered runner interprets `config`.
    ///
    /// A *name*, not an address: the address is what the consumer wrote and
    /// what reaches the cache key; the name selects code. The indirection is
    /// deliberate — it lets a hand-written `text_file` runner target name a
    /// plugin's runner and get its mechanics without going through that
    /// plugin's driver.
    pub runner: String,

    /// Opaque to everything but the named runner, which validates its own
    /// shape. Defaults to null so a runner needing no configuration (`local`)
    /// can omit it.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl RunnerConfig {
    /// Parse and validate the version. The error names the runner target
    /// because the bytes arrive without any other provenance.
    pub fn parse(bytes: &[u8], addr: &str) -> anyhow::Result<Self> {
        let cfg: Self = serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("runner {addr}: parse {RUNNER_JSON}: {e}"))?;
        if cfg.version != RUNNER_JSON_VERSION {
            anyhow::bail!(
                "runner {addr}: {RUNNER_JSON} declares version {} but this heph understands only \
                 version {RUNNER_JSON_VERSION}",
                cfg.version
            );
        }
        if cfg.fingerprint.is_empty() {
            anyhow::bail!(
                "runner {addr}: {RUNNER_JSON} has an empty `fingerprint`. The fingerprint must \
                 change whenever the environment this runner provides changes — derive it from \
                 the runner's own declared inputs, or from the resolved environment. Without it, \
                 consumers keep serving artifacts built in a stale environment."
            );
        }
        if cfg.runner.is_empty() {
            anyhow::bail!("runner {addr}: {RUNNER_JSON} has an empty `runner` name");
        }
        Ok(cfg)
    }
}

/// Configuration for the builtin `wrap` runner.
///
/// A static rewrite, entirely data — which is why a wrap runner needs no plugin
/// code at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WrapConfig {
    /// argv prepended to the target's command.
    #[serde(default)]
    pub prefix: Vec<String>,

    /// Environment applied over the target's own.
    ///
    /// This is the **hashed** channel: it lives in `runner.json`, so it is part
    /// of the runner target's hashout and therefore part of every consumer's
    /// cache key. A runner that wants to give targets an environment puts it
    /// here, baked at runner-build time.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Host environment variables pulled through at spawn time, by name.
    ///
    /// **Unhashed, and named to say so.** In the exec driver `pass_env` means
    /// "snapshotted at parse and hashed" while `runtime_pass_env` means "read
    /// at run time, excluded from the key". A runner config is resolved at run
    /// time, so a name list pulled at spawn has the latter semantics — offering
    /// it as `pass_env` would read as hashed to anyone who knows this codebase
    /// and silently inject ambient host state into every consumer's cache
    /// entry.
    ///
    /// There is deliberately no hashed `pass_env` here: bake it into [`env`]
    /// instead, where the runner target's hashout covers it.
    ///
    /// [`env`]: WrapConfig::env
    #[serde(default)]
    pub runtime_pass_env: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn parses_a_minimal_config() {
        let cfg = RunnerConfig::parse(
            &json(r#"{"version":1,"fingerprint":"abc","runner":"local"}"#),
            "//x:r",
        )
        .expect("parse");
        assert_eq!(cfg.runner, "local");
        assert_eq!(cfg.fingerprint, "abc");
    }

    /// The version must be enforced, not decorative — `RemoteManifest.version`
    /// is the in-tree example of a field written everywhere and read nowhere.
    #[test]
    fn an_unknown_version_is_rejected_by_name() {
        let err = RunnerConfig::parse(
            &json(r#"{"version":2,"fingerprint":"abc","runner":"local"}"#),
            "//x:r",
        )
        .expect_err("version 2 must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("version 2"), "{msg}");
        assert!(msg.contains("//x:r"), "{msg}");
    }

    #[test]
    fn a_missing_fingerprint_is_rejected_with_the_reason() {
        let err = RunnerConfig::parse(
            &json(r#"{"version":1,"fingerprint":"","runner":"local"}"#),
            "//x:r",
        )
        .expect_err("empty fingerprint must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("fingerprint"), "{msg}");
        assert!(msg.contains("stale environment"), "{msg}");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let err = RunnerConfig::parse(
            &json(r#"{"version":1,"fingerprint":"a","runner":"local","bogus":1}"#),
            "//x:r",
        )
        .expect_err("unknown key must be rejected");
        assert!(format!("{err:#}").contains("bogus"), "{err:#}");
    }

    /// `pass_env` is deliberately absent from the wrap config: it would mean
    /// the opposite of what it means in the exec driver.
    #[test]
    fn wrap_config_has_no_hashed_pass_env() {
        let err = serde_json::from_str::<WrapConfig>(r#"{"pass_env":["HOME"]}"#)
            .expect_err("pass_env must not be accepted");
        assert!(format!("{err}").contains("pass_env"), "{err}");
    }

    #[test]
    fn wrap_config_defaults_are_empty() {
        let w: WrapConfig = serde_json::from_str("{}").expect("parse");
        assert_eq!(w, WrapConfig::default());
    }
}
