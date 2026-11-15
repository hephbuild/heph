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

    #[test]
    fn every_top_level_key_is_in_the_schema() {
        let schema = config_json_schema();
        let props = schema["properties"].as_object().expect("properties");
        for key in [
            "version",
            "versionFlavour",
            "homeDir",
            "plugins",
            "memCache",
            "scratch",
            "tmpCache",
            "fuse",
            "lock",
            "fs",
            "cache",
            "caches",
            "telemetry",
            "resources",
        ] {
            assert!(props.contains_key(key), "{key} missing from {props:?}");
        }
        assert_eq!(schema["additionalProperties"], false);
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
