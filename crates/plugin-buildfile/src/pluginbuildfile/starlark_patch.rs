//! What heph's `[patch.crates-io]` of `starlark` (root `Cargo.toml`) adds to
//! 0.14.2, held here so an upgrade that loses it fails a test instead of
//! quietly typing every unannotated `def` as returning `Any` again, or every
//! parameter with a literal default as `Any`.

use std::collections::HashMap;

use starlark::environment::Globals;
use starlark::syntax::{AstModule, Dialect};
use starlark::typing::AstModuleTypecheck;

/// The static typechecker's errors for `src`, as their first lines.
fn type_errors(src: &str) -> Vec<String> {
    let ast = AstModule::parse("BUILD", src.to_owned(), &Dialect::Extended).expect("parses");
    let (errors, _, _, _) = ast.typecheck(&Globals::extended_internal(), &HashMap::new());
    errors
        .iter()
        .map(|e| {
            e.without_diagnostic()
                .to_string()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
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
