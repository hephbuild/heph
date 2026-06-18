//! Conversions between the prost wire types ([`crate::pb`]) and the in-process
//! `hplugin`/`hmodel`/`hcore` types. Free functions (not `From` impls) because
//! both sides are foreign to this crate (orphan rule).
//!
//! Provider-path scope for now (Addr/Value/State/Sandbox/TargetSpec/Matcher);
//! driver-path conversions (TargetDef/raw_def/Input/Output/run) are added when
//! the remote driver path needs them (M2).

use crate::pb;
use anyhow::Context as _;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::TargetAddr;
use hplugin::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use hplugin::driver::targetdef::{RawDef, RawDefBytes};
use hplugin::provider::{State, TargetSpec};
use std::collections::BTreeMap;
use std::sync::Arc;

// ---- Addr ----

pub fn addr_to_pb(a: &Addr) -> pb::Addr {
    pb::Addr {
        package: a.package.as_str().to_string(),
        name: a.name.clone(),
        args: a.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    }
}

pub fn addr_from_pb(a: pb::Addr) -> Addr {
    let args: BTreeMap<String, String> = a.args.into_iter().collect();
    Addr::new(PkgBuf::from(a.package), a.name, args)
}

// ---- Value ----

pub fn value_to_pb(v: &Value) -> pb::Value {
    use pb::value::{Kind, List, Map, Null};
    let kind = match v {
        Value::String(s) => Kind::StringVal(s.clone()),
        Value::Bool(b) => Kind::BoolVal(*b),
        Value::Float(f) => Kind::FloatVal(*f),
        Value::Int(i) => Kind::IntVal(*i),
        Value::Uint(u) => Kind::UintVal(*u),
        Value::Null() => Kind::NullVal(Null {}),
        Value::Map(m) => Kind::MapVal(Map {
            entries: m.iter().map(|(k, v)| (k.clone(), value_to_pb(v))).collect(),
        }),
        Value::List(l) => Kind::ListVal(List {
            items: l.iter().map(value_to_pb).collect(),
        }),
    };
    pb::Value { kind: Some(kind) }
}

pub fn value_from_pb(v: pb::Value) -> Value {
    use pb::value::Kind;
    match v.kind {
        Some(Kind::StringVal(s)) => Value::String(s),
        Some(Kind::BoolVal(b)) => Value::Bool(b),
        Some(Kind::FloatVal(f)) => Value::Float(f),
        Some(Kind::IntVal(i)) => Value::Int(i),
        Some(Kind::UintVal(u)) => Value::Uint(u),
        Some(Kind::NullVal(_)) | None => Value::Null(),
        Some(Kind::MapVal(m)) => Value::Map(
            m.entries
                .into_iter()
                .map(|(k, v)| (k, value_from_pb(v)))
                .collect(),
        ),
        Some(Kind::ListVal(l)) => Value::List(l.items.into_iter().map(value_from_pb).collect()),
    }
}

// ---- Provider functions (signature + def) ----
//
// A provider's BUILD-file functions cross the stable ABI as metadata (name /
// signature / doc); the handler stays guest-side and is invoked via
// `call_function`. The host reconstructs the `FnSignature` to enforce arity/type
// and render it (`heph inspect functions`, LSP hover).

use hcore::htvalue::signature::{FnSignature, Param, ParamType};

pub fn param_type_to_pb(t: &ParamType) -> pb::ParamType {
    use pb::param_type::{Kind, Scalar, Union};
    let kind = match t {
        ParamType::String => Kind::Scalar(Scalar::String as i32),
        ParamType::Bool => Kind::Scalar(Scalar::Bool as i32),
        ParamType::Int => Kind::Scalar(Scalar::Int as i32),
        ParamType::Uint => Kind::Scalar(Scalar::Uint as i32),
        ParamType::Float => Kind::Scalar(Scalar::Float as i32),
        ParamType::Null => Kind::Scalar(Scalar::Null as i32),
        ParamType::List(inner) => Kind::List(Box::new(param_type_to_pb(inner))),
        ParamType::Map(value) => Kind::Map(Box::new(param_type_to_pb(value))),
        ParamType::Union(types) => Kind::Union(Union {
            types: types.iter().map(param_type_to_pb).collect(),
        }),
    };
    pb::ParamType { kind: Some(kind) }
}

pub fn param_type_from_pb(t: pb::ParamType) -> ParamType {
    use pb::param_type::{Kind, Scalar};
    match t.kind {
        Some(Kind::Scalar(s)) => match Scalar::try_from(s).unwrap_or(Scalar::Unspecified) {
            Scalar::String | Scalar::Unspecified => ParamType::String,
            Scalar::Bool => ParamType::Bool,
            Scalar::Int => ParamType::Int,
            Scalar::Uint => ParamType::Uint,
            Scalar::Float => ParamType::Float,
            Scalar::Null => ParamType::Null,
        },
        Some(Kind::List(inner)) => ParamType::list(param_type_from_pb(*inner)),
        Some(Kind::Map(value)) => ParamType::map(param_type_from_pb(*value)),
        Some(Kind::Union(u)) => {
            ParamType::union(u.types.into_iter().map(param_type_from_pb).collect())
        }
        None => ParamType::Null,
    }
}

fn param_to_pb(p: &Param) -> pb::Param {
    pb::Param {
        name: p.name.to_string(),
        ty: Some(param_type_to_pb(&p.ty)),
        default: p.default.as_ref().map(value_to_pb),
    }
}

fn param_from_pb(p: pb::Param) -> Param {
    // `Param::name` is `&'static str` (in-process defs use string literals).
    // A def reconstructed from the wire owns its name; leak it to obtain the
    // 'static borrow. Functions are read once per process (registry wiring is a
    // `Once`), so this is a bounded, one-time leak — not a per-call cost.
    let name: &'static str = Box::leak(p.name.into_boxed_str());
    let ty = p.ty.map(param_type_from_pb).unwrap_or(ParamType::Null);
    match p.default {
        Some(d) => Param::optional(name, ty, value_from_pb(d)),
        None => Param::required(name, ty),
    }
}

pub fn fn_signature_to_pb(s: &FnSignature) -> pb::FnSignature {
    pb::FnSignature {
        positional: s.positional.iter().map(param_to_pb).collect(),
        named: s.named.iter().map(param_to_pb).collect(),
        variadic: s.variadic.as_ref().map(param_to_pb),
        returns: Some(param_type_to_pb(&s.returns)),
    }
}

pub fn fn_signature_from_pb(s: pb::FnSignature) -> FnSignature {
    FnSignature {
        positional: s.positional.into_iter().map(param_from_pb).collect(),
        named: s.named.into_iter().map(param_from_pb).collect(),
        variadic: s.variadic.map(param_from_pb),
        returns: s.returns.map(param_type_from_pb).unwrap_or(ParamType::Null),
    }
}

// ---- Schemas (provider state + driver config) ----
//
// `provider::StateField`/`StateSchema` and `driver::DriverField`/`DriverSchema`
// are the same declarative shape; both cross as `pb::SchemaField`/`pb::Schema`.

use hplugin::driver::{DriverField, DriverSchema};
use hplugin::provider::{StateField, StateSchema};

pub fn state_schema_to_pb(s: &StateSchema) -> pb::Schema {
    pb::Schema {
        fields: s
            .fields
            .iter()
            .map(|f| pb::SchemaField {
                name: f.name.clone(),
                ty: Some(param_type_to_pb(&f.ty)),
                doc: f.doc.clone(),
                required: f.required,
            })
            .collect(),
    }
}

pub fn state_schema_from_pb(s: pb::Schema) -> StateSchema {
    StateSchema {
        fields: s
            .fields
            .into_iter()
            .map(|f| StateField {
                name: f.name,
                ty: f.ty.map(param_type_from_pb).unwrap_or(ParamType::Null),
                doc: f.doc,
                required: f.required,
            })
            .collect(),
    }
}

pub fn driver_schema_to_pb(s: &DriverSchema) -> pb::Schema {
    pb::Schema {
        fields: s
            .fields
            .iter()
            .map(|f| pb::SchemaField {
                name: f.name.clone(),
                ty: Some(param_type_to_pb(&f.ty)),
                doc: f.doc.clone(),
                required: f.required,
            })
            .collect(),
    }
}

pub fn driver_schema_from_pb(s: pb::Schema) -> DriverSchema {
    DriverSchema {
        fields: s
            .fields
            .into_iter()
            .map(|f| DriverField {
                name: f.name,
                ty: f.ty.map(param_type_from_pb).unwrap_or(ParamType::Null),
                doc: f.doc,
                required: f.required,
            })
            .collect(),
    }
}

// ---- Options (plugin config map) ----
//
// A plugin's `options:` map (`BTreeMap<String, serde_yaml::Value>`) crosses the
// stable ABI as a `pb::Value` map (prost bytes). The guest reconstructs the same
// `Options` map and decodes it with `hplugin::config::decode_opt`, exactly as an
// in-process plugin does.

fn yaml_to_pb(v: &serde_yaml::Value) -> pb::Value {
    use pb::value::{Kind, List, Map, Null};
    let kind = match v {
        serde_yaml::Value::Null => Kind::NullVal(Null {}),
        serde_yaml::Value::Bool(b) => Kind::BoolVal(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Kind::IntVal(i)
            } else if let Some(u) = n.as_u64() {
                Kind::UintVal(u)
            } else {
                Kind::FloatVal(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_yaml::Value::String(s) => Kind::StringVal(s.clone()),
        serde_yaml::Value::Sequence(seq) => Kind::ListVal(List {
            items: seq.iter().map(yaml_to_pb).collect(),
        }),
        serde_yaml::Value::Mapping(m) => Kind::MapVal(Map {
            entries: m
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        serde_yaml::Value::String(s) => s.clone(),
                        other => serde_yaml::to_string(other).unwrap_or_default(),
                    };
                    (key, yaml_to_pb(v))
                })
                .collect(),
        }),
        // Tagged values carry an explicit YAML tag we don't model; cross the inner value.
        serde_yaml::Value::Tagged(t) => return yaml_to_pb(&t.value),
    };
    pb::Value { kind: Some(kind) }
}

fn pb_to_yaml(v: pb::Value) -> serde_yaml::Value {
    use pb::value::Kind;
    match v.kind {
        Some(Kind::StringVal(s)) => serde_yaml::Value::String(s),
        Some(Kind::BoolVal(b)) => serde_yaml::Value::Bool(b),
        Some(Kind::FloatVal(f)) => serde_yaml::Value::Number(serde_yaml::Number::from(f)),
        Some(Kind::IntVal(i)) => serde_yaml::Value::Number(serde_yaml::Number::from(i)),
        Some(Kind::UintVal(u)) => serde_yaml::Value::Number(serde_yaml::Number::from(u)),
        Some(Kind::NullVal(_)) | None => serde_yaml::Value::Null,
        Some(Kind::ListVal(l)) => {
            serde_yaml::Value::Sequence(l.items.into_iter().map(pb_to_yaml).collect())
        }
        Some(Kind::MapVal(m)) => {
            let mut out = serde_yaml::Mapping::new();
            for (k, v) in m.entries {
                out.insert(serde_yaml::Value::String(k), pb_to_yaml(v));
            }
            serde_yaml::Value::Mapping(out)
        }
    }
}

/// Encode a plugin `options:` map to `pb::Value` (Map) prost bytes for the ABI.
pub fn options_to_pb_bytes(opts: &BTreeMap<String, serde_yaml::Value>) -> Vec<u8> {
    use prost::Message;
    let map = pb::value::Map {
        entries: opts
            .iter()
            .map(|(k, v)| (k.clone(), yaml_to_pb(v)))
            .collect(),
    };
    pb::Value {
        kind: Some(pb::value::Kind::MapVal(map)),
    }
    .encode_to_vec()
}

/// Decode ABI options bytes back into a plugin `options:` map. Non-map / empty
/// payloads decode to an empty map.
pub fn options_from_pb_bytes(bytes: &[u8]) -> anyhow::Result<BTreeMap<String, serde_yaml::Value>> {
    use prost::Message;
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let v = pb::Value::decode(bytes).context("decode plugin options pb::Value")?;
    match v.kind {
        Some(pb::value::Kind::MapVal(m)) => Ok(m
            .entries
            .into_iter()
            .map(|(k, v)| (k, pb_to_yaml(v)))
            .collect()),
        _ => Ok(BTreeMap::new()),
    }
}

// ---- State ----

pub fn state_to_pb(s: &State) -> pb::State {
    pb::State {
        package: s.package.as_str().to_string(),
        provider: s.provider.clone(),
        state: s
            .state
            .iter()
            .map(|(k, v)| (k.clone(), value_to_pb(v)))
            .collect(),
    }
}

pub fn state_from_pb(s: pb::State) -> State {
    State {
        package: PkgBuf::from(s.package),
        provider: s.provider,
        state: s
            .state
            .into_iter()
            .map(|(k, v)| (k, value_from_pb(v)))
            .collect(),
    }
}

// ---- TargetAddr ----

pub fn target_addr_to_pb(t: &TargetAddr) -> pb::TargetAddr {
    pb::TargetAddr {
        r#ref: Some(addr_to_pb(&t.r#ref)),
        output: t.output.clone(),
        filters: t.filters.clone(),
    }
}

pub fn target_addr_from_pb(t: pb::TargetAddr) -> TargetAddr {
    TargetAddr {
        r#ref: addr_from_pb(t.r#ref.unwrap_or_default()),
        output: t.output,
        filters: t.filters,
    }
}

// ---- Sandbox ----

fn tool_to_pb(t: &Tool) -> pb::Tool {
    pb::Tool {
        r#ref: Some(target_addr_to_pb(&t.r#ref)),
        group: t.group.clone(),
        hash: t.hash,
        id: t.id.clone(),
    }
}

fn tool_from_pb(t: pb::Tool) -> Tool {
    Tool {
        r#ref: target_addr_from_pb(t.r#ref.unwrap_or_default()),
        group: t.group,
        hash: t.hash,
        id: t.id,
    }
}

fn dep_to_pb(d: &Dep) -> pb::Dep {
    let mode = match d.mode {
        Mode::None => pb::DepMode::None,
        Mode::Link => pb::DepMode::Link,
    };
    pb::Dep {
        r#ref: Some(target_addr_to_pb(&d.r#ref)),
        mode: mode as i32,
        group: d.group.clone(),
        runtime: d.runtime,
        hash: d.hash,
        id: d.id.clone(),
    }
}

fn dep_from_pb(d: pb::Dep) -> Dep {
    let mode = match pb::DepMode::try_from(d.mode).unwrap_or(pb::DepMode::None) {
        pb::DepMode::Link => Mode::Link,
        _ => Mode::None,
    };
    Dep {
        r#ref: target_addr_from_pb(d.r#ref.unwrap_or_default()),
        mode,
        group: d.group,
        runtime: d.runtime,
        hash: d.hash,
        id: d.id,
    }
}

fn env_to_pb(e: &Env) -> pb::Env {
    let value = match &e.value {
        EnvValue::Literal(s) => pb::env::Value::Literal(s.clone()),
        EnvValue::Pass => pb::env::Value::Pass(true),
    };
    pb::Env {
        value: Some(value),
        hash: e.hash,
        append: e.append,
        append_prefix: e.append_prefix.clone(),
    }
}

fn env_from_pb(e: pb::Env) -> Env {
    let value = match e.value {
        Some(pb::env::Value::Literal(s)) => EnvValue::Literal(s),
        Some(pb::env::Value::Pass(_)) => EnvValue::Pass,
        None => EnvValue::Literal(String::new()),
    };
    Env {
        value,
        hash: e.hash,
        append: e.append,
        append_prefix: e.append_prefix,
    }
}

pub fn sandbox_to_pb(s: &Sandbox) -> pb::Sandbox {
    pb::Sandbox {
        tools: s.tools.iter().map(tool_to_pb).collect(),
        deps: s.deps.iter().map(dep_to_pb).collect(),
        env: s
            .env
            .iter()
            .map(|(k, v)| (k.clone(), env_to_pb(v)))
            .collect(),
    }
}

pub fn sandbox_from_pb(s: pb::Sandbox) -> Sandbox {
    // tool_keys/dep_keys are rebuilt by push_tool/push_dep (private dedup sets).
    let mut sb = Sandbox::default();
    for t in s.tools {
        sb.push_tool(tool_from_pb(t));
    }
    for d in s.deps {
        sb.push_dep(dep_from_pb(d));
    }
    sb.env = s
        .env
        .into_iter()
        .map(|(k, v)| (k, env_from_pb(v)))
        .collect();
    sb
}

// ---- TargetSpec ----

pub fn target_spec_to_pb(t: &TargetSpec) -> pb::TargetSpec {
    pb::TargetSpec {
        addr: Some(addr_to_pb(&t.addr)),
        driver: t.driver.clone(),
        config: t
            .config
            .iter()
            .map(|(k, v)| (k.clone(), value_to_pb(v)))
            .collect(),
        labels: t.labels.clone(),
        transitive: Some(sandbox_to_pb(&t.transitive)),
    }
}

pub fn target_spec_from_pb(t: pb::TargetSpec) -> TargetSpec {
    TargetSpec {
        addr: addr_from_pb(t.addr.unwrap_or_default()),
        driver: t.driver,
        config: t
            .config
            .into_iter()
            .map(|(k, v)| (k, value_from_pb(v)))
            .collect(),
        labels: t.labels,
        transitive: sandbox_from_pb(t.transitive.unwrap_or_default()),
    }
}

// ---- Matcher ----

pub fn matcher_to_pb(m: &Matcher) -> pb::Matcher {
    use pb::matcher::{Kind, List};
    let kind = match m {
        Matcher::Addr(a) => Kind::Addr(addr_to_pb(a)),
        Matcher::Label(l) => Kind::Label(l.clone()),
        Matcher::Package(p) => Kind::Package(p.as_str().to_string()),
        Matcher::PackagePrefix(p) => Kind::PackagePrefix(p.as_str().to_string()),
        Matcher::TreeOutputTo(p) => Kind::TreeOutputTo(p.as_str().to_string()),
        Matcher::Or(ms) => Kind::Or(List {
            matchers: ms.iter().map(matcher_to_pb).collect(),
        }),
        Matcher::And(ms) => Kind::And(List {
            matchers: ms.iter().map(matcher_to_pb).collect(),
        }),
        Matcher::Not(inner) => Kind::Not(Box::new(matcher_to_pb(inner))),
    };
    pb::Matcher { kind: Some(kind) }
}

pub fn matcher_from_pb(m: pb::Matcher) -> Matcher {
    use pb::matcher::Kind;
    match m.kind {
        Some(Kind::Addr(a)) => Matcher::Addr(addr_from_pb(a)),
        Some(Kind::Label(l)) => Matcher::Label(l),
        Some(Kind::Package(p)) => Matcher::Package(PkgBuf::from(p)),
        Some(Kind::PackagePrefix(p)) => Matcher::PackagePrefix(PkgBuf::from(p)),
        Some(Kind::TreeOutputTo(p)) => Matcher::TreeOutputTo(PkgBuf::from(p)),
        Some(Kind::Or(l)) => Matcher::Or(l.matchers.into_iter().map(matcher_from_pb).collect()),
        Some(Kind::And(l)) => Matcher::And(l.matchers.into_iter().map(matcher_from_pb).collect()),
        Some(Kind::Not(inner)) => Matcher::Not(Box::new(matcher_from_pb(*inner))),
        // An empty matcher matches nothing sensible; default to an empty Or.
        None => Matcher::Or(vec![]),
    }
}

// ---- TargetDef and its parts (driver path) ----

use hplugin::driver::targetdef::path::{CodegenMode, Content as PathContent, Path};
use hplugin::driver::targetdef::{CacheConfig, Input, InputMode, Output, TargetDef};

fn input_mode_to_pb(m: &InputMode) -> pb::InputMode {
    match m {
        InputMode::Standard => pb::InputMode::Standard,
        InputMode::Link => pb::InputMode::Link,
        InputMode::Tool => pb::InputMode::Tool,
    }
}

fn input_mode_from_pb(m: i32) -> InputMode {
    match pb::InputMode::try_from(m).unwrap_or(pb::InputMode::Standard) {
        pb::InputMode::Link => InputMode::Link,
        pb::InputMode::Tool => InputMode::Tool,
        _ => InputMode::Standard,
    }
}

fn input_to_pb(i: &Input) -> pb::Input {
    pb::Input {
        r#ref: Some(target_addr_to_pb(&i.r#ref)),
        mode: input_mode_to_pb(&i.mode) as i32,
        origin_id: i.origin_id.clone(),
        annotations: i
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        hashed: i.hashed,
        runtime: i.runtime,
    }
}

fn input_from_pb(i: pb::Input) -> Input {
    Input {
        r#ref: target_addr_from_pb(i.r#ref.unwrap_or_default()),
        mode: input_mode_from_pb(i.mode),
        origin_id: i.origin_id,
        annotations: i.annotations.into_iter().collect(),
        hashed: i.hashed,
        runtime: i.runtime,
    }
}

fn codegen_to_pb(c: &CodegenMode) -> pb::CodegenMode {
    match c {
        CodegenMode::None => pb::CodegenMode::None,
        CodegenMode::Copy => pb::CodegenMode::Copy,
        CodegenMode::InPlace => pb::CodegenMode::InPlace,
    }
}

fn codegen_from_pb(c: i32) -> CodegenMode {
    match pb::CodegenMode::try_from(c).unwrap_or(pb::CodegenMode::None) {
        pb::CodegenMode::Copy => CodegenMode::Copy,
        pb::CodegenMode::InPlace => CodegenMode::InPlace,
        _ => CodegenMode::None,
    }
}

fn path_to_pb(p: &Path) -> pb::Path {
    let content = match &p.content {
        PathContent::FilePath(s) => pb::path::Content::FilePath(s.clone()),
        PathContent::DirPath(s) => pb::path::Content::DirPath(s.clone()),
        PathContent::Glob(s) => pb::path::Content::Glob(s.clone()),
    };
    pb::Path {
        content: Some(content),
        codegen_tree: codegen_to_pb(&p.codegen_tree) as i32,
        collect: p.collect,
    }
}

fn path_from_pb(p: pb::Path) -> Path {
    let content = match p.content {
        Some(pb::path::Content::FilePath(s)) => PathContent::FilePath(s),
        Some(pb::path::Content::DirPath(s)) => PathContent::DirPath(s),
        Some(pb::path::Content::Glob(s)) => PathContent::Glob(s),
        None => PathContent::FilePath(String::new()),
    };
    Path {
        content,
        codegen_tree: codegen_from_pb(p.codegen_tree),
        collect: p.collect,
    }
}

fn output_to_pb(o: &Output) -> pb::Output {
    pb::Output {
        group: o.group.clone(),
        paths: o.paths.iter().map(path_to_pb).collect(),
    }
}

fn output_from_pb(o: pb::Output) -> Output {
    Output {
        group: o.group,
        paths: o.paths.into_iter().map(path_from_pb).collect(),
    }
}

fn cache_config_to_pb(c: &CacheConfig) -> pb::CacheConfig {
    pb::CacheConfig {
        enabled: c.enabled,
        remote_enabled: c.remote_enabled,
        history: c.history,
    }
}

fn cache_config_from_pb(c: pb::CacheConfig) -> CacheConfig {
    CacheConfig {
        enabled: c.enabled,
        remote_enabled: c.remote_enabled,
        history: c.history,
    }
}

pub fn target_def_to_pb(td: &TargetDef) -> anyhow::Result<pb::TargetDef> {
    Ok(pb::TargetDef {
        addr: Some(addr_to_pb(&td.addr)),
        labels: td.labels.clone(),
        raw_def: Some(raw_def_to_blob(&td.raw_def)?),
        inputs: td.inputs.iter().map(input_to_pb).collect(),
        outputs: td.outputs.iter().map(output_to_pb).collect(),
        support_files: td.support_files.iter().map(path_to_pb).collect(),
        cache: Some(cache_config_to_pb(&td.cache)),
        pty: td.pty,
        hash: td.hash.clone().into(),
        transparent: td.transparent,
    })
}

pub fn target_def_from_pb(td: pb::TargetDef) -> anyhow::Result<TargetDef> {
    Ok(TargetDef {
        addr: addr_from_pb(td.addr.unwrap_or_default()),
        labels: td.labels,
        raw_def: raw_def_from_blob(&td.raw_def.unwrap_or_default())?,
        inputs: td.inputs.into_iter().map(input_from_pb).collect(),
        outputs: td.outputs.into_iter().map(output_from_pb).collect(),
        support_files: td.support_files.into_iter().map(path_from_pb).collect(),
        cache: cache_config_from_pb(td.cache.unwrap_or_default()),
        pty: td.pty,
        hash: td.hash.to_vec(),
        transparent: td.transparent,
    })
}

// ---- OutputArtifact (driver run outputs) ----

use hplugin::driver::outputartifact::{
    Content as OaContent, ContentFile, ContentRaw, OutputArtifact, Type as OaType,
};

fn oa_type_to_pb(t: &OaType) -> pb::ArtifactType {
    match t {
        OaType::Output => pb::ArtifactType::Output,
        OaType::Log => pb::ArtifactType::Log,
        OaType::SupportFile => pb::ArtifactType::SupportFile,
    }
}

fn oa_type_from_pb(t: i32) -> OaType {
    match pb::ArtifactType::try_from(t).unwrap_or(pb::ArtifactType::Output) {
        pb::ArtifactType::Log => OaType::Log,
        pb::ArtifactType::SupportFile => OaType::SupportFile,
        _ => OaType::Output,
    }
}

pub fn output_artifact_to_pb(oa: &OutputArtifact) -> pb::OutputArtifactRef {
    let content = match &oa.content {
        OaContent::File(f) => pb::output_artifact_ref::Content::File(pb::ContentFile {
            source_path: f.source_path.clone(),
            out_path: f.out_path.clone(),
            x: f.x,
        }),
        OaContent::Raw(r) => pb::output_artifact_ref::Content::Raw(pb::ContentRaw {
            data: r.data.clone().into(),
            path: r.path.clone(),
            x: r.x,
        }),
        OaContent::TarPath(p) => pb::output_artifact_ref::Content::TarPath(p.clone()),
        OaContent::CpioPath(p) => pb::output_artifact_ref::Content::CpioPath(p.clone()),
    };
    pb::OutputArtifactRef {
        group: oa.group.clone(),
        name: oa.name.clone(),
        r#type: oa_type_to_pb(&oa.r#type) as i32,
        content: Some(content),
        hashout: oa.hashout.clone(),
    }
}

pub fn output_artifact_from_pb(oa: pb::OutputArtifactRef) -> OutputArtifact {
    let content = match oa.content {
        Some(pb::output_artifact_ref::Content::File(f)) => OaContent::File(ContentFile {
            source_path: f.source_path,
            out_path: f.out_path,
            x: f.x,
        }),
        Some(pb::output_artifact_ref::Content::Raw(r)) => OaContent::Raw(ContentRaw {
            data: r.data.to_vec(),
            path: r.path,
            x: r.x,
        }),
        Some(pb::output_artifact_ref::Content::TarPath(p)) => OaContent::TarPath(p),
        Some(pb::output_artifact_ref::Content::CpioPath(p)) => OaContent::CpioPath(p),
        None => OaContent::Raw(ContentRaw {
            data: vec![],
            path: String::new(),
            x: false,
        }),
    };
    OutputArtifact {
        group: oa.group,
        name: oa.name,
        r#type: oa_type_from_pb(oa.r#type),
        content,
        hashout: oa.hashout,
    }
}

// ---- raw_def (opaque driver blob) ----

/// Serialize a driver's `raw_def` to a wire blob (JSON). Works on any `RawDef`,
/// whether a concrete value (in-process) or a round-tripped [`RawDefBytes`].
pub fn raw_def_to_blob(raw: &Arc<dyn RawDef>) -> anyhow::Result<pb::RawDefBlob> {
    let data = serde_json::to_vec(&**raw)?;
    Ok(pb::RawDefBlob {
        driver: String::new(),
        format: pb::raw_def_blob::Format::Json as i32,
        data: data.into(),
    })
}

/// Reconstruct a `raw_def` from a wire blob as a [`RawDefBytes`] carrier. The
/// receiving driver reads its concrete config via `TargetDef::def_de`.
pub fn raw_def_from_blob(blob: &pb::RawDefBlob) -> anyhow::Result<Arc<dyn RawDef>> {
    Ok(Arc::new(RawDefBytes::from_json_slice(&blob.data)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn addr(pkg: &str, name: &str) -> Addr {
        let mut args = BTreeMap::new();
        args.insert("goos".to_string(), "linux".to_string());
        Addr::new(PkgBuf::from(pkg), name.to_string(), args)
    }

    #[test]
    fn addr_roundtrip() {
        let a = addr("//foo/bar", "lib");
        assert_eq!(addr_from_pb(addr_to_pb(&a)), a);
    }

    #[test]
    fn value_roundtrip() {
        let v = Value::Map(HashMap::from([
            ("s".to_string(), Value::String("x".to_string())),
            ("i".to_string(), Value::Int(-3)),
            ("u".to_string(), Value::Uint(7)),
            ("f".to_string(), Value::Float(1.5)),
            ("b".to_string(), Value::Bool(true)),
            ("n".to_string(), Value::Null()),
            (
                "l".to_string(),
                Value::List(vec![Value::Int(1), Value::Int(2)]),
            ),
        ]));
        assert_eq!(value_from_pb(value_to_pb(&v)), v);
    }

    #[test]
    fn target_spec_roundtrip() {
        let mut spec = TargetSpec {
            addr: addr("//a", "x"),
            driver: "exec".to_string(),
            config: HashMap::from([("cmd".to_string(), Value::String("echo".to_string()))]),
            labels: vec!["lbl".to_string()],
            transitive: Sandbox::default(),
        };
        spec.transitive.push_dep(Dep {
            r#ref: TargetAddr {
                r#ref: addr("//b", "y"),
                output: Some("out".to_string()),
                filters: vec![],
            },
            mode: Mode::Link,
            group: "g".to_string(),
            runtime: true,
            hash: true,
            id: "id1".to_string(),
        });
        let back = target_spec_from_pb(target_spec_to_pb(&spec));
        assert_eq!(back.addr, spec.addr);
        assert_eq!(back.driver, spec.driver);
        assert_eq!(back.config, spec.config);
        assert_eq!(back.labels, spec.labels);
        assert_eq!(back.transitive.deps.len(), 1);
        assert_eq!(back.transitive.deps[0].id, "id1");
        assert!(matches!(back.transitive.deps[0].mode, Mode::Link));
    }

    #[test]
    fn matcher_roundtrip() {
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("//a")),
            Matcher::Not(Box::new(Matcher::Label("x".to_string()))),
        ]);
        assert_eq!(matcher_from_pb(matcher_to_pb(&m)), m);
    }

    #[test]
    fn raw_def_blob_roundtrip() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct D {
            x: u32,
        }
        let raw: Arc<dyn RawDef> = Arc::new(D { x: 5 });
        let blob = raw_def_to_blob(&raw).expect("to blob");
        let back = raw_def_from_blob(&blob).expect("from blob");
        // The reconstructed RawDefBytes re-serializes to the original value.
        assert_eq!(
            serde_json::to_value(&*back).expect("reserialize"),
            serde_json::json!({"x": 5})
        );
    }

    #[test]
    fn options_pb_bytes_roundtrip() {
        // A plugin options map crosses the ABI as pb::Value bytes and decodes back
        // unchanged — covering scalars, nesting, and a list.
        let yaml = r#"
gotool: "//@heph/bin:go"
parallel: 4
flag: true
nested: { a: 1, b: [x, y] }
"#;
        let opts: BTreeMap<String, serde_yaml::Value> =
            serde_yaml::from_str(yaml).expect("parse opts");
        let bytes = options_to_pb_bytes(&opts);
        let back = options_from_pb_bytes(&bytes).expect("decode opts");
        assert_eq!(back, opts);

        // Typed decode through the same path a plugin author uses.
        let gotool: String = serde_yaml::from_value(back["gotool"].clone()).expect("gotool");
        assert_eq!(gotool, "//@heph/bin:go");

        // Empty payload decodes to an empty map (absent options).
        assert!(options_from_pb_bytes(&[]).expect("empty").is_empty());
    }
}
