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

/// The dep edge for the `i`th scratch reference of a target.
///
/// `hashed: false, runtime: false` is the contract: a scratch materializes no
/// artifacts (its declaration has none) and must not touch the target's cache
/// key, because a target's outputs are required to be identical whether its
/// scratch is warm, cold, or absent. The edge exists so the graph knows about it
/// — `heph query revdeps` answers "who shares this cache?", and a bad addr is an
/// ordinary `TargetNotFoundError`.
///
/// One function for every driver that references a scratch, so the origin id,
/// the annotation and the flags cannot drift between them.
pub fn input(r#ref: hplugin::driver::TargetAddr, i: usize) -> hplugin::driver::targetdef::Input {
    use hplugin::driver::targetdef::{Input, InputMode};
    Input {
        r#ref,
        mode: InputMode::Standard,
        origin_id: format!("{SCRATCH_ORIGIN_PREFIX}|{i}"),
        annotations: std::collections::BTreeMap::from([(
            SCRATCH_ANNOTATION.to_string(),
            "true".to_string(),
        )]),
        hashed: false,
        runtime: false,
    }
}

/// The dep edges for a target's `scratch` attribute, resolved against
/// `package`, each built by [`input`].
///
/// Order is the declared order. A repeated reference is refused: it would mount
/// one directory twice and set one env var twice, which is a BUILD-file mistake
/// worth naming rather than quietly collapsing.
pub fn inputs(
    refs: &[String],
    package: &hmodel::htpkg::PkgBuf,
) -> anyhow::Result<Vec<hplugin::driver::targetdef::Input>> {
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut out = Vec::with_capacity(refs.len());
    for (i, raw) in refs.iter().enumerate() {
        let r#ref = hplugin::driver::TargetAddr::parse(raw, package)?;
        let key = r#ref.to_string();
        if let Some(first) = seen.insert(key.clone(), i) {
            anyhow::bail!(
                "scratch {key} is referenced twice (positions {first} and {i}) — a scratch \
                 mounts at one path and sets one environment variable, so referencing it \
                 again does nothing; drop the duplicate"
            );
        }
        out.push(input(r#ref, i));
    }
    Ok(out)
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

    #[test]
    fn inputs_are_unhashed_annotated_edges_in_declared_order() {
        let pkg = hmodel::htpkg::PkgBuf::from("app");
        let got = inputs(&["//c:b".to_string(), ":a".to_string()], &pkg).expect("inputs");
        let summary: Vec<_> = got
            .iter()
            .map(|i| {
                (
                    i.r#ref.to_string(),
                    i.origin_id.as_str(),
                    is_scratch(&i.annotations),
                    i.annotations.len(),
                    i.hashed,
                    i.runtime,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("//c:b".to_string(), "scratch|0", true, 1, false, false),
                ("//app:a".to_string(), "scratch|1", true, 1, false, false),
            ]
        );
    }

    #[test]
    fn a_repeated_reference_is_refused_with_both_positions() {
        let pkg = hmodel::htpkg::PkgBuf::from("app");
        // `:a` and `//app:a` are the same target: the check is on the resolved addr.
        let Err(err) = inputs(
            &[":a".to_string(), "//x:y".to_string(), "//app:a".to_string()],
            &pkg,
        ) else {
            panic!("a duplicate must be refused");
        };
        assert_eq!(
            err.to_string(),
            "scratch //app:a is referenced twice (positions 0 and 2) — a scratch mounts at one \
             path and sets one environment variable, so referencing it again does nothing; drop \
             the duplicate"
        );
    }

    #[test]
    fn no_references_is_no_edges() {
        let pkg = hmodel::htpkg::PkgBuf::from("app");
        assert!(inputs(&[], &pkg).expect("inputs").is_empty());
    }
}
