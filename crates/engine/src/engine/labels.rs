use crate::engine::Engine;
use crate::engine::error::TargetNotFoundError;
use crate::engine::request_state::RequestState;
use enclose::enclose;
use futures::TryStreamExt;
use hcore::hmemoizer::downcast_chain_ref;
use hmodel::htmatcher;
use std::collections::BTreeSet;
use std::sync::Arc;

impl Engine {
    /// Collect the unique set of labels declared by every target matching `m`.
    ///
    /// Enumerates the matching targets via `query` and folds each target's spec
    /// `labels` into a sorted `BTreeSet`. Specs are resolved off the query stream
    /// with a bounded in-flight set, so only the label set — never the full spec
    /// list — is held in memory.
    pub async fn labels(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &htmatcher::Matcher,
    ) -> anyhow::Result<BTreeSet<String>> {
        // Cap in-flight spec resolutions; the engine's own semaphores gate the
        // real work, this just bounds the orchestration set held off the stream.
        let concurrency = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .saturating_mul(2);

        let specs = Arc::clone(&self)
            .query(rs.clone(), m)
            .map_ok(move |addr| {
                enclose!((self => engine, rs) async move {
                    // A provider may `list` an addr it cannot `get` standalone —
                    // `list` is a *candidate* set, and go's is deliberately
                    // over-broad (the target set of a Go package is only known
                    // once `go list` has run, so the filtering happens at `get`).
                    // Such a target carries no labels, so skip it on a self-addr
                    // NotFound, the way the query resolver, `validate`, `revdeps`
                    // and the gitignore walk already do. A NotFound for a
                    // *different* addr — a dep of this one — is a real breakage
                    // and still propagates.
                    match engine.get_spec(rs, &addr).await {
                        Ok(spec) => Ok(Some(spec)),
                        Err(e)
                            if downcast_chain_ref::<TargetNotFoundError>(&e)
                                .is_some_and(|nf| nf.addr == addr) =>
                        {
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }
                })
            })
            .try_buffer_unordered(concurrency);
        tokio::pin!(specs);

        // Fold labels in as each spec resolves; the spec is dropped immediately.
        let mut labels = BTreeSet::new();
        while let Some(spec) = specs.try_next().await? {
            let Some(spec) = spec else { continue };
            for label in &spec.labels {
                labels.insert(label.clone());
            }
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
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn collects_unique_labels_across_matched_targets() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &["//labels:lint", "//labels:test"]),
            target("foo/bar", "b", &["//labels:lint"]),
            target("foo/baz", "c", &["//labels:fmt"]),
        ])?;

        let rs = engine.new_state();
        let labels: Vec<String> = engine
            .labels(rs, &Matcher::PackagePrefix(PkgBuf::from("foo")))
            .await?
            .into_iter()
            .collect();

        // Sorted, deduped: lint appears on two targets but once here.
        assert_eq!(
            labels,
            vec![
                "//labels:fmt".to_string(),
                "//labels:lint".to_string(),
                "//labels:test".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn scopes_labels_to_the_matcher() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &["//labels:lint"]),
            target("other", "b", &["//labels:fmt"]),
        ])?;

        let rs = engine.new_state();
        let labels: Vec<String> = engine
            .labels(rs, &Matcher::Package(PkgBuf::from("foo")))
            .await?
            .into_iter()
            .collect();

        assert_eq!(labels, vec!["//labels:lint".to_string()]);
        Ok(())
    }

    /// A provider that lists one addr per package and can `get` none of them —
    /// the shape every real provider has some corner of (go's `list` is a
    /// candidate set narrowed at `get` time by what `go list` reports).
    struct PhantomLister;

    impl crate::engine::provider::Provider for PhantomLister {
        fn config(
            &self,
            _req: crate::engine::provider::ConfigRequest,
        ) -> anyhow::Result<crate::engine::provider::ConfigResponse> {
            Ok(crate::engine::provider::ConfigResponse {
                name: "phantom".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: crate::engine::provider::ListRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<Item = anyhow::Result<crate::engine::provider::ListResponse>>
                        + Send,
                >,
            >,
        > {
            let addr = hmodel::htaddr::Addr::new(
                req.package.clone(),
                "phantom".to_string(),
                Default::default(),
            );
            Box::pin(async move {
                let items = vec![Ok(crate::engine::provider::ListResponse { addr })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: crate::engine::provider::ListPackagesRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            anyhow::Result<
                Box<
                    dyn Iterator<
                            Item = anyhow::Result<crate::engine::provider::ListPackageResponse>,
                        > + Send,
                >,
            >,
        > {
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: crate::engine::provider::GetRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::engine::provider::GetResponse, crate::engine::provider::GetError>,
        > {
            Box::pin(async { Err(crate::engine::provider::GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: crate::engine::provider::ProbeRequest,
            _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::engine::provider::ProbeResponse>>
        {
            Box::pin(async { Ok(crate::engine::provider::ProbeResponse { states: vec![] }) })
        }
    }

    /// `list` is a candidate set: a provider may advertise an addr it cannot
    /// resolve standalone. Reporting labels is a whole-graph walk, so one such
    /// candidate used to abort it with `target not found` — the labels of every
    /// real target lost to a phantom sibling. Skip it, like the query resolver,
    /// `validate`, `revdeps` and the gitignore walk already do.
    #[tokio::test]
    async fn skips_listed_addrs_that_cannot_be_resolved() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let provider =
            pluginstatictarget::Provider::new(vec![target("foo", "a", &["//labels:lint"])])?;
        engine.register_provider(move |_| Box::new(provider))?;
        engine.register_provider(|_| Box::new(PhantomLister))?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();
        let labels: Vec<String> = engine
            .labels(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .await?
            .into_iter()
            .collect();

        assert_eq!(labels, vec!["//labels:lint".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn no_matches_yields_no_labels() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[])])?;

        let rs = engine.new_state();
        let labels = engine
            .labels(rs, &Matcher::Package(PkgBuf::from("foo")))
            .await?;

        assert!(labels.is_empty());
        Ok(())
    }
}
