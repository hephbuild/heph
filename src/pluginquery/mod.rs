use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::TargetSpecValue;
use futures::future::BoxFuture;
use std::collections::HashMap;

pub const PACKAGE: &str = "@heph/query";

pub struct Provider;

/// Args that don't contribute to matching but are still valid on a query addr.
/// `_origin` and `exclude_provider` are both stamped by the engine's
/// `rewrite_query_inputs` (the former gives each dest its own mem_spec cell;
/// the latter is consumed below to limit which providers the engine iterates).
/// `_`-prefixed keys are also ignored as a convention for addr-identity-only
/// fields.
const RESERVED_ARG_KEYS: &[&str] = &["exclude_provider"];

fn is_reserved_key(k: &str) -> bool {
    k.starts_with('_') || RESERVED_ARG_KEYS.contains(&k)
}

fn build_matcher(args: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Matcher> {
    let mut matchers: Vec<Matcher> = vec![];

    if let Some(label) = args.get("label") {
        matchers.push(Matcher::Label(label.clone()));
    }
    if let Some(pkg) = args.get("package") {
        matchers.push(Matcher::Package(PkgBuf::from(pkg.as_str())));
    }
    if let Some(prefix) = args.get("package_prefix") {
        matchers.push(Matcher::PackagePrefix(PkgBuf::from(prefix.as_str())));
    }
    if let Some(out) = args.get("tree_output_to") {
        matchers.push(Matcher::TreeOutputTo(PkgBuf::from(out.as_str())));
    }

    if matchers.is_empty() {
        // Sanity-check the args: an arg with no recognised key + no reserved
        // key is a typo we should surface, not silently match everything.
        let unknown: Vec<&String> = args.keys().filter(|k| !is_reserved_key(k)).collect();
        if !unknown.is_empty() {
            anyhow::bail!("query target has unknown args: {:?}", unknown);
        }
        anyhow::bail!(
            "query target requires at least one matcher arg (label, package, package_prefix, tree_output_to)"
        );
    }

    if matchers.len() == 1 {
        Ok(matchers.remove(0))
    } else {
        Ok(Matcher::And(matchers))
    }
}

/// Parse `exclude_provider=a;b;c` into a `Vec<String>`. Empty when the arg is
/// absent. Empty fragments (e.g. `a;;b`) are dropped.
fn parse_exclude_providers(args: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    args.get("exclude_provider")
        .map(|s| {
            s.split(';')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "query".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>
    {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>>,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            if req.addr.package.as_str() != PACKAGE {
                return Err(GetError::NotFound);
            }

            let matcher = build_matcher(&req.addr.args).map_err(GetError::Other)?;
            let exclude = parse_exclude_providers(&req.addr.args);

            let addrs = req
                .executor
                .query(&matcher, &exclude)
                .await
                .map_err(GetError::Other)?;

            let deps: Vec<TargetSpecValue> = addrs
                .into_iter()
                .map(|addr| TargetSpecValue::String(addr.format()))
                .collect();

            let config = HashMap::from([("deps".to_string(), TargetSpecValue::List(deps))]);

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: crate::plugingroup::DRIVER_NAME.to_string(),
                    config,
                    labels: vec![],
                    transitive: Default::default(),
                },
            })
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Config, Engine};
    use crate::htaddr::parse_addr;
    use crate::pluginstatictarget;
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
            Some(TargetSpecValue::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 2);
        let dep_strs: Vec<&str> = deps
            .iter()
            .map(|v| match v {
                TargetSpecValue::String(s) => s.as_str(),
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
            Some(TargetSpecValue::List(l)) => l,
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
            Some(TargetSpecValue::List(l)) => l,
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
            Some(TargetSpecValue::List(l)) => l,
            _ => panic!("expected deps list"),
        };
        assert_eq!(deps.len(), 1);
        assert!(matches!(&deps[0], TargetSpecValue::String(s) if s == "//src/foo:a"));
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
            Some(TargetSpecValue::List(l)) => l,
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
