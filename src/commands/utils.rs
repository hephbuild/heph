use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::{engine, htaddr, htpkg};
use anyhow::Context;

pub fn matcher_from_args(
    arg1: &String,
    arg2: &Option<String>,
    excludes: &[String],
    _base_pkg: &PkgBuf,
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
        let addr = htaddr::parse_addr(addr_str).with_context(|| format!("parse {}", addr_str))?;
        Matcher::Addr(addr)
    };

    if excludes.is_empty() {
        return Ok(positive);
    }

    let mut and = Vec::with_capacity(1 + excludes.len());
    and.push(positive);
    for ex in excludes {
        let addr = htaddr::parse_addr(ex).with_context(|| format!("parse exclude {}", ex))?;
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
        let m = matcher_from_args(&"//foo:bar".to_string(), &None, &[], &pkg, false).unwrap();
        assert!(matches!(m, Matcher::Addr(_)));
    }

    #[test]
    fn one_exclude_wraps_in_and_with_not() {
        let pkg = PkgBuf::from("");
        let m = matcher_from_args(
            &"//foo:bar".to_string(),
            &None,
            &["//foo:baz".to_string()],
            &pkg,
            false,
        )
        .unwrap();
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
            &"//foo:bar".to_string(),
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
            &"//foo:bar".to_string(),
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
