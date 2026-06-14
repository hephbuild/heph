//! Runtime support for the spec derives (`Spec`, `SpecStruct`, `SpecEnum`,
//! `SpecUnion`).
//!
//! A target config spec is a struct whose fields are parsed out of a raw
//! BUILD-file config map (`HashMap<String, Value>`). [`FromSpecValue`] is the
//! single source of truth pairing *how a field parses* with *what shape it
//! accepts* (its [`ParamType`]); the derive reads both off the field type so
//! the parser and the LSP schema cannot drift.
//!
//! Field/value types implement [`FromSpecValue`] in one of four ways:
//!   * the primitive + container impls below (`String`, `bool`, `u32`,
//!     `Vec<String>`, the two `HashMap` shapes) — several are themselves unions
//!     (e.g. `string | list[string]`), reusing `heph_core::htvalue::parse_*`;
//!   * `#[derive(SpecStruct)]` — a nested object with well-known keys
//!     (`map[...]`);
//!   * `#[derive(SpecEnum)]` — a string-valued enum;
//!   * `#[derive(SpecUnion)]` — a value accepting one of several shapes; or a
//!     hand-written impl for a bespoke shape (see `TargetSpecCache` in
//!     `pluginexec::spec`).

use heph_core::htvalue::signature::ParamType;
use heph_core::htvalue::{
    Value, parse_bool, parse_map_string_string, parse_map_string_strings, parse_string,
    parse_strings,
};

pub use htspec_derive::{Spec, SpecEnum, SpecStruct, SpecUnion};

// The `Spec` derive macro emits `crate::htspec::DriverField` / `DriverSchema`
// (portable across the monolith re-export and this crate). Re-export them here
// so those expansions resolve wherever the macro is used.
pub use crate::driver::{DriverField, DriverSchema};

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

impl FromSpecValue for u32 {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let n: i64 = match v {
            Value::Int(i) => *i,
            Value::Uint(u) => {
                i64::try_from(*u).map_err(|_e| anyhow::anyhow!("integer too large"))?
            }
            _ => anyhow::bail!("invalid: expected int, got: {:?}", v),
        };
        u32::try_from(n).map_err(|_e| anyhow::anyhow!("invalid: expected u32, got: {n}"))
    }

    fn spec_param_type() -> ParamType {
        ParamType::Int
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
    use crate::driver::DriverField;
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

    #[derive(Spec, Debug)]
    struct ReqSpec {
        #[spec(required)]
        name: String,
        tags: Vec<String>,
    }

    #[test]
    fn required_field_absent_is_an_error() {
        // A required field that is absent fails the parse and is flagged in the
        // schema; an optional one still defaults.
        let err = ReqSpec::from(HashMap::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing required `name`"),
            "{err:#}"
        );
        let spec = ReqSpec::from(HashMap::from([(
            "name".to_string(),
            Value::String("x".into()),
        )]))
        .unwrap();
        assert_eq!(spec.name, "x");
        assert!(spec.tags.is_empty());
        let schema = ReqSpec::schema();
        let by = by_name(&schema.fields);
        assert!(by["name"].required);
        assert!(!by["tags"].required);
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

    // --- SpecEnum ---

    #[derive(SpecEnum, Debug, PartialEq, Default)]
    enum Codegen {
        #[default]
        #[spec(skip)]
        None,
        Copy,
        InPlace,
        #[spec(rename = "in-place-v2")]
        InPlaceV2,
    }

    #[test]
    fn enum_parses_snake_case_and_rename() {
        assert_eq!(
            Codegen::from_spec_value(&Value::String("copy".to_string())).unwrap(),
            Codegen::Copy
        );
        // CamelCase ident lowers to snake_case by default.
        assert_eq!(
            Codegen::from_spec_value(&Value::String("in_place".to_string())).unwrap(),
            Codegen::InPlace
        );
        // `rename` overrides the spelling.
        assert_eq!(
            Codegen::from_spec_value(&Value::String("in-place-v2".to_string())).unwrap(),
            Codegen::InPlaceV2
        );
    }

    #[test]
    fn enum_default_variant_accepts_null_but_not_its_name() {
        // A `#[default]` variant maps null → default.
        assert_eq!(
            Codegen::from_spec_value(&Value::Null()).unwrap(),
            Codegen::None
        );
        // `#[spec(skip)]` means the string "none" is *not* a valid variant.
        let err = Codegen::from_spec_value(&Value::String("none".to_string())).unwrap_err();
        assert!(format!("{err:#}").contains("expected one of"), "{err:#}");
    }

    #[test]
    fn enum_param_type_is_string() {
        assert_eq!(Codegen::spec_param_type(), ParamType::String);
    }

    // --- SpecStruct ---

    /// A nested object: `{enabled, remote, history}` with per-key defaults.
    #[derive(SpecStruct, Debug, PartialEq)]
    struct CacheCfg {
        #[spec(rename = "enabled", default = true)]
        local: bool,
        #[spec(default = true)]
        remote: bool,
        #[spec(default = 1u32, parse = parse_count)]
        history: u32,
    }

    fn cache_map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn struct_parses_known_keys_with_defaults() {
        let cfg = CacheCfg::from_spec_value(&cache_map(&[("remote", Value::Bool(false))])).unwrap();
        assert_eq!(
            cfg,
            CacheCfg {
                local: true,
                remote: false,
                history: 1
            }
        );
    }

    #[test]
    fn struct_rejects_unknown_key_and_non_map() {
        let unknown =
            CacheCfg::from_spec_value(&cache_map(&[("bogus", Value::Bool(true))])).unwrap_err();
        assert!(
            format!("{unknown:#}").contains("unknown entries"),
            "{unknown:#}"
        );
        let not_map = CacheCfg::from_spec_value(&Value::Bool(true)).unwrap_err();
        assert!(
            format!("{not_map:#}").contains("expected a map"),
            "{not_map:#}"
        );
    }

    #[test]
    fn struct_param_type_is_map_of_value_union() {
        // Heterogeneous field types collapse to map[bool | int].
        assert_eq!(CacheCfg::spec_param_type().render(), "map[bool | int]");
    }
}
