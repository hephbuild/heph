use std::collections::HashMap;
use anyhow::Context;
use crate::loosespecparser::{parse_bool, parse_map_string_string, parse_map_string_strings, parse_strings, TargetSpecValue};

pub(crate) struct TargetSpec {
    pub run: Vec<String>,
    pub deps: HashMap<String, Vec<String>>,
    pub outputs: HashMap<String, Vec<String>>,
    pub cache: TargetSpecCache,
    pub pass_env: Vec<String>,
    pub runtime_pass_env: Vec<String>,
    pub runtime_env: HashMap<String, String>,
}

pub(crate) struct TargetSpecCache {
    pub local: bool,
    pub remote: bool,
}

impl TargetSpec {
    pub fn from(m: HashMap<String, TargetSpecValue>) -> anyhow::Result<TargetSpec> {
        let mut m: HashMap<&str, &TargetSpecValue> = m
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();

        let mut spec = TargetSpec{
            run: vec![],
            cache: TargetSpecCache{
                local: true,
                remote: true,
            },
            outputs: HashMap::new(),
            deps: HashMap::new(),
            pass_env: vec![],
            runtime_pass_env: vec![],
            runtime_env: HashMap::new(),
        };


        if let Some(v) = m.remove("run") {
            spec.run = parse_strings(v).with_context(|| "parse `run`")?;
        };

        if let Some(v) = m.remove("cache")
            && !parse_bool(v).with_context(|| "parse `cache`")? {
            spec.cache = TargetSpecCache {
                local: false,
                remote: false,
            };
        }

        if let Some(v) = m.remove("out") {
            spec.outputs = parse_map_string_strings(v).with_context(|| "parse `out`")?;
        };

        if let Some(v) = m.remove("deps") {
            spec.deps = parse_map_string_strings(v).with_context(|| "parse `deps`")?;
        };

        if let Some(v) = m.remove("pass_env") {
            spec.pass_env = parse_strings(v).with_context(|| "parse `pass_env`")?;
        };

        if let Some(v) = m.remove("runtime_pass_env") {
            spec.runtime_pass_env = parse_strings(v).with_context(|| "parse `runtime_pass_env`")?;
        };

        if let Some(v) = m.remove("runtime_env") {
            spec.runtime_env = parse_map_string_string(v).with_context(|| "parse `runtime_env`")?;
        };

        if !m.is_empty() {
            let unknown_keys: Vec<&str> = m.into_keys().collect();
            anyhow::bail!("Unknown entries found: {:?}", unknown_keys)
        }

        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loosespecparser::TargetSpecValue;

    fn make_spec(extra: impl IntoIterator<Item = (&'static str, TargetSpecValue)>) -> anyhow::Result<TargetSpec> {
        let mut m = HashMap::from([
            ("run".to_string(), TargetSpecValue::String("echo".to_string())),
        ]);
        m.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        TargetSpec::from(m)
    }

    #[test]
    fn test_pass_env_parsed() {
        let spec = make_spec([("pass_env", TargetSpecValue::List(vec![
            TargetSpecValue::String("VAR1".to_string()),
            TargetSpecValue::String("VAR2".to_string()),
        ]))]).unwrap();
        assert_eq!(spec.pass_env, vec!["VAR1", "VAR2"]);
    }

    #[test]
    fn test_runtime_pass_env_parsed() {
        let spec = make_spec([("runtime_pass_env", TargetSpecValue::List(vec![
            TargetSpecValue::String("HOME".to_string()),
        ]))]).unwrap();
        assert_eq!(spec.runtime_pass_env, vec!["HOME"]);
    }

    #[test]
    fn test_runtime_env_parsed() {
        let spec = make_spec([("runtime_env", TargetSpecValue::Map(HashMap::from([
            ("KEY".to_string(), TargetSpecValue::String("value".to_string())),
        ])))]).unwrap();
        assert_eq!(spec.runtime_env.get("KEY"), Some(&"value".to_string()));
    }

    #[test]
    fn test_pass_env_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.pass_env.is_empty());
        assert!(spec.runtime_pass_env.is_empty());
        assert!(spec.runtime_env.is_empty());
    }
}
