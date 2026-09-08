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
//!     (e.g. `string | list[string]`), reusing `hcore::htvalue::parse_*`;
//!   * `#[derive(SpecStruct)]` — a nested object with well-known keys
//!     (`map[...]`);
//!   * `#[derive(SpecEnum)]` — a string-valued enum;
//!   * `#[derive(SpecUnion)]` — a value accepting one of several shapes; or a
//!     hand-written impl for a bespoke shape (see [`TargetSpecCache`], the
//!     shared `cache` attribute).

use hcore::htvalue::signature::ParamType;
use hcore::htvalue::{
    Value, parse_bool, parse_map_string_map_string_strings, parse_map_string_string,
    parse_map_string_strings, parse_string, parse_strings,
};

pub use htspec_derive::{Spec, SpecEnum, SpecStruct, SpecUnion};

mod cache;
pub use cache::TargetSpecCache;

// `Rename`'s type lives in `hcore` (it is applied by `hartifactcontent::view`);
// only its BUILD-file parsing belongs here.
mod rename;

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

/// Reject a value that carries a deferred reference in a field that cannot take
/// one.
///
/// This is where per-field safety lives, and it is why a driver needs no
/// per-field annotation. Reserving the kinds inside the shared decoder means a
/// reference in `out`, `deps`, `name`, a glob or an address filter is a parse
/// error in every driver at once — and [`Deferred`](crate::driver::Deferred) is
/// simply the one type that accepts one.
///
/// It closes the reverse direction for free too: if a driver ever changes a field
/// back from `Deferred<String>` to `String`, a BUILD file containing a reference
/// fails loudly instead of silently using the literal.
///
/// Only the kinds heph actually **resolves** are reserved — today, just `read`.
/// `${FOO:-default}`, `${OUT}`, `${src:0:3}` (bash substring expansion),
/// `${env:FOO}` (several tools' own template syntax) and `sed 's/${/X/'` all have
/// to survive being written in an ordinary field: heph expands none of them, so
/// none can be *misread* as something it does, and a reservation that swallowed
/// them would break BUILD files with nothing to do with this feature.
pub fn reject_deferred_reference(s: &str) -> anyhow::Result<()> {
    let Some(r) = hcore::template::first_deferred_ref(s) else {
        return Ok(());
    };
    anyhow::bail!(
        "`{}` is a deferred value reference, and this field does not accept one. A reference may \
         only appear in a field a driver has typed as accepting it — everything that decides \
         graph shape or identity (`deps`, `tools`, `runner`, `out`, `name`, a glob, an address \
         filter) is deliberately not one",
        r.raw
    )
}

impl FromSpecValue for String {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let s = parse_string(v)?
            .ok_or_else(|| anyhow::anyhow!("invalid: expected string, got null"))?;
        reject_deferred_reference(&s)?;
        Ok(s)
    }

    fn spec_param_type() -> ParamType {
        ParamType::String
    }
}

/// Any decodable type becomes optional for free: absent or null is `None`.
///
/// Blanket rather than one impl per `Option<T>`, so a nested config struct can
/// have an optional field without the leaf type having to know about it.
impl<T: FromSpecValue> FromSpecValue for Option<T> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        // `T::from_spec_value` carries whatever gate `T` has — for `String` that
        // is the deferred-reference reservation — so an optional field is
        // protected by exactly the same rule as a required one, for free.
        match v {
            Value::Null() => Ok(None),
            other => Ok(Some(T::from_spec_value(other)?)),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![T::spec_param_type(), ParamType::Null])
    }
}

/// A `{name: value}` map that keeps its keys in order.
///
/// Strict where [`HashMap<String, String>`] is lenient: a bare string does not
/// become `{"": s}`. The ordered form is used where the map *is* the document —
/// a credential presentation's `env`, say — and a nameless entry has no meaning
/// there.
impl FromSpecValue for std::collections::BTreeMap<String, String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        ordered_map(v, String::from_spec_value)
    }

    fn spec_param_type() -> ParamType {
        ParamType::map(ParamType::String)
    }
}

/// The same map, whose **values** may carry a deferred reference.
///
/// This is the whole of what makes a field deferrable — the difference between
/// a credential's `present.env`, where heph substitutes, and its `audience`,
/// where the text would reach the tool verbatim, is one type parameter rather
/// than a rule someone follows.
impl FromSpecValue for std::collections::BTreeMap<String, crate::driver::Deferred<String>> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        ordered_map(v, crate::driver::Deferred::<String>::from_spec_value)
    }

    fn spec_param_type() -> ParamType {
        ParamType::map(ParamType::String)
    }
}

/// A `{string: T}` map, keys checked and order preserved.
///
/// Keys are never templates in any driver — the host's walk does not recurse
/// into them, so a reference there would never be substituted and would become a
/// variable literally named `${read://a:b}`. Refused here rather than per
/// driver.
fn ordered_map<T>(
    v: &Value,
    value: impl Fn(&Value) -> anyhow::Result<T>,
) -> anyhow::Result<std::collections::BTreeMap<String, T>> {
    match v {
        Value::Map(m) => m
            .iter()
            .map(|(k, v)| {
                reject_deferred_reference(k)?;
                Ok((k.clone(), value(v)?))
            })
            .collect(),
        other => anyhow::bail!("invalid: expected {{string: string}}, got: {other:?}"),
    }
}

/// The one field type that accepts a deferred reference.
///
/// `spec_param_type` stays [`ParamType::String`] deliberately: to a BUILD-file
/// author and to the LSP this *is* a string field, and it accepts every string it
/// did before. What changed is only that one more kind of string now means
/// something.
impl FromSpecValue for crate::driver::Deferred<String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let s = parse_string(v)?
            .ok_or_else(|| anyhow::anyhow!("invalid: expected string, got null"))?;
        Ok(crate::driver::Deferred::new(s))
    }

    fn spec_param_type() -> ParamType {
        ParamType::String
    }
}

/// Opts into references, so no reservation applies: this is one of the two types
/// that accept one.
impl FromSpecValue for Vec<crate::driver::Deferred<String>> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        Ok(parse_strings(v)?
            .into_iter()
            .map(crate::driver::Deferred::new)
            .collect())
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)])
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
        let out = parse_strings(v)?;
        for s in &out {
            reject_deferred_reference(s)?;
        }
        Ok(out)
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)])
    }
}

impl FromSpecValue for std::collections::HashMap<String, Vec<String>> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let out = parse_map_string_strings(v)?;
        // Keys as well as values: a dep group or an environment variable
        // *named* `${read://a:b}` is a typo, and the host's walk does not recurse
        // into keys, so nothing would ever resolve it.
        for s in out.keys().chain(out.values().flatten()) {
            reject_deferred_reference(s)?;
        }
        Ok(out)
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

impl FromSpecValue
    for std::collections::HashMap<String, std::collections::HashMap<String, Vec<String>>>
{
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let out = parse_map_string_map_string_strings(v)?;
        for (k, inner) in &out {
            reject_deferred_reference(k)?;
            for s in inner.keys().chain(inner.values().flatten()) {
                reject_deferred_reference(s)?;
            }
        }
        Ok(out)
    }

    fn spec_param_type() -> ParamType {
        let str_or_list =
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
        ParamType::map(ParamType::map(str_or_list))
    }
}

impl FromSpecValue for std::collections::HashMap<String, String> {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        let out = parse_map_string_string(v)?;
        for s in out.keys().chain(out.values()) {
            reject_deferred_reference(s)?;
        }
        Ok(out)
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

    /// The per-field gate, in the one place that gives it to every driver at
    /// once. A reference in an ordinary field is a parse error, not a literal.
    #[test]
    fn a_deferred_reference_is_rejected_in_an_ordinary_field() {
        let err = String::from_spec_value(&Value::String("${read://infra:role-arn}".to_string()))
            .expect_err("must reject");
        let msg = format!("{err:#}");
        assert!(msg.contains("deferred value reference"), "{msg}");

        // …and in the container shapes too, or the gate would cover `run` and
        // miss `deps`.
        Vec::<String>::from_spec_value(&Value::List(vec![Value::String(
            "${read://a:b}".to_string(),
        )]))
        .expect_err("a list element");
        std::collections::HashMap::<String, String>::from_spec_value(&Value::Map(HashMap::from([
            ("k".to_string(), Value::String("${read://a:b}".to_string())),
        ])))
        .expect_err("a map value");
        // Keys too: the host's walk does not recurse into them, so a reference
        // there would never be resolved and would become a variable literally
        // named `${read://a:b}`.
        std::collections::HashMap::<String, String>::from_spec_value(&Value::Map(HashMap::from([
            ("${read://a:b}".to_string(), Value::String("1".to_string())),
        ])))
        .expect_err("a map key");
    }

    /// An unterminated `${` is a shell program's business. Refusing it in every
    /// string field would fail a `run` containing `sed 's/${/X/'`, which has
    /// nothing to do with deferred values.
    #[test]
    fn a_string_that_does_not_tokenize_is_left_alone() {
        String::from_spec_value(&Value::String("sed 's/${/X/'".to_string()))
            .expect("a shell program is not heph's business");
    }

    /// An **unknown** kind is left alone: every shell construct has to survive
    /// being written in an ordinary field, because heph expands nothing there.
    #[test]
    fn an_unrelated_dollar_brace_is_not_reserved() {
        for ok in [
            "${OUT}",
            "${SRC_CFG}",
            "${FOO:-default}",
            "$$literal",
            "plain",
        ] {
            String::from_spec_value(&Value::String(ok.to_string()))
                .unwrap_or_else(|e| panic!("{ok} must still parse: {e:#}"));
        }
    }

    /// `Deferred<String>` is the one type that accepts one — and it accepts every
    /// string it did before, so retyping a field breaks no BUILD file that was
    /// not already using a reference.
    #[test]
    fn deferred_accepts_both_a_reference_and_a_literal() {
        use crate::driver::Deferred;
        let r = Deferred::<String>::from_spec_value(&Value::String("${read://a:b}".to_string()))
            .expect("a reference");
        assert_eq!(r.raw(), "${read://a:b}");
        let l = Deferred::<String>::from_spec_value(&Value::String("plain".to_string()))
            .expect("a literal");
        assert_eq!(l.raw(), "plain");
        // To the LSP it is still a string field.
        assert_eq!(Deferred::<String>::spec_param_type(), ParamType::String);
    }

    /// The invariant that makes retyping an existing field safe: the def hash
    /// must not move for a target that uses no reference. Otherwise every target
    /// of that driver invalidates on upgrade, and a mixed fleet double-populates
    /// the shared remote for the whole workspace.
    #[test]
    fn a_deferred_field_hashes_identically_to_the_string_it_replaced() {
        use crate::driver::Deferred;
        use std::hash::{Hash as _, Hasher as _};
        let digest = |h: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
            let mut s = std::collections::hash_map::DefaultHasher::new();
            h(&mut s);
            s.finish()
        };
        let plain = digest(&|s| "echo".to_string().hash(s));
        let deferred = digest(&|s| Deferred::<String>::new("echo").hash(s));
        assert_eq!(plain, deferred);
    }

    /// The whole-driver gate is derived from the field types, so that "one field
    /// type changes and nothing else" is true. A separate attribute would be a
    /// second thing to remember, and forgetting it fails the way this feature
    /// exists to stop: the host refuses a reference and tells the author their
    /// driver does not take one, when it plainly does.
    #[test]
    fn the_derive_reports_whether_any_field_is_deferrable() {
        use crate::driver::Deferred;

        #[derive(Spec)]
        struct Plain {
            run: Vec<String>,
        }
        #[derive(Spec)]
        struct One {
            role: Deferred<String>,
        }
        #[derive(Spec)]
        struct InAList {
            run: Vec<Deferred<String>>,
        }

        assert!(!Plain::schema().accepts_deferred);
        assert!(One::schema().accepts_deferred);
        assert!(InAList::schema().accepts_deferred);
        // …and the field is still an ordinary string to the LSP.
        let one = One::schema();
        let by_name: HashMap<&str, &DriverField> =
            one.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["role"].ty, ParamType::String);

        // The same derive decodes them, so the schema flag and the decoder
        // cannot disagree about which fields take a reference.
        let plain = Plain::from(&HashMap::from([(
            "run".to_string(),
            Value::List(vec![Value::String("echo".to_string())]),
        )]))
        .expect("a literal");
        assert_eq!(plain.run, ["echo"]);
        let one = One::from(&HashMap::from([(
            "role".to_string(),
            Value::String("${read://a:b}".to_string()),
        )]))
        .expect("a reference");
        assert_eq!(one.role.raw(), "${read://a:b}");
        let in_a_list = InAList::from(&HashMap::from([(
            "run".to_string(),
            Value::List(vec![Value::String("${read://a:b}".to_string())]),
        )]))
        .expect("a reference in a list");
        assert_eq!(in_a_list.run[0].raw(), "${read://a:b}");
    }

    #[test]
    fn parses_fields_and_applies_defaults() {
        let spec = DemoSpec::from(&HashMap::from([
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
        let spec = DemoSpec::from(&HashMap::from([
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
        let err = DemoSpec::from(&HashMap::from([
            ("run".to_string(), Value::String("x".to_string())),
            ("mode".to_string(), Value::String("off".to_string())),
            ("bogus".to_string(), Value::Bool(true)),
        ]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown entries"), "{err:#}");
    }

    #[test]
    fn parse_error_carries_field_context() {
        let err = DemoSpec::from(&HashMap::from([
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
        let err = ReqSpec::from(&HashMap::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing required `name`"),
            "{err:#}"
        );
        let spec = ReqSpec::from(&HashMap::from([(
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

    /// `from` borrows: the caller keeps its config and may parse the same map
    /// again. This is what lets every driver's `parse` decode straight out of
    /// the shared `Arc<TargetSpec>` instead of deep-cloning the config first —
    /// a clone that, for a `go_compile` target, copied one entry per transitive
    /// lib on every target of every run. A signature that took the map by value
    /// would fail to compile here.
    #[test]
    fn from_borrows_the_config_and_leaves_it_reusable() {
        let config = HashMap::from([
            ("run".to_string(), Value::String("echo".to_string())),
            ("mode".to_string(), Value::String("on".to_string())),
        ]);

        let first = DemoSpec::from(&config).expect("first parse");
        let second = DemoSpec::from(&config).expect("second parse from the same map");

        assert_eq!(first.run, second.run);
        assert_eq!(first.mode, second.mode);
        // The source map is untouched — `from` never drains it.
        assert_eq!(config.len(), 2);
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
