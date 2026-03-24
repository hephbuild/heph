//! Starlark integration for Heph build files

use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{list::AllocList, Value};
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;

pub mod buildfile;

#[derive(Error, Debug)]
pub enum StarlarkError {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Evaluation error: {0}")]
    EvalError(String),

    #[error("Type error: {0}")]
    TypeError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, StarlarkError>;

/// Collected targets from Starlark evaluation
#[derive(Debug, Clone, Default)]
pub struct CollectedData {
    pub targets: Vec<TargetData>,
    pub package_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TargetData {
    pub name: String,
    pub deps: Vec<String>,
    pub srcs: Vec<String>,
    pub outs: Vec<String>,
}

/// Starlark runtime for evaluating build files
pub struct StarlarkRuntime {
    globals: starlark::environment::Globals,
    _collected: Arc<Mutex<CollectedData>>,
}

impl StarlarkRuntime {
    pub fn new() -> Self {
        let collected = Arc::new(Mutex::new(CollectedData::default()));

        let globals = GlobalsBuilder::standard().with(heph_builtins).build();

        Self {
            globals,
            _collected: collected,
        }
    }

    /// Evaluate a Starlark build file
    pub fn eval(&self, filename: &str, content: &str) -> Result<()> {
        let ast = AstModule::parse(filename, content.to_owned(), &Dialect::Standard)
            .map_err(|e| StarlarkError::ParseError(e.to_string()))?;

        let module = Module::new();
        let mut eval = Evaluator::new(&module);

        eval.eval_module(ast, &self.globals)
            .map_err(|e| StarlarkError::EvalError(e.to_string()))?;

        Ok(())
    }

    /// Load and evaluate a build file from disk
    pub fn eval_file(&self, path: &str) -> Result<()> {
        let content = std::fs::read_to_string(path)?;
        self.eval(path, &content)
    }

    /// Get collected data from the last evaluation
    pub fn get_collected(&self) -> CollectedData {
        self._collected.lock().unwrap().clone()
    }
}

impl Default for StarlarkRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Heph-specific built-in functions
#[starlark::starlark_module]
#[allow(clippy::type_complexity)]
fn heph_builtins(builder: &mut GlobalsBuilder) {
    /// Define a build target
    fn target<'v>(
        name: String,
        deps: Option<Value<'v>>,
        srcs: Option<Value<'v>>,
        outs: Option<Value<'v>>,
    ) -> anyhow::Result<String> {
        // Simple implementation that returns the target name
        // In a full implementation, we would collect this into a shared state
        let _ = (deps, srcs, outs); // Mark as used
        Ok(format!("Target: {}", name))
    }

    /// Define a package
    fn package(name: String) -> anyhow::Result<String> {
        Ok(format!("Package: {}", name))
    }

    /// Glob files
    fn glob<'v>(pattern: String, heap: &'v starlark::values::Heap) -> anyhow::Result<Value<'v>> {
        // Simple implementation that returns a list with the pattern
        // In a full implementation, we would perform actual globbing
        Ok(heap.alloc(AllocList(&[pattern])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn test_eval_simple() {
        let runtime = StarlarkRuntime::new();
        let result = runtime.eval("test.star", "1 + 1");

        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_string() {
        let runtime = StarlarkRuntime::new();
        let result = runtime.eval("test.star", r#""hello " + "world""#);

        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_list() {
        let runtime = StarlarkRuntime::new();
        let result = runtime.eval("test.star", "[1, 2, 3]");

        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_target() {
        let runtime = StarlarkRuntime::new();
        let code = indoc! {r#"
            result = target(name="my_target", deps=["dep1", "dep2"])
        "#};

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_package() {
        let runtime = StarlarkRuntime::new();
        let code = r#"package(name="my_package")"#;

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_error() {
        let runtime = StarlarkRuntime::new();
        let result = runtime.eval("test.star", "invalid syntax )");

        assert!(result.is_err());
        assert!(matches!(result, Err(StarlarkError::ParseError(_))));
    }

    #[test]
    fn test_eval_glob() {
        let runtime = StarlarkRuntime::new();
        let code = r#"files = glob("*.go")"#;

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_dict() {
        let runtime = StarlarkRuntime::new();
        let code = r#"{"key": "value", "num": 42}"#;

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_function_def() {
        let runtime = StarlarkRuntime::new();
        let code = indoc! {r#"
            def hello(name):
                return "Hello, " + name

            result = hello("World")
        "#};

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_multiple_targets() {
        let runtime = StarlarkRuntime::new();
        let code = indoc! {r#"
            target(name="target1", deps=[])
            target(name="target2", deps=["target1"])
            target(name="target3", deps=["target1", "target2"])
        "#};

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }

    #[test]
    fn test_eval_complex_build_file() {
        let runtime = StarlarkRuntime::new();
        let code = indoc! {r#"
            package(name="my_package")

            srcs = glob("*.rs")

            target(
                name="lib",
                srcs=srcs,
                deps=["//common:lib"],
            )

            target(
                name="test",
                srcs=glob("*_test.rs"),
                deps=[":lib"],
            )
        "#};

        let result = runtime.eval("test.star", code);
        assert!(result.is_ok());
    }
}
