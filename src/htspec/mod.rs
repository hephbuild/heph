//! Runtime support for `#[derive(Spec)]` / `#[derive(SpecUnion)]`.
//!
//! A target config spec is a struct whose fields are parsed out of a raw
//! BUILD-file config map (`HashMap<String, Value>`). [`FromSpecValue`] is the
//! single source of truth pairing *how a field parses* with *what shape it
//! accepts* (its [`ParamType`]); the derive macro reads both off the field
//! type so the parser and the LSP schema cannot drift.
//!
//! Container impls below (`Vec<String>`, the two `HashMap` shapes) are
//! themselves unions — they accept e.g. `string | list[string]` — reusing the
//! existing `crate::htvalue::parse_*` helpers. A field that accepts a bespoke
//! union of shapes either implements `FromSpecValue` by hand (see
//! `TargetSpecCache` in `pluginexec::spec`) or derives [`SpecUnion`] on an enum.

use crate::htvalue::signature::ParamType;
use crate::htvalue::{
    Value, parse_bool, parse_map_string_string, parse_map_string_strings, parse_string,
    parse_strings,
};

pub use htspec_derive::{Spec, SpecUnion};

/// A type parsable from a BUILD-file [`Value`] that also describes its own
/// accepted shape. Implemented for the primitive + container shapes specs use;
/// implement it by hand (or via `#[derive(SpecUnion)]`) for bespoke union types.
pub trait FromSpecValue: Sized {
    /// Parse a present config value into this type.
    fn from_spec_value(v: &Value) -> anyhow::Result<Self>;

    /// The shape this type accepts, for the LSP schema.
    fn spec_param_type() -> ParamType;
}

/// Build a union [`ParamType`] from member types, flattening nested unions and
/// dropping duplicates so `a | (a | b)` renders as `a | b`. A single member is
/// returned bare (no wrapping `Union`).
pub fn flatten_union(members: Vec<ParamType>) -> ParamType {
    let mut flat: Vec<ParamType> = Vec::new();
    for m in members {
        match m {
            ParamType::Union(inner) => {
                for t in inner {
                    if !flat.contains(&t) {
                        flat.push(t);
                    }
                }
            }
            other => {
                if !flat.contains(&other) {
                    flat.push(other);
                }
            }
        }
    }
    if flat.len() == 1 {
        flat.pop().expect("len checked")
    } else {
        ParamType::Union(flat)
    }
}

impl FromSpecValue for String {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_string(v)?.ok_or_else(|| anyhow::anyhow!("invalid: expected string, got null"))
    }

    fn spec_param_type() -> ParamType {
        ParamType::String
    }
}

impl FromSpecValue for Option<String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_string(v)
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::Null])
    }
}

impl FromSpecValue for bool {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_bool(v)
    }

    fn spec_param_type() -> ParamType {
        ParamType::Bool
    }
}

impl FromSpecValue for Vec<String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_strings(v)
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)])
    }
}

impl FromSpecValue for std::collections::HashMap<String, Vec<String>> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_map_string_strings(v)
    }

    fn spec_param_type() -> ParamType {
        let str_or_list =
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
        ParamType::union(vec![
            ParamType::String,
            ParamType::list(ParamType::String),
            ParamType::map(str_or_list),
        ])
    }
}

impl FromSpecValue for std::collections::HashMap<String, String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        parse_map_string_string(v)
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::DriverField;
    use std::collections::HashMap;

    // A spec exercising the common field shapes plus every per-field override.
    /// doc on run
    #[derive(Spec, Debug, PartialEq)]
    struct DemoSpec {
        /// Command to run.
        run: Vec<String>,
        deps: HashMap<String, Vec<String>>,
        env: HashMap<String, String>,
        #[spec(rename = "out")]
        outputs: HashMap<String, Vec<String>>,
        #[spec(default = 1u32, parse = parse_count, ty = ParamType::Int)]
        count: u32,
        #[spec(with = mode_spec)]
        mode: Mode,
    }

    #[derive(Debug, PartialEq, Default)]
    enum Mode {
        #[default]
        Off,
        On,
    }

    fn parse_count(v: &Value) -> anyhow::Result<u32> {
        match v {
            Value::Int(i) => {
                u32::try_from(*i).map_err(|_e| anyhow::anyhow!("count must be a non-negative int"))
            }
            _ => anyhow::bail!("count must be an int"),
        }
    }

    mod mode_spec {
        use super::*;
        pub fn from_spec_value(v: &Value) -> anyhow::Result<Mode> {
            match String::from_spec_value(v)?.as_str() {
                "on" => Ok(Mode::On),
                "off" => Ok(Mode::Off),
                other => anyhow::bail!("bad mode: {other}"),
            }
        }
        pub fn spec_param_type() -> ParamType {
            ParamType::String
        }
    }

    fn by_name(fields: &[DriverField]) -> HashMap<&str, &DriverField> {
        fields.iter().map(|f| (f.name.as_str(), f)).collect()
    }

    #[test]
    fn parses_fields_and_applies_defaults() {
        let spec = DemoSpec::from(HashMap::from([
            ("run".to_string(), Value::String("echo".to_string())),
            ("mode".to_string(), Value::String("on".to_string())),
        ]))
        .unwrap();
        assert_eq!(spec.run, vec!["echo"]);
        assert!(spec.deps.is_empty());
        assert_eq!(spec.count, 1, "default override honored");
        assert_eq!(spec.mode, Mode::On);
    }

    #[test]
    fn rename_maps_config_key_to_field() {
        let spec = DemoSpec::from(HashMap::from([
            ("run".to_string(), Value::String("x".to_string())),
            ("mode".to_string(), Value::String("off".to_string())),
            (
                "out".to_string(),
                Value::List(vec![Value::String("a.o".to_string())]),
            ),
        ]))
        .unwrap();
        assert_eq!(spec.outputs.get(""), Some(&vec!["a.o".to_string()]));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = DemoSpec::from(HashMap::from([
            ("run".to_string(), Value::String("x".to_string())),
            ("mode".to_string(), Value::String("off".to_string())),
            ("bogus".to_string(), Value::Bool(true)),
        ]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown entries"), "{err:#}");
    }

    #[test]
    fn parse_error_carries_field_context() {
        let err = DemoSpec::from(HashMap::from([
            ("run".to_string(), Value::String("x".to_string())),
            ("mode".to_string(), Value::String("off".to_string())),
            ("count".to_string(), Value::Bool(true)),
        ]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("parse `count`"), "{err:#}");
    }

    #[test]
    fn schema_mirrors_field_types_and_overrides() {
        let schema = DemoSpec::schema();
        let f = by_name(&schema.fields);
        assert_eq!(f["run"].ty, Vec::<String>::spec_param_type());
        assert_eq!(f["run"].ty.render(), "string | list[string]");
        assert_eq!(f["run"].doc, "Command to run.");
        // `rename` surfaces the config key, not the field name.
        assert!(f.contains_key("out"));
        assert!(!f.contains_key("outputs"));
        // `ty` override wins over the field type.
        assert_eq!(f["count"].ty, ParamType::Int);
        // `with` module supplies the schema type.
        assert_eq!(f["mode"].ty, ParamType::String);
    }

    // --- SpecUnion ---

    #[derive(SpecUnion, Debug, PartialEq)]
    enum Strings {
        Flat(Vec<String>),
        Grouped(HashMap<String, Vec<String>>),
    }

    #[test]
    fn union_tries_variants_in_order() {
        let flat =
            Strings::from_spec_value(&Value::List(vec![Value::String("a".to_string())])).unwrap();
        assert_eq!(flat, Strings::Flat(vec!["a".to_string()]));
    }

    #[test]
    fn union_param_type_flattens_members() {
        // Flat = string | list[string]; Grouped adds map[...]; dups collapse.
        let r = Strings::spec_param_type().render();
        assert_eq!(r, "string | list[string] | map[string | list[string]]");
    }
}
