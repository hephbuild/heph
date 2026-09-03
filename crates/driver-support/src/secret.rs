//! The producer→host channel for credential references.
//!
//! A target naming a credential records it as an ordinary [`Input`] marked with
//! the annotation below. Annotations are the existing way a driver tells the
//! *host* something about a dependency edge, exactly as
//! [`scratch`](crate::scratch) does — which is what keeps this off the plugin
//! ABI: no new proto message, no new `TargetDef` field, and a third-party driver
//! participates without recompiling, because it already passes annotations
//! through.
//!
//! # The flags differ from scratch, and the difference is the whole feature
//!
//! A scratch reference is `hashed: false` because a target's output must be
//! identical whether its cache is warm, cold or absent. A credential reference
//! is **`hashed: true, runtime: false`** — the same shape `hash_deps` uses:
//!
//! - **`hashed`**, because the descriptor's hashout is how the *identity* a
//!   target built under reaches its cache key. Drop it and two identities share
//!   artifacts, which is the bug this whole design exists to prevent.
//! - **not `runtime`**, because the descriptor is a recipe. Nothing about it is
//!   materialized into the sandbox; the host mints from it and renders the
//!   result. `runtime: false` also keeps `collect_transitive_deps` from folding
//!   the descriptor target's own tools and env into every consumer.
//!
//! # The name travels; the settings do not
//!
//! The annotation's value is the consumer's *name* for the credential — what the
//! command references as `$SECRET_<NAME>` and what appears in
//! `«redacted:NAME»`. Everything else lives on the referenced `secret` target's
//! spec, which the host reads directly, so there is exactly one copy of the
//! declaration and two consumers cannot disagree about what it means.

/// Input annotation marking a dep edge as a credential reference. The value is
/// the consumer's name for it.
///
/// Set by a driver whose target declared the reference (pluginexec's `secrets`
/// attribute); read by the engine, which resolves the declaration, mints a value
/// and renders it. An input without it is an ordinary dependency.
pub const SECRET_ANNOTATION: &str = "secret";

/// `origin_id` prefix for credential inputs, matching the `dep|<group>|<i>`
/// shape the other input kinds use.
pub const SECRET_ORIGIN_PREFIX: &str = "secret";

/// The consumer's name for a credential, if this input is one.
pub fn secret_name(annotations: &std::collections::BTreeMap<String, String>) -> Option<&str> {
    annotations
        .get(SECRET_ANNOTATION)
        .map(String::as_str)
        .filter(|n| !n.is_empty())
}

/// True when `annotations` marks an input as a credential reference.
pub fn is_secret(annotations: &std::collections::BTreeMap<String, String>) -> bool {
    secret_name(annotations).is_some()
}

/// The environment variable carrying a credential's path, for the `file` shape.
///
/// Mirrors pluginexec's `OUT_<GROUP>` / `SRC_<GROUP>` and pluginscratch's
/// `SCRATCH_<NAME>`: uppercase, every character outside `[A-Z0-9_]` replaced
/// with `_`. The `SECRET_` prefix means the result is a valid POSIX name even
/// when the credential's name starts with a digit.
pub fn default_env_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().saturating_add(7));
    out.push_str("SECRET_");
    for c in name.chars() {
        let u = c.to_ascii_uppercase();
        out.push(if u.is_ascii_alphanumeric() || u == '_' {
            u
        } else {
            '_'
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ann(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn the_annotation_carries_the_consumers_name() {
        assert_eq!(secret_name(&ann(&[("secret", "github")])), Some("github"));
        assert!(is_secret(&ann(&[("secret", "github")])));
    }

    /// An empty name would produce `$SECRET_` and an unnameable
    /// `«redacted:»`, so it does not count as a reference at all.
    #[test]
    fn an_empty_name_is_not_a_reference() {
        assert!(!is_secret(&ann(&[("secret", "")])));
        assert!(!is_secret(&ann(&[])));
        assert!(!is_secret(&ann(&[("scratch", "true")])));
    }

    #[test]
    fn env_names_are_posix_safe_whatever_the_credential_is_called() {
        assert_eq!(default_env_name("github"), "SECRET_GITHUB");
        assert_eq!(default_env_name("gh-app"), "SECRET_GH_APP");
        assert_eq!(default_env_name("r2.cache"), "SECRET_R2_CACHE");
        // A name starting with a digit is legal for a target and not for a
        // variable; the prefix is what makes it safe.
        assert_eq!(default_env_name("1pass"), "SECRET_1PASS");
    }
}
