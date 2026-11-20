//! The engine's answer to the BUILD-file LSP's listings ([`LspEngine::list`]).
//!
//! The LSP completes `//pkg` and `//pkg:name` from what `heph query` would
//! list: every registered provider, with its states and probes, under
//! [`Discovery::KeepGoing`], so one failing provider costs only its own
//! entries. Each call is its own root request, so its own cache scope: an
//! editor session holds one engine for hours, and a scope per call is what
//! makes a package created, a target renamed or a BUILD file fixed show up
//! on the next completion (see `docs/LSP.md`).
//!
//! [`LspEngine::list`]: hplugin::lsp::LspEngine::list

use crate::engine::Engine;
use crate::engine::discovery::{Discovery, Gaps, Stage, is_cancellation};
use crate::engine::packages::merge_packages;
use crate::engine::request_state::RequestState;
use futures::TryStreamExt as _;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hplugin::lsp::{Listed, ListedEntry, Listing};
use std::sync::Arc;

impl Engine {
    /// [`LspEngine::list`](hplugin::lsp::LspEngine::list): `what`, in a cache
    /// scope of its own.
    pub(crate) async fn lsp_list(self: Arc<Self>, what: Listing) -> anyhow::Result<Listed> {
        let rs = self.new_state();
        let scope = rs.cache_scope().id().get();
        let scope_ended = self.watch_scope_end(scope);
        let (gaps, entries) = match &what {
            Listing::Packages { under } => {
                let gaps = Gaps::new(format!("//{under}/..."));
                let entries = self.lsp_packages(&rs, under, &gaps).await?;
                (gaps, entries)
            }
            Listing::Targets { package } => {
                let gaps = Gaps::new(format!("//{package}:*"));
                let entries = Arc::clone(&self).lsp_targets(&rs, package, &gaps).await?;
                (gaps, entries)
            }
        };
        // The scope ends once this was its last holder; `scope_ended` resolves
        // when every provider heard it.
        drop(rs);
        let skipped = gaps
            .report()
            .groups
            .into_iter()
            .flat_map(|g| {
                let stage = g.stage.as_str();
                let provider = g.provider;
                g.examples
                    .into_iter()
                    .map(move |e| format!("{stage} {provider} {}: {}", e.scope, e.cause))
            })
            .collect();
        tracing::debug!(
            scope,
            listing = ?what,
            entries = entries.len(),
            "lsp listing"
        );
        Ok(Listed {
            scope,
            entries,
            skipped,
            scope_ended,
        })
    }

    /// The packages at or below `under`, each with the first provider (in
    /// registration order) that listed it: the merge `heph query` walks.
    async fn lsp_packages(
        &self,
        rs: &Arc<RequestState>,
        under: &str,
        gaps: &Gaps,
    ) -> anyhow::Result<Vec<ListedEntry>> {
        let m = Matcher::PackagePrefix(PkgBuf::from(under));
        let lists = self.provider_packages(&m, rs).await;
        let mut listed = Vec::with_capacity(lists.len());
        let mut names = Vec::with_capacity(lists.len());
        for (provider, res) in self.providers.iter().zip(lists) {
            match res {
                Ok(pkgs) => {
                    listed.push(pkgs);
                    names.push(provider.name.as_str());
                }
                Err(e) if is_cancellation(rs, &e) => return Err(e),
                Err(e) => gaps.record(
                    Stage::Packages,
                    &provider.name,
                    "all packages".to_string(),
                    &e,
                ),
            }
        }
        let mut entries: Vec<ListedEntry> = merge_packages(&m, &listed)
            .into_iter()
            .map(|pkg| {
                // Each block is sorted, so membership is a binary search.
                let provider = listed
                    .iter()
                    .zip(&names)
                    .find(|(block, _)| block.binary_search(&pkg).is_ok())
                    .map_or("", |(_, name)| name)
                    .to_string();
                ListedEntry {
                    name: pkg,
                    provider,
                }
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The targets in `package`, each with the provider that listed it.
    async fn lsp_targets(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        package: &str,
        gaps: &Arc<Gaps>,
    ) -> anyhow::Result<Vec<ListedEntry>> {
        let m = Matcher::Package(PkgBuf::from(package));
        let found: Vec<_> = Arc::clone(&self)
            .query_listed(Arc::clone(rs), &m, Discovery::KeepGoing(Arc::clone(gaps)))
            .try_collect()
            .await?;
        let mut entries: Vec<ListedEntry> = found
            .into_iter()
            .map(|(addr, lister)| ListedEntry {
                provider: self
                    .providers
                    .get(lister)
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
                name: addr.name.clone(),
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        entries.dedup_by(|a, b| a.name == b.name);
        Ok(entries)
    }
}
