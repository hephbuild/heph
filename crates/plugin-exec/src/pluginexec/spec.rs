use hplugin::driver::targetdef::path::CodegenMode;
use hplugin::htspec::{Spec, TargetSpecCache};
use std::collections::HashMap;

/// Exec-driver target config. `#[derive(Spec)]` generates `TargetSpec::from`
/// (parser) and `TargetSpec::schema` (LSP schema) from these fields, so the two
/// can never drift; doc comments below become the schema docs. `codegen` is a
/// [`SpecEnum`](hplugin::htspec::SpecEnum); `cache` is the shared
/// [`TargetSpecCache`] knob (a bool shorthand or a `{enabled, remote, history}`
/// dict).
#[derive(Spec)]
pub(crate) struct TargetSpec {
    /// Command to execute, as an argv list. `$OUT`, `$SRC_<group>`, `$TOOL_<group>` and declared env vars are available.
    pub run: Vec<String>,
    /// Hashed + runtime dependencies, grouped by name → list of target addresses.
    pub deps: HashMap<String, Vec<String>>,
    /// Dependencies that contribute to the input hash but are not materialized at runtime.
    pub hash_deps: HashMap<String, Vec<String>>,
    /// Dependencies materialized at runtime but excluded from the input hash.
    pub runtime_deps: HashMap<String, Vec<String>>,
    /// Names of `deps`/`runtime_deps` groups staged **read-only**: materialized
    /// once into the shared stage and hardlinked into each sandbox instead of
    /// byte-copied per consumer. Use for deps a target only reads and never
    /// mutates (e.g. a hermetic toolchain). No effect under the FUSE sandbox,
    /// which already shares inputs lazily.
    pub read_only_deps: Vec<String>,
    /// Scratch caches this target uses, as a list of `scratch` target addresses.
    /// Each is mounted at the path *its declaration* names, and its declared
    /// `env` variable is set for the run. A scratch is not an input: it is never
    /// materialized from artifacts and never affects this target's cache key.
    pub scratch: Vec<String>,
    /// Build tools, grouped by name → list of target addresses; symlinked under `tools/`.
    pub tools: HashMap<String, Vec<String>>,
    /// Declared outputs, grouped by name → list of output paths the target writes.
    #[spec(rename = "out")]
    pub outputs: HashMap<String, Vec<String>>,
    /// Extra files materialized into the sandbox but not hashed as deps.
    pub support_files: Vec<String>,
    /// Codegen mode: `copy` or `in_place`. Omit for a normal (non-codegen) target.
    pub codegen: CodegenMode,
    /// Caching: bool toggles local+remote, or a dict `{enabled, remote, history}`.
    pub cache: TargetSpecCache,
    /// Environment variables set for the command.
    pub env: HashMap<String, String>,
    /// Names of host environment variables passed through (hashed). `"*"` passes all.
    pub pass_env: Vec<String>,
    /// Host env vars passed through at runtime only (not hashed). `"*"` passes all.
    pub runtime_pass_env: Vec<String>,
    /// Environment variables set at runtime only (not hashed).
    pub runtime_env: HashMap<String, String>,
    /// Credentials this target needs, as name → `secret` target address.
    ///
    /// The name is what the command references as `$SECRET_<NAME>`, what a
    /// shape's variables are derived from, and what appears in
    /// `«redacted:NAME»`. The address names a `secret` target whose declaration
    /// says how to obtain the value; nothing about the value itself is written
    /// here or anywhere else in the build graph.
    ///
    /// Each becomes a `hashed: true, runtime: false` input — the `hash_deps`
    /// shape — so the *identity* a target builds under reaches its cache key
    /// while the value never does.
    pub secrets: HashMap<String, String>,
    /// Exec runner this command runs under: a target address producing a
    /// `runner.json`. Absent (or the literal `"local"`) spawns on this host as
    /// before. The runner is a hashed dependency, so changing it changes this
    /// target's cache key.
    pub runner: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::Value;
    use hcore::htvalue::signature::ParamType;
    use hplugin::driver::DriverField;

    fn make_spec(
        extra: impl IntoIterator<Item = (&'static str, Value)>,
    ) -> anyhow::Result<TargetSpec> {
        let mut m = HashMap::from([("run".to_string(), Value::String("echo".to_string()))]);
        m.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        TargetSpec::from(&m)
    }

    #[test]
    fn test_schema_lists_known_fields_with_types() {
        let schema = TargetSpec::schema();
        let by_name: HashMap<&str, &DriverField> =
            schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        // Every config key TargetSpec::from understands must appear in the schema,
        // so the two lists are caught drifting apart.
        for key in [
            "run",
            "out",
            "cache",
            "support_files",
            "codegen",
            "deps",
            "hash_deps",
            "runtime_deps",
            "read_only_deps",
            "tools",
            "env",
            "pass_env",
            "runtime_pass_env",
            "runtime_env",
            "runner",
            "secrets",
        ] {
            assert!(by_name.contains_key(key), "schema missing field `{key}`");
        }
        // Types are unions mirroring the `parse_*` helpers.
        let str_or_list =
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]);
        assert_eq!(by_name["run"].ty, str_or_list);
        assert_eq!(by_name["run"].ty.render(), "string | list[string]");
        assert_eq!(
            by_name["deps"].ty,
            ParamType::union(vec![
                ParamType::String,
                ParamType::list(ParamType::String),
                ParamType::map(str_or_list.clone()),
            ])
        );
        assert_eq!(
            by_name["env"].ty,
            ParamType::union(vec![ParamType::String, ParamType::map(ParamType::String)])
        );
        assert!(!by_name["run"].doc.is_empty());
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
        // `cache` dict is a `SpecStruct`: unknown keys are rejected with the
        // generic "unknown entries found" message.
        assert!(
            format!("{err:#}").contains("unknown entries found"),
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

    #[test]
    fn test_codegen_in_place_parsed() {
        let spec = make_spec([("codegen", Value::String("in_place".to_string()))]).unwrap();
        assert_eq!(spec.codegen, CodegenMode::InPlace);
    }

    #[test]
    fn test_codegen_copy_parsed() {
        let spec = make_spec([("codegen", Value::String("copy".to_string()))]).unwrap();
        assert_eq!(spec.codegen, CodegenMode::Copy);
    }

    #[test]
    fn test_codegen_link_now_errors() {
        let err = match make_spec([("codegen", Value::String("link".to_string()))]) {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        // `codegen` is now a `SpecEnum`: an unknown string lists the valid
        // variants and the field name (from the `parse \`codegen\`` context).
        let msg = format!("{err:#}");
        assert!(msg.contains("codegen"), "{msg}");
        assert!(msg.contains("expected one of"), "{msg}");
        assert!(msg.contains("link"), "{msg}");
    }

    #[test]
    fn test_codegen_default_none() {
        let spec = make_spec([]).unwrap();
        assert_eq!(spec.codegen, CodegenMode::None);
    }
}
