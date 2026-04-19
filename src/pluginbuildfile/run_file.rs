use crate::engine::provider::TargetSpecValue;
use crate::pluginbuildfile::provider::Provider;
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::float::UnpackFloat;
use starlark::values::list::UnpackList;
use starlark::values::{UnpackValue, Value, ValueLike};
use starlark::starlark_module;
use std::any::Any;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct OnTargetPayload {
    pub name: String,
    pub driver: String,
    pub labels: Vec<String>,
    pub config: HashMap<String, TargetSpecValue>,
}

#[derive(Debug)]
pub(crate) struct OnStatePayload {
    pub name: String,
    pub labels: Vec<String>,
}

pub(crate) struct RunResult {
    pub targets: Vec<OnTargetPayload>,
    pub states: Vec<OnStatePayload>,
}

#[derive(ProvidesStaticType)]
pub(crate) struct Extra<'a> {
    pub pkg: &'a String,
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

    panic!("Unsupported starlark value type: {}", v.get_type());
}

#[starlark_module]
fn starlark_module(builder: &mut GlobalsBuilder) {
    fn target<'v>(eval: &mut Evaluator<'v, '_, '_>, args: &Arguments<'v, '_>) -> starlark::Result<String> {
        args.no_positional_args(eval.heap())?;

        let mut m = args.names_map()?;

        let mut name = String::new();
        let mut driver = String::new();
        let mut labels: Vec<String>= vec![];
        let config = m.iter().filter_map(|e| {
            match e.0.as_str() {
                "name" => {
                    name = e.1.unpack_str()?.to_string();
                    None
                },
                "driver" => {
                    driver = e.1.unpack_str()?.to_string();
                    None
                },
                "labels" => {
                    labels = UnpackList::<String>::unpack_value_err(*e.1).ok().map(|l| l.items).unwrap_or_default();
                    None
                },
                _ => Some((e.0.as_str().to_string(), starlark_to_rust(e.1)))
            }
        }).collect();

        let p = OnTargetPayload{
            name,
            driver,
            labels,
            config,
        };

        (eval.extra.ok_or(starlark::Error::new_other(anyhow::anyhow!("Extra not found")))?.downcast_ref::<Extra>().unwrap().on_target)(p);

        Ok("".to_string())
    }
}

impl Provider {
    pub(crate) fn run_pkg(&self, pkg: &str) -> anyhow::Result<RunResult> {
        self.run_pkg_inner(pkg) // TODO: memo
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
        self.run_file_inner(pkg, filename) // TODO: memo
    }

    fn run_file_inner(&self, pkg: &str, filename: &str) -> anyhow::Result<RunResult> {
        let path_buf = self.root.join(pkg).join(filename);
        let path = path_buf.as_path();
        let ast: AstModule = AstModule::parse_file(path, &Dialect::Extended).map_err(|e| anyhow::anyhow!(e))?;

        let mut builder = GlobalsBuilder::new().with(starlark_module);
        builder.set("true", true);
        builder.set("false", false);
        let globals = builder.build();

        let module = Module::new();

        let targets = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        let states = std::rc::Rc::new(std::cell::RefCell::new(vec![]));

        {
            let extra = Extra{
                pkg: &pkg,
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

            eval.eval_module(ast, &globals).map_err(|e| anyhow::anyhow!(e))?;
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
    use std::sync::Mutex;
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
    config_bool = true,
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