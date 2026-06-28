use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::{engine, htaddr, htpkg, htquery};
use anyhow::Context;

/// Resolve the target selection for the `run`/`query` commands. Exactly one of
/// the query form (`-e '<expr>'`) or the positional form (`<addr>` /
/// `<label> <package>`) must be supplied. Exclusion is expressed inside the
/// query with the `!` operator (e.g. `-e '//... && !//vendor/...'`).
pub fn resolve_matcher(
    query: &Option<String>,
    arg1: &Option<String>,
    arg2: &Option<String>,
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    if let Some(q) = query {
        if arg1.is_some() {
            anyhow::bail!(
                "cannot combine -e/--query with positional TARGET arguments; use one or the other"
            );
        }
        let matcher =
            htquery::parse(q, base_pkg).with_context(|| format!("parsing query {q:?}"))?;
        // Record the parsed matcher's shape for telemetry from the one place the
        // real parser actually ran — counts only, never the expression text.
        let mut counts = htelemetry::telemetry::QueryExprCounts::default();
        count_matcher(&matcher, &mut counts);
        htelemetry::telemetry::record_query_expr(&counts);
        return Ok(matcher);
    }

    let arg1 = arg1.as_ref().ok_or_else(|| {
        anyhow::anyhow!("missing TARGET_ADDRESS/LABEL argument (or pass a query with -e '<expr>')")
    })?;
    matcher_from_args(arg1, arg2, base_pkg, allow_all)
}

pub fn matcher_from_args(
    arg1: &str,
    arg2: &Option<String>,
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    if let Some(package_matcher) = &arg2 {
        let label = arg1;

        if label == "all" {
            if !allow_all {
                anyhow::bail!("label `all` not allowed")
            }

            htpkg::parse(package_matcher, &engine::get_cwp()?)
        } else {
            Ok(Matcher::And(vec![
                Matcher::Label(label.into()),
                htpkg::parse(package_matcher, &engine::get_cwp()?)?,
            ]))
        }
    } else {
        let addr_str = arg1;
        let addr = htaddr::parse_addr_with_base(addr_str, base_pkg)
            .with_context(|| format!("parse {}", addr_str))?;
        Ok(Matcher::Addr(addr))
    }
}

/// Tally a parsed query matcher's nodes into telemetry counts. Walks the tree
/// the real parser produced, so the syntax is never re-interpreted; counts only.
fn count_matcher(m: &Matcher, c: &mut htelemetry::telemetry::QueryExprCounts) {
    match m {
        Matcher::Addr(_) => c.addr += 1,
        Matcher::Label(_) => c.label += 1,
        Matcher::Package(_) => c.package += 1,
        Matcher::PackagePrefix(_) => c.package_prefix += 1,
        Matcher::TreeOutputTo(_) => c.tree_output += 1,
        Matcher::Or(terms) => {
            c.or += 1;
            terms.iter().for_each(|t| count_matcher(t, c));
        }
        Matcher::And(terms) => {
            c.and += 1;
            terms.iter().for_each(|t| count_matcher(t, c));
        }
        Matcher::Not(inner) => {
            c.not += 1;
            count_matcher(inner, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htmatcher::Matcher;

    #[test]
    fn count_matcher_tallies_node_kinds() {
        let pkg = PkgBuf::from("");
        let m = htquery::parse("//a/... && label(foo) && !label(bar)", &pkg).expect("parse");
        let mut c = htelemetry::telemetry::QueryExprCounts::default();
        count_matcher(&m, &mut c);
        assert_eq!(c.label, 2);
        assert_eq!(c.not, 1);
        assert_eq!(c.and, 1, "the && chain is one And node");
        assert_eq!(c.package_prefix, 1, "`//a/...` is a package prefix");
        assert_eq!(c.addr, 0);
    }

    #[test]
    fn positional_addr_returns_addr() {
        let pkg = PkgBuf::from("");
        let m = matcher_from_args("//foo:bar", &None, &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn colon_name_resolves_against_base_pkg() {
        let pkg = PkgBuf::from("foo/bar");
        let m = matcher_from_args(":build", &None, &pkg, false).unwrap();
        match m {
            Matcher::Addr(addr) => {
                assert_eq!(addr.package.as_str(), "foo/bar");
                assert_eq!(addr.name, "build");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn resolve_matcher_uses_query_when_present() {
        let pkg = PkgBuf::from("");
        let q = Some("//foo/... && !//foo/vendor/...".to_string());
        let m = resolve_matcher(&q, &None, &None, &pkg, false).unwrap();
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
    fn resolve_matcher_falls_back_to_positional() {
        let pkg = PkgBuf::from("");
        let m = resolve_matcher(&None, &Some("//foo:bar".to_string()), &None, &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn resolve_matcher_rejects_query_with_positional() {
        let pkg = PkgBuf::from("");
        let err = resolve_matcher(
            &Some("//foo".to_string()),
            &Some("//foo:bar".to_string()),
            &None,
            &pkg,
            false,
        )
        .err()
        .expect("expected conflict error");
        assert!(
            format!("{err:#}").contains("cannot combine"),
            "expected conflict message: {err:#}"
        );
    }

    #[test]
    fn resolve_matcher_requires_some_selection() {
        let pkg = PkgBuf::from("");
        assert!(resolve_matcher(&None, &None, &None, &pkg, false).is_err());
    }

    #[test]
    fn invalid_query_surfaces_context() {
        let pkg = PkgBuf::from("");
        let err = resolve_matcher(&Some("bogus(x)".to_string()), &None, &None, &pkg, false)
            .err()
            .expect("expected parse error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("parsing query"),
            "expected 'parsing query' in error chain: {chain}"
        );
    }
}
