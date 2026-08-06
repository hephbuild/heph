//! The shared `rename` target attribute.
//!
//! A driver that lets a target place its selected outputs exposes a `rename`
//! field of type [`Rename`]. It accepts either form:
//!
//! ```python
//! rename = "bin/myserver"                        # the selected output goes here
//! rename = {"app/build/out/server": "bin/srv"}   # these exact paths go here
//! ```
//!
//! The string form exists so an author never has to know where a dependency
//! emitted its outputs: it renames whatever survives `include`/`exclude`, which
//! must be exactly one file. The dict form is the precise escape hatch for
//! moving several files at once, and its keys are matched exactly against
//! emitted paths.
//!
//! Defined here, next to [`TargetSpecCache`](super::TargetSpecCache), so every
//! driver that grows a `rename` knob parses it identically.

use hcore::hartifactcontent::Rename;
use hcore::htvalue::Value;
use hcore::htvalue::signature::ParamType;

use crate::htspec::FromSpecValue;

impl FromSpecValue for Rename {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        match v {
            Value::String(s) => Ok(Rename::Sole(s.clone())),
            Value::Map(m) => {
                let mut out = std::collections::BTreeMap::new();
                for (k, val) in m {
                    let Value::String(dst) = val else {
                        anyhow::bail!("`rename` dict values must be strings; key '{k}' is not");
                    };
                    out.insert(k.clone(), dst.clone());
                }
                Ok(Rename::Exact(out))
            }
            _ => anyhow::bail!(
                "`rename` must be a string (the destination for the single selected \
                 output) or a dict of exact source path -> destination"
            ),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse(v: Value) -> anyhow::Result<Rename> {
        Rename::from_spec_value(&v)
    }

    #[test]
    fn absent_is_the_default() {
        assert_eq!(Rename::default(), Rename::None);
        assert!(Rename::default().is_none());
    }

    #[test]
    fn string_form_parses_as_sole() {
        assert_eq!(
            parse(Value::String("bin/myserver".into())).unwrap(),
            Rename::Sole("bin/myserver".into())
        );
    }

    #[test]
    fn dict_form_parses_as_exact() {
        let v = Value::Map(
            [("a/x".to_string(), Value::String("out/x".into()))]
                .into_iter()
                .collect(),
        );
        assert_eq!(
            parse(v).unwrap(),
            Rename::Exact(BTreeMap::from([("a/x".to_string(), "out/x".to_string())]))
        );
    }

    #[test]
    fn empty_dict_is_an_empty_exact_map() {
        assert_eq!(
            parse(Value::Map(Default::default())).unwrap(),
            Rename::Exact(BTreeMap::new())
        );
    }

    #[test]
    fn non_string_dict_value_names_the_offending_key() {
        let v = Value::Map(
            [("a/x".to_string(), Value::Bool(true))]
                .into_iter()
                .collect(),
        );
        let err = parse(v).expect_err("must reject a non-string destination");
        assert!(format!("{err:#}").contains("a/x"));
    }

    #[test]
    fn other_shapes_are_rejected() {
        assert!(parse(Value::Bool(true)).is_err());
        assert!(parse(Value::List(vec![])).is_err());
    }

    /// The schema drives LSP completion, so it must advertise both forms.
    #[test]
    fn param_type_is_the_union_of_both_forms() {
        assert_eq!(
            Rename::spec_param_type(),
            ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
        );
    }
}
