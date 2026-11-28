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

/// The most chars [`Value::to_starlark`] writes before it cuts the rest to `…`.
pub const STARLARK_DISPLAY_CAP: usize = 256;

impl Value {
    /// The value as a BUILD author would write it — `3`, `["X"]`,
    /// `{"X": 1}`, `True`, `None` — for an error message, never Rust's
    /// `Debug` (`Int(3)`). Map keys are sorted, so the text is stable; control
    /// characters in a string are escaped; a value whose text would pass
    /// [`STARLARK_DISPLAY_CAP`] chars is cut there with `…`.
    pub fn to_starlark(&self) -> String {
        let mut out = String::new();
        // `Err` only means the cap was reached, and `…` already says so.
        if write_starlark(self, &mut out).is_err() {
            debug_assert!(out.ends_with('…'));
        }
        out
    }

    /// Structural equality, order-free for maps, with floats compared by
    /// their bits: `==` calls `-0.0` and `0.0` equal and a `NaN` unequal to
    /// itself, and both would misjudge whether two values are the same.
    pub fn same(&self, other: &Self) -> bool {
        use Value as V;
        match (self, other) {
            (V::Float(x), V::Float(y)) => x.to_bits() == y.to_bits(),
            (V::Map(x), V::Map(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| v.same(w)))
            }
            (V::List(x), V::List(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(v, w)| v.same(w))
            }
            (V::String(x), V::String(y)) => x == y,
            (V::Bool(x), V::Bool(y)) => x == y,
            (V::Int(x), V::Int(y)) => x == y,
            (V::Uint(x), V::Uint(y)) => x == y,
            (V::Null(), V::Null()) => true,
            // Listed rather than `_`, so a new variant fails to compile until
            // it gets its own arm above.
            (
                V::Float(_)
                | V::Map(_)
                | V::List(_)
                | V::String(_)
                | V::Bool(_)
                | V::Int(_)
                | V::Uint(_)
                | V::Null(),
                _,
            ) => false,
        }
    }
}

/// The cap was reached: stop writing.
struct Capped;

/// Push `s` onto `out`, or as much of it as fits under the cap and then `…`.
fn push_capped(out: &mut String, s: &str) -> Result<(), Capped> {
    let used = out.chars().count();
    let room = STARLARK_DISPLAY_CAP.saturating_sub(used);
    if s.chars().count() <= room {
        out.push_str(s);
        return Ok(());
    }
    out.extend(s.chars().take(room));
    out.push('…');
    Err(Capped)
}

fn write_starlark(v: &Value, out: &mut String) -> Result<(), Capped> {
    match v {
        Value::String(s) => push_capped(out, &starlark_str(s)),
        Value::Bool(true) => push_capped(out, "True"),
        Value::Bool(false) => push_capped(out, "False"),
        Value::Int(i) => push_capped(out, &i.to_string()),
        Value::Uint(u) => push_capped(out, &u.to_string()),
        Value::Float(f) => push_capped(out, &starlark_float(*f)),
        Value::Null() => push_capped(out, "None"),
        Value::List(items) => {
            push_capped(out, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    push_capped(out, ", ")?;
                }
                write_starlark(item, out)?;
            }
            push_capped(out, "]")
        }
        Value::Map(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_unstable();
            push_capped(out, "{")?;
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    push_capped(out, ", ")?;
                }
                push_capped(out, &starlark_str(k))?;
                push_capped(out, ": ")?;
                if let Some(item) = m.get(k) {
                    write_starlark(item, out)?;
                }
            }
            push_capped(out, "}")
        }
    }
}

/// `s` as a Starlark string literal: double-quoted, with `"`, `\` and every
/// control character escaped (`\n`, `\t`, `\x1b`), so it reads as written and
/// never moves a terminal's cursor.
pub fn starlark_str(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() && c.is_ascii() => {
                // Writing to a `String` cannot fail.
                write!(out, "\\x{:02x}", u32::from(c)).unwrap_or_default();
            }
            c if c.is_control() => {
                write!(out, "\\u{:04x}", u32::from(c)).unwrap_or_default();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A float as Starlark writes one: always with a `.` or an exponent, so it
/// does not read as an int.
fn starlark_float(f: f64) -> String {
    if f.is_nan() {
        "float(\"nan\")".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 {
            "float(\"+inf\")"
        } else {
            "float(\"-inf\")"
        }
        .to_owned()
    } else {
        // `Debug` keeps the `.0` that `Display` drops from a whole float.
        format!("{f:?}")
    }
}

pub fn parse_string(v: &Value) -> anyhow::Result<Option<String>> {
    match v {
        Value::Null() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        v => Err(anyhow::anyhow!(
            "invalid: expected string, got: {}",
            v.to_starlark()
        )),
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
            "invalid: expected string or [string], got: {}",
            v.to_starlark()
        )),
    }
}

/// The error both lenient map parsers return for a value of the wrong shape.
fn map_mismatch(v: &Value) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {}",
        v.to_starlark()
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
                    .with_context(|| format!("key {}", starlark_str(k)))
                    .map(|inner| (k.clone(), inner))
            })
            .collect(),
        v => Err(anyhow::anyhow!(
            "invalid: expected {{string: {{string: [string]}}}} got: {}",
            v.to_starlark()
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
        _ => Err(anyhow::anyhow!(
            "invalid: expected bool, got: {}",
            v.to_starlark()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_starlark_writes_literals() {
        let v = Value::Map(HashMap::from([
            (
                "b".to_owned(),
                Value::List(vec![s("X"), Value::Int(-3), Value::Uint(4)]),
            ),
            ("a".to_owned(), Value::Bool(true)),
            ("c".to_owned(), Value::Null()),
            ("d".to_owned(), Value::Float(1.0)),
        ]));
        assert_eq!(
            v.to_starlark(),
            r#"{"a": True, "b": ["X", -3, 4], "c": None, "d": 1.0}"#
        );
        assert_eq!(s("q\"\\\x1b\n").to_starlark(), r#""q\"\\\x1b\n""#);
        assert_eq!(
            Value::Float(f64::INFINITY).to_starlark(),
            r#"float("+inf")"#
        );
    }

    #[test]
    fn to_starlark_cuts_at_the_cap() {
        let v = Value::List((0..1000).map(Value::Int).collect());
        let text = v.to_starlark();
        assert_eq!(text.chars().count(), STARLARK_DISPLAY_CAP + 1);
        assert!(text.ends_with('…'));
    }

    /// F2: no `Int(`/`String(`/`List(` in what a parser says about a value.
    #[test]
    fn parse_errors_name_values_as_starlark() {
        let errs = [
            parse_string(&Value::Int(3)),
            parse_strings(&Value::Map(HashMap::from([(
                "X".to_owned(),
                Value::Int(1),
            )])))
            .map(|_| None),
            parse_bool(&Value::List(vec![s("X")])).map(|_| None),
            parse_map_string_strings(&Value::Bool(true)).map(|_| None),
        ];
        let texts: Vec<String> = errs
            .into_iter()
            .map(|r| format!("{:#}", r.expect_err("a wrong type fails")))
            .collect();
        for t in &texts {
            for leak in ["Int(", "String(", "List(", "Map(", "Bool("] {
                assert!(!t.contains(leak), "{t}");
            }
        }
        assert!(texts[0].ends_with("got: 3"), "{}", texts[0]);
        assert!(texts[1].ends_with(r#"got: {"X": 1}"#), "{}", texts[1]);
        assert!(texts[2].ends_with(r#"got: ["X"]"#), "{}", texts[2]);
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    #[test]
    fn same_compares_floats_by_bits_and_maps_order_free() {
        assert!(!Value::Float(-0.0).same(&Value::Float(0.0)));
        assert!(Value::Float(f64::NAN).same(&Value::Float(f64::NAN)));
        assert!(!Value::Int(1).same(&Value::Uint(1)));
        let a = Value::Map(HashMap::from([
            ("x".to_string(), s("1")),
            ("y".to_string(), s("2")),
        ]));
        let b = Value::Map(HashMap::from([
            ("y".to_string(), s("2")),
            ("x".to_string(), s("1")),
        ]));
        assert!(a.same(&b));
        assert!(!Value::List(vec![s("1"), s("2")]).same(&Value::List(vec![s("2"), s("1")])));
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
            (Value::Bool(true), format!("{outer}True")),
            (Value::Int(1), format!("{outer}1")),
            (Value::Uint(1), format!("{outer}1")),
            (Value::Float(1.5), format!("{outer}1.5")),
            // A list with a bad element reports the whole value, not the element.
            (
                list(vec![s("a"), Value::Int(2)]),
                format!("{outer}[\"a\", 2]"),
            ),
            (
                map(vec![("k", Value::Bool(false))]),
                "invalid: expected string or [string], got: False".to_string(),
            ),
            (
                map(vec![("k", list(vec![Value::Int(3)]))]),
                "invalid: expected string, got: 3".to_string(),
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
            (Value::Bool(true), format!("{outer}True")),
            (Value::Int(1), format!("{outer}1")),
            (list(vec![s("a")]), format!("{outer}[\"a\"]")),
            (
                map(vec![("k", list(vec![s("a")]))]),
                "invalid: expected string, got: [\"a\"]".to_string(),
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
            "key \"linux\": invalid: expected string, [string], {string: string} or {string: [string]} got: 1"
        );
        assert_eq!(
            err(parse_map_string_map_string_strings(&s("a"))),
            "invalid: expected {string: {string: [string]}} got: \"a\""
        );
    }
}
