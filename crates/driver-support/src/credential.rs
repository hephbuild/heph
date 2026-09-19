//! The producer→host channel for credential references.
//!
//! A target referencing a credential records it as an ordinary [`Input`] with
//! `hashed: false, runtime: false` — a graph edge that materializes nothing
//! through the normal input path and contributes nothing to the parent's key —
//! marked with the annotation below.
//!
//! This is the same channel [`scratch`](crate::scratch) uses, and for the same
//! reason: annotations are how a driver already tells the *host* something about
//! a dependency edge, so a credential reference needs no new `TargetDef` field
//! and no change to the shape of an `Input`.
//!
//! What is *not* the same is what happens afterwards. A scratch reaches the
//! sandbox as a symlink the driver places; a credential reaches it as a
//! [`CredentialMount`](hplugin::driver::CredentialMount) the host resolved,
//! acquired and materialized — because the material is the host's and must never
//! be visible to the graph. The edge exists so `heph query revdeps` and
//! `heph inspect deps` can see it, and so the engine knows which credentials a
//! target is allowed to receive.
//!
//! The *settings* never travel this way. They live on the referenced `credential`
//! target's spec config, which the host reads directly — so there is exactly one
//! copy of them and two consumers cannot disagree about which identity to use.
//!
//! [`Input`]: hplugin::driver::targetdef::Input

/// Input annotation marking a dep edge as a credential reference. Value must be
/// the string `"true"`.
///
/// Set by a driver whose target declared the reference (pluginexec's
/// `credentials` attribute); read by the engine, which resolves the referenced
/// declaration, walks its source chain and presents the material. An input
/// without it is an ordinary dependency.
pub const CREDENTIAL_ANNOTATION: &str = "credential";

/// `origin_id` prefix for credential inputs, matching the `dep|<group>|<i>` shape
/// the other input kinds use.
///
/// Distinct from the dep prefixes so a credential can never collide with a dep
/// group literally named `credential`, and so the id reads as what it is in
/// `heph inspect`.
pub const CREDENTIAL_ORIGIN_PREFIX: &str = "credential";

/// True when `annotations` marks an input as a credential reference.
pub fn is_credential(annotations: &std::collections::BTreeMap<String, String>) -> bool {
    annotations.get(CREDENTIAL_ANNOTATION).map(String::as_str) == Some("true")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ann(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_the_exact_true_marker_counts() {
        assert!(is_credential(&ann(&[("credential", "true")])));
        assert!(!is_credential(&ann(&[])));
        assert!(!is_credential(&ann(&[("credential", "false")])));
        // Not truthy-parsed: an annotation is a string channel, and accepting
        // near-misses would make a typo silently mean "yes" — which here would
        // route an ordinary dependency through the credential path.
        assert!(!is_credential(&ann(&[("credential", "1")])));
        assert!(!is_credential(&ann(&[("credential", "True")])));
    }

    #[test]
    fn a_scratch_annotation_does_not_mark_a_credential() {
        assert!(!is_credential(&ann(&[("scratch", "true")])));
    }
}
