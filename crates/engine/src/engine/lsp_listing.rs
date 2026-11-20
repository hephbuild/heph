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
use crate::engine::discovery::{Discovery, Gaps};
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
        // The watcher rides on the scope from its mint: however this future
        // ends — answered, failed or dropped mid-listing — the scope ends with
        // its last holder and the watcher goes with it.
        let (rs, scope_ended) = self.new_watched_state();
        let scope = rs.cache_scope().id();
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
        let skipped = gaps.report().selection_gaps();
        tracing::debug!(
            scope = scope.get(),
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
        let (indices, blocks): (Vec<usize>, Vec<_>) = self
            .provider_packages_keep_going(&m, rs, gaps)
            .await?
            .into_iter()
            .unzip();
        let mut entries: Vec<ListedEntry> = merge_packages(&m, &blocks)
            .into_iter()
            .map(|pkg| {
                // Each block is sorted, so membership is a binary search; the
                // blocks are in registration order, so the first hit is the
                // first lister.
                let provider = blocks
                    .iter()
                    .zip(&indices)
                    .find(|(block, _)| block.binary_search(&pkg).is_ok())
                    .and_then(|(_, &i)| self.providers.get(i))
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                ListedEntry {
                    name: pkg,
                    provider,
                }
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The targets in `package`, each name once, with the first provider (in
    /// registration order) that listed a target of that name.
    async fn lsp_targets(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        package: &str,
        gaps: &Arc<Gaps>,
    ) -> anyhow::Result<Vec<ListedEntry>> {
        let m = Matcher::Package(PkgBuf::from(package));
        let mut found: Vec<(String, usize)> = Arc::clone(&self)
            .query_listed(Arc::clone(rs), &m, Discovery::KeepGoing(Arc::clone(gaps)))
            .map_ok(|(addr, lister)| (addr.name.clone(), lister))
            .try_collect()
            .await?;
        // Several addrs can share a name (variants), each from its own lister,
        // in whatever order the walk completed: order by name, then by lister,
        // so the dedup keeps the earliest-registered one.
        found.sort_unstable();
        found.dedup_by(|a, b| a.0 == b.0);
        Ok(found
            .into_iter()
            .map(|(name, lister)| ListedEntry {
                provider: self
                    .providers
                    .get(lister)
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
                name,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::fault_provider::{FaultProvider, Faults};
    use hbuiltins::pluginstatictarget;

    fn target(addr: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            ..Default::default()
        }
    }

    /// An engine with one fault provider per `(name, targets, faults)`, in
    /// that registration order.
    fn engine_with(
        providers: Vec<(&'static str, Vec<&str>, Faults)>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        for (name, targets, faults) in providers {
            let provider = FaultProvider::new(
                targets.into_iter().map(target).collect(),
                Faults {
                    name: Some(name),
                    ..faults
                },
            )?;
            engine.register_provider(move |_| Box::new(provider))?;
        }
        Ok((Arc::new(engine), root))
    }

    /// Two providers listing the same package and the same target name: each
    /// entry is credited to the first one registered, whichever answered
    /// first.
    #[tokio::test]
    async fn credit_goes_to_the_first_lister() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            ("first", vec!["//p:t"], Faults::default()),
            (
                "second",
                vec!["//p:t", "//p:only_second"],
                Faults::default(),
            ),
        ])?;
        let pkgs = Arc::clone(&engine)
            .lsp_list(Listing::Packages {
                under: "p".to_string(),
            })
            .await?;
        assert_eq!(
            pkgs.entries,
            [ListedEntry {
                name: "p".to_string(),
                provider: "first".to_string()
            }],
            "{pkgs:?}"
        );
        let targets = Arc::clone(&engine)
            .lsp_list(Listing::Targets {
                package: "p".to_string(),
            })
            .await?;
        let got: Vec<(&str, &str)> = targets
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.provider.as_str()))
            .collect();
        assert_eq!(
            got,
            [("only_second", "second"), ("t", "first")],
            "{targets:?}"
        );
        Ok(())
    }

    /// A listing's skips reach `Listed.skipped` typed, one group per stage and
    /// provider.
    #[tokio::test]
    async fn skipped_is_typed_per_stage_and_provider() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            (
                "broken",
                vec!["//a:t"],
                Faults {
                    fail_list_packages: true,
                    ..Default::default()
                },
            ),
            ("healthy", vec!["//b:t"], Faults::default()),
        ])?;
        let listed = Arc::clone(&engine)
            .lsp_list(Listing::Packages {
                under: "b".to_string(),
            })
            .await?;
        assert_eq!(listed.entries.len(), 1, "{listed:?}");
        let [gap] = listed.skipped.as_slice() else {
            panic!("one group: {listed:?}");
        };
        assert_eq!(
            (gap.stage.as_str(), gap.provider.as_str(), gap.count),
            ("packages", "broken", 1)
        );
        assert_eq!(gap.examples.len(), 1, "{gap:?}");
        assert_eq!(gap.examples[0].scope, "all packages");
        assert!(!gap.examples[0].cause.is_empty(), "{gap:?}");
        Ok(())
    }

    /// More skips in one group than it keeps examples of: the group keeps
    /// [`EXAMPLES_PER_GROUP`] of them and the exact count, so the rest are
    /// not silently lost. (`lsp_list` reports `selection_gaps` as is; no
    /// single listing the fault provider can stage skips more than one scope
    /// per group.)
    ///
    /// [`EXAMPLES_PER_GROUP`]: crate::engine::discovery::EXAMPLES_PER_GROUP
    #[test]
    fn skipped_carries_the_count_past_the_examples() {
        use crate::engine::discovery::{EXAMPLES_PER_GROUP, Stage};
        let gaps = Gaps::new("//...");
        let err = anyhow::anyhow!("boom");
        for i in 0..EXAMPLES_PER_GROUP + 3 {
            gaps.record(Stage::Spec, "go", format!("//p:t{i}"), &err);
        }
        let skipped = gaps.report().selection_gaps();
        let [gap] = skipped.as_slice() else {
            panic!("one group: {skipped:?}");
        };
        assert_eq!(gap.count, EXAMPLES_PER_GROUP + 3, "{gap:?}");
        assert_eq!(gap.examples.len(), EXAMPLES_PER_GROUP, "{gap:?}");
    }
}
