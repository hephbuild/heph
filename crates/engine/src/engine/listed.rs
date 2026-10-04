//! `heph validate`'s check that every provider lists the labels its specs
//! carry.
//!
//! A label selector trusts a provider's listing: it decides membership from
//! the labels `list` reported and never resolves a candidate to second-guess
//! them. So a provider that lists labels `get` does not return selects the
//! wrong targets, silently. Nothing on the build path checks — that is the
//! point of trusting the listing — so this is where it is held to its specs.

use crate::engine::Engine;
use crate::engine::discovery::{Discovery, Stage, is_cancellation, provider_of};
use crate::engine::provider::ListRequest;
use crate::engine::query::{Candidate, merge_listings, skip_unresolvable};
use crate::engine::request_state::RequestState;
use futures::StreamExt;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::{MatchResult, Matcher};
use hmodel::htpkg::PkgBuf;
use rustc_hash::FxHashMap;
use std::fmt;
use std::sync::Arc;

/// A provider listed a target with labels its resolved spec does not carry.
#[derive(Debug, Clone)]
pub struct ListedLabelsMismatch {
    pub addr: Addr,
    /// The provider whose `list` made the claim, and whose `get` resolved it.
    pub listed_by: String,
    /// Sorted, deduplicated.
    pub listed: Vec<String>,
    /// Sorted, deduplicated.
    pub resolved: Vec<String>,
}

impl fmt::Display for ListedLabelsMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "provider `{}` lists {} with labels [{}], but its spec has [{}]",
            self.listed_by,
            self.addr.format(),
            self.listed.join(", "),
            self.resolved.join(", "),
        )?;
        let only_listed = difference(&self.listed, &self.resolved);
        let only_resolved = difference(&self.resolved, &self.listed);
        if !only_listed.is_empty() {
            write!(
                f,
                "; listed but not in the spec: {}",
                only_listed.join(", ")
            )?;
        }
        if !only_resolved.is_empty() {
            write!(
                f,
                "; in the spec but not listed: {}",
                only_resolved.join(", ")
            )?;
        }
        write!(
            f,
            ". Provider `{}` must list exactly the labels its `get` returns",
            self.listed_by,
        )
    }
}

impl std::error::Error for ListedLabelsMismatch {}

/// Labels in `a` and not in `b`. Both are short.
fn difference<'a>(a: &'a [String], b: &[String]) -> Vec<&'a str> {
    a.iter()
        .filter(|l| !b.contains(l))
        .map(String::as_str)
        .collect()
}

fn normalized(labels: &[String]) -> Vec<String> {
    let mut out = labels.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

impl Engine {
    /// Every target in scope of `m` whose provider listed labels that differ
    /// from its resolved spec's.
    ///
    /// Lists each package `m` can reach, as the selector walk does, and
    /// resolves the spec of every candidate listed with labels. A candidate
    /// the lister's own `get` declines is not its target and has no labels to
    /// get wrong. Under [`Discovery::KeepGoing`] a package or candidate that
    /// cannot be read is recorded and skipped, like the other checks' walks.
    pub async fn listed_label_mismatches(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &Matcher,
        discovery: Discovery,
    ) -> anyhow::Result<Vec<ListedLabelsMismatch>> {
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
                    let res = Arc::clone(&engine).package_mismatches(&rs, m, &pkg).await;
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
    ) -> anyhow::Result<Vec<ListedLabelsMismatch>> {
        let states = Arc::clone(&self).probe_segments(rs, pkg).await?;
        let executor: Arc<dyn hplugin::provider::ProviderExecutor> =
            Arc::new(crate::engine::result::EngineProviderExecutor::for_list(
                Arc::downgrade(&self),
                Arc::clone(rs),
            ));

        // Every listing in the package, in provider order, then reconciled the
        // way the selector walk does (`merge_listings`): only an addr whose
        // listings all carry the same set is decided from it — anything else is
        // resolved by the walk, so it has no listing to hold to its spec. Go's
        // bare `build`, listed in a library where a BUILD file defines its own,
        // is one: two providers, two sets, the spec decides.
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut listed_by: FxHashMap<Addr, &str> = FxHashMap::default();
        for provider in &self.providers {
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
                if item.addr.package != *pkg || m.matches_addr(&item.addr) == MatchResult::MatchNo {
                    continue;
                }
                listed_by
                    .entry(item.addr.clone())
                    .or_insert(provider.name.as_str());
                candidates.push(Candidate {
                    addr: item.addr,
                    labels: item.labels,
                });
            }
        }

        let mut out = Vec::new();
        for Candidate { addr, labels } in merge_listings(candidates) {
            let Some(labels) = labels else { continue };
            let spec = Arc::clone(&self).get_spec(Arc::clone(rs), &addr).await;
            let Some(spec) = skip_unresolvable(&addr, spec)? else {
                continue;
            };
            // A listing is a claim about the lister's own target. One its own
            // `get` declines is a candidate that is not there, from its side —
            // like go's `test` in a package with no tests — even when another
            // provider resolves the same addr (the buildfile provider matches a
            // target name whatever its args, so it answers go's `build@v=host`
            // in a library with the BUILD file's `build`).
            let listed_by = listed_by.get(&addr).copied().unwrap_or_default();
            if spec.provider != listed_by {
                continue;
            }
            let listed = normalized(&labels);
            let resolved = normalized(&spec.labels);
            if resolved != listed {
                out.push(ListedLabelsMismatch {
                    listed_by: listed_by.to_string(),
                    addr,
                    listed,
                    resolved,
                });
            }
        }
        Ok(out)
    }
}
