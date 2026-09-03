//! The producer→host channel for scratch references.
//!
//! A target referencing a scratch cache records it as an ordinary [`Input`] with
//! `hashed: false, runtime: false` — a graph edge that materializes nothing and
//! contributes nothing to the parent's key — marked with the annotation below.
//!
//! Annotations are the existing way a driver tells the *host* something about a
//! dependency edge; [`stage::READ_ONLY_ANNOTATION`](crate::stage::READ_ONLY_ANNOTATION)
//! and [`stage::STAGE_PER_FILE_ANNOTATION`](crate::stage::STAGE_PER_FILE_ANNOTATION)
//! are the precedent. Using one here is what keeps scratch off the plugin ABI
//! entirely: no new proto message, no new `TargetDef` field, no `ABI_SEMVER` bump,
//! and a third-party driver participates without being recompiled, because it
//! already passes annotations through.
//!
//! The *settings* never travel this way. They live on the referenced `scratch`
//! target's spec config, which the host reads directly — so there is exactly one
//! copy of them and two consumers cannot disagree.

/// Input annotation marking a dep edge as a scratch reference. Value must be the
/// string `"true"`.
///
/// Set by a driver whose target declared the reference (pluginexec's `scratch`
/// attribute); read by the engine, which resolves the referenced declaration and
/// mounts its directory. An input without it is an ordinary dependency.
pub const SCRATCH_ANNOTATION: &str = "scratch";

/// `origin_id` prefix for scratch inputs, matching the `dep|<group>|<i>` shape
/// the other input kinds use.
///
/// Distinct from the dep prefixes so a scratch can never collide with a dep group
/// literally named `scratch`, and so the id reads as what it is in `heph inspect`.
pub const SCRATCH_ORIGIN_PREFIX: &str = "scratch";

/// True when `annotations` marks an input as a scratch reference.
pub fn is_scratch(annotations: &std::collections::BTreeMap<String, String>) -> bool {
    annotations.get(SCRATCH_ANNOTATION).map(String::as_str) == Some("true")
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
        assert!(is_scratch(&ann(&[("scratch", "true")])));
        assert!(!is_scratch(&ann(&[])));
        assert!(!is_scratch(&ann(&[("scratch", "false")])));
        // Not truthy-parsed: an annotation is a string channel, and accepting
        // near-misses would make a typo silently mean "yes".
        assert!(!is_scratch(&ann(&[("scratch", "1")])));
        assert!(!is_scratch(&ann(&[("scratch", "True")])));
    }

    #[test]
    fn an_unrelated_annotation_does_not_mark_a_scratch() {
        assert!(!is_scratch(&ann(&[("read_only", "true")])));
    }
}
