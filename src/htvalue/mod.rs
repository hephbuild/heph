use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod signature;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Bool(bool),
    Float(f64),
    Int(i64),
    Uint(u64),
    Null(),
    Map(HashMap<String, Value>),
    List(Vec<Value>),
}

pub fn parse_string(v: &Value) -> anyhow::Result<Option<String>> {
    match v {
        Value::Null() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        v => Err(anyhow::anyhow!("invalid: expected string, got: {:?}", v)),
    }
}

pub fn parse_strings(v: &Value) -> anyhow::Result<Vec<String>> {
    match v {
        Value::Null() => Ok(vec![]),
        Value::List(v) => v
            .iter()
            .try_fold(Vec::new(), |mut acc, v| match parse_string(v)? {
                None => Ok(acc),
                Some(s) => {
                    acc.push(s);

                    Ok(acc)
                }
            }),
        Value::String(s) => Ok(vec![s.clone()]),
        v => Err(anyhow::anyhow!(
            "invalid: expected string or [string], got: {:?}",
            v
        )),
    }
}

pub fn parse_map_string_strings(v: &Value) -> anyhow::Result<HashMap<String, Vec<String>>> {
    Ok(if let Ok(ss) = parse_strings(v) {
        HashMap::from([("".to_string(), ss)])
    } else {
        match v {
            Value::Map(m) => m
                .iter()
                .map(|(k, v)| parse_strings(v).map(|ss| (k.clone(), ss)))
                .collect::<anyhow::Result<HashMap<_, _>>>(),
            v => Err(anyhow::anyhow!(
                "invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {:?}",
                v
            )),
        }?
    })
}

pub fn parse_map_string_string(v: &Value) -> anyhow::Result<HashMap<String, String>> {
    Ok(if let Ok(ss) = parse_string(v) {
        if let Some(ss) = ss {
            HashMap::from([("".to_string(), ss)])
        } else {
            HashMap::new()
        }
    } else {
        match v {
            Value::Map(m) => m
                .iter()
                .filter_map(|(k, v)| match parse_string(v) {
                    Ok(Some(ss)) => Some(Ok((k.clone(), ss))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                })
                .collect::<anyhow::Result<HashMap<_, _>>>(),
            v => Err(anyhow::anyhow!(
                "invalid: expected string, [string], {{string: string}} or {{string: [string]}} got: {:?}",
                v
            )),
        }?
    })
}

pub fn parse_bool(v: &Value) -> anyhow::Result<bool> {
    match v {
        Value::Null() => Ok(false),
        Value::Bool(b) => Ok(*b),
        _ => Err(anyhow::anyhow!("invalid: expected bool, got: {:?}", v)),
    }
}
