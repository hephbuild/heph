use crate::engine::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use crate::engine::driver::TargetAddr;
use crate::htaddr;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::{parse_map_string_string, parse_map_string_strings, TargetSpecValue};
use crate::pluginbuildfile::provider::Provider;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::float::UnpackFloat;
use starlark::values::list::UnpackList;
use starlark::values::{UnpackValue, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

static GLOBALS: OnceLock<Globals> = OnceLock::new();

fn get_globals() -> &'static Globals {
    GLOBALS.get_or_init(|| {
        let mut builder = GlobalsBuilder::new().with(starlark_module);
        builder.set("True", true);
        builder.set("False", false);
        builder.build()
    })
}

#[derive(Debug)]
pub(crate) struct OnTargetPayload {
    pub name: String,
    pub driver: String,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
    pub config: HashMap<String, TargetSpecValue>,
}

impl Sandbox {
    fn from(m: TargetSpecValue, pkg: &PkgBuf) -> anyhow::Result<Self> {
        let m = match m {
            TargetSpecValue::Map(m) => {
                m
            },
            TargetSpecValue::Null() => {
                return Ok(Default::default());
            }
            _ => anyhow::bail!("Expected map, got {:?}", m)
        };

        let mut m: HashMap<&str, &TargetSpecValue> = m
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();

        let mut sandbox = Self {
            ..Default::default()
        };

        if let Some(v) = m.remove("deps") {
            sandbox.deps = parse_map_string_strings(v)?.iter().enumerate().flat_map(|(i, (k, ss))| ss.iter().map(move |s| {
                Ok(Dep {
                    r#ref: TargetAddr::parse(s, pkg)?,
                    mode: Mode::None,
                    group: k.to_string(),
                    runtime: true,
                    hash: true,
                    id: format!("dep|{}|{}", k, i),
                })
            })).collect::<anyhow::Result<Vec<_>>>().with_context(|| "parse `deps`")?;
        }

        if let Some(v) = m.remove("env") {
            sandbox.env = parse_map_string_string(v).with_context(|| "parse `env`")?.into_iter().map(|(k, v)| {
                (k, Env {
                    value: EnvValue::Literal(v.into()),
                    hash: true,
                    append: false,
                    append_prefix: "".to_string(),
                })
            }).collect::<HashMap<_, _>>();
        }

        if let Some(v) = m.remove("tools") {
            sandbox.tools = parse_map_string_strings(v)?.iter().enumerate().flat_map(|(i, (k, ss))| ss.iter().map(move |s| {
                Ok(Tool {
                    r#ref: TargetAddr::parse(s, pkg)?,
                    group: k.to_string(),
                    hash: true,
                    id: format!("tool|{}|{}", k, i),
                })
            })).collect::<anyhow::Result<Vec<_>>>().with_context(|| "parse `tools`")?;
        }

        if !m.is_empty() {
            let unknown_keys: Vec<&str> = m.into_keys().collect();
            anyhow::bail!("Unknown entries found: {:?}", unknown_keys)
        }

        Ok(sandbox)
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct OnStatePayload {
    pub name: String,
    pub labels: Vec<String>,
}

pub(crate) struct RunResult {
    pub targets: Vec<OnTargetPayload>,
    pub states: Vec<OnStatePayload>,
}

#[allow(dead_code)]
#[derive(ProvidesStaticType)]
pub(crate) struct Extra<'a> {
    pub pkg: &'a str,
    pub on_state: Box<dyn Fn(OnStatePayload) -> anyhow::Result<()>>,
    pub on_target: Box<dyn Fn(OnTargetPayload) -> anyhow::Result<()>>,
}

fn starlark_to_rust(v: &Value) -> TargetSpecValue {
    if let Some(s) = v.unpack_str() {
        return TargetSpecValue::String(s.to_string());
    }

    if let Some(b) = v.unpack_bool() {
        return TargetSpecValue::Bool(b);
    }

    if let Some(i) = v.unpack_i32() {
        return TargetSpecValue::Int(i as i64);
    }

    if let Ok(Some(UnpackFloat(f))) = UnpackFloat::unpack_value(*v) {
        return TargetSpecValue::Float(f);
    }

    if let Ok(Some(l)) = UnpackList::<Value>::unpack_value(*v) {
        return TargetSpecValue::List(l.items.iter().map(starlark_to_rust).collect());
    }

    if let Some(d) = DictRef::from_value(*v) {
        let map = d.iter()
            .filter_map(|(k, val)| k.unpack_str().map(|s| (s.to_string(), starlark_to_rust(&val))))
            .collect();
        return TargetSpecValue::Map(map);
    }

    panic!("starlark_to_rust: Unsupported starlark value type: {}", v.get_type());
}

#[starlark_module]
fn starlark_module(builder: &mut GlobalsBuilder) {
    fn target<'v>(eval: &mut Evaluator<'v, '_, '_>, args: &Arguments<'v, '_>) -> starlark::Result<String> {
        args.no_positional_args(eval.heap())?;
        let extra = eval.extra.unwrap().downcast_ref::<Extra>().unwrap();

        let m = args.names_map()?;

        let mut name = String::new();
        let mut driver = String::new();
        let mut labels: Vec<String> = vec![];
        let mut transitive: Sandbox = Default::default();
        let config = m.iter().map(|e| -> anyhow::Result<Option<(String, TargetSpecValue)>> {
            match e.0.as_str() {
                "name" => {
                    if let Some(s) = e.1.unpack_str() { name = s.to_string(); }
                    Ok(None)
                },
                "driver" => {
                    if let Some(s) = e.1.unpack_str() { driver = s.to_string(); }
                    Ok(None)
                },
                "labels" => {
                    labels = UnpackList::<String>::unpack_value_err(*e.1)
                        .map_err(|e| anyhow::anyhow!("{e}"))?.items;
                    Ok(None)
                },
                "transitive" => {
                    transitive = Sandbox::from(starlark_to_rust(e.1), &PkgBuf::from(extra.pkg))?;
                    Ok(None)
                },
                _ => Ok(Some((e.0.as_str().to_string(), starlark_to_rust(e.1))))
            }
        }).collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<HashMap<String, TargetSpecValue>>();

        let p = OnTargetPayload{
            name: name.clone(),
            driver,
            labels,
            transitive,
            config,
        };

        (extra.on_target)(p)?;

        Ok(htaddr::Addr{
            package: PkgBuf::from(extra.pkg),
            name,
            args: Default::default(),
        }.format())
    }
}

impl Provider {
    pub(crate) fn run_pkg(&self, pkg: &str) -> anyhow::Result<RunResult> {
        self.run_pkg_inner(pkg).with_context(|| format!("pkg: `{}`", pkg)) // TODO: memo
    }

    fn run_pkg_inner(&self, pkg: &str) -> anyhow::Result<RunResult> {
        for pattern in &self.build_file_patterns {
            let path = self.root.join(pkg).join(pattern);
            if path.exists() {
                return self.run_file(pkg, pattern);
            }
        }

        Ok(RunResult{
            targets: vec![],
            states: vec![],
        })
    }

    pub(crate) fn run_file(&self, pkg: &str, filename: &str) -> anyhow::Result<RunResult> {
        self.run_file_inner(pkg, filename).with_context(|| format!("file: {:?}", filename)) // TODO: memo
    }

    fn run_file_inner(&self, pkg: &str, filename: &str) -> anyhow::Result<RunResult> {
        let path_buf = self.root.join(pkg).join(filename);
        let path = path_buf.as_path();
        let ast: AstModule = AstModule::parse_file(path, &Dialect::Extended).map_err(|e| anyhow::anyhow!(e))?;

        let globals = get_globals();

        let module = Module::new();

        let targets = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        let states = std::rc::Rc::new(std::cell::RefCell::new(vec![]));

        {
            let extra = Extra{
                pkg,
                on_target: {
                    let targets = targets.clone();
                    Box::new(move |p| {
                        targets.borrow_mut().push(p);

                        Ok(())
                    })
                },
                on_state: {
                    let states = states.clone();
                    Box::new(move |p| {
                        states.borrow_mut().push(p);

                        Ok(())
                    })
                },
            };
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&extra);

            eval.eval_module(ast, globals).map_err(|e| anyhow::anyhow!(e))?;
        }

        let targets = std::rc::Rc::try_unwrap(targets).unwrap().into_inner();
        let states = std::rc::Rc::try_unwrap(states).unwrap().into_inner();

        Ok(RunResult{
            targets,
            states,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_run_file_inner() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
    labels = ["label1", "label2"],
    config_str = "hello",
    config_int = 42,
    config_bool = True,
    config_float = 1.5,
    config_list = ["a", 1],
)
"#;
        let filename = "BUILD".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let result = provider.run_file(&pkg_name, &filename).unwrap();

        assert_eq!(result.targets.len(), 1);
        let target = &result.targets[0];
        assert_eq!(target.name, "mytarget");
        assert_eq!(target.driver, "mydriver");
        assert_eq!(target.labels, vec!["label1".to_string(), "label2".to_string()]);

        if let Some(TargetSpecValue::String(s)) = target.config.get("config_str") {
            assert_eq!(s, "hello");
        } else {
            panic!("Expected string for config_str, got {:?}", target.config.get("config_str"));
        }

        if let Some(TargetSpecValue::Int(i)) = target.config.get("config_int") {
            assert_eq!(*i, 42);
        } else {
            panic!("Expected int for config_int, got {:?}", target.config.get("config_int"));
        }

        if let Some(TargetSpecValue::Bool(b)) = target.config.get("config_bool") {
            assert!(*b);
        } else {
            panic!("Expected bool for config_bool, got {:?}", target.config.get("config_bool"));
        }

        if let Some(TargetSpecValue::Float(f)) = target.config.get("config_float") {
            assert_eq!(*f, 1.5);
        } else {
            panic!("Expected float for config_float, got {:?}", target.config.get("config_float"));
        }

        if let Some(TargetSpecValue::List(l)) = target.config.get("config_list") {
            assert_eq!(l.len(), 2);
            if let TargetSpecValue::String(s) = &l[0] {
                assert_eq!(s, "a");
            } else {
                panic!("Expected string in list");
            }
            if let TargetSpecValue::Int(i) = &l[1] {
                assert_eq!(*i, 1);
            } else {
                panic!("Expected int in list");
            }
        } else {
            panic!("Expected list for config_list");
        }
    }

    fn make_provider(tmp_dir: &tempfile::TempDir) -> Provider {
        Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        }
    }

    fn run_transitive(build_content: &str) -> anyhow::Result<Sandbox> {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        let filename = "BUILD";
        fs::write(pkg_path.join(filename), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let result = provider.run_file(pkg_name, filename)?;
        Ok(result.targets.into_iter().next().map(|t| t.transitive).unwrap_or_default())
    }

    #[test]
    fn test_sandbox_empty_by_default() {
        let sandbox = run_transitive(r#"target(name="t", driver="d")"#).unwrap();
        assert!(sandbox.deps.is_empty());
        assert!(sandbox.tools.is_empty());
        assert!(sandbox.env.is_empty());
    }

    #[test]
    fn test_sandbox_null_transitive() {
        let sandbox = run_transitive(r#"target(name="t", driver="d", transitive=None)"#);
        // None is not a valid starlark value here; expect error or default
        // starlark doesn't have None by default in our globals, so this should error
        assert!(sandbox.is_err() || sandbox.unwrap().empty());
    }

    #[test]
    fn test_sandbox_deps_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"group1": ["//mypkg:other"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        let dep = &sandbox.deps[0];
        assert_eq!(dep.group, "group1");
        assert!(dep.runtime);
        assert!(dep.hash);
        assert_eq!(dep.r#ref.r#ref.name, "other");
        assert_eq!(dep.id, "dep|group1|0");
    }

    #[test]
    fn test_sandbox_multiple_deps() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"g": ["//mypkg:a", "//mypkg:b"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 2);
        let names: Vec<_> = sandbox.deps.iter().map(|d| d.r#ref.r#ref.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn test_sandbox_tools_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "tools": {"toolgroup": ["//mypkg:mytool"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.tools.len(), 1);
        let tool = &sandbox.tools[0];
        assert_eq!(tool.group, "toolgroup");
        assert!(tool.hash);
        assert_eq!(tool.r#ref.r#ref.name, "mytool");
        assert_eq!(tool.id, "tool|toolgroup|0");
    }

    #[test]
    fn test_sandbox_env_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "env": {"MY_VAR": "my_value"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.env.len(), 1);
        let env = sandbox.env.get("MY_VAR").unwrap();
        assert!(env.hash);
        assert!(!env.append);
        assert!(env.append_prefix.is_empty());
        match &env.value {
            EnvValue::Literal(s) => assert_eq!(s, "my_value"),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn test_sandbox_all_fields() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"dg": ["//mypkg:dep1"]},
        "tools": {"tg": ["//mypkg:tool1"]},
        "env": {"K": "V"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        assert_eq!(sandbox.tools.len(), 1);
        assert_eq!(sandbox.env.len(), 1);
    }

    #[test]
    fn test_sandbox_unknown_key_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "unknown_key": "value",
    },
)
"#;
        assert!(run_transitive(content).is_err());
    }

    #[test]
    fn test_sandbox_not_map_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = "bad",
)
"#;
        assert!(run_transitive(content).is_err());
    }

    #[test]
    fn test_run_pkg_inner_multiple_patterns() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
)
"#;
        let filename = "BUILD.heph".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec!["BUILD".to_string(), "BUILD.heph".to_string()],
            ..Provider::default()
        };

        let result = provider.run_pkg(&pkg_name).unwrap();

        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "mytarget");
    }
}