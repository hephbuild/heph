//! Go build *variants*: named, static bundles of toolchain factors declared via
//! `provider_state(provider="go", variants={...})`.
//!
//! A user-facing target selects a variant by name with a single `@v=NAME` addr
//! arg; the provider resolves the closest ancestor package that defines that
//! name ([`resolve_user`]) and threads the resulting `{v, vp}` coordinate (a
//! [`VariantRef`]) — plus, verbatim, down the whole dependency graph — onto every
//! internal / dependency address. An internal address therefore always carries
//! both `v` and `vp`, and re-resolves to the *same* [`Factors`] no matter which
//! subtree it lives in ([`resolve_internal`]).
//!
//! Variants do **not** compound across the tree: each definition is static and
//! self-contained. The closest-ancestor lookup only chooses *which* definition
//! applies — it never merges a shallower one into a deeper one.

use crate::plugingo::factors::{Factors, VariantRef};
use anyhow::{Context, bail};
use hcore::htvalue::{Value, parse_strings};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{ProviderExecutor, State};

/// The go provider name, used to filter states fetched via the executor (which
/// returns states for all providers).
const GO_PROVIDER: &str = "go";

/// Keys accepted inside a single variant definition struct. `goos`/`goarch` are
/// required; the rest are optional.
const VARIANT_KEYS: &[&str] = &[
    "goos",
    "goarch",
    "tags",
    "goexperiment",
    "gcflags",
    "ldflags",
    "cgo_enabled",
];

/// Parse one variant definition (`Value::Map`) into concrete [`Factors`].
/// Rejects unknown keys and missing required fields (fail-fast).
fn parse_variant_def(name: &str, v: &Value) -> anyhow::Result<Factors> {
    let Value::Map(m) = v else {
        bail!("go variant `{name}` must be a struct, got: {v:?}");
    };
    for key in m.keys() {
        if !VARIANT_KEYS.contains(&key.as_str()) {
            bail!(
                "unknown key `{key}` in go variant `{name}` (allowed: {})",
                VARIANT_KEYS.join(", ")
            );
        }
    }
    let req_str = |k: &str| -> anyhow::Result<String> {
        match m.get(k) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => bail!("go variant `{name}`: `{k}` must be a string, got: {other:?}"),
            None => bail!("go variant `{name}`: `{k}` is required"),
        }
    };
    let goos = req_str("goos")?;
    let goarch = req_str("goarch")?;
    let list = |k: &str| -> anyhow::Result<Vec<String>> {
        match m.get(k) {
            Some(val) => {
                parse_strings(val).with_context(|| format!("go variant `{name}`: parsing `{k}`"))
            }
            None => Ok(Vec::new()),
        }
    };
    let mut build_tags = list("tags")?;
    build_tags.sort();
    let mut goexperiment = list("goexperiment")?;
    goexperiment.sort();
    let gcflags = list("gcflags")?;
    let ldflags = list("ldflags")?;
    let cgo_enabled = match m.get("cgo_enabled") {
        Some(Value::Bool(b)) => *b,
        Some(other) => bail!("go variant `{name}`: `cgo_enabled` must be a bool, got: {other:?}"),
        None => false,
    };
    Ok(Factors {
        goos,
        goarch,
        build_tags,
        goexperiment,
        gcflags,
        ldflags,
        cgo_enabled,
    })
}

/// Look up variant `name` in a single state's `variants` map.
fn variant_in_state(state: &State, name: &str) -> anyhow::Result<Option<Factors>> {
    let Some(variants_val) = state.state.get("variants") else {
        return Ok(None);
    };
    let Value::Map(variants) = variants_val else {
        bail!("go provider_state `variants` must be a map, got: {variants_val:?}");
    };
    match variants.get(name) {
        Some(def) => Ok(Some(parse_variant_def(name, def)?)),
        None => Ok(None),
    }
}

/// All variant names defined across `states`, sorted and deduped. Used to
/// enumerate targets in `list` and to build helpful "available: [...]" errors.
pub fn defined_variant_names(states: &[State]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for s in states {
        if let Some(Value::Map(variants)) = s.state.get("variants") {
            for k in variants.keys() {
                names.insert(k.clone());
            }
        }
    }
    names.into_iter().collect()
}

/// The [`VariantRef`]s applicable to a package (for `list` enumeration): one per
/// distinct variant name defined in `states`, each pinned to its closest-ancestor
/// defining package. `states` are the package's own ancestry.
pub fn applicable(states: &[State]) -> Vec<VariantRef> {
    defined_variant_names(states)
        .into_iter()
        .filter_map(|name| resolve_user(&name, states).ok().map(|(_, vref)| vref))
        .collect()
}

/// Resolve a user-facing target's variant: pick the closest (deepest) ancestor
/// package among `states` that defines variant `name`. `states` are the target
/// package's own ancestry (already go-filtered by the engine), so the closest
/// definition is the one with the longest package path.
pub fn resolve_user(name: &str, states: &[State]) -> anyhow::Result<(Factors, VariantRef)> {
    let mut best: Option<(&State, Factors)> = None;
    for s in states {
        if let Some(f) = variant_in_state(s, name)? {
            let deeper = match &best {
                Some((bs, _)) => s.package.as_str().len() > bs.package.as_str().len(),
                None => true,
            };
            if deeper {
                best = Some((s, f));
            }
        }
    }
    match best {
        Some((s, f)) => Ok((f, VariantRef::new(name, s.package.as_str()))),
        None => bail!(
            "no go variant `{name}` is defined for this package or any ancestor \
             (available: [{}]); declare it with \
             provider_state(provider=\"go\", variants={{\"{name}\": {{...}}}})",
            defined_variant_names(states).join(", ")
        ),
    }
}

/// Resolve an internal / dependency target's variant. The addr pins the defining
/// package via `vp`, so the lookup is exact-package. Prefer the states the engine
/// already provided (a hit whenever `vp` is an ancestor of the target — the
/// common case, e.g. a root-defined variant); only when `vp` is in a disjoint
/// subtree do we fetch its states through the executor.
pub async fn resolve_internal(
    vref: &VariantRef,
    states: &[State],
    executor: &dyn ProviderExecutor,
) -> anyhow::Result<Factors> {
    for s in states {
        if s.package.as_str() == vref.pkg
            && let Some(f) = variant_in_state(s, &vref.name)?
        {
            return Ok(f);
        }
    }
    // `vp` is not in the target's own ancestry — fetch its states directly. This
    // registers no dep edge (config lookup, not a build dependency).
    let vp_states = executor
        .states(&PkgBuf::from(vref.pkg.as_str()))
        .await
        .with_context(|| format!("fetching states for go variant package `{}`", vref.pkg))?;
    for s in &vp_states {
        if s.provider == GO_PROVIDER
            && s.package.as_str() == vref.pkg
            && let Some(f) = variant_in_state(s, &vref.name)?
        {
            return Ok(f);
        }
    }
    bail!(
        "go variant `{}` is not defined at package `{}` (pinned via the `vp` addr arg)",
        vref.name,
        vref.pkg
    )
}

/// Resolve the variant for any variant-parameterized target address, returning
/// the concrete [`Factors`] plus the [`VariantRef`] to thread onto its sub-target
/// and dependency addresses.
///
/// - internal / dep address (`v` + `vp`): exact-package lookup via
///   [`resolve_internal`].
/// - user address (`v` only): closest-ancestor lookup via [`resolve_user`].
/// - no `v`: hard error — there is no implicit default variant.
pub async fn resolve(
    addr: &Addr,
    states: &[State],
    executor: &dyn ProviderExecutor,
) -> anyhow::Result<(Factors, VariantRef)> {
    let Some(name) = addr.args.get("v") else {
        bail!(
            "go target `:{}` requires a variant: specify `@v=NAME` \
             (available: [{}])",
            addr.name,
            defined_variant_names(states).join(", ")
        );
    };
    match addr.args.get("vp") {
        Some(vp) => {
            let vref = VariantRef::new(name.clone(), vp.clone());
            let f = resolve_internal(&vref, states, executor).await?;
            Ok((f, vref))
        }
        None => resolve_user(name, states),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::Value;
    use hmodel::htpkg::PkgBuf;
    use std::collections::HashMap;

    fn variant_val(fields: &[(&str, Value)]) -> Value {
        Value::Map(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn go_state(pkg: &str, variants: &[(&str, Value)]) -> State {
        let variants_map: HashMap<String, Value> = variants
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        State {
            package: PkgBuf::from(pkg),
            provider: GO_PROVIDER.to_string(),
            state: HashMap::from([("variants".to_string(), Value::Map(variants_map))]),
        }
    }

    fn linux_amd64() -> Value {
        variant_val(&[
            ("goos", Value::String("linux".into())),
            ("goarch", Value::String("amd64".into())),
        ])
    }

    #[test]
    fn parse_requires_goos_goarch() {
        let err = parse_variant_def(
            "v",
            &variant_val(&[("goos", Value::String("linux".into()))]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("`goarch` is required"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_key() {
        let def = variant_val(&[
            ("goos", Value::String("linux".into())),
            ("goarch", Value::String("amd64".into())),
            ("bogus", Value::Bool(true)),
        ]);
        let err = parse_variant_def("v", &def).unwrap_err();
        assert!(err.to_string().contains("unknown key `bogus`"), "{err}");
    }

    #[test]
    fn parse_full_variant() {
        let def = variant_val(&[
            ("goos", Value::String("linux".into())),
            ("goarch", Value::String("arm64".into())),
            (
                "tags",
                Value::List(vec![Value::String("b".into()), Value::String("a".into())]),
            ),
            (
                "goexperiment",
                Value::List(vec![Value::String("arenas".into())]),
            ),
            (
                "gcflags",
                Value::List(vec![Value::String("-l".into()), Value::String("-N".into())]),
            ),
            (
                "ldflags",
                Value::List(vec![Value::String("-s".into()), Value::String("-w".into())]),
            ),
            ("cgo_enabled", Value::Bool(true)),
        ]);
        let f = parse_variant_def("release", &def).unwrap();
        assert_eq!(f.goos, "linux");
        assert_eq!(f.goarch, "arm64");
        assert_eq!(f.build_tags, vec!["a", "b"]); // sorted
        assert_eq!(f.goexperiment, vec!["arenas"]);
        assert_eq!(f.gcflags, vec!["-l", "-N"]); // order preserved
        assert_eq!(f.ldflags, vec!["-s", "-w"]);
        assert!(f.cgo_enabled);
    }

    #[test]
    fn resolve_user_picks_closest_ancestor() {
        let states = vec![
            go_state("", &[("release", linux_amd64())]),
            go_state(
                "app",
                &[(
                    "release",
                    variant_val(&[
                        ("goos", Value::String("darwin".into())),
                        ("goarch", Value::String("arm64".into())),
                    ]),
                )],
            ),
        ];
        let (f, vref) = resolve_user("release", &states).unwrap();
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("darwin", "arm64"));
        assert_eq!(vref.pkg, "app"); // deepest definition wins
        assert_eq!(vref.name, "release");
    }

    #[test]
    fn resolve_user_unknown_variant_errors() {
        let states = vec![go_state("", &[("release", linux_amd64())])];
        let err = resolve_user("prod", &states).unwrap_err();
        assert!(err.to_string().contains("no go variant `prod`"), "{err}");
        assert!(
            err.to_string().contains("release"),
            "lists available: {err}"
        );
    }
}
