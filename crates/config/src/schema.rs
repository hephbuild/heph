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
//!
//! The contract: the schema never rejects a config the parser loads, with one
//! named exemption. serde_yaml reads a plain number or boolean into a string
//! field (`version: 1`, `homeDir: 5`, `skip: [2024]`); the schema keeps
//! `type: string` there on purpose, because a number where heph expects text
//! is almost always a mistake (`version: 1.10` loads as `1.1`). Everywhere
//! else the schema may be looser than the parser, never stricter.

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
    let mut schema = schemars::schema_for!(ConfigYaml).to_value();
    strip_null_defaults(&mut schema);
    schema.sort_all_objects();
    schema
}

/// Remove every `"default": null`. yaml-language-server inserts a key's
/// default on completion, so `homeDir: null` would be typed for the user; a
/// default that is not null (`telemetry.enabled: true`) stays.
fn strip_null_defaults(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("default").is_some_and(serde_json::Value::is_null) {
                map.remove("default");
            }
            map.values_mut().for_each(strip_null_defaults);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_null_defaults),
        _ => {}
    }
}

/// A `#[schemars(transform)]` for a collection serde_yaml also reads from an
/// empty value: `plugins:` followed only by comments loads as no plugins, so
/// the schema accepts `null` there too.
pub(crate) fn empty_is_null(schema: &mut Schema) {
    let Some(ty) = schema.get("type").and_then(serde_json::Value::as_str) else {
        return;
    };
    if matches!(ty, "array" | "object") {
        let types = serde_json::json!([ty, "null"]);
        schema.insert("type".to_string(), types);
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

/// What an editor shows on hover for builtin pool `pool`.
fn builtin_pool_description(pool: &str) -> &'static str {
    use hcore::capacity::{CPU, MEMORY, SANDBOX};
    match pool {
        CPU => "CPUs, or a percentage of the parallelism (`50%`).",
        MEMORY => "A size (`24G`), or a percentage of the memory this process may use (`80%`).",
        SANDBOX => {
            "Build sandboxes on disk, live or awaiting removal: a count, or a percentage of the \
             default."
        }
        _ => "A builtin pool: an amount in its unit, or a percentage of its default.",
    }
}

/// Mirrors `ResourcesConfig`'s `Deserialize`: pool name to an amount, to
/// `{unit, value}`, or to the older `[unit, value]`. The builtin pools are
/// listed so an editor offers them.
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

        // `[unit, value]`, each optional: what serde reads a `{unit, value}`
        // struct from when the YAML is a sequence. Not offered, but it loads.
        let sequence = |units: serde_json::Value, value: &serde_json::Value| {
            serde_json::json!({
                "type": "array",
                "maxItems": 2,
                "prefixItems": [
                    { "enum": units },
                    { "anyOf": [value, { "type": "null" }] }
                ]
            })
        };

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
                    },
                    sequence(serde_json::json!([unit.name(), null]), &value)
                ]
            })
        };

        // Any unit's amount: `count`'s grammar is inside both of the others.
        let any_amount = serde_json::json!({ "anyOf": [cpu, bytes] });
        let mut units: Vec<serde_json::Value> = Unit::ALL.map(|u| u.name().into()).to_vec();
        units.push(serde_json::Value::Null);
        let custom_sequence = sequence(units.clone().into(), &any_amount);
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
                },
                custom_sequence
            ]
        });
        let builtins: serde_json::Map<String, serde_json::Value> = hcore::capacity::BUILTIN_POOLS
            .into_iter()
            .filter_map(|pool| {
                let unit = Unit::of_builtin(pool)?;
                Some((
                    pool.to_string(),
                    builtin(unit, builtin_pool_description(pool)),
                ))
            })
            .collect();
        json_schema!({
            "description": "Resource pools, by name: an amount, or `{unit, value}`.",
            // `resources:` with nothing after it (or only comments) loads as
            // no pools.
            "type": ["object", "null"],
            "properties": builtins,
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

    /// Rendering twice gives the same bytes: no map whose order depends on
    /// hashing or insertion reaches the output.
    #[test]
    fn schema_text_is_deterministic() {
        let a = serde_json::to_vec(&config_json_schema()).expect("render");
        let b = serde_json::to_vec(&config_json_schema()).expect("render");
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
    ///
    /// These tests model the editor through serde_yaml's scalar resolution,
    /// not YAML 1.2 core's, which yaml-language-server uses: for ordinary
    /// scalars the two agree, but an exotic one (`0b101`, `0o17`, an `!fs`
    /// enum tag) may read as another type in the editor than here.
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
        // A key with nothing after it but comments: serde_yaml reads the empty
        // value as an empty sequence or map.
        "plugins:\n  # - builtin: go\n",
        "resources:\n  # cpu: 8\n",
        "caches:\n",
        "caches:\n  remote:\n",
        "fs:\n  skip:\n",
        "scratch:\n  restoreScopes:\n",
        // The `[unit, value]` form the `untagged` pool parser accepted.
        "resources: {gpu: [count, 2], vram: [bytes], cpu: [cpu, 50%], slots: []}\n",
    ];

    /// Every config the parser accepts, the schema accepts: an editor never
    /// marks a config that loads. The one exemption, a number or boolean
    /// written into a string field, is [`STRICTER_ON_PURPOSE`]'s.
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
    /// cannot bound a number's size or a fraction's smallest step, and JSON
    /// has one `null` where YAML has `~` and an empty value. These load
    /// nowhere, and the schema lets them through.
    #[test]
    fn what_the_schema_leaves_to_the_parser() {
        let validator = validator();
        for yaml in [
            "resources: {cpu: \"0.0000001\"}\n",
            "resources: {memory: \"99999999999999999999\"}\n",
            "resources: {memory: 99999999999T}\n",
            "resources: {sandbox: \"99999999999999999999%\"}\n",
            // `resources:` alone loads as no pools; `~` does not, and the
            // editor sees both as `null`.
            "resources: ~\n",
        ] {
            let loads = serde_yaml::from_str::<ConfigYaml>(yaml).is_ok();
            assert!(!loads, "{yaml:?} loads");
            assert!(validator.is_valid(&instance(yaml)), "{yaml:?}");
        }
    }

    /// The contract's one named exemption: serde_yaml reads a plain number,
    /// boolean or `~` into a string field, and the schema keeps
    /// `type: string` there anyway, because a number where heph expects
    /// text is almost always a mistake (`version: 1.10` loads as `1.1`).
    /// The parser loads every row and the schema rejects it.
    const STRICTER_ON_PURPOSE: &[&str] = &[
        "version: 1\n",
        "version: 1.10\n",
        "homeDir: 5\n",
        "fs:\n  skip: [2024]\n",
        "scratch:\n  scope: true\n",
        "caches:\n  c:\n    region: 1\n",
        "scratch:\n  restoreScopes: [~]\n",
    ];

    #[test]
    fn the_schema_is_stricter_on_purpose_for_strings() {
        let validator = validator();
        for yaml in STRICTER_ON_PURPOSE {
            serde_yaml::from_str::<ConfigYaml>(yaml)
                .unwrap_or_else(|e| panic!("{yaml:?} no longer loads ({e}): drop its row"));
            assert!(
                !validator.is_valid(&instance(yaml)),
                "the schema now accepts {yaml:?}. Looser is allowed by the contract, so this \
                 is not a bug: drop the row from STRICTER_ON_PURPOSE (and the exemption from \
                 docs/CONFIG_SCHEMA.md if no row is left)"
            );
        }
    }

    /// One step from an object to a value inside it, with the sibling keys
    /// (and a value for each) the object needs around that value to load.
    #[derive(Debug, Clone, PartialEq)]
    struct Hop {
        step: Step,
        siblings: Vec<(String, &'static str)>,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Step {
        /// A listed property.
        Key(String),
        /// Any key of a map (`additionalProperties`), written as `x`, which
        /// is a valid pool and cache name.
        AnyKey,
        /// An item of an array.
        Item,
    }

    /// A place a value can be written, with each schema the walk found for
    /// it (one per `anyOf` branch that reaches it).
    #[derive(Debug)]
    struct Position {
        path: Vec<Hop>,
        nodes: Vec<serde_json::Value>,
    }

    /// `node` with `$ref`s followed.
    fn deref<'a>(
        schema: &'a serde_json::Value,
        node: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        match node["$ref"].as_str() {
            Some(r) => deref(
                schema,
                schema
                    .pointer(r.trim_start_matches('#'))
                    .unwrap_or_else(|| panic!("{r}")),
            ),
            None => node,
        }
    }

    /// The siblings `object` needs next to `key` for the entry to load: its
    /// `required` keys, what `key` itself requires (`checksum` needs `url`),
    /// and one of a `oneOf` of required keys (a plugin's source) unless
    /// `key` or another sibling is one.
    fn siblings(object: &serde_json::Value, key: &str) -> Vec<(String, &'static str)> {
        let names = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        let mut out: Vec<(String, &'static str)> = names(&object["required"])
            .into_iter()
            .filter(|r| r != key)
            .map(|r| (r, "1"))
            .collect();
        out.extend(
            names(&object["dependentRequired"][key])
                .into_iter()
                .map(|r| (r, "s")),
        );
        let one_of: Vec<String> = object["oneOf"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|alt| names(&alt["required"]))
            .collect();
        let covered = one_of
            .iter()
            .any(|k| k == key || out.iter().any(|(s, _)| s == k));
        if let (false, Some(first)) = (covered, one_of.first()) {
            out.push((first.clone(), "s"));
        }
        out
    }

    /// Every position under `node`, which sits at `path`.
    fn walk(
        schema: &serde_json::Value,
        node: &serde_json::Value,
        path: &mut Vec<Hop>,
        out: &mut Vec<Position>,
    ) {
        assert!(path.len() < 10, "the walk recursed too deep at {path:?}");
        let node = deref(schema, node);
        for key in ["anyOf", "oneOf", "allOf"] {
            for alt in node[key].as_array().into_iter().flatten() {
                walk(schema, alt, path, out);
            }
        }
        let mut children: Vec<(Hop, &serde_json::Value)> = Vec::new();
        for (key, sub) in node["properties"].as_object().into_iter().flatten() {
            let hop = Hop {
                step: Step::Key(key.clone()),
                siblings: siblings(node, key),
            };
            children.push((hop, sub));
        }
        for (step, sub) in [
            (Step::AnyKey, &node["additionalProperties"]),
            (Step::Item, &node["items"]),
        ] {
            if sub.is_object() {
                let siblings = Vec::new();
                children.push((Hop { step, siblings }, sub));
            }
        }
        for (hop, sub) in children {
            path.push(hop);
            match out.iter_mut().find(|p| p.path == *path) {
                Some(p) => p.nodes.push(sub.clone()),
                None => out.push(Position {
                    path: path.clone(),
                    nodes: vec![sub.clone()],
                }),
            }
            walk(schema, sub, path, out);
            path.pop();
        }
    }

    /// The YAML document that writes `sample` at `path`, block style; an
    /// empty `sample` is a key (or `-`) with nothing after it.
    fn document(path: &[Hop], sample: &str) -> String {
        fn mapping(path: &[Hop], sample: &str, indent: usize, out: &mut String) {
            let pad = " ".repeat(indent);
            let key = match &path[0].step {
                Step::Key(k) => k.as_str(),
                Step::AnyKey => "x",
                Step::Item => panic!("an item where a key belongs: {path:?}"),
            };
            out.push_str(&format!("{pad}{key}:"));
            value(&path[1..], sample, indent, out);
            for (k, v) in &path[0].siblings {
                out.push_str(&format!("{pad}{k}: {v}\n"));
            }
        }
        fn value(rest: &[Hop], sample: &str, indent: usize, out: &mut String) {
            match rest.first().map(|h| &h.step) {
                None if sample.is_empty() => out.push('\n'),
                None => out.push_str(&format!(" {sample}\n")),
                Some(Step::Item) => {
                    out.push_str(&format!("\n{}-", " ".repeat(indent + 2)));
                    value(&rest[1..], sample, indent + 2, out);
                }
                Some(_) => {
                    out.push('\n');
                    mapping(rest, sample, indent + 2, out);
                }
            }
        }
        let mut out = String::new();
        mapping(path, sample, 0, &mut out);
        out
    }

    /// Whether every schema found for a position is a plain string, `null`
    /// aside: the field type of the named exemption.
    fn is_plain_string(schema: &serde_json::Value, nodes: &[serde_json::Value]) -> bool {
        nodes.iter().all(|node| {
            let node = deref(schema, node);
            let constrained = [
                "enum", "const", "pattern", "format", "anyOf", "oneOf", "allOf",
            ]
            .iter()
            .any(|k| node.get(*k).is_some());
            let types: Vec<&str> = match &node["type"] {
                serde_json::Value::String(t) => vec![t.as_str()],
                serde_json::Value::Array(ts) => ts.iter().filter_map(|t| t.as_str()).collect(),
                _ => Vec::new(),
            };
            !constrained && types.iter().filter(|t| **t != "null").eq(["string"].iter())
        })
    }

    /// The parser's verdict, `resources:` resolved: a custom pool written
    /// without a unit is checked only then.
    fn parser_loads(yaml: &str) -> bool {
        serde_yaml::from_str::<ConfigYaml>(yaml).is_ok_and(|c| c.resources.resolve().is_ok())
    }

    /// The guard that keeps the contract as fields are added: at every
    /// position the schema describes (`$ref`s followed, into nested objects,
    /// map values and array items), each kind of YAML value the parser loads
    /// the schema accepts. The only exceptions are the named exemption's: a
    /// non-string scalar at a plain string position.
    ///
    /// Like the other tests here, it models the editor through serde_yaml's
    /// scalar resolution (see [`instance`]).
    #[test]
    fn at_every_position_what_loads_validates() {
        const SAMPLES: &[&str] = &["1", "1.5", "\"s\"", "true", "~", "[]", "{}", ""];
        const NON_STRING_SCALARS: &[&str] = &["1", "1.5", "true", "~", ""];
        let schema = config_json_schema();
        let validator = validator();
        let mut positions = Vec::new();
        walk(&schema, &schema, &mut Vec::new(), &mut positions);
        // Every key at the root, both caches, every pool shape, plugins and
        // their fields: a walker that stops following a kind of node drops
        // well under this.
        assert!(
            positions.len() >= 40,
            "the walk found only {} positions: {:#?}",
            positions.len(),
            positions.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
        let (mut checked, mut exempt, mut broken) = (0, 0, Vec::new());
        for position in &positions {
            let string_field = is_plain_string(&schema, &position.nodes);
            for sample in SAMPLES {
                let yaml = document(&position.path, sample);
                checked += 1;
                if !parser_loads(&yaml) || validator.is_valid(&instance(&yaml)) {
                    continue;
                }
                if string_field && NON_STRING_SCALARS.contains(sample) {
                    exempt += 1;
                } else {
                    broken.push(yaml);
                }
            }
        }
        println!(
            "{} positions, {checked} documents, {exempt} under the string exemption",
            positions.len()
        );
        assert!(
            broken.is_empty(),
            "these load but the schema rejects them: {broken:#?}"
        );
        // The exemption is in use; if it ever is not, drop it from the docs.
        assert!(exempt > 0, "no position used the string exemption");
    }

    /// The document writer, on each kind of step: what the walk feeds the
    /// parser is the YAML it means.
    #[test]
    fn document_writes_each_kind_of_step() {
        let key = |k: &str| Hop {
            step: Step::Key(k.to_string()),
            siblings: Vec::new(),
        };
        let any = Hop {
            step: Step::AnyKey,
            siblings: Vec::new(),
        };
        let item = Hop {
            step: Step::Item,
            siblings: Vec::new(),
        };
        let with_sibling = Hop {
            step: Step::Key("options".to_string()),
            siblings: vec![("builtin".to_string(), "s")],
        };
        assert_eq!(document(&[key("version")], "1"), "version: 1\n");
        assert_eq!(document(&[key("caches")], ""), "caches:\n");
        assert_eq!(
            document(&[key("caches"), any, key("uri")], "\"s\""),
            "caches:\n  x:\n    uri: \"s\"\n"
        );
        assert_eq!(
            document(&[key("plugins"), item.clone(), with_sibling], "{}"),
            "plugins:\n  -\n    options: {}\n    builtin: s\n"
        );
        assert_eq!(
            document(&[key("fs"), key("skip"), item], "~"),
            "fs:\n  skip:\n    - ~\n"
        );
    }
}
