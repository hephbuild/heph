//! Conversions between the prost wire types ([`crate::pb`]) and the in-process
//! `hplugin`/`hmodel`/`hcore` types. Free functions (not `From` impls) because
//! both sides are foreign to this crate (orphan rule).
//!
//! Provider-path scope for now (Addr/Value/State/Sandbox/TargetSpec/Matcher);
//! driver-path conversions (TargetDef/raw_def/Input/Output/run) are added when
//! the remote driver path needs them (M2).

use crate::pb;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use hplugin::driver::TargetAddr;
use hplugin::provider::{State, TargetSpec};
use std::collections::BTreeMap;

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
        env: s.env.iter().map(|(k, v)| (k.clone(), env_to_pb(v))).collect(),
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
            ("l".to_string(), Value::List(vec![Value::Int(1), Value::Int(2)])),
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
}
