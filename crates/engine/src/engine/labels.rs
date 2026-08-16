use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use futures::TryStreamExt;
use hmodel::htmatcher;
use std::collections::BTreeSet;
use std::sync::Arc;

impl Engine {
    /// Collect the unique set of labels declared by every target matching `m`.
    ///
    /// Enumerates the matching targets via `query_spec` — so a listed candidate
    /// that doesn't resolve is skipped rather than fatal — and folds each spec's
    /// `labels` into a sorted `BTreeSet`. Specs arrive off a bounded in-flight
    /// set and are dropped as they are folded, so only the label set — never the
    /// full spec list — is held in memory.
    pub async fn labels(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &htmatcher::Matcher,
    ) -> anyhow::Result<BTreeSet<String>> {
        let specs = self.query_spec(rs, m);
        tokio::pin!(specs);

        let mut labels = BTreeSet::new();
        while let Some(spec) = specs.try_next().await? {
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
