//! Declarative signatures for plugin-exposed functions.
//!
//! A [`FnSignature`] describes a function's typed inputs (positional + named,
//! each required or optional-with-default) and its typed return value, using the
//! same value kinds as [`Value`]. The engine enforces signatures at BUILD-eval
//! time ([`FnSignature::validate_args`] / [`FnSignature::validate_return`]),
//! turning a typo, missing argument, wrong type, or bad arity into a hard error
//! that names the offending function and parameter.

use super::Value;
use std::collections::HashMap;

/// A parameter / return type, mirroring the kinds of [`Value`]. Container kinds
/// carry their (homogeneous) element type.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamType {
    String,
    Bool,
    Int,
    Uint,
    Float,
    Null,
    /// Homogeneous list; element type boxed.
    List(Box<ParamType>),
    /// String-keyed map; value type boxed.
    Map(Box<ParamType>),
    /// Accepts any one of several types, e.g. `cache` being `bool | map[bool]`.
    /// Rendered as the members joined by ` | `; matches if any member matches.
    Union(Vec<ParamType>),
    /// A record with named, heterogeneously-typed fields. Unlike [`ParamType::Map`]
    /// (homogeneous value type, arbitrary keys), a struct fixes a known set of
    /// field names, each with its own type. All fields are optional: a value
    /// matches if every present key names a known field of matching type.
    Struct(Vec<StructField>),
    /// A string drawn from a closed set of literals, e.g. exec's `workdir`
    /// (`"caller" | "tree_root" | …`). Rendered as the quoted literals joined
    /// by ` | `; matches only a string equal to one of them.
    ///
    /// The set is non-empty: an empty one crosses the plugin ABI as no
    /// literals at all, which reads back as any string. `#[derive(SpecEnum)]`
    /// refuses to compile an enum with no parseable variant for that reason.
    Enum(Vec<String>),
}

/// One named field of a [`ParamType::Struct`].
#[derive(Clone, Debug, PartialEq)]
pub struct StructField {
    pub name: String,
    pub ty: ParamType,
}

impl ParamType {
    /// Convenience: `list[T]`.
    pub fn list(inner: ParamType) -> ParamType {
        ParamType::List(Box::new(inner))
    }

    /// Convenience: `map[T]` (string-keyed).
    pub fn map(value: ParamType) -> ParamType {
        ParamType::Map(Box::new(value))
    }

    /// Convenience: a union accepting any of `types`.
    pub fn union(types: Vec<ParamType>) -> ParamType {
        ParamType::Union(types)
    }

    /// Convenience: a struct with the given `(name, type)` fields.
    pub fn strukt(fields: Vec<(&str, ParamType)>) -> ParamType {
        ParamType::Struct(
            fields
                .into_iter()
                .map(|(name, ty)| StructField {
                    name: name.to_string(),
                    ty,
                })
                .collect(),
        )
    }

    /// Human-readable name used in rendered signatures and error messages.
    pub fn render(&self) -> String {
        match self {
            ParamType::String => "string".to_string(),
            ParamType::Bool => "bool".to_string(),
            ParamType::Int => "int".to_string(),
            ParamType::Uint => "uint".to_string(),
            ParamType::Float => "float".to_string(),
            ParamType::Null => "null".to_string(),
            ParamType::List(inner) => format!("list[{}]", inner.render()),
            ParamType::Map(value) => format!("map[{}]", value.render()),
            ParamType::Union(types) => types
                .iter()
                .map(ParamType::render)
                .collect::<Vec<_>>()
                .join(" | "),
            ParamType::Struct(fields) => {
                let rendered = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.ty.render()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("struct({rendered})")
            }
            ParamType::Enum(literals) => literals
                .iter()
                .map(|l| format!("{l:?}"))
                .collect::<Vec<_>>()
                .join(" | "),
        }
    }

    /// The string literals this type accepts as a value, through unions: the
    /// members of every [`ParamType::Enum`] reachable without entering a
    /// container. Empty when the type is not (or does not include) an enum —
    /// a list or map of enums offers nothing for the value itself.
    pub fn literals(&self) -> Vec<&str> {
        match self {
            ParamType::Enum(literals) => literals.iter().map(String::as_str).collect(),
            ParamType::Union(types) => {
                let mut out: Vec<&str> = Vec::new();
                for l in types.iter().flat_map(ParamType::literals) {
                    if !out.contains(&l) {
                        out.push(l);
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Whether `v` structurally matches this type. Recurses into `List`/`Map`
    /// element types; an empty list/map trivially matches.
    pub fn matches(&self, v: &Value) -> bool {
        if let ParamType::Union(types) = self {
            return types.iter().any(|t| t.matches(v));
        }
        match (self, v) {
            (ParamType::String, Value::String(_)) => true,
            (ParamType::Enum(literals), Value::String(s)) => literals.iter().any(|l| l == s),
            (ParamType::Bool, Value::Bool(_)) => true,
            (ParamType::Int, Value::Int(_)) => true,
            (ParamType::Uint, Value::Uint(_)) => true,
            (ParamType::Float, Value::Float(_)) => true,
            (ParamType::Null, Value::Null()) => true,
            (ParamType::List(inner), Value::List(items)) => items.iter().all(|e| inner.matches(e)),
            (ParamType::Map(value), Value::Map(m)) => m.values().all(|e| value.matches(e)),
            // Every present key must name a known field whose type matches.
            // Fields are optional, so missing keys are fine; unknown keys fail.
            (ParamType::Struct(fields), Value::Map(m)) => m.iter().all(|(k, v)| {
                fields
                    .iter()
                    .find(|f| &f.name == k)
                    .is_some_and(|f| f.ty.matches(v))
            }),
            _ => false,
        }
    }

    /// The "got …" half of a mismatch of `v` against this type.
    ///
    /// A scalar is named by its kind — or, for a string offered where literals
    /// are accepted, by the string itself, since "expected \"a\" | \"b\", got
    /// string" names no mistake. A list or map also names the first entry that
    /// does not match and what it holds, `map (`helper`: "k8s")`, so a typo
    /// deep in a dict is found without reading the whole type. Quoted strings
    /// are cut at 64 characters.
    pub fn describe_mismatch(&self, v: &Value) -> String {
        match v {
            Value::Map(_) | Value::List(_) => match self.first_mismatch(v) {
                Some((path, what)) => format!("{} (`{path}`: {what})", value_kind(v)),
                None => value_kind(v).to_string(),
            },
            _ => self.describe_leaf(v),
        }
    }

    /// A scalar mismatch: the string itself where literals are accepted,
    /// otherwise the value's kind.
    fn describe_leaf(&self, v: &Value) -> String {
        match v {
            Value::String(s) if !self.literals().is_empty() => format!("{:?}", capped(s)),
            _ => value_kind(v).to_string(),
        }
    }

    /// The path to the first entry of container `v` that does not match, and
    /// what is there. `None` when the container's own shape is the mismatch,
    /// or a union has more than one member of that shape (descending into the
    /// wrong one would name the wrong entry). Map keys are visited in sorted
    /// order, so the answer does not depend on hash order.
    fn first_mismatch(&self, v: &Value) -> Option<(String, String)> {
        let ty = match self {
            ParamType::Union(types) => {
                let mut fits = types.iter().filter(|t| t.holds_shape_of(v));
                let only = fits.next()?;
                if fits.next().is_some() {
                    return None;
                }
                only
            }
            ty => ty,
        };
        let step = |seg: String, ty: &ParamType, e: &Value| match ty.first_mismatch(e) {
            Some((rest, what)) => (join_path(seg, &rest), what),
            None => (seg, ty.describe_leaf(e)),
        };
        match (ty, v) {
            (ParamType::List(inner), Value::List(items)) => items
                .iter()
                .enumerate()
                .find(|(_, e)| !inner.matches(e))
                .map(|(i, e)| step(format!("[{i}]"), inner, e)),
            (ParamType::Map(inner), Value::Map(m)) => sorted(m)
                .into_iter()
                .find(|(_, e)| !inner.matches(e))
                .map(|(k, e)| step(capped(k).into_owned(), inner, e)),
            (ParamType::Struct(fields), Value::Map(m)) => {
                sorted(m).into_iter().find_map(|(k, e)| {
                    match fields.iter().find(|f| &f.name == k) {
                        None => Some((capped(k).into_owned(), "unknown field".to_string())),
                        Some(f) if !f.ty.matches(e) => Some(step(capped(k).into_owned(), &f.ty, e)),
                        Some(_) => None,
                    }
                })
            }
            _ => None,
        }
    }

    /// Whether `v` has the container shape this (non-union) type describes.
    fn holds_shape_of(&self, v: &Value) -> bool {
        matches!(
            (self, v),
            (ParamType::List(_), Value::List(_))
                | (ParamType::Map(_) | ParamType::Struct(_), Value::Map(_))
        )
    }
}

/// The value-kind name of `v`, for error messages.
fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Uint(_) => "uint",
        Value::Float(_) => "float",
        Value::Null() => "null",
        Value::Map(_) => "map",
        Value::List(_) => "list",
    }
}

/// The longest user string a mismatch message quotes, in characters.
const QUOTE_CAP: usize = 64;

/// `s`, cut to [`QUOTE_CAP`] characters with a trailing `…` when longer.
fn capped(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().nth(QUOTE_CAP).is_some() {
        let mut out: String = s.chars().take(QUOTE_CAP).collect();
        out.push('…');
        out.into()
    } else {
        s.into()
    }
}

/// `m`'s entries in key order, so a message does not depend on hash order.
fn sorted(m: &HashMap<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = m.iter().collect();
    entries.sort_unstable_by_key(|(k, _)| *k);
    entries
}

/// `inner` appended to the path `outer`: `a` + `b` is `a.b`, `a` + `[0]` is
/// `a[0]`.
fn join_path(outer: String, inner: &str) -> String {
    if inner.starts_with('[') {
        outer + inner
    } else {
        format!("{outer}.{inner}")
    }
}

/// One declared parameter. `default == None` means required.
#[derive(Clone, Debug)]
pub struct Param {
    pub name: &'static str,
    pub ty: ParamType,
    pub default: Option<Value>,
}

impl Param {
    /// A required parameter (no default).
    pub fn required(name: &'static str, ty: ParamType) -> Param {
        Param {
            name,
            ty,
            default: None,
        }
    }

    /// An optional parameter substituting `default` when omitted.
    pub fn optional(name: &'static str, ty: ParamType, default: Value) -> Param {
        Param {
            name,
            ty,
            default: Some(default),
        }
    }
}

/// The declarative signature of a plugin-exposed function.
///
/// `variadic`, when set, collects any positional arguments beyond the declared
/// `positional` ones — each is type-checked against the variadic param's type
/// and passed through as an individual positional (Go `path.Join`-style
/// `f(a, b, c)`). With no variadic, surplus positionals are an arity error.
#[derive(Clone, Debug)]
pub struct FnSignature {
    pub positional: Vec<Param>,
    pub named: Vec<Param>,
    pub variadic: Option<Param>,
    pub returns: ParamType,
}

impl FnSignature {
    /// Render as `name(p1: type, opt?: type, *rest: type, k1: type) -> ret` for
    /// `inspect functions`. Optional params (those with a default) are marked
    /// with a trailing `?` on the name — the TypeScript / LSP convention — so
    /// required vs optional is visible without spelling out each default.
    pub fn render(&self, name: &str) -> String {
        let render_param = |p: &Param| {
            let opt = if p.default.is_some() { "?" } else { "" };
            format!("{}{}: {}", p.name, opt, p.ty.render())
        };
        let params = self
            .positional
            .iter()
            .map(render_param)
            // A variadic param is inherently zero-or-more; `*name` already
            // conveys that, so it is never additionally `?`-marked.
            .chain(
                self.variadic
                    .iter()
                    .map(|p| format!("*{}: {}", p.name, p.ty.render())),
            )
            .chain(self.named.iter().map(render_param))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{name}({params}) -> {}", self.returns.render())
    }

    /// Validate a call's arguments against this signature, hard-failing on any
    /// violation (arity, missing required, unknown named, wrong type). Returns
    /// the normalized arguments with optional defaults substituted. Every error
    /// names `fn_display` and the offending parameter.
    pub fn validate_args(
        &self,
        fn_display: &str,
        positional: Vec<Value>,
        mut named: HashMap<String, Value>,
    ) -> anyhow::Result<(Vec<Value>, HashMap<String, Value>)> {
        if self.variadic.is_none() && positional.len() > self.positional.len() {
            anyhow::bail!(
                "{fn_display}: expected at most {} positional argument(s), got {}",
                self.positional.len(),
                positional.len()
            );
        }

        let mut pos = positional.into_iter();
        let mut out_positional = Vec::with_capacity(self.positional.len());
        for param in &self.positional {
            match pos.next() {
                Some(v) => {
                    if !param.ty.matches(&v) {
                        anyhow::bail!(
                            "{fn_display}: positional argument `{}` expected {}, got {}",
                            param.name,
                            param.ty.render(),
                            param.ty.describe_mismatch(&v)
                        );
                    }
                    out_positional.push(v);
                }
                None => match &param.default {
                    Some(d) => out_positional.push(d.clone()),
                    None => anyhow::bail!(
                        "{fn_display}: missing required positional argument `{}`",
                        param.name
                    ),
                },
            }
        }

        // Surplus positionals flow into the variadic param (type-checked
        // individually) and pass through as individual positionals.
        if let Some(var) = &self.variadic {
            for v in pos {
                if !var.ty.matches(&v) {
                    anyhow::bail!(
                        "{fn_display}: variadic argument `{}` expected {}, got {}",
                        var.name,
                        var.ty.render(),
                        var.ty.describe_mismatch(&v)
                    );
                }
                out_positional.push(v);
            }
        }

        let mut out_named = HashMap::with_capacity(self.named.len());
        for param in &self.named {
            match named.remove(param.name) {
                Some(v) => {
                    if !param.ty.matches(&v) {
                        anyhow::bail!(
                            "{fn_display}: argument `{}` expected {}, got {}",
                            param.name,
                            param.ty.render(),
                            param.ty.describe_mismatch(&v)
                        );
                    }
                    out_named.insert(param.name.to_string(), v);
                }
                None => match &param.default {
                    Some(d) => {
                        out_named.insert(param.name.to_string(), d.clone());
                    }
                    None => {
                        anyhow::bail!("{fn_display}: missing required argument `{}`", param.name)
                    }
                },
            }
        }

        if let Some(key) = named.keys().next() {
            anyhow::bail!("{fn_display}: unknown keyword argument `{key}`");
        }

        Ok((out_positional, out_named))
    }

    /// Validate a function's return value against its declared return type.
    pub fn validate_return(&self, fn_display: &str, v: &Value) -> anyhow::Result<()> {
        if !self.returns.matches(v) {
            anyhow::bail!(
                "{fn_display}: return value expected {}, got {}",
                self.returns.render(),
                self.returns.describe_mismatch(v)
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig_one_string() -> FnSignature {
        FnSignature {
            positional: vec![Param::required("pattern", ParamType::String)],
            named: vec![],
            variadic: None,
            returns: ParamType::list(ParamType::String),
        }
    }

    #[test]
    fn union_renders_and_matches_any_member() {
        let ty = ParamType::union(vec![ParamType::Bool, ParamType::map(ParamType::Bool)]);
        assert_eq!(ty.render(), "bool | map[bool]");
        assert!(ty.matches(&Value::Bool(true)));
        assert!(ty.matches(&Value::Map(std::collections::HashMap::from([(
            "skip".to_string(),
            Value::Bool(true)
        )]))));
        assert!(!ty.matches(&Value::Int(3)));
    }

    #[test]
    fn enum_renders_quoted_and_matches_only_its_literals() {
        let ty = ParamType::Enum(vec!["caller".to_string(), "tree_root".to_string()]);
        assert_eq!(ty.render(), r#""caller" | "tree_root""#);
        assert!(ty.matches(&Value::String("tree_root".to_string())));
        // The near miss, a prefix, and the empty string are all refused.
        for bad in ["root", "tree_roo", "tree_root_", "", "Caller"] {
            assert!(!ty.matches(&Value::String(bad.to_string())), "{bad}");
        }
        assert!(!ty.matches(&Value::Bool(true)));

        // In a union the literals and the other members both render and match.
        let u = ParamType::union(vec![ParamType::Bool, ty.clone()]);
        assert_eq!(u.render(), r#"bool | "caller" | "tree_root""#);
        assert!(u.matches(&Value::Bool(false)));
        assert!(u.matches(&Value::String("caller".to_string())));
        assert_eq!(u.literals(), ["caller", "tree_root"]);
        // A list of enums offers nothing for the value itself.
        assert!(ParamType::list(ty).literals().is_empty());
        assert!(ParamType::String.literals().is_empty());
    }

    #[test]
    fn a_wrong_literal_names_the_string_it_got() {
        let sig = FnSignature {
            positional: vec![],
            named: vec![Param::required(
                "mode",
                ParamType::Enum(vec!["a".to_string(), "b".to_string()]),
            )],
            variadic: None,
            returns: ParamType::Null,
        };
        let err = sig
            .validate_args(
                "f",
                vec![],
                HashMap::from([("mode".to_string(), Value::String("c".to_string()))]),
            )
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            r#"f: argument `mode` expected "a" | "b", got "c""#
        );
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// A container names the first entry that does not match, through a
    /// union, a struct, a map and a list, in sorted key order.
    #[test]
    fn a_container_mismatch_names_the_offending_path_and_value() {
        let dialect = ParamType::Enum(vec!["aws".to_string(), "docker".to_string()]);
        let present = ParamType::strukt(vec![
            ("env", ParamType::map(ParamType::String)),
            (
                "helper",
                ParamType::union(vec![
                    dialect.clone(),
                    ParamType::strukt(vec![("dialect", dialect)]),
                ]),
            ),
        ]);
        let v = map(&[("helper", s("k8s"))]);
        assert_eq!(present.describe_mismatch(&v), r#"map (`helper`: "k8s")"#);
        // Into the union's struct member, then its enum field.
        let v = map(&[("helper", map(&[("dialect", s("gcp"))]))]);
        assert_eq!(
            present.describe_mismatch(&v),
            r#"map (`helper.dialect`: "gcp")"#
        );
        // An unknown key, and the first offender in sorted order.
        let v = map(&[("zzz", s("x")), ("env", map(&[("B", Value::Int(1))]))]);
        assert_eq!(present.describe_mismatch(&v), "map (`env.B`: int)");
        let v = map(&[("bogus", s("x"))]);
        assert_eq!(
            present.describe_mismatch(&v),
            "map (`bogus`: unknown field)"
        );
        // A list index, and a map of lists.
        let ty = ParamType::map(ParamType::list(ParamType::String));
        let v = map(&[("a", Value::List(vec![s("ok"), Value::Bool(true)]))]);
        assert_eq!(ty.describe_mismatch(&v), "map (`a[1]`: bool)");
        // The container's own shape is the mismatch: just its kind.
        assert_eq!(ParamType::String.describe_mismatch(&map(&[])), "map");
        // Two union members of the container's shape: no guess.
        let ambiguous = ParamType::union(vec![
            ParamType::map(ParamType::Bool),
            ParamType::map(ParamType::Int),
        ]);
        assert_eq!(ambiguous.describe_mismatch(&map(&[("a", s("x"))])), "map");
        // Scalars keep their kind.
        assert_eq!(ParamType::String.describe_mismatch(&Value::Int(1)), "int");
    }

    /// A quoted user string is cut at 64 characters, by character, not byte.
    #[test]
    fn a_quoted_mismatch_is_capped() {
        let ty = ParamType::Enum(vec!["a".to_string()]);
        let exact = "é".repeat(64);
        assert_eq!(ty.describe_mismatch(&s(&exact)), format!("{exact:?}"));
        let long = "é".repeat(65);
        assert_eq!(ty.describe_mismatch(&s(&long)), format!("\"{exact}…\""));
        // A key on the path is capped too.
        let key = "k".repeat(70);
        let v = map(&[(key.as_str(), s("x"))]);
        assert_eq!(
            ParamType::map(ty).describe_mismatch(&v),
            format!("map (`{}…`: \"x\")", "k".repeat(64))
        );
    }

    #[test]
    fn struct_renders_fields_in_order() {
        let ty = ParamType::strukt(vec![
            ("env", ParamType::map(ParamType::String)),
            ("pass_env", ParamType::list(ParamType::String)),
        ]);
        assert_eq!(
            ty.render(),
            "struct(env: map[string], pass_env: list[string])"
        );
    }

    #[test]
    fn struct_matches_subset_of_known_fields_and_rejects_unknown() {
        use std::collections::HashMap;
        let ty = ParamType::strukt(vec![
            ("env", ParamType::map(ParamType::String)),
            ("pass_env", ParamType::list(ParamType::String)),
        ]);
        // Empty map and a subset both match (fields optional).
        assert!(ty.matches(&Value::Map(HashMap::new())));
        assert!(ty.matches(&Value::Map(HashMap::from([(
            "pass_env".to_string(),
            Value::List(vec![Value::String("HOME".to_string())])
        )]))));
        // Wrong value type for a known field fails.
        assert!(!ty.matches(&Value::Map(HashMap::from([(
            "env".to_string(),
            Value::Bool(true)
        )]))));
        // Unknown key fails.
        assert!(!ty.matches(&Value::Map(HashMap::from([(
            "nope".to_string(),
            Value::String("x".to_string())
        )]))));
    }

    #[test]
    fn bool_or_struct_union_accepts_both_forms() {
        use std::collections::HashMap;
        let ty = ParamType::union(vec![
            ParamType::Bool,
            ParamType::strukt(vec![("env", ParamType::map(ParamType::String))]),
        ]);
        assert!(ty.matches(&Value::Bool(false)));
        assert!(ty.matches(&Value::Map(HashMap::from([(
            "env".to_string(),
            Value::Map(HashMap::from([(
                "K".to_string(),
                Value::String("v".to_string())
            )]))
        )]))));
    }

    #[test]
    fn missing_required_positional_errors() {
        let err = sig_one_string()
            .validate_args("fs.glob", vec![], HashMap::new())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("fs.glob"), "{msg}");
        assert!(msg.contains("pattern"), "{msg}");
    }

    #[test]
    fn wrong_positional_type_errors() {
        let err = sig_one_string()
            .validate_args("fs.glob", vec![Value::Int(7)], HashMap::new())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected string"), "{msg}");
        assert!(msg.contains("got int"), "{msg}");
    }

    #[test]
    fn too_many_positional_errors() {
        let err = sig_one_string()
            .validate_args(
                "fs.glob",
                vec![Value::String("a".into()), Value::String("b".into())],
                HashMap::new(),
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("at most 1 positional"),
            "{err:#}"
        );
    }

    #[test]
    fn unknown_named_errors() {
        let mut named = HashMap::new();
        named.insert("bogus".to_string(), Value::Int(1));
        let err = sig_one_string()
            .validate_args("fs.glob", vec![Value::String("a".into())], named)
            .unwrap_err();
        assert!(format!("{err:#}").contains("bogus"), "{err:#}");
    }

    #[test]
    fn missing_required_named_errors() {
        let sig = FnSignature {
            positional: vec![],
            named: vec![Param::required("provider", ParamType::String)],
            variadic: None,
            returns: ParamType::Null,
        };
        let err = sig.validate_args("ps", vec![], HashMap::new()).unwrap_err();
        assert!(format!("{err:#}").contains("provider"), "{err:#}");
    }

    #[test]
    fn optional_named_default_substituted() {
        let sig = FnSignature {
            positional: vec![],
            named: vec![Param::optional("abs", ParamType::Bool, Value::Bool(false))],
            variadic: None,
            returns: ParamType::Null,
        };
        let (_, named) = sig.validate_args("f", vec![], HashMap::new()).unwrap();
        assert_eq!(named.get("abs"), Some(&Value::Bool(false)));
    }

    #[test]
    fn list_element_type_checked() {
        let sig = FnSignature {
            positional: vec![Param::required("elems", ParamType::list(ParamType::String))],
            named: vec![],
            variadic: None,
            returns: ParamType::String,
        };
        // Mixed-type list rejected.
        let bad = sig
            .validate_args(
                "fs.join",
                vec![Value::List(vec![Value::String("a".into()), Value::Int(1)])],
                HashMap::new(),
            )
            .unwrap_err();
        assert!(
            format!("{bad:#}").contains("expected list[string]"),
            "{bad:#}"
        );
        // Homogeneous list accepted.
        assert!(
            sig.validate_args(
                "fs.join",
                vec![Value::List(vec![Value::String("a".into())])],
                HashMap::new(),
            )
            .is_ok()
        );
    }

    #[test]
    fn variadic_collects_surplus_positionals() {
        let sig = FnSignature {
            positional: vec![],
            named: vec![],
            variadic: Some(Param::required("elems", ParamType::String)),
            returns: ParamType::String,
        };
        // Any number of positionals accepted, passed through individually.
        let (pos, _) = sig
            .validate_args(
                "fs.join",
                vec![
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ],
                HashMap::new(),
            )
            .unwrap();
        assert_eq!(pos.len(), 3);
        // Zero positionals is fine.
        assert!(sig.validate_args("fs.join", vec![], HashMap::new()).is_ok());
        // Each variadic element is type-checked.
        let err = sig
            .validate_args("fs.join", vec![Value::Int(1)], HashMap::new())
            .unwrap_err();
        assert!(format!("{err:#}").contains("expected string"), "{err:#}");
    }

    #[test]
    fn variadic_renders_with_star() {
        let sig = FnSignature {
            positional: vec![],
            named: vec![],
            variadic: Some(Param::required("elems", ParamType::String)),
            returns: ParamType::String,
        };
        assert_eq!(sig.render("join"), "join(*elems: string) -> string");
    }

    #[test]
    fn validate_return_checks_type() {
        let sig = sig_one_string();
        assert!(
            sig.validate_return("fs.glob", &Value::List(vec![Value::String("a".into())]))
                .is_ok()
        );
        let err = sig
            .validate_return("fs.glob", &Value::String("oops".into()))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("return value expected list[string]"),
            "{err:#}"
        );
    }

    #[test]
    fn render_reads_well() {
        assert_eq!(
            sig_one_string().render("glob"),
            "glob(pattern: string) -> list[string]"
        );
    }

    #[test]
    fn optional_params_render_with_question_mark() {
        // Optional (defaulted) params get a trailing `?`; required ones don't,
        // and a variadic stays `*name` without an extra `?`.
        let sig = FnSignature {
            positional: vec![
                Param::required("src", ParamType::String),
                Param::optional("abs", ParamType::Bool, Value::Bool(false)),
            ],
            named: vec![Param::optional(
                "sep",
                ParamType::String,
                Value::String("/".into()),
            )],
            variadic: None,
            returns: ParamType::String,
        };
        assert_eq!(
            sig.render("join"),
            "join(src: string, abs?: bool, sep?: string) -> string"
        );
    }
}
