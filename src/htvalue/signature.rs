//! Declarative signatures for provider-exposed functions.
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
        }
    }

    /// Whether `v` structurally matches this type. Recurses into `List`/`Map`
    /// element types; an empty list/map trivially matches.
    pub fn matches(&self, v: &Value) -> bool {
        match (self, v) {
            (ParamType::String, Value::String(_)) => true,
            (ParamType::Bool, Value::Bool(_)) => true,
            (ParamType::Int, Value::Int(_)) => true,
            (ParamType::Uint, Value::Uint(_)) => true,
            (ParamType::Float, Value::Float(_)) => true,
            (ParamType::Null, Value::Null()) => true,
            (ParamType::List(inner), Value::List(items)) => items.iter().all(|e| inner.matches(e)),
            (ParamType::Map(value), Value::Map(m)) => m.values().all(|e| value.matches(e)),
            _ => false,
        }
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

/// The declarative signature of a provider-exposed function.
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
    /// Render as `name(p1: type, *rest: type, k1: type) -> ret` for `inspect functions`.
    pub fn render(&self, name: &str) -> String {
        let params = self
            .positional
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty.render()))
            .chain(
                self.variadic
                    .iter()
                    .map(|p| format!("*{}: {}", p.name, p.ty.render())),
            )
            .chain(
                self.named
                    .iter()
                    .map(|p| format!("{}: {}", p.name, p.ty.render())),
            )
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
                            value_kind(&v)
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
                        value_kind(&v)
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
                            value_kind(&v)
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
                value_kind(v)
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
}
