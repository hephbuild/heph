#[cfg(test)]
mod tests {
    use crate::engine::{Config, Engine};
    use crate::htaddr::parse_addr;
    use crate::htmatcher::Matcher;
    use crate::htvalue::Value;
    use crate::pluginquery::*;
    use crate::pluginstatictarget;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn labeled_target(pkg: &str, name: &str, labels: &[&str]) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: format!("//{pkg}:{name}"),
            driver: "exec".to_string(),
            run: None,
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
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
    async fn query_by_label_returns_group_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            labeled_target("foo", "a", &["//labels:lint"]),
            labeled_target("foo", "b", &[]),
            labeled_target("bar", "c", &["//labels:lint"]),
        ])?;

        let rs = engine.new_state();
        let addr = parse_addr(&format!("//{PACKAGE}:q@label=//labels:lint"))?;
        let spec = engine.get_spec(rs, &addr).await?;

        assert_eq!(spec.driver, crate::plugingroup::DRIVER_NAME);
        let deps = match spec.config.get("deps") {
            Some(Value::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 2);
        let dep_strs: Vec<&str> = deps
            .iter()
            .map(|v| match v {
                Value::String(s) => s.as_str(),
                _ => panic!("expected string"),
            })
            .collect();
        assert!(dep_strs.contains(&"//foo:a"));
        assert!(dep_strs.contains(&"//bar:c"));
        Ok(())
    }

    #[tokio::test]
    async fn query_by_package_returns_group_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            labeled_target("pkg/a", "x", &[]),
            labeled_target("pkg/a", "y", &[]),
            labeled_target("other", "z", &[]),
        ])?;

        let rs = engine.new_state();
        let addr = parse_addr(&format!("//{PACKAGE}:q@package=pkg/a"))?;
        let spec = engine.get_spec(rs, &addr).await?;

        assert_eq!(spec.driver, crate::plugingroup::DRIVER_NAME);
        let deps = match spec.config.get("deps") {
            Some(Value::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn query_by_package_prefix_returns_group_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            labeled_target("src/foo/a", "x", &[]),
            labeled_target("src/foo/b", "y", &[]),
            labeled_target("src/bar", "z", &[]),
        ])?;

        let rs = engine.new_state();
        let addr = parse_addr(&format!("//{PACKAGE}:q@package_prefix=src/foo"))?;
        let spec = engine.get_spec(rs, &addr).await?;

        assert_eq!(spec.driver, crate::plugingroup::DRIVER_NAME);
        let deps = match spec.config.get("deps") {
            Some(Value::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn query_multiple_matchers_combined_as_and() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            labeled_target("src/foo", "a", &["//labels:lint"]),
            labeled_target("src/foo", "b", &[]),
            labeled_target("other", "c", &["//labels:lint"]),
        ])?;

        let rs = engine.new_state();
        let addr = parse_addr(&format!(
            "//{PACKAGE}:q@package=src/foo,label=//labels:lint"
        ))?;
        let spec = engine.get_spec(rs, &addr).await?;

        let deps = match spec.config.get("deps") {
            Some(Value::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 1);
        assert!(matches!(&deps[0], Value::String(s) if s == "//src/foo:a"));
        Ok(())
    }

    #[tokio::test]
    async fn query_empty_matcher_args_errors() {
        let result = build_matcher(&Default::default());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("at least one matcher arg")
        );
    }

    #[tokio::test]
    async fn query_non_query_package_returns_not_found() -> anyhow::Result<()> {
        let engine = make_engine(vec![])?;
        let rs = engine.new_state();
        let addr = parse_addr("//some/other:target")?;
        let result = engine.get_spec(rs, &addr).await;
        assert!(result.is_err());
        Ok(())
    }

    // ---- exclude_provider arg handling ----

    #[test]
    fn parse_exclude_providers_empty_when_arg_absent() {
        let args = std::collections::BTreeMap::new();
        assert!(parse_exclude_providers(&args).is_empty());
    }

    #[test]
    fn parse_exclude_providers_single() {
        let args =
            std::collections::BTreeMap::from([("exclude_provider".to_string(), "go".to_string())]);
        assert_eq!(parse_exclude_providers(&args), vec!["go".to_string()]);
    }

    #[test]
    fn parse_exclude_providers_multi_semicolon_separated() {
        let args = std::collections::BTreeMap::from([(
            "exclude_provider".to_string(),
            "go;buildfile".to_string(),
        )]);
        assert_eq!(
            parse_exclude_providers(&args),
            vec!["go".to_string(), "buildfile".to_string()]
        );
    }

    #[test]
    fn parse_exclude_providers_drops_empty_fragments() {
        let args = std::collections::BTreeMap::from([(
            "exclude_provider".to_string(),
            ";;a;;".to_string(),
        )]);
        assert_eq!(parse_exclude_providers(&args), vec!["a".to_string()]);
    }

    #[test]
    fn build_matcher_ignores_exclude_provider() {
        // exclude_provider is a reserved key and must not contribute to matching.
        let args = std::collections::BTreeMap::from([
            ("label".to_string(), "x".to_string()),
            ("exclude_provider".to_string(), "go".to_string()),
        ]);
        let m = build_matcher(&args).expect("matcher should build");
        // Single matcher arm — Label only.
        assert!(matches!(m, Matcher::Label(_)));
    }

    #[test]
    fn build_matcher_rejects_unknown_keys() {
        let args =
            std::collections::BTreeMap::from([("totally_unknown".to_string(), "x".to_string())]);
        let result = build_matcher(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown args"));
    }

    #[tokio::test]
    async fn query_excludes_named_provider_via_exclude_arg() -> anyhow::Result<()> {
        // Single provider registered: pluginstatictarget. exclude_provider=
        // pluginstatictarget should yield an empty result.
        let engine = make_engine(vec![
            labeled_target("foo", "a", &["//labels:lint"]),
            labeled_target("foo", "b", &["//labels:lint"]),
        ])?;

        let rs = engine.new_state();
        let addr = parse_addr(&format!(
            "//{PACKAGE}:q@label=//labels:lint,exclude_provider=pluginstatictarget"
        ))?;
        let spec = engine.get_spec(rs, &addr).await?;

        let deps = match spec.config.get("deps") {
            Some(Value::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(
            deps.len(),
            0,
            "excluding the only producing provider must yield zero deps"
        );
        Ok(())
    }
}
