//! Listed facts: what a provider's `list` says about each target without
//! `get`, how far the engine trusts it, and `heph validate`'s check that it is
//! true.
//!
//! The walks trust a listed **No**: a candidate whose merged facts rule it out
//! is dropped and never resolved — on selection walks and in the dep set of a
//! query target alike. A listed **Yes** is only a candidate whose existence is
//! confirmed as for any other. Nothing on the build path checks the facts —
//! that is the point of trusting them — so this is where they are held to the
//! specs and defs they describe.

use crate::engine::Engine;
use crate::engine::discovery::{Discovery, Stage, is_cancellation, provider_of};
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::provider::{ListRequest, TargetSpec};
use crate::engine::query::skip_unresolvable;
use crate::engine::request_state::RequestState;
use futures::StreamExt;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::{ListedFacts, MatchResult, Matcher};
use hmodel::htpkg::PkgBuf;
use std::fmt;
use std::sync::Arc;

/// Whether a request's walks act on listed facts.
///
/// Set per request by the front end — the CLI maps `HEPH_NO_LISTED_FACTS=1` to
/// [`Ignore`](Self::Ignore) — and never read from the environment by the
/// engine. It is outside every cache key on purpose: the dep set a query target
/// gets is already in its def hash, so a key moves only when trust changed the
/// dep set, which only a lying fact can do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListedFactsTrust {
    /// A listed No drops a candidate without `get`.
    #[default]
    Trust,
    /// Every candidate a fact would have decided is resolved instead, as
    /// before listed facts existed.
    Ignore,
}

impl ListedFactsTrust {
    /// The trust a kill switch selects: `Ignore` when it is set.
    pub fn from_kill_switch(set: bool) -> Self {
        if set { Self::Ignore } else { Self::Trust }
    }

    pub fn trusts(self) -> bool {
        self == Self::Trust
    }
}

/// Whether any path in the def's outputs is codegen — what a listed
/// `has_codegen` claims. `support_files` don't count.
pub fn def_has_codegen(def: &TargetDef) -> bool {
    use crate::engine::driver::targetdef::path::CodegenMode;
    def.outputs
        .iter()
        .flat_map(|o| o.paths.iter())
        .any(|p| !matches!(p.codegen_tree, CodegenMode::None))
}

/// A listed fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedField {
    Labels,
    Driver,
    HasCodegen,
}

impl fmt::Display for ListedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Labels => "labels",
            Self::Driver => "driver",
            Self::HasCodegen => "has_codegen",
        })
    }
}

/// Every known field of `facts` that differs from what the target resolved
/// to, as `(field, listed, resolved)`. `def` is `None` when it was not
/// resolved, and then `has_codegen` is not compared.
pub fn listed_differences(
    facts: &ListedFacts,
    spec: &TargetSpec,
    def: Option<&TargetDef>,
) -> Vec<(ListedField, String, String)> {
    let mut out = Vec::new();
    if let Some(listed) = facts.labels() {
        let resolved = ListedFacts::default().with_labels(spec.labels.iter().cloned());
        let resolved = resolved.labels().unwrap_or_default();
        if listed != resolved {
            out.push((
                ListedField::Labels,
                format!("[{}]", listed.join(", ")),
                format!("[{}]", resolved.join(", ")),
            ));
        }
    }
    if let Some(listed) = facts.driver()
        && listed != spec.driver
    {
        out.push((ListedField::Driver, listed.to_string(), spec.driver.clone()));
    }
    if let (Some(listed), Some(def)) = (facts.has_codegen(), def) {
        let resolved = def_has_codegen(def);
        if listed != resolved {
            out.push((
                ListedField::HasCodegen,
                listed.to_string(),
                resolved.to_string(),
            ));
        }
    }
    out
}

/// How to stop acting on listed facts, for every message that reports one
/// wrong: the run's shrug-path warning and `heph validate`.
pub const KILL_SWITCH_HINT: &str =
    "set HEPH_NO_LISTED_FACTS=1 to resolve every target instead of trusting listed facts";

/// What `heph validate` found wrong with a provider's listing.
#[derive(Debug, Clone)]
pub enum ListedFactMismatch {
    /// A known field differs from the resolved spec (or def).
    Field {
        addr: Addr,
        /// The provider whose `list` made the claim, and whose `get` resolved it.
        provider: String,
        field: ListedField,
        listed: String,
        resolved: String,
    },
    /// A listing with a known field, for an addr another provider resolves.
    /// Every provider that lists the addr's name describes the addr (by its
    /// listing of that exact addr, else by all of its listings of the name),
    /// so this one's facts decide alongside the resolver's — or alone, if the
    /// resolver does not list the name.
    NotResolver {
        addr: Addr,
        provider: String,
        resolved_by: String,
    },
}

impl ListedFactMismatch {
    pub fn addr(&self) -> &Addr {
        match self {
            Self::Field { addr, .. } | Self::NotResolver { addr, .. } => addr,
        }
    }
}

impl fmt::Display for ListedFactMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field {
                addr,
                provider,
                field,
                listed,
                resolved,
            } => write!(
                f,
                "provider `{provider}` lists {} with {field} {listed}, but it resolves to {field} \
                 {resolved}. Provider `{provider}` must list exactly what its `get` returns, or \
                 leave the field unknown; {KILL_SWITCH_HINT}",
                addr.format(),
            ),
            Self::NotResolver {
                addr,
                provider,
                resolved_by,
            } => write!(
                f,
                "provider `{provider}` lists {} with listed facts, but provider `{resolved_by}` \
                 resolves it. Every lister of a name describes its targets, so `{provider}`'s \
                 facts decide for `{resolved_by}`'s target: list it with no facts, or with \
                 facts equal to what `{resolved_by}` resolves; {KILL_SWITCH_HINT}",
                addr.format(),
            ),
        }
    }
}

impl std::error::Error for ListedFactMismatch {}

/// One provider's listing of one addr, with at least one known fact.
struct Listing {
    provider: Arc<str>,
    addr: Addr,
    facts: ListedFacts,
}

impl Engine {
    /// Every listing in scope of `m` whose known facts differ from what its
    /// target resolves to, and every listing with a known fact for an addr
    /// another provider resolves.
    ///
    /// Lists each package `m` can reach and resolves every listed entry with a
    /// known field — on its own, never trusting a listed fact. An entry no
    /// provider resolves is a candidate that is not there, and has nothing to
    /// get wrong. Under [`Discovery::KeepGoing`] a package or candidate that
    /// cannot be read is recorded and skipped, like the other checks' walks.
    pub async fn listed_fact_mismatches(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &Matcher,
        discovery: Discovery,
    ) -> anyhow::Result<Vec<ListedFactMismatch>> {
        let gaps = discovery.gaps().cloned();
        let pkgs: Vec<String> = self
            .packages(m, &rs)
            .await?
            .collect::<anyhow::Result<_>>()?;
        let per_pkg = futures::stream::iter(pkgs)
            .map(|pkg| {
                let engine = Arc::clone(&self);
                let rs = Arc::clone(&rs);
                let gaps = gaps.clone();
                async move {
                    let pkg = PkgBuf::from(pkg);
                    let res = Arc::clone(&engine)
                        .package_mismatches(&rs, m, &pkg, gaps.as_deref())
                        .await;
                    match (res, &gaps) {
                        (Err(e), Some(gaps)) if !is_cancellation(&rs, &e) => {
                            gaps.record(Stage::List, provider_of(&e), format!("//{pkg}"), &e);
                            Ok(Vec::new())
                        }
                        (res, _) => res,
                    }
                }
            })
            .buffered(crate::engine::fanout::discovery_concurrency());
        futures::pin_mut!(per_pkg);

        let mut out = Vec::new();
        while let Some(found) = per_pkg.next().await {
            out.extend(found?);
        }
        Ok(out)
    }

    async fn package_mismatches(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        m: &Matcher,
        pkg: &PkgBuf,
        gaps: Option<&crate::engine::discovery::Gaps>,
    ) -> anyhow::Result<Vec<ListedFactMismatch>> {
        let states = Arc::clone(&self).probe_segments(rs, pkg).await?;
        let executor: Arc<dyn hplugin::provider::ProviderExecutor> =
            Arc::new(crate::engine::result::EngineProviderExecutor::for_list(
                Arc::downgrade(&self),
                Arc::clone(rs),
            ));

        // Each provider's own claims, one per distinct (provider, addr,
        // facts): a provider listing an addr twice the same way is one claim,
        // twice differently is two.
        let mut listings: Vec<Listing> = Vec::new();
        for provider in &self.providers {
            let name: Arc<str> = Arc::from(provider.name.as_str());
            let items = provider
                .provider
                .list(
                    ListRequest {
                        request_id: rs.request_id().to_string(),
                        package: pkg.clone(),
                        states: states
                            .iter()
                            .filter(|s| s.provider == provider.name)
                            .cloned()
                            .collect(),
                        executor: Arc::clone(&executor),
                    },
                    rs.ctoken(),
                )
                .await?;
            for item in items {
                let item = item?;
                if item.addr.package != *pkg
                    || item.facts.is_unknown()
                    || m.matches_addr(&item.addr) == MatchResult::MatchNo
                {
                    continue;
                }
                let duplicate = listings
                    .iter()
                    .any(|l| *l.provider == *name && l.addr == item.addr && l.facts == item.facts);
                if !duplicate {
                    listings.push(Listing {
                        provider: Arc::clone(&name),
                        addr: item.addr,
                        facts: item.facts,
                    });
                }
            }
        }

        let mut out = Vec::new();
        for Listing {
            provider,
            addr,
            facts,
        } in listings
        {
            let spec = match skip_unresolvable(
                &addr,
                Arc::clone(&self).get_spec(Arc::clone(rs), &addr).await,
            ) {
                Ok(Some(spec)) => spec,
                Ok(None) => continue,
                Err(e) => match gaps {
                    Some(gaps) if !is_cancellation(rs, &e) => {
                        gaps.record(Stage::Spec, provider_of(&e), addr.format(), &e);
                        continue;
                    }
                    _ => return Err(e),
                },
            };
            if spec.provider.as_str() != &*provider {
                out.push(ListedFactMismatch::NotResolver {
                    addr,
                    provider: provider.to_string(),
                    resolved_by: spec.provider.clone(),
                });
                continue;
            }
            let def = if facts.has_codegen().is_some() {
                match skip_unresolvable(
                    &addr,
                    Arc::clone(&self).get_def(Arc::clone(rs), &addr).await,
                ) {
                    Ok(def) => def,
                    Err(e) => match gaps {
                        Some(gaps) if !is_cancellation(rs, &e) => {
                            gaps.record(Stage::Def, &spec.provider, addr.format(), &e);
                            None
                        }
                        _ => return Err(e),
                    },
                }
            } else {
                None
            };
            let def = def.as_ref().map(|d| d.target_def.as_ref());
            for (field, listed, resolved) in listed_differences(&facts, &spec, def) {
                out.push(ListedFactMismatch::Field {
                    addr: addr.clone(),
                    provider: provider.to_string(),
                    field,
                    listed,
                    resolved,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path};
    use crate::engine::driver::targetdef::{CacheConfig, Output};
    use crate::engine::matcher_spec::match_spec;
    use crate::engine::matcher_target::match_target;
    use hmodel::htpkg::PkgBuf;

    /// The whole message `heph validate` prints for a lying field, pinned:
    /// it is what a user reads to find the provider and the way out.
    #[test]
    fn field_mismatch_display() {
        let m = ListedFactMismatch::Field {
            addr: Addr::new(PkgBuf::from("p"), "t".to_string(), Default::default()),
            provider: "go".to_string(),
            field: ListedField::Driver,
            listed: "exec".to_string(),
            resolved: "bash".to_string(),
        };
        assert_eq!(
            m.to_string(),
            "provider `go` lists //p:t with driver exec, but it resolves to driver bash. \
             Provider `go` must list exactly what its `get` returns, or leave the field \
             unknown; set HEPH_NO_LISTED_FACTS=1 to resolve every target instead of trusting \
             listed facts"
        );
    }

    fn target(labels: &[&str], driver: &str, codegen: bool) -> (TargetSpec, TargetDef) {
        let addr = Addr::new(PkgBuf::from("p"), "t".to_string(), Default::default());
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: driver.to_string(),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            ..Default::default()
        };
        let def = TargetDef {
            addr,
            labels: spec.labels.clone(),
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![Output {
                group: "out".to_string(),
                paths: vec![Path {
                    content: Content::DirPath("p/gen".to_string()),
                    codegen_tree: if codegen {
                        CodegenMode::Copy
                    } else {
                        CodegenMode::None
                    },
                    collect: false,
                }],
            }],
            support_files: vec![],
            cache: CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        };
        (spec, def)
    }

    /// Every way a listing can describe `(spec, def)` truthfully: each field
    /// unknown or exact.
    fn honest_facts(spec: &TargetSpec, def: &TargetDef) -> Vec<ListedFacts> {
        let mut out = Vec::new();
        for labels in [false, true] {
            for driver in [false, true] {
                for codegen in [false, true] {
                    let mut f = ListedFacts::default();
                    if labels {
                        f = f.with_labels(spec.labels.iter().cloned());
                    }
                    if driver {
                        f = f.with_driver(spec.driver.as_str());
                    }
                    if codegen {
                        f = f.with_has_codegen(def_has_codegen(def));
                    }
                    out.push(f);
                }
            }
        }
        out
    }

    /// Every matcher over a small alphabet, `Not`/`And`/`Or` to depth 2.
    fn matchers() -> Vec<Matcher> {
        let atoms = vec![
            Matcher::Label("a".to_string()),
            Matcher::Label("b".to_string()),
            Matcher::Driver("bash".to_string()),
            Matcher::Driver("exec".to_string()),
            Matcher::Driver(String::new()),
            Matcher::TreeOutputTo(PkgBuf::from("")),
            Matcher::TreeOutputTo(PkgBuf::from("q")),
            Matcher::Package(PkgBuf::from("p")),
        ];
        let mut out = atoms.clone();
        for a in &atoms {
            out.push(Matcher::Not(Box::new(a.clone())));
            for b in &atoms {
                out.push(Matcher::And(vec![a.clone(), b.clone()]));
                out.push(Matcher::Or(vec![
                    a.clone(),
                    Matcher::Not(Box::new(b.clone())),
                ]));
                out.push(Matcher::Not(Box::new(Matcher::And(vec![
                    a.clone(),
                    b.clone(),
                ]))));
            }
        }
        out.push(Matcher::And(vec![]));
        out.push(Matcher::Or(vec![]));
        out
    }

    /// Invariant 2's other half: facts consistent with a target never decide
    /// differently from the target itself. Exhaustive over a small domain —
    /// every matcher above, every truthful listing, every target shape —
    /// rather than sampled.
    #[test]
    fn listed_verdict_agrees_with_resolved() {
        let mut decided = 0usize;
        for labels in [&[][..], &["a"][..], &["a", "b"][..]] {
            for driver in ["bash", "exec", "go_compile"] {
                for codegen in [false, true] {
                    let (spec, def) = target(labels, driver, codegen);
                    for m in matchers() {
                        let resolved = match match_spec(&m, &spec) {
                            MatchResult::MatchShrug => match_target(&m, &spec, &def),
                            v => v,
                        };
                        assert_ne!(resolved, MatchResult::MatchShrug, "{m:?}");
                        for facts in honest_facts(&spec, &def) {
                            let listed = m.matches_listed(&spec.addr, &facts);
                            if listed != MatchResult::MatchShrug {
                                decided += 1;
                                assert_eq!(
                                    listed, resolved,
                                    "{m:?} with {facts:?} on {labels:?}/{driver}/{codegen}"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(decided > 1_000, "vacuous: only {decided} listed verdicts");
    }

    #[test]
    fn differences_name_every_field_that_lies() {
        let (spec, def) = target(&["a"], "bash", true);
        let lie = ListedFacts::default()
            .with_labels(["b"])
            .with_driver("exec")
            .with_has_codegen(false);
        let fields: Vec<ListedField> = listed_differences(&lie, &spec, Some(&def))
            .into_iter()
            .map(|(f, _, _)| f)
            .collect();
        assert_eq!(
            fields,
            vec![
                ListedField::Labels,
                ListedField::Driver,
                ListedField::HasCodegen
            ]
        );
        // Without the def, `has_codegen` is not compared.
        assert_eq!(listed_differences(&lie, &spec, None).len(), 2);
        // Labels compare as a set.
        let shuffled = target(&["b", "a"], "bash", true).0;
        let honest = ListedFacts::default().with_labels(["a", "b", "a"]);
        assert!(listed_differences(&honest, &shuffled, None).is_empty());
    }
}
