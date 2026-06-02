use crate::engine::driver::targetdef::path::CodegenMode;
use crate::htvalue::{
    Value, parse_bool, parse_map_string_string, parse_map_string_strings, parse_string,
    parse_strings,
};
use anyhow::Context;
use std::collections::HashMap;

pub(crate) struct TargetSpec {
    pub run: Vec<String>,
    pub deps: HashMap<String, Vec<String>>,
    pub hash_deps: HashMap<String, Vec<String>>,
    pub runtime_deps: HashMap<String, Vec<String>>,
    pub tools: HashMap<String, Vec<String>>,
    pub outputs: HashMap<String, Vec<String>>,
    pub support_files: Vec<String>,
    pub codegen: CodegenMode,
    pub cache: TargetSpecCache,
    pub env: HashMap<String, String>,
    pub pass_env: Vec<String>,
    pub runtime_pass_env: Vec<String>,
    pub runtime_env: HashMap<String, String>,
}

pub(crate) struct TargetSpecCache {
    pub local: bool,
    pub remote: bool,
    /// How many cache revisions to retain for this target. Default 1.
    pub history: u32,
}

/// Parse the `cache` attribute, which accepts either a bare bool (legacy:
/// `cache = True/False` toggles both local and remote, history 1) or a dict
/// `cache = {"enabled": bool, "remote": bool, "history": int}` where any unset
/// key falls back to its default (enabled/remote true, history 1).
fn parse_cache(v: &Value) -> anyhow::Result<TargetSpecCache> {
    match v {
        Value::Bool(b) => Ok(TargetSpecCache {
            local: *b,
            remote: *b,
            history: 1,
        }),
        Value::Map(m) => {
            let mut c = TargetSpecCache {
                local: true,
                remote: true,
                history: 1,
            };
            for (k, val) in m {
                match k.as_str() {
                    "enabled" => {
                        c.local = parse_bool(val).with_context(|| "parse `cache.enabled`")?
                    }
                    "remote" => {
                        c.remote = parse_bool(val).with_context(|| "parse `cache.remote`")?
                    }
                    "history" => c.history = parse_cache_history(val)?,
                    other => anyhow::bail!("unknown `cache` entry: {other:?}"),
                }
            }
            Ok(c)
        }
        _ => anyhow::bail!("`cache` must be a bool or a dict"),
    }
}

fn parse_cache_history(v: &Value) -> anyhow::Result<u32> {
    let n: i64 = match v {
        Value::Int(i) => *i,
        Value::Uint(u) => i64::try_from(*u).context("`cache.history` too large")?,
        _ => anyhow::bail!("`cache.history` must be an integer"),
    };
    if n < 1 {
        anyhow::bail!("`cache.history` must be >= 1, got {n}");
    }
    u32::try_from(n).context("`cache.history` too large")
}

impl TargetSpec {
    pub fn from(m: HashMap<String, Value>) -> anyhow::Result<TargetSpec> {
        let mut m: HashMap<&str, &Value> = m.iter().map(|(k, v)| (k.as_str(), v)).collect();

        let mut spec = TargetSpec {
            run: vec![],
            cache: TargetSpecCache {
                local: true,
                remote: true,
                history: 1,
            },
            outputs: HashMap::new(),
            support_files: vec![],
            codegen: CodegenMode::None,
            deps: HashMap::new(),
            hash_deps: HashMap::new(),
            runtime_deps: HashMap::new(),
            tools: HashMap::new(),
            env: HashMap::new(),
            pass_env: vec![],
            runtime_pass_env: vec![],
            runtime_env: HashMap::new(),
        };

        if let Some(v) = m.remove("run") {
            spec.run = parse_strings(v).with_context(|| "parse `run`")?;
        };

        if let Some(v) = m.remove("cache") {
            spec.cache = parse_cache(v).with_context(|| "parse `cache`")?;
        }

        if let Some(v) = m.remove("out") {
            spec.outputs = parse_map_string_strings(v).with_context(|| "parse `out`")?;
        };

        if let Some(v) = m.remove("support_files") {
            spec.support_files = parse_strings(v).with_context(|| "parse `support_files`")?;
        };

        if let Some(v) = m.remove("codegen") {
            spec.codegen = match parse_string(v)? {
                Some(s) => match s.as_str() {
                    "copy" => Ok(CodegenMode::Copy),
                    "link" => Ok(CodegenMode::Link),
                    _ => Err(anyhow::anyhow!("invalid codegen mode: {}", s)),
                },
                None => Ok(CodegenMode::None),
            }
            .with_context(|| "parse `codegen`")?;
        };

        if let Some(v) = m.remove("deps") {
            spec.deps = parse_map_string_strings(v).with_context(|| "parse `deps`")?;
        };

        if let Some(v) = m.remove("hash_deps") {
            spec.hash_deps = parse_map_string_strings(v).with_context(|| "parse `hash_deps`")?;
        };

        if let Some(v) = m.remove("runtime_deps") {
            spec.runtime_deps =
                parse_map_string_strings(v).with_context(|| "parse `runtime_deps`")?;
        };

        if let Some(v) = m.remove("tools") {
            spec.tools = parse_map_string_strings(v).with_context(|| "parse `tools`")?;
        };

        if let Some(v) = m.remove("env") {
            spec.env = parse_map_string_string(v).with_context(|| "parse `env`")?;
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
            anyhow::bail!("unknown entries found: {:?}", unknown_keys)
        }

        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htvalue::Value;

    fn make_spec(
        extra: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> anyhow::Result<TargetSpec> {
        let mut m = HashMap::from([("run".to_string(), Value::String("echo".to_string()))]);
        m.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        TargetSpec::from(m)
    }

    #[test]
    fn test_cache_bool_true() {
        let spec = make_spec([("cache", Value::Bool(true))]).unwrap();
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_bool_false_disables_both() {
        let spec = make_spec([("cache", Value::Bool(false))]).unwrap();
        assert!(!spec.cache.local);
        assert!(!spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_default_when_absent() {
        let spec = make_spec([]).unwrap();
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_dict_partial_keys_use_defaults() {
        // Only `remote` set — `enabled` and `history` fall back to defaults.
        let spec = make_spec([(
            "cache",
            Value::Map(HashMap::from([("remote".to_string(), Value::Bool(false))])),
        )])
        .unwrap();
        assert!(spec.cache.local, "enabled defaults to true");
        assert!(!spec.cache.remote);
        assert_eq!(spec.cache.history, 1);
    }

    #[test]
    fn test_cache_dict_history_honored() {
        let spec = make_spec([(
            "cache",
            Value::Map(HashMap::from([("history".to_string(), Value::Int(5))])),
        )])
        .unwrap();
        assert_eq!(spec.cache.history, 5);
        assert!(spec.cache.local);
        assert!(spec.cache.remote);
    }

    #[test]
    fn test_cache_dict_unknown_key_errors() {
        let err = match make_spec([(
            "cache",
            Value::Map(HashMap::from([("bogus".to_string(), Value::Bool(true))])),
        )]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("unknown `cache` entry"),
            "{err:#}"
        );
    }

    #[test]
    fn test_cache_dict_zero_history_errors() {
        let err = match make_spec([(
            "cache",
            Value::Map(HashMap::from([("history".to_string(), Value::Int(0))])),
        )]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains(">= 1"), "{err:#}");
    }

    #[test]
    fn test_pass_env_parsed() {
        let spec = make_spec([(
            "pass_env",
            Value::List(vec![
                Value::String("VAR1".to_string()),
                Value::String("VAR2".to_string()),
            ]),
        )])
        .unwrap();
        assert_eq!(spec.pass_env, vec!["VAR1", "VAR2"]);
    }

    #[test]
    fn test_runtime_pass_env_parsed() {
        let spec = make_spec([(
            "runtime_pass_env",
            Value::List(vec![Value::String("HOME".to_string())]),
        )])
        .unwrap();
        assert_eq!(spec.runtime_pass_env, vec!["HOME"]);
    }

    #[test]
    fn test_runtime_env_parsed() {
        let spec = make_spec([(
            "runtime_env",
            Value::Map(HashMap::from([(
                "KEY".to_string(),
                Value::String("value".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(spec.runtime_env.get("KEY"), Some(&"value".to_string()));
    }

    #[test]
    fn test_pass_env_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.pass_env.is_empty());
        assert!(spec.runtime_pass_env.is_empty());
        assert!(spec.runtime_env.is_empty());
    }

    #[test]
    fn test_tools_parsed() {
        let spec = make_spec([(
            "tools",
            Value::List(vec![Value::String("//some:tool".to_string())]),
        )])
        .unwrap();
        assert_eq!(spec.tools.get(""), Some(&vec!["//some:tool".to_string()]));
    }

    #[test]
    fn test_tools_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.tools.is_empty());
    }

    #[test]
    fn test_support_files_parsed() {
        let spec = make_spec([(
            "support_files",
            Value::List(vec![
                Value::String("foo.txt".to_string()),
                Value::String("data/*.json".to_string()),
                Value::String("subdir/".to_string()),
            ]),
        )])
        .unwrap();
        assert_eq!(
            spec.support_files,
            vec!["foo.txt", "data/*.json", "subdir/"]
        );
    }

    #[test]
    fn test_support_files_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.support_files.is_empty());
    }

    #[test]
    fn test_hash_deps_parsed() {
        let spec = make_spec([(
            "hash_deps",
            Value::List(vec![Value::String("//some:hash".to_string())]),
        )])
        .unwrap();
        assert_eq!(
            spec.hash_deps.get(""),
            Some(&vec!["//some:hash".to_string()])
        );
    }

    #[test]
    fn test_runtime_deps_parsed() {
        let spec = make_spec([(
            "runtime_deps",
            Value::Map(HashMap::from([(
                "rt".to_string(),
                Value::String("//some:rt".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(
            spec.runtime_deps.get("rt"),
            Some(&vec!["//some:rt".to_string()])
        );
    }

    #[test]
    fn test_hash_runtime_deps_empty_by_default() {
        let spec = make_spec([]).unwrap();
        assert!(spec.hash_deps.is_empty());
        assert!(spec.runtime_deps.is_empty());
    }

    #[test]
    fn test_tools_grouped() {
        let spec = make_spec([(
            "tools",
            Value::Map(HashMap::from([(
                "cc".to_string(),
                Value::String("//toolchain:gcc".to_string()),
            )])),
        )])
        .unwrap();
        assert_eq!(
            spec.tools.get("cc"),
            Some(&vec!["//toolchain:gcc".to_string()])
        );
    }
}
