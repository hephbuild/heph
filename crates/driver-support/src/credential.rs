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

/// How many credentials `target` declared: its inputs the engine treats as
/// credential references (both flags false *and* the annotation, the engine's
/// own predicate).
pub fn declared(target: &hplugin::driver::targetdef::TargetDef) -> usize {
    target
        .inputs
        .iter()
        .filter(|i| !i.hashed && !i.runtime && is_credential(&i.annotations))
        .count()
}

/// Whether a run of `target` authenticates with the `mounts` the host
/// presented: `false` when it declared no credentials and received none.
///
/// For a driver that consumes mounts itself rather than handing them to a
/// subprocess. A target that declared credentials and received fewer mounts is
/// refused rather than run anonymously: a mount lost between host and driver
/// would otherwise turn a declared identity into none, and the 401 that
/// follows would point at the server rather than at the lost mount.
pub fn check_presented(
    target: &hplugin::driver::targetdef::TargetDef,
    mounts: &[hplugin::driver::CredentialMount],
) -> anyhow::Result<bool> {
    let declared = declared(target);
    if declared == 0 && mounts.is_empty() {
        return Ok(false);
    }
    anyhow::ensure!(
        mounts.len() >= declared,
        "{} declares {declared} credential(s), but the host presented {}. The host is likely \
         older than the driver; upgrade heph",
        target.addr.format(),
        mounts.len()
    );
    Ok(true)
}

/// The dep edges for a target's `credentials` attribute, resolved against
/// `package`.
///
/// Both flags false is not an optimization, it is the contract: nothing about a
/// credential may enter `hashin` — not the material, not the chosen source, not
/// the declaration, not even the names of the variables it presents. Because the
/// exclusion is structural, adding a credential to a target cannot move its
/// cache key.
///
/// What the edge *does* buy is the graph: `heph query revdeps` answers "who
/// needs this identity?", `heph inspect deps` shows it, and a bad addr is an
/// ordinary `TargetNotFoundError` rather than a 403 much later.
///
/// One function for every driver that takes the attribute, so the origin ids,
/// the annotation and the duplicate refusal cannot drift between them.
pub fn inputs(
    refs: &[String],
    package: &hmodel::htpkg::PkgBuf,
) -> anyhow::Result<Vec<hplugin::driver::targetdef::Input>> {
    use anyhow::Context as _;
    use hplugin::driver::targetdef::{Input, InputMode};
    use std::collections::BTreeMap;

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(refs.len());
    for (i, raw) in refs.iter().enumerate() {
        let r#ref = hplugin::driver::TargetAddr::parse(raw, package)
            .with_context(|| format!("credentials[{i}] {raw:?}"))?;
        let key = r#ref.to_string();
        if let Some(first) = seen.insert(key.clone(), i) {
            anyhow::bail!(
                "credential {key} is referenced twice (positions {first} and {i}) — a credential \
                 presents one set of variables and files, so referencing it again does nothing; \
                 drop the duplicate"
            );
        }
        out.push(Input {
            r#ref,
            mode: InputMode::Standard,
            origin_id: format!("{CREDENTIAL_ORIGIN_PREFIX}|{i}"),
            annotations: BTreeMap::from([(CREDENTIAL_ANNOTATION.to_string(), "true".to_string())]),
            hashed: false,
            runtime: false,
        });
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

    fn def(n: usize) -> hplugin::driver::targetdef::TargetDef {
        let refs: Vec<String> = (0..n).map(|i| format!("//auth:c{i}")).collect();
        let addr = hmodel::htaddr::parse_addr("//dl:file").expect("addr");
        hplugin::driver::targetdef::TargetDef {
            inputs: inputs(&refs, &addr.package).expect("inputs"),
            addr,
            ..Default::default()
        }
    }

    fn mount() -> hplugin::driver::CredentialMount {
        hplugin::driver::CredentialMount {
            addr: hmodel::htaddr::parse_addr("//auth:c0").expect("addr"),
            env: BTreeMap::new(),
            path_prefix: vec![],
            redact: vec![],
        }
    }

    /// Nothing declared and nothing presented is anonymous; a declaration
    /// that arrived without its mount is refused, never run anonymously.
    #[test]
    fn check_presented_refuses_a_lost_mount() {
        assert_eq!(declared(&def(2)), 2);
        assert!(!check_presented(&def(0), &[]).expect("anonymous"));
        assert!(check_presented(&def(1), &[mount()]).expect("declared"));
        let err = format!(
            "{:#}",
            check_presented(&def(1), &[]).expect_err("lost mount")
        );
        assert!(err.contains("declares 1 credential(s)"), "{err}");
    }
}
