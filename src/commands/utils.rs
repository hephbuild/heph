use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::{engine, htaddr, htpkg, htquery};
use anyhow::Context;

/// Resolve the target selection for the `run`/`query` commands. Exactly one of
/// the query form (`-q '<expr>'`) or the positional form (`<addr>` /
/// `<label> <package>`) must be supplied; `-e/--exclude` applies to either.
pub fn resolve_matcher(
    query: &Option<String>,
    arg1: &Option<String>,
    arg2: &Option<String>,
    excludes: &[String],
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    if let Some(q) = query {
        if arg1.is_some() {
            anyhow::bail!(
                "cannot combine -q/--query with positional TARGET arguments; use one or the other"
            );
        }
        let positive =
            htquery::parse(q, base_pkg).with_context(|| format!("parsing query {q:?}"))?;
        return apply_excludes(positive, excludes, base_pkg);
    }

    let arg1 = arg1.as_ref().ok_or_else(|| {
        anyhow::anyhow!("missing TARGET_ADDRESS/LABEL argument (or pass a query with -q '<expr>')")
    })?;
    matcher_from_args(arg1, arg2, excludes, base_pkg, allow_all)
}

pub fn matcher_from_args(
    arg1: &str,
    arg2: &Option<String>,
    excludes: &[String],
    base_pkg: &PkgBuf,
    allow_all: bool,
) -> anyhow::Result<Matcher> {
    let positive = if let Some(package_matcher) = &arg2 {
        let label = arg1;

        if label == "all" {
            if !allow_all {
                anyhow::bail!("label `all` not allowed")
            }

            htpkg::parse(package_matcher, &engine::get_cwp()?)?
        } else {
            Matcher::And(vec![
                Matcher::Label(label.into()),
                htpkg::parse(package_matcher, &engine::get_cwp()?)?,
            ])
        }
    } else {
        let addr_str = arg1;
        let addr = htaddr::parse_addr_with_base(addr_str, base_pkg)
            .with_context(|| format!("parse {}", addr_str))?;
        Matcher::Addr(addr)
    };

    apply_excludes(positive, excludes, base_pkg)
}

/// Wrap `positive` in `And([positive, !Addr(ex), …])` for each `-e` exclusion.
fn apply_excludes(
    positive: Matcher,
    excludes: &[String],
    base_pkg: &PkgBuf,
) -> anyhow::Result<Matcher> {
    if excludes.is_empty() {
        return Ok(positive);
    }

    let mut and = Vec::with_capacity(1 + excludes.len());
    and.push(positive);
    for ex in excludes {
        let addr = htaddr::parse_addr_with_base(ex, base_pkg)
            .with_context(|| format!("parse exclude {}", ex))?;
        and.push(Matcher::Not(Box::new(Matcher::Addr(addr))));
    }
    Ok(Matcher::And(and))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htmatcher::Matcher;

    #[test]
    fn no_excludes_returns_positive_unchanged() {
        let pkg = PkgBuf::from("");
        let m = matcher_from_args("//foo:bar", &None, &[], &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn colon_name_resolves_against_base_pkg() {
        let pkg = PkgBuf::from("foo/bar");
        let m = matcher_from_args(":build", &None, &[], &pkg, false).unwrap();
        match m {
            Matcher::Addr(addr) => {
                assert_eq!(addr.package.as_str(), "foo/bar");
                assert_eq!(addr.name, "build");
            }
            other => panic!("expected Addr, got {other:?}"),
        }
    }

    #[test]
    fn one_exclude_wraps_in_and_with_not() {
        let pkg = PkgBuf::from("");
        let m =
            matcher_from_args("//foo:bar", &None, &["//foo:baz".to_string()], &pkg, false).unwrap();
        match m {
            Matcher::And(children) => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[0], Matcher::Addr(_)));
                assert!(matches!(children[1], Matcher::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn multiple_excludes_each_become_not() {
        let pkg = PkgBuf::from("");
        let m = matcher_from_args(
            "//foo:bar",
            &None,
            &["//foo:a".to_string(), "//foo:b".to_string()],
            &pkg,
            false,
        )
        .unwrap();
        match m {
            Matcher::And(children) => {
                assert_eq!(children.len(), 3);
                assert!(matches!(children[1], Matcher::Not(_)));
                assert!(matches!(children[2], Matcher::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn invalid_exclude_address_surfaces_context() {
        let pkg = PkgBuf::from("");
        let err = matcher_from_args(
            "//foo:bar",
            &None,
            &["not an address".to_string()],
            &pkg,
            false,
        )
        .err()
        .expect("expected parse error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("parse exclude"),
            "expected 'parse exclude' in error chain: {chain}"
        );
    }
}
