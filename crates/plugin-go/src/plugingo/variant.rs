//! Go build *variants*: named bundles of toolchain factors declared via
//! `provider_state(provider="go", variants={...})`.
//!
//! # Scoping — module, not repo
//!
//! Variants belong to a Go **module** (the `go.mod` directory). The *universe* of
//! a module is every variant declared anywhere in that module's package subtree.
//! Two resolution modes:
//!
//! - **Binary / entry targets** (`@v=NAME`, no `vp`): resolve by **ancestry**,
//!   walking the target package up to (and including) its module root — the
//!   closest declaration wins. Repo-level declarations *above* the module root do
//!   not apply. See [`resolve_ancestry`].
//! - **Library / dependency targets** (`@v=NAME,vp=PKG`): resolve `(name, vp)`
//!   against the module **universe**. `vp` pins the declaring package, so the same
//!   name declared at different levels stays unambiguous. See [`resolve_in_universe`].
//!
//! A binary threads the `{name, vp}` it resolved (`vp` = the winning declaring
//! package) onto every dependency address, so libs re-resolve to the *same*
//! factors no matter which subtree they live in — the universe is module-wide, so
//! a variant declared at a sibling package is still found.
//!
//! # Inheritance
//!
//! A variant may carry `inherit = "OTHER"` to start from another variant in the
//! **same `variants` map** and override selected fields (list fields replace, not
//! append). `goos`/`goarch` may be omitted when inherited. Cycles are rejected.

use crate::plugingo::factors::{BuildMode, Factors, VariantRef, race_from_args};
use anyhow::{Context, bail};
use hcore::htvalue::{Value, parse_strings};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{ProviderExecutor, State};
use std::collections::BTreeMap;

/// The go provider name, used to filter states fetched via the executor (which
/// returns states for all providers).
const GO_PROVIDER: &str = "go";

/// Keys accepted inside a single variant definition struct.
const VARIANT_KEYS: &[&str] = &[
    "goos",
    "goarch",
    "tags",
    "goexperiment",
    "gcflags",
    "ldflags",
    "buildmode",
    "inherit",
];

/// A variant definition before inheritance is applied: every factor is optional
/// (an omitted field inherits from `inherit`, if any), plus the optional parent
/// reference.
#[derive(Debug, Clone, Default)]
struct RawVariant {
    goos: Option<String>,
    goarch: Option<String>,
    tags: Option<Vec<String>>,
    goexperiment: Option<Vec<String>>,
    gcflags: Option<Vec<String>>,
    ldflags: Option<Vec<String>>,
    buildmode: Option<BuildMode>,
    inherit: Option<String>,
}

/// Whether `pkg` lies within the module rooted at `module_root` (i.e. is the root
/// itself or a descendant). `module_root == ""` (a root `go.mod`) contains
/// everything.
fn under_module_root(pkg: &str, module_root: &str) -> bool {
    module_root.is_empty() || pkg == module_root || pkg.starts_with(&format!("{module_root}/"))
}

/// Parse one raw variant definition (`Value::Map`). Rejects unknown keys; wrong
/// value types fail-fast.
fn parse_raw(name: &str, v: &Value) -> anyhow::Result<RawVariant> {
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
    let opt_str = |k: &str| -> anyhow::Result<Option<String>> {
        match m.get(k) {
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => bail!("go variant `{name}`: `{k}` must be a string, got: {other:?}"),
            None => Ok(None),
        }
    };
    let opt_list = |k: &str| -> anyhow::Result<Option<Vec<String>>> {
        match m.get(k) {
            Some(val) => {
                Ok(Some(parse_strings(val).with_context(|| {
                    format!("go variant `{name}`: parsing `{k}`")
                })?))
            }
            None => Ok(None),
        }
    };
    let buildmode = match opt_str("buildmode")? {
        Some(s) => Some(BuildMode::parse(&s).ok_or_else(|| {
            anyhow::anyhow!(
                "go variant `{name}`: unknown `buildmode` `{s}` (allowed: {})",
                BuildMode::NAMES.join(", ")
            )
        })?),
        None => None,
    };
    Ok(RawVariant {
        goos: opt_str("goos")?,
        goarch: opt_str("goarch")?,
        tags: opt_list("tags")?,
        goexperiment: opt_list("goexperiment")?,
        gcflags: opt_list("gcflags")?,
        ldflags: opt_list("ldflags")?,
        buildmode,
        inherit: opt_str("inherit")?,
    })
}

/// Parse a state's `variants` provider_state map into raw (pre-inheritance) defs.
/// Returns an empty map if the state declares no variants.
fn raw_variants(state: &State) -> anyhow::Result<BTreeMap<String, RawVariant>> {
    let Some(variants_val) = state.state.get("variants") else {
        return Ok(BTreeMap::new());
    };
    let Value::Map(variants) = variants_val else {
        bail!("go provider_state `variants` must be a map, got: {variants_val:?}");
    };
    variants
        .iter()
        .map(|(k, v)| Ok((k.clone(), parse_raw(k, v)?)))
        .collect()
}

/// Resolve `name` within a single raw map, applying inheritance. `inherit`
/// references another variant in the *same* map. `visiting` carries the active
/// chain for cycle detection.
fn resolve_in_map(
    name: &str,
    raw: &BTreeMap<String, RawVariant>,
    visiting: &mut Vec<String>,
) -> anyhow::Result<Factors> {
    if visiting.iter().any(|n| n == name) {
        visiting.push(name.to_string());
        bail!("go variant inheritance cycle: {}", visiting.join(" -> "));
    }
    let def = raw
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("go variant `{name}` is not defined"))?;

    let mut f = match &def.inherit {
        Some(parent) => {
            visiting.push(name.to_string());
            let base = resolve_in_map(parent, raw, visiting).with_context(|| {
                format!("resolving go variant `{name}`'s `inherit = \"{parent}\"`")
            })?;
            visiting.pop();
            base
        }
        None => Factors::default(),
    };

    // Declared fields overlay the (possibly inherited) base — list fields replace.
    if let Some(v) = &def.goos {
        f.goos = v.clone();
    }
    if let Some(v) = &def.goarch {
        f.goarch = v.clone();
    }
    if let Some(v) = &def.tags {
        f.build_tags = v.clone();
        f.build_tags.sort();
    }
    if let Some(v) = &def.goexperiment {
        f.goexperiment = v.clone();
        f.goexperiment.sort();
    }
    if let Some(v) = &def.gcflags {
        f.gcflags = v.clone();
    }
    if let Some(v) = &def.ldflags {
        f.ldflags = v.clone();
    }
    if let Some(v) = def.buildmode {
        f.buildmode = v;
    }

    if f.goos.is_empty() || f.goarch.is_empty() {
        bail!(
            "go variant `{name}`: `goos` and `goarch` are required \
             (declare them, or inherit from a variant that does)"
        );
    }
    Ok(f)
}

/// Look up `name` in a single state's variants (with inheritance), or `None` if
/// the state doesn't declare it.
fn variant_in_state(state: &State, name: &str) -> anyhow::Result<Option<Factors>> {
    let raw = raw_variants(state)?;
    if !raw.contains_key(name) {
        return Ok(None);
    }
    Ok(Some(resolve_in_map(name, &raw, &mut Vec::new())?))
}

/// Every variant name declared across `states` (deduped, sorted) — for `list`
/// enumeration and "available: [...]" errors.
pub fn defined_variant_names(states: &[State]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for s in states {
        if let Ok(raw) = raw_variants(s) {
            names.extend(raw.into_keys());
        }
    }
    names.into_iter().collect()
}

/// The `(VariantRef, Factors)` pairs applicable to a **binary/entry** target: one
/// per variant name declared in the module-bounded ancestry, pinned to its
/// closest declaring package, with its resolved factors. `states` is the target
/// package's ancestry. Callers map away the `Factors` when they only need the
/// refs; those that must inspect factors (e.g. filter `test` targets to the host
/// `goos`/`goarch`) keep them.
pub fn ancestry_variants_with_factors(
    states: &[State],
    module_root: &str,
) -> Vec<(VariantRef, Factors)> {
    let mut names = std::collections::BTreeSet::new();
    for s in states {
        if under_module_root(s.package.as_str(), module_root)
            && let Ok(raw) = raw_variants(s)
        {
            names.extend(raw.into_keys());
        }
    }
    names
        .into_iter()
        .filter_map(|name| {
            resolve_ancestry(&name, states, module_root)
                .ok()
                .map(|(f, v)| (v, f))
        })
        .collect()
}

/// The full module **universe** as `(VariantRef, Factors)` pairs — every variant
/// declared anywhere in the module, keyed by `(name, declaring-pkg)`. `states`
/// are all provider states in the module subtree ([`ProviderExecutor::states_under`]).
/// A variant that fails to resolve (bad inheritance, missing goos) is a hard
/// error — a broken declaration must surface, not be silently dropped.
pub fn build_universe(states: &[State]) -> anyhow::Result<Vec<(VariantRef, Factors)>> {
    let mut out = Vec::new();
    for s in states {
        if s.provider != GO_PROVIDER {
            continue;
        }
        let raw = raw_variants(s)?;
        for name in raw.keys() {
            let f = resolve_in_map(name, &raw, &mut Vec::new())
                .with_context(|| format!("in package `{}`", s.package.as_str()))?;
            out.push((VariantRef::new(name.clone(), s.package.as_str()), f));
        }
    }
    Ok(out)
}

/// The `VariantRef`s of a module **universe** — every `(name, declaring-pkg)`
/// declared across `states` (a module subtree, from `states_under`). For `list`
/// of library targets, so a variant declared at a sibling package is enumerated.
pub fn universe_variants(states: &[State]) -> anyhow::Result<Vec<VariantRef>> {
    Ok(build_universe(states)?
        .into_iter()
        .map(|(vref, _)| vref)
        .collect())
}

/// Resolve a **binary/entry** target's variant: the closest (deepest) ancestor
/// package that declares `name`, bounded at `module_root` (declarations above the
/// module root are ignored). `states` is the target package's ancestry.
pub fn resolve_ancestry(
    name: &str,
    states: &[State],
    module_root: &str,
) -> anyhow::Result<(Factors, VariantRef)> {
    let mut best: Option<(&State, Factors)> = None;
    for s in states {
        if !under_module_root(s.package.as_str(), module_root) {
            continue;
        }
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
            "no go variant `{name}` is declared for this package or any ancestor \
             within its module (available: [{}]); declare it with \
             provider_state(provider=\"go\", variants={{\"{name}\": {{...}}}})",
            defined_variant_names(states).join(", ")
        ),
    }
}

/// Resolve a **library** target's variant against a module universe: the exact
/// `(name, vp)` entry. `universe` comes from [`build_universe`].
pub fn resolve_in_universe(
    vref: &VariantRef,
    universe: &[(VariantRef, Factors)],
) -> anyhow::Result<Factors> {
    universe
        .iter()
        .find(|(v, _)| v.name == vref.name && v.pkg == vref.pkg)
        .map(|(_, f)| f.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "go variant `{}` is not declared at package `{}` (pinned via the `vp` addr arg)",
                vref.name,
                vref.pkg
            )
        })
}

/// Resolve the variant for any variant-parameterized target address.
///
/// - **library / dependency** address (`v` + `vp`): exact `(name, vp)` lookup in
///   the module universe (fetched via `executor.states_under(module_root)`).
/// - **binary / entry** address (`v` only): closest-ancestor lookup, module-bounded.
/// - no `v`: hard error — there is no implicit default variant.
///
/// `module_root` is the target's `go.mod` package (workspace-relative).
/// `vp_same_module` says whether the addr's `vp` (if any) names a package in the
/// target's *own* go module — a real nearest-`go.mod` check the caller performs
/// (a plain path-prefix test is insufficient: a `vp` can prefix-match while
/// living inside a *nested* submodule). Only an in-module `vp` is honored; a
/// foreign one (a cross-module dependency's pin) is ignored so its module's
/// variant declaration can't leak in.
pub async fn resolve(
    addr: &Addr,
    req_states: &[State],
    module_root: &str,
    executor: &dyn ProviderExecutor,
    vp_same_module: bool,
) -> anyhow::Result<(Factors, VariantRef)> {
    let Some(name) = addr.args.get("v") else {
        bail!(
            "go target `:{}` requires a variant: specify `@v=NAME` (available: [{}])",
            addr.name,
            defined_variant_names(req_states).join(", ")
        );
    };
    // Race is orthogonal to the variant: it comes off the addr, not the
    // `variants` declaration, and overlays whichever factor set resolves below.
    let race = race_from_args(&addr.args)?;
    let apply_race = |(mut f, v): (Factors, VariantRef)| -> (Factors, VariantRef) {
        f.race = race;
        // Resolve the effective buildmode once, here, rather than at each of the
        // places that consume it: a linux race build cannot be PIE (see
        // `BuildMode::for_race`). Doing it at resolution keeps the compile's
        // `-shared` and the link's `-buildmode=` deriving from one value, so they
        // cannot drift apart — and it means the override is visible in the def
        // hash, so a race archive is never confused with a `pie` one.
        f.buildmode = f.buildmode.for_race(&f.goos, race);
        (f, v.with_race(race))
    };

    match addr.args.get("vp") {
        // Only honor `vp` when it lies within the target's OWN go module. The
        // variant universe is bounded to the module: a `vp` threaded from a
        // consumer in a *different* module (a cross-module dependency) must not
        // drag that foreign module's variant declaration in. Re-resolve the name
        // by ancestry within this module instead — the dep provides its own
        // same-named variant, or it is genuinely undeclared here.
        Some(vp) if vp_same_module => {
            let vref = VariantRef::new(name.clone(), vp.clone());
            // Fast path: a `vp` already in the target's ancestry (`req_states` —
            // common for a module-root-declared variant) needs no fetch.
            if let Some(f) = req_states
                .iter()
                .filter(|s| s.package.as_str() == vp)
                .find_map(|s| variant_in_state(s, name).transpose())
                .transpose()?
            {
                return Ok(apply_race((f, vref)));
            }
            // Otherwise fetch `vp`'s own declaration directly. `states_under(vp)`
            // includes `vp`'s package (siblings of the target are reachable this
            // way, which ancestry-only `req_states` cannot supply).
            let vp_states = executor
                .states_under(&PkgBuf::from(vp.as_str()))
                .await
                .with_context(|| format!("fetching variant declaration at `{vp}`"))?;
            let universe = build_universe(&vp_states)?;
            let f = resolve_in_universe(&vref, &universe)?;
            Ok(apply_race((f, vref)))
        }
        // No `vp`, or a `vp` outside this module: resolve by module-bounded ancestry.
        _ => resolve_ancestry(name, req_states, module_root).map(apply_race),
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

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }

    fn list(xs: &[&str]) -> Value {
        Value::List(xs.iter().map(|x| Value::String((*x).into())).collect())
    }

    fn linux_amd64() -> Value {
        variant_val(&[("goos", s("linux")), ("goarch", s("amd64"))])
    }

    #[test]
    fn parse_requires_goos_goarch() {
        let raw = go_state("", &[("v", variant_val(&[("goos", s("linux"))]))]);
        let err = variant_in_state(&raw, "v").unwrap_err();
        assert!(err.to_string().contains("required"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_key() {
        let st = go_state(
            "",
            &[(
                "v",
                variant_val(&[
                    ("goos", s("linux")),
                    ("goarch", s("amd64")),
                    ("bogus", s("x")),
                ]),
            )],
        );
        let err = variant_in_state(&st, "v").unwrap_err();
        assert!(err.to_string().contains("unknown key `bogus`"), "{err}");
    }

    #[test]
    fn parse_full_variant() {
        let st = go_state(
            "",
            &[(
                "release",
                variant_val(&[
                    ("goos", s("linux")),
                    ("goarch", s("arm64")),
                    ("tags", list(&["b", "a"])),
                    ("goexperiment", list(&["arenas"])),
                    ("gcflags", list(&["-l", "-N"])),
                    ("ldflags", list(&["-s", "-w"])),
                ]),
            )],
        );
        let f = variant_in_state(&st, "release").unwrap().unwrap();
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("linux", "arm64"));
        assert_eq!(f.build_tags, vec!["a", "b"]); // sorted
        assert_eq!(f.goexperiment, vec!["arenas"]);
        assert_eq!(f.gcflags, vec!["-l", "-N"]); // order preserved
        assert_eq!(f.ldflags, vec!["-s", "-w"]);
    }

    #[test]
    fn buildmode_defaults_to_exe() {
        let st = go_state("", &[("v", linux_amd64())]);
        let f = variant_in_state(&st, "v").unwrap().unwrap();
        assert_eq!(
            f.buildmode,
            BuildMode::Exe,
            "an undeclared buildmode must match plain `go build`, not PIE"
        );
    }

    #[test]
    fn buildmode_pie_is_parsed() {
        let st = go_state(
            "",
            &[(
                "v",
                variant_val(&[
                    ("goos", s("linux")),
                    ("goarch", s("amd64")),
                    ("buildmode", s("pie")),
                ]),
            )],
        );
        let f = variant_in_state(&st, "v").unwrap().unwrap();
        assert_eq!(f.buildmode, BuildMode::Pie);
    }

    // An unrecognized buildmode silently falling back to the default would change
    // the linkage of the shipped binary without saying so.
    #[test]
    fn buildmode_unknown_value_errors() {
        let st = go_state(
            "",
            &[(
                "v",
                variant_val(&[
                    ("goos", s("linux")),
                    ("goarch", s("amd64")),
                    ("buildmode", s("c-shared")),
                ]),
            )],
        );
        let err = variant_in_state(&st, "v").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown `buildmode` `c-shared`"), "{msg}");
        assert!(
            msg.contains("exe, pie"),
            "must list the allowed modes: {msg}"
        );
    }

    #[test]
    fn buildmode_is_inherited_and_overridable() {
        let st = go_state(
            "",
            &[
                (
                    "base",
                    variant_val(&[
                        ("goos", s("linux")),
                        ("goarch", s("amd64")),
                        ("buildmode", s("pie")),
                    ]),
                ),
                ("child", variant_val(&[("inherit", s("base"))])),
                (
                    "child_exe",
                    variant_val(&[("inherit", s("base")), ("buildmode", s("exe"))]),
                ),
            ],
        );
        assert_eq!(
            variant_in_state(&st, "child").unwrap().unwrap().buildmode,
            BuildMode::Pie
        );
        assert_eq!(
            variant_in_state(&st, "child_exe")
                .unwrap()
                .unwrap()
                .buildmode,
            BuildMode::Exe
        );
    }

    #[test]
    fn inherit_overlays_declared_fields() {
        let st = go_state(
            "",
            &[
                (
                    "base",
                    variant_val(&[
                        ("goos", s("linux")),
                        ("goarch", s("amd64")),
                        ("tags", list(&["base"])),
                        ("ldflags", list(&["-s"])),
                    ]),
                ),
                (
                    "release",
                    // inherits base's goos/goarch/ldflags, overrides tags (replace).
                    variant_val(&[("inherit", s("base")), ("tags", list(&["prod"]))]),
                ),
            ],
        );
        let f = variant_in_state(&st, "release").unwrap().unwrap();
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("linux", "amd64"));
        assert_eq!(f.ldflags, vec!["-s"]); // inherited
        assert_eq!(f.build_tags, vec!["prod"]); // replaced, not appended
    }

    #[test]
    fn inherit_cycle_errors() {
        let st = go_state(
            "",
            &[
                ("a", variant_val(&[("inherit", s("b"))])),
                ("b", variant_val(&[("inherit", s("a"))])),
            ],
        );
        let err = variant_in_state(&st, "a").unwrap_err();
        assert!(format!("{err:#}").contains("inheritance cycle"), "{err:#}");
    }

    #[test]
    fn resolve_ancestry_picks_closest_within_module() {
        // Repo root declares release=linux; the module root `app` overrides it
        // darwin. A binary in `app` must get the module's, and the repo-root one
        // (above the module) must be ignored.
        let states = vec![
            go_state("", &[("release", linux_amd64())]),
            go_state(
                "app",
                &[(
                    "release",
                    variant_val(&[("goos", s("darwin")), ("goarch", s("arm64"))]),
                )],
            ),
        ];
        let (f, vref) = resolve_ancestry("release", &states, "app").unwrap();
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("darwin", "arm64"));
        assert_eq!(vref.pkg, "app");
    }

    #[test]
    fn resolve_ancestry_ignores_declarations_above_module_root() {
        // release is only at the repo root, which is ABOVE the module root `app`.
        let states = vec![go_state("", &[("release", linux_amd64())])];
        let err = resolve_ancestry("release", &states, "app").unwrap_err();
        assert!(err.to_string().contains("no go variant `release`"), "{err}");
    }

    #[test]
    fn universe_disambiguates_same_name_by_vp() {
        // `release` at two packages with different factors — the universe keeps
        // both, keyed by declaring package.
        let module_states = vec![
            go_state("app", &[("release", linux_amd64())]),
            go_state(
                "app/cmd",
                &[(
                    "release",
                    variant_val(&[("goos", s("darwin")), ("goarch", s("arm64"))]),
                )],
            ),
        ];
        let universe = build_universe(&module_states).unwrap();
        let at_app = resolve_in_universe(&VariantRef::new("release", "app"), &universe).unwrap();
        let at_cmd =
            resolve_in_universe(&VariantRef::new("release", "app/cmd"), &universe).unwrap();
        assert_eq!(at_app.goos, "linux");
        assert_eq!(at_cmd.goos, "darwin");
    }

    /// Mock executor exercising `states_under` only; `result`/`query` must never
    /// be reached during variant resolution.
    struct UniverseExec {
        under: HashMap<String, Vec<State>>,
    }

    impl ProviderExecutor for UniverseExec {
        fn result<'a>(
            &'a self,
            _addr: &'a Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<std::sync::Arc<hplugin::eresult::EResult>>>
        {
            unimplemented!("variant resolution must not resolve a result")
        }
        fn query<'a>(
            &'a self,
            _m: &'a hmodel::htmatcher::Matcher,
            _skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
            unimplemented!("variant resolution must not query")
        }
        fn states_under<'a>(
            &'a self,
            prefix: &'a PkgBuf,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<State>>> {
            let out = self.under.get(prefix.as_str()).cloned().unwrap_or_default();
            Box::pin(async move { Ok(out) })
        }
    }

    // The load-bearing case: `release` is declared ONLY at sibling `//app/cmd`.
    // A library `//app/lib`'s dep addr is pinned `vp=app/cmd`. Its own ancestry
    // (module root `app`) does NOT contain it, so resolution must fetch the module
    // universe via `states_under(app)` and find the sibling's declaration.
    #[tokio::test]
    async fn resolve_library_finds_sibling_variant_via_universe() {
        let lib_ancestry: Vec<State> = vec![]; // //app/lib sees no variant of its own
        let exec = UniverseExec {
            // `states_under(vp)` — vp is the sibling `app/cmd`.
            under: HashMap::from([(
                "app/cmd".to_string(),
                vec![go_state("app/cmd", &[("release", linux_amd64())])],
            )]),
        };
        let addr = Addr::new(
            PkgBuf::from("app/lib"),
            "build_lib".to_string(),
            VariantRef::new("release", "app/cmd").to_args(),
        );
        let (f, vref) = resolve(&addr, &lib_ancestry, "app", &exec, true)
            .await
            .expect("sibling variant resolves via the module universe");
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("linux", "amd64"));
        assert_eq!(vref.pkg, "app/cmd");
    }

    // A `vp` from a consumer in a DIFFERENT go module (a cross-module dependency)
    // must NOT be honored: the universe is bounded to the target's own module.
    // Here `dev/app/graphql`'s dep addr carries `vp=mgmt/go` (a foreign module),
    // so the caller passes `vp_same_module = false`; resolution ignores the `vp`
    // and resolves `release` by ancestry within `dev/app`, rebasing `vp` to the
    // target's own module. `states_under` is never consulted.
    #[tokio::test]
    async fn foreign_vp_outside_module_resolves_by_ancestry() {
        let ancestry = vec![go_state("dev/app", &[("release", linux_amd64())])];
        // Empty universe: if resolution wrongly consulted `states_under(mgmt/go)`
        // it would find nothing and fail, proving the ancestry path is taken.
        let exec = UniverseExec {
            under: HashMap::new(),
        };
        let addr = Addr::new(
            PkgBuf::from("dev/app/graphql"),
            "build_lib".to_string(),
            VariantRef::new("release", "mgmt/go").to_args(),
        );
        let (f, vref) = resolve(&addr, &ancestry, "dev/app", &exec, false)
            .await
            .expect("foreign vp falls back to module-bounded ancestry");
        assert_eq!((f.goos.as_str(), f.goarch.as_str()), ("linux", "amd64"));
        assert_eq!(
            vref.pkg, "dev/app",
            "vp rebased to the target's own module, not the foreign `mgmt/go`"
        );
    }

    // ---- race ----

    fn race_addr(pkg: &str, variant: &str) -> Addr {
        Addr::new(
            PkgBuf::from(pkg),
            "build_test".to_string(),
            VariantRef::new(variant, "").with_race(true).to_args(),
        )
    }

    fn no_universe() -> UniverseExec {
        UniverseExec {
            under: HashMap::new(),
        }
    }

    /// The integration point for the linux race rule: a variant that explicitly
    /// declares `buildmode = "pie"` still resolves to `exe` under race, because
    /// `go tool link` would otherwise emit a binary that dies at startup with
    /// "ThreadSanitizer failed to allocate".
    ///
    /// Resolving it here (rather than at each consumer) is what keeps the
    /// compile's `-shared` and the link's `-buildmode=` deriving from one value.
    #[tokio::test]
    async fn linux_race_resolves_pie_down_to_exe() {
        let pie_linux = variant_val(&[
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("buildmode", s("pie")),
        ]);
        let states = vec![go_state("app", &[("dev", pie_linux)])];
        let (f, vref) = resolve(
            &race_addr("app", "dev"),
            &states,
            "app",
            &no_universe(),
            false,
        )
        .await
        .expect("resolves");
        assert_eq!(f.buildmode, BuildMode::Exe, "linux race must not be PIE");
        assert!(!f.buildmode.needs_shared(), "and must drop -shared with it");
        assert!(f.race);
        assert!(vref.race, "the race flag threads onto dependency addrs");
    }

    /// darwin supports pie+race, so the same declaration is honoured there —
    /// the override is scoped to where Go actually forbids the combination.
    #[tokio::test]
    async fn darwin_race_keeps_a_declared_pie() {
        let pie_darwin = variant_val(&[
            ("goos", s("darwin")),
            ("goarch", s("arm64")),
            ("buildmode", s("pie")),
        ]);
        let states = vec![go_state("app", &[("dev", pie_darwin)])];
        let (f, _) = resolve(
            &race_addr("app", "dev"),
            &states,
            "app",
            &no_universe(),
            false,
        )
        .await
        .expect("resolves");
        assert_eq!(f.buildmode, BuildMode::Pie);
        assert!(f.buildmode.needs_shared());
    }

    /// An ordinary (non-race) build is untouched by any of this: the declared
    /// buildmode survives on linux too.
    #[tokio::test]
    async fn non_race_linux_keeps_a_declared_pie() {
        let pie_linux = variant_val(&[
            ("goos", s("linux")),
            ("goarch", s("amd64")),
            ("buildmode", s("pie")),
        ]);
        let states = vec![go_state("app", &[("dev", pie_linux)])];
        let addr = Addr::new(
            PkgBuf::from("app"),
            "build_test".to_string(),
            VariantRef::new("dev", "").to_args(),
        );
        let (f, vref) = resolve(&addr, &states, "app", &no_universe(), false)
            .await
            .expect("resolves");
        assert_eq!(f.buildmode, BuildMode::Pie);
        assert!(!f.race);
        assert!(!vref.race);
    }
}
