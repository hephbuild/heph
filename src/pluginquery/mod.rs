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

// `_origin` is stamped on query input addrs by the engine's `rewrite_query_inputs`
// so each requesting target gets its own `mem_spec` cell. `build_matcher` reads
// no `_` prefixed keys — they exist purely to differentiate addr identity and
// are intentionally ignored here.
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
        matchers.push(Matcher::TreeOutputTo(out.clone()));
    }

    match matchers.len() {
        0 => anyhow::bail!(
            "query target requires at least one matcher arg (label, package, package_prefix, tree_output_to)"
        ),
        1 => Ok(matchers.remove(0)),
        _ => Ok(Matcher::And(matchers)),
    }
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

            let addrs = req
                .executor
                .query(&matcher)
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
}
