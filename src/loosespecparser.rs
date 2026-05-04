use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum TargetSpecValue {
    String(String),
    Bool(bool),
    Float(f64),
    Int(i64),
    Uint(u64),
    Null(),
    Map(HashMap<String, TargetSpecValue>),
    List(Vec<TargetSpecValue>),
}

pub fn parse_strings(v: &TargetSpecValue) -> anyhow::Result<Vec<String>> {
    match v {
        TargetSpecValue::Null() => Ok(vec![]),
        TargetSpecValue::List(v) => v.iter().try_fold(Vec::new(), |mut acc, v| match v {
            TargetSpecValue::Null() => Ok(acc),
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

pub fn parse_map_string_strings(v: &TargetSpecValue) -> anyhow::Result<HashMap<String, Vec<String>>> {
    Ok(if let Ok(ss) = parse_strings(v) {
        HashMap::from([("".to_string(), ss)])
    } else {
        match v {
            TargetSpecValue::Map(m) => m.iter()
                .map(|(k, v)| parse_strings(v).map(|ss| (k.clone(), ss)))
                .collect::<anyhow::Result<HashMap<_, _>>>(),
            v => Err(anyhow::anyhow!("invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {:?}", v)),
        }?
    })
}

pub fn parse_bool(v: &TargetSpecValue) -> anyhow::Result<bool> {
    match v {
        TargetSpecValue::Null() => Ok(false),
        TargetSpecValue::Bool(b) => Ok(*b),
        _ => Err(anyhow::anyhow!("invalid: expected bool, got: {:?}", v)),
    }
}