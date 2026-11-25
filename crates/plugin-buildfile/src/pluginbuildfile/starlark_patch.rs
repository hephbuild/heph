//! What heph's `[patch.crates-io]` of `starlark` (root `Cargo.toml`) adds to
//! 0.14.2, held here so an upgrade that loses it fails a test instead of
//! quietly typing every unannotated `def` as returning `Any` again, every
//! parameter with a literal default as `Any`, or every declared record, enum
//! and module-level call result as `Any`.

use std::collections::HashMap;

use starlark::environment::Globals;
use starlark::syntax::{AstModule, Dialect};
use starlark::typing::{AstModuleTypecheck, Interface, Ty, TypeMap, error_types};

/// The static typechecker's errors for `src`, as their first lines.
fn type_errors(src: &str) -> Vec<String> {
    type_errors_loading(src, &HashMap::new())
}

/// [`type_errors`], with `loads` as the interfaces of what `src` loads.
fn type_errors_loading(src: &str, loads: &HashMap<String, Interface>) -> Vec<String> {
    let (errors, _, _) = typecheck(src, loads);
    errors.iter().map(first_line).collect()
}

/// The static typechecker's errors, type map and interface for `src`.
fn typecheck(
    src: &str,
    loads: &HashMap<String, Interface>,
) -> (Vec<starlark::Error>, TypeMap, Interface) {
    let ast = AstModule::parse("BUILD", src.to_owned(), &Dialect::Extended).expect("parses");
    let (errors, types, iface, _) = ast.typecheck(&Globals::extended_internal(), loads);
    (errors, types, iface)
}

fn first_line(e: &starlark::Error) -> String {
    e.without_diagnostic()
        .to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A record and an enum declared at the top level are built in the lint
/// pre-pass, so a def that uses one is checked against its fields and values
/// rather than against `Any`.
#[test]
fn record_and_enum_built_in_the_pre_pass() {
    let errors = type_errors(
        r#"
Lib = record(name = str)
Mode = enum("debug", "release")

def use(l: Lib, m: Mode):
    l.nme
    m.nope
    Lib(name = 1)
"#,
    );
    assert_eq!(
        errors,
        [
            "The attribute `nme` is not available on the type `Lib`",
            "The attribute `nope` is not available on the type `Mode`",
            "Expected type `str` but got `int`",
        ]
    );
}

/// A loaded record types an annotation in the loading file:
/// `Interface::get_type` carries the type a name denotes, beside its value's
/// type.
#[test]
fn loaded_record_types_an_annotation() {
    let (errors, _, iface) = typecheck("Lib = record(name = str)\n", &HashMap::new());
    assert!(errors.is_empty(), "{errors:?}");
    assert!(iface.get_type("Lib").is_some(), "Lib denotes a type");

    let loads = HashMap::from([("lib.star".to_owned(), iface)]);
    let errors = type_errors_loading(
        r#"
load("lib.star", "Lib")

def use(l: Lib):
    l.nme
"#,
        &loads,
    );
    assert_eq!(
        errors,
        ["The attribute `nme` is not available on the type `Lib`"]
    );
}

/// A module-level call's result takes its callee's return type: a record
/// constructor's instance, a native's declared return type.
#[test]
fn module_call_result_typed() {
    let errors = type_errors(
        r#"
Lib = record(name = str)
cfg = Lib(name = "x")
n = len([])

def use():
    cfg.nme
    n.upper()
"#,
    );
    assert_eq!(
        errors,
        [
            "The attribute `nme` is not available on the type `Lib`",
            "The attribute `upper` is not available on the type `int`",
        ]
    );
}

/// An "Expected type" error names the types it is about (`error_types`), and
/// a record among them is found by identity (`Ty::user_type_ids`) in the
/// module's declarations (`module_bindings().declaration`), on its line.
#[test]
fn error_types_name_the_declaration() {
    let (errors, types, _) = typecheck(
        r#"
A = record(x = str)
B = record(x = str)

def f(a: A):
    pass

def g(b: B):
    f(b)
"#,
        &HashMap::new(),
    );
    let [error] = errors.as_slice() else {
        panic!("one error: {errors:?}");
    };
    assert_eq!(first_line(error), "Expected type `A` but got `B`");

    let declared: HashMap<_, _> = types
        .module_bindings()
        .filter_map(|b| {
            let (id, span) = b.declaration?;
            Some((id, (b.name.to_owned(), span.resolve_span().begin.line + 1)))
        })
        .collect();
    let mentioned: Vec<_> = error_types(error)
        .iter()
        .flat_map(Ty::user_type_ids)
        .map(|id| declared.get(&id).cloned())
        .collect();
    assert_eq!(
        mentioned,
        [Some(("A".to_owned(), 2)), Some(("B".to_owned(), 3))]
    );
}

#[test]
fn unannotated_return_is_inferred() {
    let errors = type_errors(
        r#"
def name_of(n: int):
    return str(n)

def use():
    n: int = name_of(1)
"#,
    );
    assert_eq!(errors, ["Expected type `int` but got `str`"]);
}

#[test]
fn inferred_return_includes_none_when_the_body_falls_through() {
    let errors = type_errors(
        r#"
def maybe(n: int):
    if n > 0:
        return "pos"

def use():
    s: str = maybe(1)
    n: int = maybe(1)
"#,
    );
    assert_eq!(errors, ["Expected type `int` but got `None | str`"]);
}

#[test]
fn unannotated_param_typed_from_literal_default() {
    let errors = type_errors(
        r#"
def make(n = 1, opt = None):
    s: str = n

def use():
    make(n = "s")
    make(opt = 1)
"#,
    );
    assert_eq!(
        errors,
        [
            "Expected type `str` but got `int`",
            "Expected type `int` but got `str`"
        ]
    );
}

#[test]
fn float_default_accepts_int() {
    let errors = type_errors(
        r#"
def scaled(scale = 1.0):
    pass

def use():
    scaled(2)
    scaled(2.5)
"#,
    );
    assert_eq!(errors, Vec::<String>::new());
}

/// The inference is the lint typechecker's only: a module that opts into
/// evaluation-time typechecking still sees `Any` for the parameter, so
/// evaluation is unchanged.
#[test]
fn evaluation_is_unchanged_by_the_inference() {
    use starlark::environment::Module;
    use starlark::eval::Evaluator;

    let eval = |src: &str| {
        let ast = AstModule::parse("BUILD", src.to_owned(), &Dialect::Extended).expect("parses");
        Module::with_temp_heap(|module| {
            let mut ev = Evaluator::new(&module);
            ev.eval_module(ast, &Globals::extended_internal())
                .map(drop)
                .map_err(|e| format!("{e:#}"))
        })
    };
    eval("# @starlark-rust: typecheck\ndef f(n = 1):\n    s: str = n\n").expect("evaluates");
    // The marker does typecheck: the same body against an annotation fails.
    let err = eval("# @starlark-rust: typecheck\ndef f(n: int = 1):\n    s: str = n\n")
        .expect_err("an annotated int is not a str");
    assert!(err.contains("Expected type `str` but got `int`"), "{err}");
}
