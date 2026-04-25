use std::collections::HashMap;
use crate::engine::provider::TargetSpecValue;
use crate::engine::provider::TargetSpecValue::Null;

pub(crate) struct TargetSpec {
    pub run: Vec<String>,
    pub outputs: HashMap<String, Vec<String>>,
    pub cache: TargetSpecCache,
}

pub(crate) struct TargetSpecCache {
    pub local: bool,
    pub remote: bool,
}

fn parse_strings(v: &TargetSpecValue) -> anyhow::Result<Vec<String>> {
    match v {
        Null() => Ok(vec![]),
        TargetSpecValue::List(v) => v.iter().try_fold(Vec::new(), |mut acc, v| match v {
            Null() => Ok(acc),
            TargetSpecValue::String(s) => {
                acc.push(s.clone());

                Ok(acc)
            },
            v => Err(anyhow::anyhow!("invalid: {:?}", v)),
        }),
        TargetSpecValue::String(s) => Ok(vec![s.clone()]),
        v => Err(anyhow::anyhow!("invalid: expected string or [string], got: {:?}", v)),
    }
}

fn parse_bool(v: &TargetSpecValue) -> anyhow::Result<bool> {
    match v {
        Null() => Ok(false),
        TargetSpecValue::Bool(b) => Ok(*b),
        _ => Err(anyhow::anyhow!("invalid: expected bool, got: {:?}", v)),
    }
}

impl TargetSpec {
    pub fn from(m: HashMap<String, TargetSpecValue>) -> anyhow::Result<TargetSpec> {
        let mut spec = TargetSpec{
            run: vec![],
            cache: TargetSpecCache{
                local: true,
                remote: true,
            },
            outputs: HashMap::new(),
        };

        if let Some(v) = m.get("run") {
            spec.run = parse_strings(v)?;
        };

        if let Some(v) = m.get("cache")
            && !parse_bool(v)? {
            spec.cache = TargetSpecCache{
                local: false,
                remote: false,
            };
        }

        if let Some(v) = m.get("out") {
            if let Ok(ss) = parse_strings(v) {
                spec.outputs = HashMap::from([("".to_string(), ss)]);
            } else {
                spec.outputs = match v {
                    TargetSpecValue::Map(m) => m.iter()
                        .map(|(k, v)| parse_strings(v).map(|ss| (k.clone(), ss)))
                        .collect::<anyhow::Result<HashMap<_, _>>>(),
                    v => Err(anyhow::anyhow!("invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {:?}", v)),
                }?;
            }
        };

        Ok(spec)
    }
}
