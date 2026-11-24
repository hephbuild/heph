use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod signature;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Bool(bool),
    Float(f64),
    Int(i64),
    Uint(u64),
    Null(),
    Map(HashMap<String, Value>),
    List(Vec<Value>),
}

pub fn parse_string(v: &Value) -> anyhow::Result<Option<String>> {
    match v {
        Value::Null() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        v => Err(anyhow::anyhow!("invalid: expected string, got: {:?}", v)),
    }
}

pub fn parse_strings(v: &Value) -> anyhow::Result<Vec<String>> {
    match v {
        Value::Null() => Ok(vec![]),
        Value::List(v) => v
            .iter()
            .try_fold(Vec::new(), |mut acc, v| match parse_string(v)? {
                None => Ok(acc),
                Some(s) => {
                    acc.push(s);

                    Ok(acc)
                }
            }),
        Value::String(s) => Ok(vec![s.clone()]),
        v => Err(anyhow::anyhow!(
            "invalid: expected string or [string], got: {:?}",
            v
        )),
    }
}

/// The error both lenient map parsers return for a value of the wrong shape.
fn map_mismatch(v: &Value) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {:?}",
        v
    )
}

pub fn parse_map_string_strings(v: &Value) -> anyhow::Result<HashMap<String, Vec<String>>> {
    match v {
        Value::Map(m) => m
            .iter()
            .map(|(k, v)| parse_strings(v).map(|ss| (k.clone(), ss)))
            .collect(),
        // A list with a bad element reports the whole value, not the element.
        Value::Null() | Value::String(_) | Value::List(_) => match parse_strings(v) {
            Ok(ss) => Ok(HashMap::from([(String::new(), ss)])),
            Err(_) => Err(map_mismatch(v)),
        },
        v => Err(map_mismatch(v)),
    }
}

/// A map whose values are themselves `{string: [string]}` maps — the shape of an
/// attribute that keys a grouped dep map by something else, e.g. `docker_build`'s
/// `context_by_platform`. Strict: unlike [`parse_map_string_strings`], there is
/// no bare-string or bare-list shorthand, because the outer key carries meaning
/// (which platform) that cannot be defaulted.
pub fn parse_map_string_map_string_strings(
    v: &Value,
) -> anyhow::Result<HashMap<String, HashMap<String, Vec<String>>>> {
    match v {
        Value::Map(m) => m
            .iter()
            .map(|(k, v)| {
                parse_map_string_strings(v)
                    .with_context(|| format!("key {k:?}"))
                    .map(|inner| (k.clone(), inner))
            })
            .collect(),
        v => Err(anyhow::anyhow!(
            "invalid: expected {{string: {{string: [string]}}}} got: {v:?}"
        )),
    }
}

pub fn parse_map_string_string(v: &Value) -> anyhow::Result<HashMap<String, String>> {
    match v {
        Value::Null() => Ok(HashMap::new()),
        Value::String(s) => Ok(HashMap::from([(String::new(), s.clone())])),
        Value::Map(m) => m
            .iter()
            .filter_map(|(k, v)| match parse_string(v) {
                Ok(Some(ss)) => Some(Ok((k.clone(), ss))),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
            .collect(),
        v => Err(map_mismatch(v)),
    }
}

pub fn parse_bool(v: &Value) -> anyhow::Result<bool> {
    match v {
        Value::Null() => Ok(false),
        Value::Bool(b) => Ok(*b),
        _ => Err(anyhow::anyhow!("invalid: expected bool, got: {:?}", v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn list(vs: Vec<Value>) -> Value {
        Value::List(vs)
    }

    fn map(kvs: Vec<(&str, Value)>) -> Value {
        Value::Map(kvs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn strs(vs: &[&str]) -> Vec<String> {
        vs.iter().map(|v| v.to_string()).collect()
    }

    /// The error text, chain included, of a parse that must fail.
    fn err<T: std::fmt::Debug>(r: anyhow::Result<T>) -> String {
        format!("{:#}", r.expect_err("expected an error"))
    }

    #[test]
    fn map_string_strings_outputs() {
        let cases: Vec<(Value, HashMap<String, Vec<String>>)> = vec![
            (Value::Null(), HashMap::from([(String::new(), vec![])])),
            (s("a"), HashMap::from([(String::new(), strs(&["a"]))])),
            (
                list(vec![s("a"), Value::Null(), s("b")]),
                HashMap::from([(String::new(), strs(&["a", "b"]))]),
            ),
            (list(vec![]), HashMap::from([(String::new(), vec![])])),
            (map(vec![]), HashMap::new()),
            (
                map(vec![
                    ("x", s("a")),
                    ("y", list(vec![s("b"), s("c")])),
                    ("z", Value::Null()),
                    ("é", s("ü")),
                ]),
                HashMap::from([
                    ("x".to_string(), strs(&["a"])),
                    ("y".to_string(), strs(&["b", "c"])),
                    ("z".to_string(), vec![]),
                    ("é".to_string(), strs(&["ü"])),
                ]),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(parse_map_string_strings(&input).unwrap(), want, "{input:?}");
        }
    }

    #[test]
    fn map_string_strings_errors() {
        let outer =
            "invalid: expected string, [string], {string: string} or {string: [string]} got: ";
        let cases: Vec<(Value, String)> = vec![
            (Value::Bool(true), format!("{outer}Bool(true)")),
            (Value::Int(1), format!("{outer}Int(1)")),
            (Value::Uint(1), format!("{outer}Uint(1)")),
            (Value::Float(1.5), format!("{outer}Float(1.5)")),
            // A list with a bad element reports the whole value, not the element.
            (
                list(vec![s("a"), Value::Int(2)]),
                format!("{outer}List([String(\"a\"), Int(2)])"),
            ),
            (
                map(vec![("k", Value::Bool(false))]),
                "invalid: expected string or [string], got: Bool(false)".to_string(),
            ),
            (
                map(vec![("k", list(vec![Value::Int(3)]))]),
                "invalid: expected string, got: Int(3)".to_string(),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(err(parse_map_string_strings(&input)), want, "{input:?}");
        }
    }

    #[test]
    fn map_string_string_outputs() {
        let cases: Vec<(Value, HashMap<String, String>)> = vec![
            (Value::Null(), HashMap::new()),
            (s("a"), HashMap::from([(String::new(), "a".to_string())])),
            (map(vec![]), HashMap::new()),
            (
                map(vec![("x", s("a")), ("y", Value::Null()), ("é", s("ü"))]),
                HashMap::from([
                    ("x".to_string(), "a".to_string()),
                    ("é".to_string(), "ü".to_string()),
                ]),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(parse_map_string_string(&input).unwrap(), want, "{input:?}");
        }
    }

    #[test]
    fn map_string_string_errors() {
        let outer =
            "invalid: expected string, [string], {string: string} or {string: [string]} got: ";
        let cases: Vec<(Value, String)> = vec![
            (Value::Bool(true), format!("{outer}Bool(true)")),
            (Value::Int(1), format!("{outer}Int(1)")),
            (list(vec![s("a")]), format!("{outer}List([String(\"a\")])")),
            (
                map(vec![("k", list(vec![s("a")]))]),
                "invalid: expected string, got: List([String(\"a\")])".to_string(),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(err(parse_map_string_string(&input)), want, "{input:?}");
        }
    }

    #[test]
    fn map_string_map_string_strings_pins() {
        let input = map(vec![("linux", map(vec![("x", s("a"))])), ("mac", s("b"))]);
        assert_eq!(
            parse_map_string_map_string_strings(&input).unwrap(),
            HashMap::from([
                (
                    "linux".to_string(),
                    HashMap::from([("x".to_string(), strs(&["a"]))])
                ),
                (
                    "mac".to_string(),
                    HashMap::from([(String::new(), strs(&["b"]))])
                ),
            ])
        );
        assert_eq!(
            err(parse_map_string_map_string_strings(&map(vec![(
                "linux",
                Value::Int(1)
            )]))),
            "key \"linux\": invalid: expected string, [string], {string: string} or {string: [string]} got: Int(1)"
        );
        assert_eq!(
            err(parse_map_string_map_string_strings(&s("a"))),
            "invalid: expected {string: {string: [string]}} got: String(\"a\")"
        );
    }
}
