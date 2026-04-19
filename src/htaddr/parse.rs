use std::collections::HashMap;
use pest::Parser;
use pest_derive::Parser;
use crate::htaddr::addr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Parser)]
#[grammar = "htaddr/taddr.pest"]
struct TAddrParser;

pub fn parse_addr(input: &str) -> Result<Addr, String> {
    let pairs = TAddrParser::parse(Rule::taddr, input)
        .map_err(|e| format!("Parsing error: {}", e))?;

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
