//! "Did you mean" for a target not found (error-reporting D4, I4).
//!
//! On the error path only. [`Engine::load_not_found`] lists the missing
//! target's package once per request, after its `get` said not found (the
//! root's `get_def`, or a dependency's [`missing_dep_or`]
//! (crate::engine::request_state::RequestState::missing_dep_or)), and keeps
//! the names. [`suggest`] then reads them, without awaiting, when the request
//! records the failure.
//!
//! Fail closed (I4): the listing runs with a [`NoopExecutor`], so it can
//! never build; a listing that failed or skipped a provider suggests
//! nothing; the package walk (`list_packages`) is never called, only the
//! request's memoized walks are read.

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use hcore::diagnostic::{Hint, HintKind};
use hcore::failure::FailureRecord;
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::error::TargetNotFoundError;
use hplugin::provider::{NoopExecutor, ProviderExecutor};
use std::sync::Arc;

/// The most target names a root not-found lists.
pub(crate) const LISTED: usize = 5;

/// One package's target names, sorted and deduped, as every provider listed
/// them; `None` when a provider's probe or listing failed, so the names may
/// be short. Loaded at most once per package per request.
pub(crate) type ListingCell = Arc<tokio::sync::OnceCell<Option<Arc<[String]>>>>;

impl Engine {
    /// List `pkg` for a not-found's suggestions, once per request
    /// ([`ListingCell`]). Error path only: every caller just had `get`
    /// report a target in `pkg` missing, which probed it already.
    pub(crate) async fn load_not_found(self: &Arc<Self>, rs: &Arc<RequestState>, pkg: &PkgBuf) {
        let cell = rs.not_found_cell(pkg);
        cell.get_or_init(|| async {
            let noop: Arc<dyn ProviderExecutor> = Arc::new(NoopExecutor);
            let scan = Arc::clone(self)
                .scan_package(rs, &noop, pkg.clone(), true)
                .await
                .ok()
                .filter(super::query::PackageScan::complete)?;
            let mut names: Vec<String> = scan
                .candidates
                .into_iter()
                .map(|c| c.addr.name.clone())
                .collect();
            names.sort_unstable();
            names.dedup();
            Some(names.into())
        })
        .await;
    }
}

/// `rec`, the record of `e` against `culprit`, with D4's suggestions when
/// `e` is a target not found: `culprit` itself (the root), or a dependency
/// `culprit` declared ([`MissingDep`](hplugin::error::MissingDep)).
///
/// - Its package listed targets: the nearest names as full addrs, and for
///   the root the package's targets too (at most [`LISTED`]).
/// - It listed none, and the request's memoized package walks prove the
///   package does not exist: the nearest packages, the target name kept
///   (`//lib:a` for `//libb:a`), and for the root the message
///   "no package //libb".
/// - Otherwise, or when the record already has hints of its own, `rec`.
pub(crate) fn suggest(
    rs: &RequestState,
    rec: FailureRecord,
    culprit: &Addr,
    e: &anyhow::Error,
    scrub: &crate::engine::failure_record::Scrub,
) -> FailureRecord {
    let Some(missing) = downcast_chain_ref::<TargetNotFoundError>(e).map(|nf| &nf.addr) else {
        return rec;
    };
    let root = missing == culprit;
    let declared = rec
        .missing_dep
        .as_ref()
        .is_some_and(|m| m.dep == missing.format());
    if !(root || declared) || !rec.diagnostic.hints.is_empty() {
        return rec;
    }
    let Some(Some(names)) = rs.not_found_listing(&missing.package) else {
        return rec;
    };
    let (message, hints) = if names.is_empty() {
        let Some(packages) = absent_packages(rs, &missing.package) else {
            return rec;
        };
        let near = hcore::suggest::suggestions(
            missing.package.as_str(),
            packages.iter().map(String::as_str),
        );
        let addrs: Vec<String> = near
            .names
            .iter()
            .map(|p| in_package(missing, PkgBuf::from(*p)))
            .collect();
        let message = root.then(|| format!("no package //{}", missing.package.as_str()));
        (message, hints_of(&near, &addrs))
    } else {
        let others = || {
            names
                .iter()
                .map(String::as_str)
                .filter(|n| *n != missing.name)
        };
        let near = hcore::suggest::suggestions(&missing.name, others());
        let addrs: Vec<String> = near
            .names
            .iter()
            .map(|n| {
                Addr::new(
                    missing.package.clone(),
                    (*n).to_owned(),
                    missing.args.clone(),
                )
                .format()
            })
            .collect();
        let mut hints = hints_of(&near, &addrs);
        if root && others().next().is_some() {
            hints.push(Hint::new(
                HintKind::ValidValues,
                format!(
                    "targets in //{}: {}",
                    missing.package.as_str(),
                    hcore::suggest::listed(others(), LISTED)
                ),
            ));
        }
        (None, hints)
    };
    if message.is_none() && hints.is_empty() {
        return rec;
    }
    crate::engine::failure_record::suggested(rec, scrub, message, hints)
}

/// `near`'s hints, each name shown as its `addrs` entry.
fn hints_of(near: &hcore::suggest::Suggestions<'_>, addrs: &[String]) -> Vec<Hint> {
    if near.over_cap.is_some() {
        return near.hints();
    }
    let addrs: Vec<&str> = addrs.iter().map(String::as_str).collect();
    hcore::suggest::did_you_mean(&addrs)
}

/// `missing`'s target name and args in package `pkg`, formatted.
fn in_package(missing: &Addr, pkg: PkgBuf) -> String {
    Addr::new(pkg, missing.name.clone(), missing.args.clone()).format()
}

/// The packages the request's memoized walks hold, when they prove `pkg`
/// does not exist: every provider has a walk under `pkg` or one of its
/// ancestors, and none holds `pkg`. `None` when any provider has no such
/// walk, its walk failed, or one holds `pkg` (a package with no targets).
/// Reads memoized walks only; never walks (I4).
fn absent_packages(rs: &RequestState, pkg: &PkgBuf) -> Option<Vec<String>> {
    let engine = rs.data.engine.upgrade()?;
    let mut all: Vec<String> = Vec::new();
    for provider in &engine.providers {
        let walk = pkg.parent_packages().find_map(|prefix| {
            rs.data
                .mem_packages
                .peek(format!("{}:{}", provider.name, prefix.as_str()).as_str())
        })?;
        let walk = walk.ok()?;
        if walk
            .binary_search_by(|p| p.as_str().cmp(pkg.as_str()))
            .is_ok()
        {
            return None;
        }
        all.extend(walk.iter().cloned());
    }
    all.sort_unstable();
    all.dedup();
    Some(all)
}
