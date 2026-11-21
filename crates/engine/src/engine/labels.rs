use crate::engine::Engine;
use crate::engine::clean::is_addr_only;
use crate::engine::discovery::Discovery;
use crate::engine::request_state::RequestState;
use futures::TryStreamExt;
use hmodel::htmatcher;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Where [`Engine::labels`] reads a target's labels from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LabelSource {
    /// A candidate's listed labels, when every provider that lists it agrees
    /// on them; its spec otherwise. A listed candidate is **not** confirmed
    /// to exist, so the set may hold a label only a target that does not
    /// resolve carries (go lists `test` in a package before `go list` knows
    /// whether it has tests). In exchange, a target its provider describes in
    /// its listing costs no `get`: for a Go target that is a `_golist` run.
    #[default]
    Listed,
    /// Every candidate's spec: only targets that exist, at the cost of
    /// resolving each one.
    Resolved,
}

impl Engine {
    /// Collect the unique set of labels declared by the targets matching `m`.
    ///
    /// With [`LabelSource::Resolved`], or a matcher an addr cannot decide,
    /// enumerates the matching targets via `query_spec` — so a listed candidate
    /// that doesn't resolve is skipped rather than fatal — and folds each
    /// spec's `labels`. With [`LabelSource::Listed`], folds each candidate's
    /// listed labels and resolves only the candidates whose labels are
    /// unknown (see [`LabelSource::Listed`]); `HEPH_NO_LISTED_FACTS=1` makes
    /// every one unknown, which is the resolved answer.
    ///
    /// Labels arrive off a bounded in-flight set and are dropped as they are
    /// folded into a sorted `BTreeSet`, so only the label set — never the full
    /// candidate or spec list — is held in memory.
    pub async fn labels(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &htmatcher::Matcher,
        discovery: Discovery,
        source: LabelSource,
    ) -> anyhow::Result<BTreeSet<String>> {
        let mut labels = BTreeSet::new();
        if source == LabelSource::Resolved || !is_addr_only(m) {
            let specs = self.query_spec(rs, m, discovery);
            tokio::pin!(specs);
            while let Some(spec) = specs.try_next().await? {
                labels.extend(spec.labels.iter().cloned());
            }
            return Ok(labels);
        }

        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .saturating_mul(2);
        let gaps = discovery.gaps().cloned();
        let found = Arc::clone(&self)
            .listed_candidates(Arc::clone(&rs), m, discovery)
            .map_ok(|c| {
                let engine = Arc::clone(&self);
                let rs = Arc::clone(&rs);
                let gaps = gaps.clone();
                async move {
                    if let Some(listed) = c.facts.labels() {
                        return Ok(listed.to_vec());
                    }
                    Ok(engine
                        .resolve_candidate_spec(rs, gaps, c.addr)
                        .await?
                        .map(|spec| spec.labels.clone())
                        .unwrap_or_default())
                }
            })
            .try_buffer_unordered(concurrency);
        tokio::pin!(found);
        while let Some(found) = found.try_next().await? {
            labels.extend(found);
        }
        Ok(labels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use hbuiltins::pluginstatictarget;
    use hmodel::htmatcher::Matcher;
    use hmodel::htpkg::PkgBuf;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn target(pkg: &str, name: &str, labels: &[&str]) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: format!("//{pkg}:{name}"),
            driver: "exec".to_string(),
            run: None,
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn make_engine(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn collects_unique_labels_across_matched_targets() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &["lint", "test"]),
            target("foo/bar", "b", &["lint"]),
            target("foo/baz", "c", &["fmt"]),
        ])?;

        let rs = engine.new_state();
        let labels: Vec<String> = engine
            .labels(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("foo")),
                Discovery::Complete,
                LabelSource::Listed,
            )
            .await?
            .into_iter()
            .collect();

        // Sorted, deduped: lint appears on two targets but once here.
        assert_eq!(
            labels,
            vec!["fmt".to_string(), "lint".to_string(), "test".to_string(),]
        );
        Ok(())
    }

    #[tokio::test]
    async fn scopes_labels_to_the_matcher() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &["lint"]),
            target("other", "b", &["fmt"]),
        ])?;

        let rs = engine.new_state();
        let labels: Vec<String> = engine
            .labels(
                rs,
                &Matcher::Package(PkgBuf::from("foo")),
                Discovery::Complete,
                LabelSource::Listed,
            )
            .await?
            .into_iter()
            .collect();

        assert_eq!(labels, vec!["lint".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn no_matches_yields_no_labels() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[])])?;

        let rs = engine.new_state();
        let labels = engine
            .labels(
                rs,
                &Matcher::Package(PkgBuf::from("foo")),
                Discovery::Complete,
                LabelSource::Listed,
            )
            .await?;

        assert!(labels.is_empty());
        Ok(())
    }

    use crate::engine::discovery::Gaps;
    use crate::engine::fault_provider::{FaultProvider, Faults, GetLog};
    use crate::engine::listed::ListedFactsTrust;
    use hmodel::htaddr::Addr;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(pkg), name.to_string(), Default::default())
    }

    /// An engine over one [`FaultProvider`] per `faults`, each listing
    /// `targets()`, and every provider's `get` log.
    fn fault_engine(
        targets: impl Fn() -> Vec<pluginstatictarget::Target>,
        providers: Vec<Faults>,
    ) -> anyhow::Result<(Arc<Engine>, Vec<Arc<GetLog>>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        let mut logs = Vec::new();
        for faults in providers {
            let provider = FaultProvider::new(targets(), faults)?;
            logs.push(provider.gets());
            engine.register_provider(move |_| Box::new(provider))?;
        }
        Ok((Arc::new(engine), logs, root))
    }

    async fn collect(
        engine: &Arc<Engine>,
        rs: Arc<RequestState>,
        discovery: Discovery,
        source: LabelSource,
    ) -> anyhow::Result<Vec<String>> {
        Ok(Arc::clone(engine)
            .labels(
                rs,
                &Matcher::PackagePrefix(PkgBuf::from("")),
                discovery,
                source,
            )
            .await?
            .into_iter()
            .collect())
    }

    /// The point of the listed source: a target whose provider listed its
    /// labels is never resolved. For a Go target that `get` is a `_golist`.
    #[tokio::test]
    async fn listed_labels_resolve_nothing() -> anyhow::Result<()> {
        let (engine, logs, _root) = fault_engine(
            || vec![target("foo", "a", &["lint"]), target("bar", "b", &["fmt"])],
            vec![Faults::default()],
        )?;

        let labels = collect(
            &engine,
            engine.new_state(),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;

        assert_eq!(labels, vec!["fmt", "lint"]);
        assert_eq!(logs[0].addrs(), Vec::<Addr>::new(), "no candidate resolved");
        Ok(())
    }

    /// The trade-off, both ways: go lists `test` in a package before it
    /// knows whether the package has tests. The listed source counts that
    /// label; `--resolve` drops the target that does not exist.
    #[tokio::test]
    async fn a_listed_target_that_does_not_resolve_counts_unless_resolved() -> anyhow::Result<()> {
        let (engine, _logs, _root) = fault_engine(
            || vec![target("foo", "build", &["go-build"])],
            vec![Faults {
                vanished: vec![(addr("foo", "test"), vec!["test".to_string()])],
                ..Default::default()
            }],
        )?;

        let listed = collect(
            &engine,
            engine.new_state(),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;
        assert_eq!(listed, vec!["go-build", "test"]);

        let resolved = collect(
            &engine,
            engine.new_state(),
            Discovery::Complete,
            LabelSource::Resolved,
        )
        .await?;
        assert_eq!(resolved, vec!["go-build"]);
        Ok(())
    }

    /// A listing with no labels (a plugin built before listed facts) is
    /// resolved, and only those candidates are.
    #[tokio::test]
    async fn a_candidate_listed_without_labels_is_resolved() -> anyhow::Result<()> {
        let (engine, logs, _root) = fault_engine(
            || vec![target("foo", "a", &["lint"])],
            vec![Faults {
                facts_unknown: true,
                vanished: vec![(addr("foo", "test"), vec!["test".to_string()])],
                ..Default::default()
            }],
        )?;

        let labels = collect(
            &engine,
            engine.new_state(),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;

        assert_eq!(labels, vec!["lint", "test"]);
        assert_eq!(logs[0].addrs(), vec![addr("foo", "a")]);
        Ok(())
    }

    /// `HEPH_NO_LISTED_FACTS=1` reads no listed label, so the answer is the
    /// resolved one.
    #[tokio::test]
    async fn ignoring_listed_facts_gives_the_resolved_answer() -> anyhow::Result<()> {
        let (engine, _logs, _root) = fault_engine(
            || vec![target("foo", "build", &["go-build"])],
            vec![Faults {
                vanished: vec![(addr("foo", "test"), vec!["test".to_string()])],
                ..Default::default()
            }],
        )?;

        let labels = collect(
            &engine,
            engine.new_state_with_listed_facts_trust(ListedFactsTrust::Ignore),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;

        assert_eq!(labels, vec!["go-build"]);
        Ok(())
    }

    /// Two providers listing one name with different labels: neither
    /// listing decides, the spec does.
    #[tokio::test]
    async fn providers_that_disagree_are_resolved() -> anyhow::Result<()> {
        let (engine, logs, _root) = fault_engine(
            || vec![target("foo", "a", &["real"])],
            vec![
                Faults {
                    name: Some("first"),
                    ..Default::default()
                },
                Faults {
                    name: Some("second"),
                    listed_labels: vec![(addr("foo", "a"), vec!["lie".to_string()])],
                    ..Default::default()
                },
            ],
        )?;

        let labels = collect(
            &engine,
            engine.new_state(),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;

        assert_eq!(labels, vec!["real"]);
        assert_eq!(logs[0].addrs(), vec![addr("foo", "a")]);
        Ok(())
    }

    /// Under keep-going, a provider whose `list` of a package failed might
    /// have disagreed with the listings that are there, so none of them
    /// decides in that package: its candidates are resolved.
    #[tokio::test]
    async fn a_skipped_listing_makes_its_package_resolve() -> anyhow::Result<()> {
        let (engine, _logs, _root) = fault_engine(
            || vec![target("foo", "build", &["go-build"])],
            vec![
                Faults {
                    name: Some("lister"),
                    vanished: vec![(addr("foo", "test"), vec!["test".to_string()])],
                    ..Default::default()
                },
                Faults {
                    name: Some("failing"),
                    fail_list: vec!["foo".to_string()],
                    ..Default::default()
                },
            ],
        )?;

        let gaps = Gaps::new("//...");
        let labels = collect(
            &engine,
            engine.new_state(),
            Discovery::keep_going_unless(false, &gaps),
            LabelSource::Listed,
        )
        .await?;

        assert_eq!(labels, vec!["go-build"]);
        assert!(!gaps.is_empty(), "the failed listing is reported");
        Ok(())
    }
}
