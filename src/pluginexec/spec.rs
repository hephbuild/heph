use std::collections::HashMap;
use anyhow::Context;
use crate::loosespecparser::{parse_bool, parse_map_string_strings, parse_strings, TargetSpecValue};

pub(crate) struct TargetSpec {
    pub run: Vec<String>,
    pub deps: HashMap<String, Vec<String>>,
    pub outputs: HashMap<String, Vec<String>>,
    pub cache: TargetSpecCache,
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
        };


        if let Some(v) = m.remove("run") {
            spec.run = parse_strings(v).with_context(|| "parse `run`")?;
        };

        if let Some(v) = m.remove("cache")
            && !parse_bool(v)? {
            spec.cache = TargetSpecCache{
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

        if !m.is_empty() {
            let unknown_keys: Vec<&str> = m.into_keys().collect();
            anyhow::bail!("Unknown entries found: {:?}", unknown_keys)
        }

        Ok(spec)
    }
}
