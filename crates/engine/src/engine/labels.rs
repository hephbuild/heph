use crate::engine::Engine;
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
    /// resolving each one. `heph inspect labels --exact`.
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
        if source == LabelSource::Resolved || !m.is_addr_only() {
            let specs = self.query_spec(rs, m, discovery);
            tokio::pin!(specs);
            while let Some(spec) = specs.try_next().await? {
                insert_new(&mut labels, &spec.labels);
            }
            return Ok(labels);
        }

        // Candidates whose labels are listed are folded as they arrive, into
        // their own set; only the rest take a slot in the bounded set of
        // in-flight resolutions.
        let mut listed = BTreeSet::new();
        {
            let gaps = discovery.gaps().cloned();
            let resolved = Arc::clone(&self)
                .listed_candidates(Arc::clone(&rs), m, discovery)
                .try_filter_map(|c| {
                    std::future::ready(Ok(match c.facts.labels() {
                        Some(known) => {
                            insert_new(&mut listed, known);
                            None
                        }
                        None => Some(c.addr),
                    }))
                })
                .map_ok(move |addr| {
                    Arc::clone(&self).resolve_candidate_spec(Arc::clone(&rs), gaps.clone(), addr)
                })
                .try_buffer_unordered(crate::engine::fanout::discovery_concurrency())
                .try_filter_map(|spec| std::future::ready(Ok(spec)));
            tokio::pin!(resolved);
            while let Some(spec) = resolved.try_next().await? {
                insert_new(&mut labels, &spec.labels);
            }
        }
        labels.append(&mut listed);
        Ok(labels)
    }
}

/// Add each of `new` to `set`, cloning only the ones it does not hold yet.
fn insert_new(set: &mut BTreeSet<String>, new: &[String]) {
    for label in new {
        if !set.contains(label) {
            set.insert(label.clone());
        }
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
        collect_matching(
            engine,
            rs,
            &Matcher::PackagePrefix(PkgBuf::from("")),
            discovery,
            source,
        )
        .await
    }

    async fn collect_matching(
        engine: &Arc<Engine>,
        rs: Arc<RequestState>,
        m: &Matcher,
        discovery: Discovery,
        source: LabelSource,
    ) -> anyhow::Result<Vec<String>> {
        Ok(Arc::clone(engine)
            .labels(rs, m, discovery, source)
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
    /// label; `--exact` drops the target that does not exist.
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

    /// A provider whose package listing failed under keep-going is missing
    /// from every package, so no listing decides anywhere: the target that
    /// does not resolve drops its label.
    #[tokio::test]
    async fn a_skipped_package_listing_makes_every_package_resolve() -> anyhow::Result<()> {
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
                    fail_list_packages: true,
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
        assert!(!gaps.is_empty(), "the failed package listing is reported");
        Ok(())
    }

    /// An addr, and a negated package, are decided from the addr: folded from
    /// the listings, nothing resolved. A label matcher is not, so its walk
    /// resolves.
    #[tokio::test]
    async fn listed_labels_follow_the_matcher() -> anyhow::Result<()> {
        let targets = || {
            vec![
                target("foo", "a", &["lint"]),
                target("foo", "b", &["fmt"]),
                target("bar", "c", &["test"]),
            ]
        };

        for (m, want) in [
            (Matcher::Addr(addr("foo", "a")), vec!["lint"]),
            (
                Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo")))),
                vec!["test"],
            ),
        ] {
            let (engine, logs, _root) = fault_engine(targets, vec![Faults::default()])?;
            let labels = collect_matching(
                &engine,
                engine.new_state(),
                &m,
                Discovery::Complete,
                LabelSource::Listed,
            )
            .await?;
            assert_eq!(labels, want, "{m:?}");
            assert_eq!(logs[0].addrs(), Vec::<Addr>::new(), "{m:?}: resolved");
        }

        let (engine, logs, _root) = fault_engine(targets, vec![Faults::default()])?;
        let labels = collect_matching(
            &engine,
            engine.new_state(),
            &Matcher::Label("fmt".to_string()),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await?;
        assert_eq!(labels, vec!["fmt"]);
        assert_eq!(
            logs[0].addrs(),
            vec![addr("foo", "b")],
            "a label walk resolves"
        );
        Ok(())
    }

    /// A complete walk fails on a `list` error, and joins the package tasks
    /// still running before it returns, so none outlives the walk holding
    /// the request.
    #[tokio::test]
    async fn listed_labels_fail_on_a_list_error_and_drain() -> anyhow::Result<()> {
        let (engine, _logs, _root) = fault_engine(
            || vec![target("p", "a", &["x"]), target("q", "slow", &["x"])],
            vec![Faults {
                fail_list: vec!["p".to_string()],
                // `p`'s list fails only once `q`'s has started, so `q` is in
                // flight when the walk meets the error.
                gate: Some(Arc::new(tokio::sync::Notify::new())),
                slow_list: Some(("q".to_string(), std::time::Duration::from_millis(300))),
                ..Default::default()
            }],
        )?;

        let rs = engine.new_state();
        let err = collect(
            &engine,
            rs.clone(),
            Discovery::Complete,
            LabelSource::Listed,
        )
        .await
        .expect_err("a failed list fails a complete walk");
        assert!(format!("{err:#}").contains("syntax error"), "{err:#}");
        assert_eq!(
            Arc::strong_count(&rs),
            1,
            "a package task was still running, holding the request"
        );
        Ok(())
    }

    /// A cancelled walk is an error, never the labels it had found so far —
    /// under keep-going too, where a cancellation is not a skip.
    #[tokio::test]
    async fn a_cancelled_listed_walk_is_an_error() -> anyhow::Result<()> {
        let started = Arc::new(tokio::sync::Notify::new());
        let (engine, _logs, _root) = fault_engine(
            || vec![target("p", "a", &["x"]), target("q", "b", &["y"])],
            vec![Faults {
                cancel_at: Some((
                    crate::engine::discovery::Stage::List,
                    "q".to_string(),
                    Arc::clone(&started),
                )),
                ..Default::default()
            }],
        )?;

        let gaps = Gaps::new("//...");
        let rs = engine.new_state();
        let walk = tokio::spawn({
            let (engine, rs, gaps) = (Arc::clone(&engine), Arc::clone(&rs), gaps.clone());
            async move { collect(&engine, rs, Discovery::KeepGoing(gaps), LabelSource::Listed).await }
        });
        started.notified().await;
        rs.ctoken().cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), walk).await??;
        let err = res.expect_err("a cancelled walk is not a complete one");
        assert!(
            crate::engine::discovery::is_cancellation(&rs, &err),
            "{err:#}"
        );
        assert!(gaps.is_empty(), "{:?}", gaps.report());
        Ok(())
    }
}
