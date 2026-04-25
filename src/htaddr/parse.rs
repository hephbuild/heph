use std::collections::HashMap;
use pest::Parser;
use pest_derive::Parser;
use crate::htaddr::addr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Parser)]
#[grammar = "htaddr/taddr.pest"]
struct TAddrParser;

fn resolve_relative_pkg(base: &PkgBuf, rel: &str) -> anyhow::Result<String> {
    let mut components: Vec<&str> = base.as_str().split('/').filter(|s| !s.is_empty()).collect();
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    for component in rel.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(anyhow::anyhow!("relative path '{}' escapes root", rel));
                }
            }
            c => components.push(c),
        }
    }
    Ok(components.join("/"))
}

pub fn parse_addr_with_base(input: &str, base: &PkgBuf) -> anyhow::Result<Addr> {
    if input.starts_with("//") {
        return parse_addr(input);
    }

    // ":name" or ":name@args"
    if input.starts_with(':') {
        return parse_addr(&format!("//{}{}", base, input));
    }

    // "./path" or "../path", optionally followed by ":name" and/or "@args"
    if input.starts_with("./") || input.starts_with("../") {
        let colon_pos = input.find(':');
        let at_pos = input.find('@');
        // path ends at the first ':' or '@'
        let path_end = match (colon_pos, at_pos) {
            (Some(c), Some(a)) => c.min(a),
            (Some(c), None) => c,
            (None, Some(a)) => a,
            (None, None) => input.len(),
        };
        let path_part = &input[..path_end];
        let rest = &input[path_end..];
        let pkg = resolve_relative_pkg(base, path_part)?;
        let full = if rest.starts_with(':') || rest.is_empty() {
            if rest.is_empty() {
                let name = pkg.rsplit('/').next().unwrap_or("");
                if name.is_empty() {
                    return Err(anyhow::anyhow!("cannot derive name from path '{}'", path_part));
                }
                format!("//{}:{}", pkg, name)
            } else {
                format!("//{}{}", pkg, rest)
            }
        } else {
            // rest starts with '@', no explicit name — derive from pkg
            let name = pkg.rsplit('/').next().unwrap_or("");
            if name.is_empty() {
                return Err(anyhow::anyhow!("cannot derive name from path '{}'", path_part));
            }
            format!("//{}:{}{}", pkg, name, rest)
        };
        return parse_addr(&full);
    }

    // bare "name" or "name@args" — no slashes, colons, or leading dots
    parse_addr(&format!("//{}:{}", base, input))
}

pub fn parse_addr(input: &str) -> anyhow::Result<Addr> {
    let pairs = TAddrParser::parse(Rule::taddr, input)?;

    let mut package = PkgBuf::from("");
    let mut name = String::new();
    let mut args = HashMap::new();

    for pair in pairs {
        if pair.as_rule() == Rule::taddr {
            for inner_pair in pair.into_inner() {
                match inner_pair.as_rule() {
                    Rule::package_ident => {
                        package = PkgBuf::from(inner_pair.as_str());
                    }
                    Rule::name_ident => {
                        name = inner_pair.as_str().to_string();
                    }
                    Rule::arg => {
                        let mut key = String::new();
                        let mut value = String::new();
                        for arg_inner in inner_pair.into_inner() {
                            match arg_inner.as_rule() {
                                Rule::key => {
                                    key = arg_inner.as_str().to_string();
                                }
                                Rule::value => {
                                    let inner_val = arg_inner.into_inner().next().unwrap();
                                    value = match inner_val.as_rule() {
                                        Rule::quoted_string => {
                                            inner_val.into_inner().find(|p| p.as_rule() == Rule::string_content)
                                                .map(|p| p.as_str().to_string())
                                                .unwrap_or_default()
                                        }
                                        Rule::bare_value => inner_val.as_str().to_string(),
                                        _ => String::new(),
                                    };
                                }
                                _ => {}
                            }
                        }
                        args.insert(key, value);
                    }
                    Rule::args => {
                        for arg_pair in inner_pair.into_inner() {
                            if arg_pair.as_rule() == Rule::arg {
                                let mut key = String::new();
                                let mut value = String::new();
                                for arg_inner in arg_pair.into_inner() {
                                    match arg_inner.as_rule() {
                                        Rule::key => {
                                            key = arg_inner.as_str().to_string();
                                        }
                                        Rule::value => {
                                            let inner_val = arg_inner.into_inner().next().unwrap();
                                            value = match inner_val.as_rule() {
                                                Rule::quoted_string => {
                                                    inner_val.into_inner().find(|p| p.as_rule() == Rule::string_content)
                                                        .map(|p| p.as_str().to_string())
                                                        .unwrap_or_default()
                                                }
                                                Rule::bare_value => inner_val.as_str().to_string(),
                                                _ => String::new(),
                                            };
                                        }
                                        _ => {}
                                    }
                                }
                                args.insert(key, value);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(Addr {
        package,
        name,
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_taddr_simple() {
        let input = "//pkg:name";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.package, "pkg");
        assert_eq!(res.name, "name");
        assert!(res.args.is_empty());
    }

    #[test]
    fn test_parse_taddr_args() {
        let input = "//pkg:name@a=b,c=\"d e\"";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.package, "pkg");
        assert_eq!(res.name, "name");
        assert_eq!(res.args.get("a").unwrap(), "b");
        assert_eq!(res.args.get("c").unwrap(), "d e");
    }

    #[test]
    fn test_parse_taddr_no_value() {
        let input = "//pkg:name@a";
        let res = parse_addr(input).unwrap();
        assert_eq!(res.args.get("a").unwrap(), "");
    }

    #[test]
    fn test_parse_addr_with_base_absolute() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("//other:name", &base).unwrap();
        assert_eq!(res.package, "other");
        assert_eq!(res.name, "name");
    }

    #[test]
    fn test_parse_addr_with_base_colon_name() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base(":mytarget", &base).unwrap();
        assert_eq!(res.package, "base/pkg");
        assert_eq!(res.name, "mytarget");
    }

    #[test]
    fn test_parse_addr_with_base_bare_name() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("mytarget", &base).unwrap();
        assert_eq!(res.package, "base/pkg");
        assert_eq!(res.name, "mytarget");
    }

    #[test]
    fn test_parse_addr_with_base_bare_name_args() {
        let base = PkgBuf::from("base/pkg");
        let res = parse_addr_with_base("mytarget@k=v", &base).unwrap();
        assert_eq!(res.package, "base/pkg");
        assert_eq!(res.name, "mytarget");
        assert_eq!(res.args.get("k").unwrap(), "v");
    }

    #[test]
    fn test_parse_addr_with_base_dot_slash() {
        let base = PkgBuf::from("a/b");
        let res = parse_addr_with_base("./sub", &base).unwrap();
        assert_eq!(res.package, "a/b/sub");
        assert_eq!(res.name, "sub");
    }

    #[test]
    fn test_parse_addr_with_base_dot_slash_explicit_name() {
        let base = PkgBuf::from("a/b");
        let res = parse_addr_with_base("./sub:other", &base).unwrap();
        assert_eq!(res.package, "a/b/sub");
        assert_eq!(res.name, "other");
    }

    #[test]
    fn test_parse_addr_with_base_dot_dot_slash() {
        let base = PkgBuf::from("a/b/c");
        let res = parse_addr_with_base("../sibling", &base).unwrap();
        assert_eq!(res.package, "a/b/sibling");
        assert_eq!(res.name, "sibling");
    }

    #[test]
    fn test_parse_addr_with_base_dot_dot_slash_args() {
        let base = PkgBuf::from("a/b/c");
        let res = parse_addr_with_base("../sibling@k=v", &base).unwrap();
        assert_eq!(res.package, "a/b/sibling");
        assert_eq!(res.name, "sibling");
        assert_eq!(res.args.get("k").unwrap(), "v");
    }

    #[test]
    fn test_parse_addr_with_base_escapes_root_fails() {
        let base = PkgBuf::from("a");
        let res = parse_addr_with_base("../../escape", &base);
        assert!(res.is_err());
    }

    #[test]
    fn test_parse_taddr_invalid() {
        let cases = vec![
            "pkg:name",           // Missing //
            "//pkg name",         // Missing :
            "//:name",            // Empty package name
            "//pkg:",             // Empty name ident
            "//pkg:name@a=\"val", // Unclosed quote
            "//pkg:name@a=b c",   // Unexpected space/trailing content
            "//pkg:name:",        // Unexpected space/trailing content
        ];

        for case in cases {
            let res = parse_addr(case);
            assert!(res.is_err(), "Expected error for case: {}", case);
        }
    }
}
