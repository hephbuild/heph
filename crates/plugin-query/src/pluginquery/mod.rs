use anyhow::Context;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hmodel::htquery;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, TargetSpec,
};
use std::collections::{BTreeMap, HashMap};

pub const PACKAGE: &str = "@heph/query";

/// Addr arg holding a query-language expression (see [`htquery`]).
pub const EXPR_ARG: &str = "expr";

/// Addr arg holding the package an `expr` query's relative patterns
/// (`./x`, `..`, `.`) resolve against. `_`-prefixed so it is treated as an
/// addr-identity-only field, never a matcher arg.
pub const BASE_ARG: &str = "_base";

/// Addr arg naming providers to skip when resolving the query (`;`-separated).
pub const EXCLUDE_PROVIDER_ARG: &str = "exclude_provider";

/// `exclude_provider` sentinel that opts OUT of the engine's automatic
/// exclusion of the requesting target's own provider, letting a query enumerate
/// sibling targets from that same provider — e.g. a BUILD-file `query()` that
/// must see other BUILD-file targets. No real provider is named this, so it
/// excludes nothing.
pub const NO_PROVIDER_EXCLUSION: &str = "__none__";

/// Compose the `@heph/query` address for a query-language expression, mirroring
/// [`hbuiltins::pluginfs::file_addr`]/`glob_addr`. `base` is the package that
/// relative patterns in `expr` resolve against (pass `""` for root).
/// `exclude_providers` names providers the resolution skips (pass `&[]` for the
/// engine's default behaviour, or `&[NO_PROVIDER_EXCLUSION]` to opt out of the
/// auto-exclusion). Resolves to a group of every matching target.
pub fn query_addr(expr: &str, base: &str, exclude_providers: &[&str]) -> Addr {
    let mut args = BTreeMap::from([(EXPR_ARG.to_string(), expr.to_string())]);
    if !base.is_empty() {
        args.insert(BASE_ARG.to_string(), base.to_string());
    }
    if !exclude_providers.is_empty() {
        args.insert(
            EXCLUDE_PROVIDER_ARG.to_string(),
            exclude_providers.join(";"),
        );
    }
    Addr::new(PkgBuf::from(PACKAGE), "query".to_string(), args)
}

/// Returns `true` if `addr` refers to a query target.
pub fn is_query_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PACKAGE
}

pub struct Provider;

/// Args that don't contribute to matching but are still valid on a query addr.
/// `_origin` and `exclude_provider` are both stamped by the engine's
/// `rewrite_query_inputs` (the former gives each dest its own mem_spec cell;
/// the latter is consumed below to limit which providers the engine iterates).
/// `_`-prefixed keys are also ignored as a convention for addr-identity-only
/// fields.
const RESERVED_ARG_KEYS: &[&str] = &[EXCLUDE_PROVIDER_ARG];

fn is_reserved_key(k: &str) -> bool {
    k.starts_with('_') || RESERVED_ARG_KEYS.contains(&k)
}

pub fn build_matcher(args: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Matcher> {
    let Some(expr) = args.get(EXPR_ARG) else {
        // Sanity-check the args: an arg with no recognised key + no reserved
        // key is a typo we should surface, not silently match everything.
        let unknown: Vec<&String> = args.keys().filter(|k| !is_reserved_key(k)).collect();
        if !unknown.is_empty() {
            anyhow::bail!("query target has unknown args: {:?}", unknown);
        }
        anyhow::bail!("query target requires an `expr` arg (the query-language expression)");
    };

    let base = args.get(BASE_ARG).map(String::as_str).unwrap_or("");
    htquery::parse(expr, &PkgBuf::from(base))
        .with_context(|| format!("parsing query expr {expr:?}"))
}

/// Parse `exclude_provider=a;b;c` into a `Vec<String>`. Empty when the arg is
/// absent. Empty fragments (e.g. `a;;b`) are dropped.
pub fn parse_exclude_providers(args: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    args.get(EXCLUDE_PROVIDER_ARG)
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
                    driver: hbuiltins::plugingroup::DRIVER_NAME.to_string(),
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

    #[test]
    fn query_addr_roundtrips_expr_and_base() {
        let addr = query_addr("//foo/... && label(test)", "cwd/pkg", &[]);
        assert_eq!(addr.package.as_str(), PACKAGE);
        assert_eq!(addr.name, "query");
        assert_eq!(addr.args.get(EXPR_ARG).unwrap(), "//foo/... && label(test)");
        assert_eq!(addr.args.get(BASE_ARG).unwrap(), "cwd/pkg");
        assert!(is_query_addr(&addr));
    }

    #[test]
    fn query_addr_omits_empty_base() {
        let addr = query_addr("//foo", "", &[]);
        assert!(!addr.args.contains_key(BASE_ARG));
        assert!(!addr.args.contains_key(EXCLUDE_PROVIDER_ARG));
    }

    #[test]
    fn query_addr_encodes_exclude_providers() {
        let addr = query_addr("//foo", "", &[NO_PROVIDER_EXCLUSION]);
        assert_eq!(
            addr.args.get(EXCLUDE_PROVIDER_ARG).unwrap(),
            NO_PROVIDER_EXCLUSION
        );
        // The sentinel names no real provider, so nothing is excluded.
        assert_eq!(
            parse_exclude_providers(&addr.args),
            vec![NO_PROVIDER_EXCLUSION]
        );
    }

    #[test]
    fn build_matcher_parses_expr() {
        let addr = query_addr("//foo/... && !//foo/vendor/...", "", &[]);
        let m = build_matcher(&addr.args).unwrap();
        match m {
            Matcher::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], Matcher::PackagePrefix(_)));
                assert!(matches!(children[1], Matcher::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn build_matcher_resolves_expr_relative_to_base() {
        let addr = query_addr("./sub", "cwd/pkg", &[]);
        let m = build_matcher(&addr.args).unwrap();
        assert_eq!(m, Matcher::Package(PkgBuf::from("cwd/pkg/sub")));
    }

    #[test]
    fn build_matcher_requires_expr() {
        let err = build_matcher(&std::collections::BTreeMap::new())
            .err()
            .expect("expected error");
        assert!(format!("{err:#}").contains("requires an `expr`"));
    }

    #[test]
    fn build_matcher_rejects_legacy_keys() {
        // Legacy discrete keys are gone — they read as unknown args now.
        let args = std::collections::BTreeMap::from([("label".to_string(), "x".to_string())]);
        let err = build_matcher(&args).err().expect("expected error");
        assert!(format!("{err:#}").contains("unknown args"));
    }

    #[test]
    fn build_matcher_surfaces_bad_expr() {
        let addr = query_addr("bogus(x)", "", &[]);
        let err = build_matcher(&addr.args)
            .err()
            .expect("expected parse error");
        assert!(format!("{err:#}").contains("parsing query expr"));
    }
}
