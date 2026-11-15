//! The `.hephconfig` JSON Schema: what an editor completes, hovers and checks
//! keys against.
//!
//! Derived from the serde types in [`crate::config_yaml`], so a key added to
//! the parser is in the schema with no second edit. The types whose
//! `Deserialize` is hand-rolled (an untagged shorthand, a source key chosen at
//! parse time) describe themselves here, by hand, next to nothing else: each
//! `json_schema` below mirrors one `Deserialize` impl, and the schema's tests
//! in this module check every example they offer against the real parser.
//!
//! The schema is advisory. The parser is the authority on what loads, and the
//! LSP publishes the parser's own error, not a schema verdict.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use crate::config_yaml::{
    CONFIG_FILE_NAME, CONFIG_FILE_NAME_LEGACY, ConfigYaml, FuseEnabled, PluginSpec, ResourcesConfig,
};

/// The schema of one `.hephconfig` file (the base or a profile), as JSON
/// Schema 2020-12. Definitions sit under `$defs`, referenced as
/// `#/$defs/<Name>`.
pub fn config_json_schema() -> serde_json::Value {
    schemars::schema_for!(ConfigYaml).to_value()
}

/// Whether `file_name` is a heph config file: the base (`.hephconfig`, or the
/// legacy `.hephconfig2`) or a profile laid over it (`ci.hephconfig`).
pub fn is_config_file_name(file_name: &str) -> bool {
    [CONFIG_FILE_NAME, CONFIG_FILE_NAME_LEGACY]
        .iter()
        .any(|base| file_name.ends_with(base))
}

/// Mirrors `FuseEnabled`'s `Deserialize`: a boolean, or one of its words.
impl JsonSchema for FuseEnabled {
    fn schema_name() -> Cow<'static, str> {
        "FuseEnabled".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "`true`, `false`, or `auto` (decided per target from its inputs).",
            "anyOf": [
                { "type": "boolean" },
                { "enum": ["auto", "on", "off"] }
            ]
        })
    }
}

/// Mirrors `PluginSpec`'s `Deserialize`: exactly one source key, plus
/// `options` and (for `url:`) `checksum`.
impl JsonSchema for PluginSpec {
    fn schema_name() -> Cow<'static, str> {
        "PluginSpec".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "One plugin: exactly one of `builtin`, `path` or `url`, plus its `options`.",
            "type": "object",
            "properties": {
                "builtin": {
                    "type": "string",
                    "description": "A compiled-in plugin, by name (`buildfile`, `exec`, `go`, …)."
                },
                "path": {
                    "type": "string",
                    "description": "A local `*-plugin.json` manifest, relative to the workspace root."
                },
                "url": {
                    "type": "string",
                    "description": "A remote `*-plugin.json` manifest."
                },
                "checksum": {
                    "type": "string",
                    "description": "`sha256:<hex>` of the manifest a `url:` source downloads. Only valid with `url:`."
                },
                "options": {
                    "type": "object",
                    "description": "Passed to the plugin's construction; each plugin checks its own keys."
                }
            },
            "additionalProperties": false
        })
    }
}

/// Mirrors `ResourcesConfig`'s `Deserialize`: pool name to an amount, or to
/// `{unit, value}`. The builtin pools are listed so an editor offers them.
impl JsonSchema for ResourcesConfig {
    fn schema_name() -> Cow<'static, str> {
        "ResourcesConfig".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        let units: Vec<&str> = hcore::capacity::Unit::ALL.map(|u| u.name()).to_vec();
        let amount = serde_json::json!({ "type": ["integer", "number", "string"] });
        let pool = serde_json::json!({
            "anyOf": [
                amount,
                {
                    "type": "object",
                    "properties": {
                        "unit": {
                            "enum": units,
                            "description": "How the pool's amounts are written: `count` (a whole number), `cpu` (CPUs, a decimal), `bytes` (a size such as `24G`)."
                        },
                        "value": {
                            "type": ["integer", "number", "string"],
                            "description": "The pool's capacity. Omitted is unbounded; `0` is set per run (`--resource <pool>=<n>`)."
                        }
                    },
                    "additionalProperties": false
                }
            ]
        });
        let builtin = |description: &str| {
            let mut p = pool.clone();
            if let Some(o) = p.as_object_mut() {
                o.insert("description".into(), description.into());
            }
            p
        };
        json_schema!({
            "description": "Resource pools, by name: an amount, or `{unit, value}`.",
            "type": "object",
            "properties": {
                (hcore::capacity::CPU): builtin("CPUs, or a percentage of the parallelism (`50%`)."),
                (hcore::capacity::MEMORY): builtin("A size (`24G`), or a percentage of the memory this process may use (`80%`).")
            },
            "propertyNames": {
                "pattern": format!("^[a-z][a-z0-9_]{{0,{}}}$", hcore::capacity::MAX_POOL_NAME_LEN - 1)
            },
            "additionalProperties": pool
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_base_legacy_and_profile_names() {
        for name in [
            ".hephconfig",
            ".hephconfig2",
            "ci.hephconfig",
            "ci.hephconfig2",
        ] {
            assert!(is_config_file_name(name), "{name}");
        }
        for name in ["hephconfig", ".hephconfig.bak", "BUILD", ".hephconfig3"] {
            assert!(!is_config_file_name(name), "{name}");
        }
    }

    /// A YAML document that puts a `bogus` key where each closed object of
    /// the schema sits, keyed by that object's JSON pointer (`""` is the
    /// root).
    const BOGUS_AT: &[(&str, &str)] = &[
        ("", "bogus: 1\n"),
        ("/$defs/CacheConfig", "cache:\n  bogus: 1\n"),
        ("/$defs/FsConfig", "fs:\n  bogus: 1\n"),
        ("/$defs/FuseConfig", "fuse:\n  bogus: 1\n"),
        ("/$defs/LockConfig", "lock:\n  bogus: 1\n"),
        // MemCacheConfig's fields are required; the unknown key comes first,
        // so its error is raised before a missing field's would be.
        ("/$defs/MemCacheConfig", "memCache:\n  bogus: 1\n"),
        ("/$defs/PluginSpec", "plugins:\n- bogus: 1\n"),
        (
            "/$defs/RemoteCacheConfigPatch",
            "caches:\n  m:\n    bogus: 1\n",
        ),
        ("/$defs/ScratchConfig", "scratch:\n  bogus: 1\n"),
        ("/$defs/TelemetryConfig", "telemetry:\n  bogus: 1\n"),
        (
            "/$defs/ResourcesConfig/additionalProperties/anyOf/1",
            "resources:\n  gpu:\n    bogus: 1\n",
        ),
    ];

    /// The keys a parser error lists as expected: serde's ``expected `a` ``,
    /// ``expected `a` or `b` `` and ``expected one of `a`, `b` ``, or
    /// PluginSpec's own `(expected builtin/path/url/options/checksum)`.
    fn expected_keys(message: &str) -> Vec<String> {
        let Some((_, rest)) = message.split_once("expected ") else {
            return Vec::new();
        };
        let mut keys: Vec<String> = if rest.contains('`') {
            rest.split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect()
        } else {
            rest.split(')')
                .next()
                .unwrap_or_default()
                .split('/')
                .map(str::to_string)
                .collect()
        };
        keys.sort();
        keys
    }

    /// The schema and the parser agree on every closed object: a key the
    /// schema does not list fails to parse, and the keys the parser then
    /// names as expected are exactly the schema's `properties`. A closed
    /// object with no row in [`BOGUS_AT`] fails the test, so a new one cannot
    /// drift unchecked.
    #[test]
    fn closed_objects_match_the_parser() {
        let schema = config_json_schema();
        let defs = schema["$defs"].as_object().expect("$defs");
        let mut closed: Vec<String> = defs
            .iter()
            .filter(|(_, d)| d["additionalProperties"] == false)
            .map(|(name, _)| format!("/$defs/{name}"))
            .collect();
        assert_eq!(schema["additionalProperties"], false, "the root is closed");
        closed.push(String::new());
        for pointer in &closed {
            assert!(
                BOGUS_AT.iter().any(|(p, _)| p == pointer),
                "closed object {pointer:?} has no row in BOGUS_AT"
            );
        }
        for (pointer, yaml) in BOGUS_AT {
            let node = schema
                .pointer(pointer)
                .unwrap_or_else(|| panic!("{pointer:?}"));
            assert_eq!(node["additionalProperties"], false, "{pointer:?} is closed");
            let mut props: Vec<String> = node["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{pointer:?} has properties"))
                .keys()
                .cloned()
                .collect();
            props.sort();
            let err = match serde_yaml::from_str::<ConfigYaml>(yaml) {
                Ok(_) => panic!("{pointer:?}: `bogus` loaded from {yaml:?}"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains("bogus"), "{pointer:?}: {err}");
            assert_eq!(expected_keys(&err), props, "{pointer:?}: {err}");
        }
    }

    #[test]
    fn expected_keys_reads_every_message_form() {
        for (message, keys) in [
            ("unknown field `x`, expected `a` at line 1", vec!["a"]),
            ("unknown field `x`, expected `b` or `a`", vec!["a", "b"]),
            (
                "unknown field `x`, expected one of `c`, `a`, `b`",
                vec!["a", "b", "c"],
            ),
            (
                "unknown plugin field `x` (expected path/url) at line 2",
                vec!["path", "url"],
            ),
        ] {
            assert_eq!(expected_keys(message), keys, "{message}");
        }
    }

    /// Every literal a schema node offers (`enum`, `const`, a boolean
    /// type), `$ref`s followed and `null` left out.
    fn literals(schema: &serde_json::Value, node: &serde_json::Value) -> Vec<serde_json::Value> {
        let node = match node["$ref"].as_str() {
            Some(r) => schema
                .pointer(r.trim_start_matches('#'))
                .unwrap_or_else(|| panic!("{r}")),
            None => node,
        };
        let mut out = Vec::new();
        for key in ["anyOf", "oneOf"] {
            for alt in node[key].as_array().into_iter().flatten() {
                out.extend(literals(schema, alt));
            }
        }
        out.extend(node["enum"].as_array().into_iter().flatten().cloned());
        out.extend(node.get("const").cloned());
        let is_bool = match &node["type"] {
            serde_json::Value::String(t) => t == "boolean",
            serde_json::Value::Array(ts) => ts.iter().any(|t| t == "boolean"),
            _ => false,
        };
        if is_bool {
            out.extend([true.into(), false.into()]);
        }
        out.retain(|v| !v.is_null());
        out
    }

    /// Every literal the schema offers for a closed set of values loads when
    /// written as an editor inserts it (unquoted).
    #[test]
    fn every_offered_literal_loads() {
        let schema = config_json_schema();
        for (pointer, yaml, expect) in [
            (
                "/$defs/LockConfig/properties/backend",
                "lock:\n  backend: {}\n",
                &["fs", "mem"][..],
            ),
            (
                "/$defs/FuseConfig/properties/enabled",
                "fuse:\n  enabled: {}\n",
                &["true", "false", "auto", "on", "off"][..],
            ),
            (
                "/$defs/ResourcesConfig/additionalProperties/anyOf/1/properties/unit",
                "resources:\n  gpu:\n    unit: {}\n",
                &["count", "cpu", "bytes"][..],
            ),
        ] {
            let node = schema
                .pointer(pointer)
                .unwrap_or_else(|| panic!("{pointer}"));
            let offered: Vec<String> = literals(&schema, node)
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            for word in expect {
                assert!(offered.iter().any(|o| o == word), "{pointer}: {offered:?}");
            }
            for word in &offered {
                let doc = yaml.replace("{}", word);
                serde_yaml::from_str::<ConfigYaml>(&doc)
                    .unwrap_or_else(|e| panic!("{pointer}: {doc:?} does not load: {e}"));
            }
        }
    }

    /// What the hand-written schemas offer as values must load.
    #[test]
    fn hand_written_examples_parse() {
        for yaml in [
            "fuse: {enabled: auto}\n",
            "fuse: {enabled: on}\n",
            "fuse: {enabled: true}\n",
            "resources: {cpu: 50%, memory: 24G, gpu: {unit: count, value: 2}}\n",
            "resources: {slots: {unit: cpu}}\n",
            "plugins: [{url: https://x/p.json, checksum: 'sha256:00', options: {a: 1}}]\n",
            "plugins: [{builtin: exec}, {path: p.json}]\n",
        ] {
            serde_yaml::from_str::<ConfigYaml>(yaml).unwrap_or_else(|e| panic!("{yaml}: {e}"));
        }
    }

    #[test]
    fn hand_written_units_match_the_parser() {
        let schema = config_json_schema();
        let units = &schema["$defs"]["ResourcesConfig"]["additionalProperties"]["anyOf"][1]["properties"]
            ["unit"]["enum"];
        assert_eq!(units, &serde_json::json!(["count", "cpu", "bytes"]));
    }
}
