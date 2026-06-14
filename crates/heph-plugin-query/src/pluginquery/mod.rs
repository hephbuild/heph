use futures::future::BoxFuture;
use heph_core::hasync::Cancellable;
use heph_core::htvalue::Value;
use heph_model::htmatcher::Matcher;
use heph_model::htpkg::PkgBuf;
use heph_plugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, TargetSpec,
};
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

pub fn build_matcher(args: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Matcher> {
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
pub fn parse_exclude_providers(args: &std::collections::BTreeMap<String, String>) -> Vec<String> {
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
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
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

            let deps: Vec<Value> = addrs
                .into_iter()
                .map(|addr| Value::String(addr.format()))
                .collect();

            let config = HashMap::from([("deps".to_string(), Value::List(deps))]);

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: heph_builtins::plugingroup::DRIVER_NAME.to_string(),
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
