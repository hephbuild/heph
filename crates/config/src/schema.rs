//! The `.hephconfig` JSON Schema: what an editor (through
//! yaml-language-server) completes, hovers and validates against.
//!
//! Derived from the serde types in [`crate::config_yaml`], so a key added to
//! the parser is in the schema with no second edit. The types whose
//! `Deserialize` is hand-rolled (an untagged shorthand, a source key chosen at
//! parse time) describe themselves here, by hand, next to nothing else: each
//! `json_schema` below mirrors one `Deserialize` impl, and the schema's tests
//! in this module check every example they offer against the real parser.
//!
//! The schema is advisory. The parser is the authority on what loads.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use crate::config_yaml::{ConfigYaml, FuseEnabled, PluginSpec, ResourcesConfig};

/// The schema of one `.hephconfig` file (the base or a profile), as JSON
/// Schema 2020-12. Definitions sit under `$defs`, referenced as
/// `#/$defs/<Name>`.
///
/// Every object's keys are sorted, so the value is the same whatever order
/// `serde_json`'s map keeps (`preserve_order` is a feature any crate in the
/// build can turn on).
pub fn config_json_schema() -> serde_json::Value {
    sorted(schemars::schema_for!(ConfigYaml).to_value())
}

/// [`config_json_schema`] as `heph tool config-schema` prints it, and as
/// `docs/hephconfig.schema.json` holds it: pretty JSON and a trailing newline.
pub fn config_json_schema_text() -> serde_json::Result<String> {
    let mut text = serde_json::to_string_pretty(&config_json_schema())?;
    text.push('\n');
    Ok(text)
}

/// `value` with every object's keys in sorted order, at every depth.
fn sorted(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Mirrors `FuseEnabled`'s `Deserialize`: a boolean, or one of its words
/// (`"true"` and `"false"` quoted are words too).
impl JsonSchema for FuseEnabled {
    fn schema_name() -> Cow<'static, str> {
        "FuseEnabled".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "`true`, `false`, or `auto` (decided per target from its inputs).",
            "anyOf": [
                { "type": "boolean" },
                { "enum": ["auto", "on", "off", "true", "false"] }
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
            "oneOf": [
                { "required": ["builtin"] },
                { "required": ["path"] },
                { "required": ["url"] }
            ],
            "dependentRequired": { "checksum": ["url"] },
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
                    "type": ["object", "null"],
                    "description": "Passed to the plugin's construction; each plugin checks its own keys."
                }
            },
            "additionalProperties": false
        })
    }
}

// The amount grammars of `hcore::capacity`, as patterns over the text a YAML
// string carries. Each is exact for the syntax `Unit::parse` and
// `Capacity::parse` accept; what a pattern cannot say (a value past
// `u64::MAX`, a `cpu` fraction below 0.000001) is left to the parser, so the
// schema is looser there and never stricter.

/// `count`: ASCII digits (`whole`).
const COUNT_PATTERN: &str = "^[0-9]+$";
/// `cpu`: digits, then optionally `.` and at least one digit (`parse_cpu`).
const CPU_PATTERN: &str = r"^[0-9]+(\.[0-9]+)?$";
/// `bytes`: digits, then nothing, `b`, or `k`/`m`/`g`/`t` alone or followed
/// by `b` or `ib`, in any case (`parse_memory_size`).
const BYTES_PATTERN: &str = "^[0-9]+([bB]|[kKmMgGtT]([bB]|[iI][bB])?)?$";
/// A percentage: digits that are not all zeros, then `%` (`Capacity::parse`).
const PERCENT_PATTERN: &str = "^[0-9]*[1-9][0-9]*%$";

/// One amount grammar, published under `$defs` so the pools reference it.
macro_rules! amount_schema {
    ($ty:ident, $description:literal, $number:literal, $pattern:ident) => {
        struct $ty;

        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                stringify!($ty).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "description": $description,
                    "anyOf": [
                        // A YAML number is written back as text the way
                        // `Scalar::into_text` does: `2.0` reads as `2`.
                        { "type": $number, "minimum": 0 },
                        { "type": "string", "pattern": $pattern }
                    ]
                })
            }
        }
    };
}

amount_schema!(
    CountAmount,
    "A whole number: `2`.",
    "integer",
    COUNT_PATTERN
);
amount_schema!(
    CpuAmount,
    "CPUs, a decimal in steps of 0.000001: `2`, `0.5`.",
    "number",
    CPU_PATTERN
);
amount_schema!(
    ByteAmount,
    "Bytes, as a whole number or a size: `1024`, `512M`, `24G`, `1GiB`.",
    "integer",
    BYTES_PATTERN
);

/// A percentage of a builtin pool's default capacity.
struct Percent;

impl JsonSchema for Percent {
    fn schema_name() -> Cow<'static, str> {
        "Percent".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A percentage of the pool's default capacity, at least `1%`; over `100%` overcommits.",
            "type": "string",
            "pattern": PERCENT_PATTERN
        })
    }
}

/// Mirrors `ResourcesConfig`'s `Deserialize`: pool name to an amount, or to
/// `{unit, value}`. The builtin pools are listed so an editor offers them.
///
/// A builtin pool's unit is fixed, and the parser checks its value while it
/// reads the file; so does a custom pool that writes `unit:`. A custom pool
/// written without one takes its unit from whichever file declares it (a
/// profile may set only the value), so its value is any amount some unit
/// accepts.
impl JsonSchema for ResourcesConfig {
    fn schema_name() -> Cow<'static, str> {
        "ResourcesConfig".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        use hcore::capacity::Unit;
        let count = generator.subschema_for::<CountAmount>().to_value();
        let cpu = generator.subschema_for::<CpuAmount>().to_value();
        let bytes = generator.subschema_for::<ByteAmount>().to_value();
        let percent = generator.subschema_for::<Percent>().to_value();
        let amount_of = |unit: Unit| match unit {
            Unit::Count => count.clone(),
            Unit::Cpu => cpu.clone(),
            Unit::Bytes => bytes.clone(),
        };
        let value_description = "The pool's capacity. Omitted is unbounded; `0` is set per run \
                                 (`--resource <pool>=<n>`).";

        // A builtin: its unit's amount or a percentage, alone or as
        // `{unit, value}` with its own unit only.
        let builtin = |unit: Unit, description: &str| {
            let value = serde_json::json!({ "anyOf": [amount_of(unit), percent] });
            serde_json::json!({
                "description": description,
                "anyOf": [
                    value,
                    {
                        "type": "object",
                        "properties": {
                            "unit": {
                                "enum": [unit.name(), null],
                                "description": format!("A builtin pool's unit is always `{unit}`.")
                            },
                            "value": {
                                "anyOf": [value, { "type": "null" }],
                                "description": value_description
                            }
                        },
                        "additionalProperties": false
                    }
                ]
            })
        };

        // Any unit's amount: `count`'s grammar is inside both of the others.
        let any_amount = serde_json::json!({ "anyOf": [cpu, bytes] });
        let mut units: Vec<serde_json::Value> = Unit::ALL.map(|u| u.name().into()).to_vec();
        units.push(serde_json::Value::Null);
        let value_for_unit: Vec<serde_json::Value> = Unit::ALL
            .into_iter()
            .map(|unit| {
                serde_json::json!({
                    "if": { "required": ["unit"], "properties": { "unit": { "const": unit.name() } } },
                    "then": {
                        "properties": { "value": { "anyOf": [amount_of(unit), { "type": "null" }] } }
                    }
                })
            })
            .collect();
        let custom = serde_json::json!({
            "anyOf": [
                any_amount,
                {
                    "type": "object",
                    "properties": {
                        "unit": {
                            "enum": units,
                            "description": "How the pool's amounts are written: `count` (a whole number), `cpu` (CPUs, a decimal), `bytes` (a size such as `24G`)."
                        },
                        "value": {
                            "anyOf": [any_amount, { "type": "null" }],
                            "description": value_description
                        }
                    },
                    "allOf": value_for_unit,
                    "additionalProperties": false
                }
            ]
        });
        json_schema!({
            "description": "Resource pools, by name: an amount, or `{unit, value}`.",
            "type": "object",
            "properties": {
                (hcore::capacity::CPU): builtin(Unit::Cpu, "CPUs, or a percentage of the parallelism (`50%`)."),
                (hcore::capacity::MEMORY): builtin(Unit::Bytes, "A size (`24G`), or a percentage of the memory this process may use (`80%`)."),
                (hcore::capacity::SANDBOX): builtin(Unit::Count, "Build sandboxes on disk, live or awaiting removal: a count, or a percentage of the default.")
            },
            "propertyNames": {
                "pattern": format!("^[a-z][a-z0-9_]{{0,{}}}$", hcore::capacity::MAX_POOL_NAME_LEN - 1)
            },
            "additionalProperties": custom
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        (
            "/$defs/ResourcesConfig/properties/cpu/anyOf/1",
            "resources:\n  cpu:\n    bogus: 1\n",
        ),
        (
            "/$defs/ResourcesConfig/properties/memory/anyOf/1",
            "resources:\n  memory:\n    bogus: 1\n",
        ),
        (
            "/$defs/ResourcesConfig/properties/sandbox/anyOf/1",
            "resources:\n  sandbox:\n    bogus: 1\n",
        ),
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
        // `unit: ~` is the same as no `unit:`.
        assert_eq!(units, &serde_json::json!(["count", "cpu", "bytes", null]));
    }

    /// The committed copy of the schema, which is also the URL people point
    /// their editor at.
    fn golden_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/hephconfig.schema.json")
    }

    /// `docs/hephconfig.schema.json` is exactly what `heph tool config-schema`
    /// prints, so every schema change shows up in review as a diff of it.
    #[test]
    fn golden_schema_is_current() {
        let text = config_json_schema_text().expect("render");
        let path = golden_path();
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &text).expect("write the golden schema");
            return;
        }
        let golden = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            golden == text,
            "docs/hephconfig.schema.json is out of date with the config types; \
             regenerate it with `UPDATE_GOLDEN=1 cargo test -p config golden` and commit the diff"
        );
    }

    /// Rendering twice gives the same bytes: no map whose order depends on
    /// hashing or insertion reaches the output.
    #[test]
    fn schema_text_is_deterministic() {
        let a = config_json_schema_text().expect("render");
        let b = config_json_schema_text().expect("render");
        assert_eq!(a, b);
    }

    /// The schema's validator, built the way yaml-language-server reads it:
    /// draft from `$schema`, no remote references.
    fn validator() -> jsonschema::Validator {
        jsonschema::validator_for(&config_json_schema()).expect("the schema compiles")
    }

    /// A YAML document as the JSON instance yaml-language-server validates.
    /// An empty (or comment-only) document has no root node, so the editor
    /// checks nothing; it reads here as `{}`, which every schema of an
    /// all-optional object accepts.
    fn instance(yaml: &str) -> serde_json::Value {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("well-formed YAML");
        match serde_json::to_value(value).expect("YAML that JSON can hold") {
            serde_json::Value::Null => serde_json::json!({}),
            other => other,
        }
    }

    /// The schema's errors for `yaml`, one line each.
    fn schema_errors(validator: &jsonschema::Validator, yaml: &str) -> Vec<String> {
        validator
            .iter_errors(&instance(yaml))
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect()
    }

    /// Every config file in `example/`, by path.
    fn example_configs() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .map(|e| e.expect("dir entry").path())
                .collect();
            entries.sort();
            for path in entries {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_dir() {
                    walk(&path, out);
                } else if name.ends_with(".hephconfig") || name.ends_with(".hephconfig2") {
                    let text = std::fs::read_to_string(&path).expect("read config");
                    out.push((path.display().to_string(), text));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../example"),
            &mut out,
        );
        out
    }

    /// Configs that load, written to touch every key and every form a value
    /// takes. Most are the parser tests' own documents.
    const LOADS: &[&str] = &[
        "",
        "# only a comment\n",
        "{}\n",
        "version: v1.2.3\nversionFlavour: debug\nhomeDir: .heph3\n",
        "versionFlavour: \"\"\n",
        "version: ~\nhomeDir: ~\n",
        "memCache:\n  perEntryBytes: 16384\n  capacityBytes: 67108864\n\
         tmpCache:\n  perEntryBytes: 1048576\n  capacityBytes: 134217728\n",
        "memCache: ~\n",
        "cache:\n  spillThresholdBytes: 104857600\n",
        "cache: {}\n",
        "caches:\n  remote:\n    uri: s3://my-bucket/heph\n    read: true\n    write: false\n\
         \x20 local:\n    uri: file:///tmp/heph-cache\n",
        "caches:\n  c:\n    uri: s3://b/p\n    concurrency: 32\n",
        "caches:\n  remote:\n    uri: s3://my-bucket/heph\n    \
         endpoint: https://accountid.r2.cloudflarestorage.com\n    region: auto\n",
        // Profile patches: one field of an entry, the rest inherited.
        "caches: {main: {read: false}}\n",
        "caches:\n  remote:\n    write: false\n",
        "caches:\n  remote:\n    endpoint: http://localhost:9000\n",
        "caches: {}\n",
        "fs:\n  skip: [vendor, \"**/node_modules/**\"]\n",
        "fuse:\n  enabled: true\n",
        "fuse:\n  enabled: false\n",
        "fuse:\n  enabled: auto\n",
        "fuse:\n  enabled: on\n",
        "fuse:\n  enabled: \"off\"\n",
        "fuse:\n  enabled: \"true\"\n",
        "fuse: {}\n",
        "fuse: {enabled: ~}\n",
        "lock:\n  backend: mem\n",
        "lock:\n  backend: fs\n",
        "telemetry:\n  enabled: false\n",
        "telemetry: {}\n",
        "scratch:\n  scope: ${git:branch}\n  restoreScopes: [master]\n  seedOnFork: false\n",
        "plugins: []\n",
        "plugins:\n  - builtin: buildfile\n    options:\n      patterns: [BUILD]\n  - builtin: go\n",
        "plugins:\n  - path: tools/heph-go-plugin.json\n  - url: https://x/p.json\n    \
         checksum: sha256:9f86d0\n    options: {a: 1}\n",
        "plugins:\n  - builtin: exec\n    options: {}\n  - builtin: bash\n    options: ~\n",
        "resources: {cpu: 0}\n",
        "resources: {cpu: 8, memory: 24G, sandbox: 8}\n",
        "resources: {cpu: 1.5, memory: \"32G\"}\n",
        "resources: {memory: 4096}\n",
        "resources: {cpu: 50%, memory: 75%, sandbox: \"10%\"}\n",
        "resources: {cpu: 150%}\n",
        "resources: {gpu: 0, docker: 2, slots: 0.5, vram: 24G}\n",
        "resources: {gpu: {unit: count, value: 2}}\n",
        "resources: {vram: {unit: bytes, value: 24G}}\n",
        "resources: {slots: {unit: cpu, value: 0.5}}\n",
        "resources: {cpu: {unit: cpu, value: 4}}\n",
        "resources: {memory: {unit: bytes, value: 50%}}\n",
        "resources: {gpu: {value: 3}}\n",
        "resources: {gpu: {unit: count}}\n",
        "resources: {gpu: {}}\n",
        "resources: {gpu: {unit: ~, value: ~}}\n",
        "resources: {sandbox: 0}\n",
        "resources: {sandbox: {value: 0}}\n",
        "resources:\n  cpu: 8\n  memory: 24G\n  docker: 2\n  slots: {unit: cpu, value: 2}\n",
        "resources:\n  cpu: 4\n  slots: 0.5\n",
        "telemetry: ~\ncache: {spillThresholdBytes: ~}\n",
    ];

    /// Every config the parser accepts, the schema accepts: an editor never
    /// marks a config that loads.
    #[test]
    fn every_config_that_parses_validates() {
        let validator = validator();
        let mut corpus: Vec<(String, String)> = LOADS
            .iter()
            .map(|y| ((*y).to_string(), (*y).to_string()))
            .collect();
        let examples = example_configs();
        assert!(!examples.is_empty(), "found no config under example/");
        corpus.extend(examples);
        for (name, yaml) in &corpus {
            // The loader reads an empty file as the default without parsing it.
            if !yaml.is_empty() {
                serde_yaml::from_str::<ConfigYaml>(yaml)
                    .unwrap_or_else(|e| panic!("{name:?} is not a loading config: {e}"));
            }
            let errors = schema_errors(&validator, yaml);
            assert!(
                errors.is_empty(),
                "{name:?} loads but the schema rejects it: {errors:#?}"
            );
        }
    }

    /// Configs the parser rejects and the schema can tell apart, each with
    /// what is wrong.
    const REJECTED: &[(&str, &str)] = &[
        ("bogus: 1\n", "unknown key at the root"),
        ("cache:\n  bogus: 1\n", "unknown key in cache"),
        (
            "caches:\n  r:\n    uri: memory:///r\n    bogus: 1\n",
            "unknown key in a cache",
        ),
        ("fs:\n  bogus: 1\n", "unknown key in fs"),
        ("fuse:\n  bogus: 1\n", "unknown key in fuse"),
        ("lock:\n  bogus: 1\n", "unknown key in lock"),
        (
            "memCache:\n  perEntryBytes: 1\n",
            "a required field missing",
        ),
        ("scratch:\n  bogus: 1\n", "unknown key in scratch"),
        ("telemetry:\n  bogus: 1\n", "unknown key in telemetry"),
        (
            "plugins:\n- builtin: exec\n  bogus: 1\n",
            "unknown key in a plugin",
        ),
        (
            "plugins:\n- builtin: exec\n  path: p.json\n",
            "two plugin sources",
        ),
        (
            "plugins:\n- path: p.json\n  url: https://x/p.json\n",
            "two plugin sources",
        ),
        ("plugins:\n- options: {a: 1}\n", "no plugin source"),
        (
            "plugins:\n- builtin: exec\n  checksum: sha256:00\n",
            "checksum with builtin",
        ),
        (
            "plugins:\n- path: p.json\n  checksum: sha256:00\n",
            "checksum with path",
        ),
        ("lock:\n  backend: sqlite\n", "a bad lock backend"),
        ("fuse:\n  enabled: maybe\n", "a bad fuse mode"),
        (
            "resources: {gpu: {unit: gpus, value: 1}}\n",
            "a bad pool unit",
        ),
        (
            "resources: {gpu: {unit: count, valu: 1}}\n",
            "unknown key in a pool",
        ),
        ("resources: {Cpu: 1}\n", "a bad pool name"),
        ("resources: {\"gpu \": 1}\n", "a bad pool name"),
        ("resources: {9gpu: 1}\n", "a bad pool name"),
        ("resources: [cpu]\n", "resources not a map"),
        ("resources: {cpu: all}\n", "a cpu amount that is a word"),
        ("resources: {cpu: -1}\n", "a negative amount"),
        ("resources: {cpu: true}\n", "a boolean amount"),
        ("resources: {memory: \"4 G\"}\n", "a size with a space"),
        ("resources: {memory: 1.5}\n", "a fractional byte count"),
        ("resources: {cpu: 0%}\n", "a zero percentage"),
        ("resources: {cpu: \"50 %\"}\n", "a percentage with a space"),
        ("resources: {cpu: 50%%}\n", "a doubled percent sign"),
        ("resources: {gpu: 50%}\n", "a percentage of a custom pool"),
        (
            "resources: {gpu: {unit: count, value: 24G}}\n",
            "a size in a count pool",
        ),
        (
            "resources: {gpu: {unit: count, value: 1.5}}\n",
            "a fraction in a count pool",
        ),
        (
            "resources: {cpu: {unit: count, value: 2}}\n",
            "a builtin pool's unit changed",
        ),
        (
            "resources: {memory: {unit: cpu}}\n",
            "a builtin pool's unit changed",
        ),
        (
            "resources: {sandbox: {unit: bytes, value: 1G}}\n",
            "a builtin pool's unit changed",
        ),
    ];

    /// Every row of [`REJECTED`] fails both to parse (and resolve) and to
    /// validate: the schema catches what the parser enforces.
    #[test]
    fn configs_the_parser_rejects_the_schema_rejects() {
        let validator = validator();
        for (yaml, why) in REJECTED {
            let loads = serde_yaml::from_str::<ConfigYaml>(yaml)
                .map_err(anyhow::Error::from)
                .and_then(|c| c.resources.resolve().map(|_| c));
            assert!(loads.is_err(), "{why}: {yaml:?} loads");
            assert!(
                !validator.is_valid(&instance(yaml)),
                "{why}: the schema accepts {yaml:?}"
            );
        }
    }

    /// Each amount, written for each kind of pool: whether it parses, and
    /// whether the schema accepts it, agree. A custom pool without `unit:`
    /// accepts what any unit does, since another file may give the unit.
    #[test]
    fn amounts_agree_with_the_parser() {
        use hcore::capacity::{Capacity, Unit, Zero};
        let validator = validator();
        let values = [
            "0", "2", "007", "0.5", "1.5", "2.0", "1e3", ".5", "5.", "-1", "-0.5", "\"2\"",
            "\"0.5\"", "\"1.\"", "\".5\"", "\"\"", "24G", "24g", "24GB", "24gib", "24GiB", "1b",
            "1B", "24ib", "24 G", "G", "24X", "24K", "512M", "1T", "50%", "1%", "100%", "150%",
            "0%", "00%", "010%", "-5%", "\"%\"", "50%%", "\"50 %\"", "true", "~", "[1]",
        ];
        // The parser's verdict for a value written for `pool` with `unit`.
        let parses = |pool: &str, unit: Option<&str>, value: &str| -> bool {
            let yaml = match unit {
                Some(u) => format!("resources:\n  {pool}: {{unit: {u}, value: {value}}}\n"),
                None => format!("resources:\n  {pool}: {value}\n"),
            };
            match serde_yaml::from_str::<ConfigYaml>(&yaml) {
                // A custom pool written without a unit is checked once the
                // files are merged: it parses if some unit takes its text.
                Ok(c) if unit.is_none() && Unit::of_builtin(pool).is_none() => {
                    let text = c.resources.0[pool].value.clone().unwrap_or_default();
                    Unit::ALL.into_iter().any(|u| {
                        Capacity::parse(pool, u, &text, Zero::Allowed)
                            .is_ok_and(|c| matches!(c, Capacity::Amount(_)))
                    })
                }
                Ok(c) => c.resources.resolve().is_ok(),
                Err(_) => false,
            }
        };
        let mut cases: Vec<(&str, Option<&str>)> = vec![
            ("cpu", None),
            ("memory", None),
            ("sandbox", None),
            ("gpu", None),
            ("cpu", Some("cpu")),
            ("memory", Some("bytes")),
            ("sandbox", Some("count")),
        ];
        cases.extend(Unit::ALL.map(|u| ("gpu", Some(u.name()))));
        let mut disagree = Vec::new();
        for (pool, unit) in &cases {
            for value in values {
                let yaml = match unit {
                    Some(u) => format!("resources:\n  {pool}: {{unit: {u}, value: {value}}}\n"),
                    None => format!("resources:\n  {pool}: {value}\n"),
                };
                let parser = parses(pool, *unit, value);
                let schema = validator.is_valid(&instance(&yaml));
                if parser != schema {
                    disagree.push(format!("{yaml:?}: parser {parser}, schema {schema}"));
                }
            }
        }
        assert!(disagree.is_empty(), "{disagree:#?}");
    }

    /// Where the schema is looser than the parser, on purpose: a pattern
    /// cannot bound a number's size or a fraction's smallest step. These load
    /// nowhere, and the schema lets them through.
    #[test]
    fn what_the_schema_leaves_to_the_parser() {
        let validator = validator();
        for yaml in [
            "resources: {cpu: \"0.0000001\"}\n",
            "resources: {memory: \"99999999999999999999\"}\n",
            "resources: {memory: 99999999999T}\n",
            "resources: {sandbox: \"99999999999999999999%\"}\n",
        ] {
            let loads = serde_yaml::from_str::<ConfigYaml>(yaml).is_ok();
            assert!(!loads, "{yaml:?} loads");
            assert!(validator.is_valid(&instance(yaml)), "{yaml:?}");
        }
    }
}
