//! The `requireTypeAnnotations` rule: every `def` parameter carries a type,
//! `*args` and `**kwargs` included; the return type may be left out (heph's
//! starlark infers it from the def's returns). A parameter whose default is a
//! literal may leave its type out too: heph's starlark types it from the
//! literal (`n = 1` is an `int`), see [`typed_by_default`]. Lambdas are
//! exempt, as Starlark syntax cannot annotate them.
//!
//! Pure over a parsed module, so `build-lint-check` and the BUILD-file LSP
//! report the same findings from the same code.

use super::report::{Finding, Kind};
use crate::pluginbuildfile::ast::{bound_names, walk_stmt};
use crate::pluginbuildfile::types;
use starlark::codemap::Span;
use starlark::syntax::AstModule;
use starlark::syntax::ast::{AstExpr, AstLiteral, AstParameter, Expr, ParameterP, Stmt};
use std::collections::HashSet;

/// Every unannotated `def` parameter in `ast`, nested defs included, in
/// source order. Positions are 1-based, columns counted in characters.
pub(crate) fn check(ast: &AstModule) -> Vec<Finding> {
    missing(ast).into_iter().map(|m| finding(ast, m)).collect()
}

/// One unannotated parameter: where it is, and what to say about it.
#[derive(Debug)]
pub(crate) struct Missing {
    /// The parameter's span, as written (`x = 1`, `*args`).
    pub span: Span,
    /// What is wrong and the edit that fixes it.
    pub message: String,
}

/// [`check`]'s findings as spans, for a caller that marks them in place
/// (the BUILD-file LSP), in source order.
pub(crate) fn missing(ast: &AstModule) -> Vec<Missing> {
    let mut out = Vec::new();
    // Only a module that rebinds `True` or `False` needs this, but `bound_names`
    // is one walk and the rule must not guess.
    let bound = bound_names(ast);
    walk_stmt(ast.statement(), &mut |s| {
        let Stmt::Def(def) = &s.node else {
            return;
        };
        for param in &def.params {
            if let Some(name) = unannotated(param, &bound) {
                out.push(Missing {
                    span: param.span,
                    message: message(&def.name.node.ident, &name),
                });
            }
        }
    });
    out
}

/// The parameter as written (`x`, `*args`, `**kwargs`) when it has no type.
///
/// Deliberately exhaustive, with no wildcard arm: a parameter kind a starlark
/// upgrade adds must be decided here, not silently passed.
fn unannotated(param: &AstParameter, bound: &HashSet<&str>) -> Option<String> {
    match &param.node {
        // Markers, not parameters: nothing to annotate.
        ParameterP::Slash | ParameterP::NoArgs => None,
        ParameterP::Normal(_, None, Some(default)) if typed_by_default(default, bound) => None,
        ParameterP::Normal(id, ty, _default) => ty.is_none().then(|| id.node.ident.clone()),
        ParameterP::Args(id, ty) => ty.is_none().then(|| format!("*{}", id.node.ident)),
        ParameterP::KwArgs(id, ty) => ty.is_none().then(|| format!("**{}", id.node.ident)),
    }
}

/// Whether heph's starlark types an unannotated parameter from `default`.
///
/// This is the fork's rule (`literal_default_ty` in
/// `starlark/src/typing/bindings.rs`), restated over the parse tree, which
/// carries no name resolution: `1`, `-1`, `1.5`, `"s"`, `True`, `False`, and a
/// list, dict or tuple display. `None` and every other expression leave the
/// parameter `Any`, so it still needs an annotation.
///
/// The fork takes `True`/`False` only when they resolve to the globals. Here a
/// module that binds either name anywhere ([`bound_names`], flat) loses the
/// exemption: erring toward a finding keeps the rule from passing a parameter
/// the typechecker sees as `Any`. `rule_agrees_with_the_typechecker` holds the
/// two to the same answers.
fn typed_by_default(default: &AstExpr, bound: &HashSet<&str>) -> bool {
    let number = |e: &AstExpr| {
        matches!(
            e.node,
            Expr::Literal(AstLiteral::Int(_) | AstLiteral::Float(_))
        )
    };
    match &default.node {
        Expr::Literal(AstLiteral::String(_)) => true,
        Expr::Literal(_) => number(default),
        Expr::Minus(e) | Expr::Plus(e) => number(e),
        Expr::Identifier(id) => {
            let name = id.node.ident.as_str();
            matches!(name, "True" | "False") && !bound.contains(name)
        }
        Expr::List(_) | Expr::Dict(_) | Expr::Tuple(_) => true,
        _ => false,
    }
}

fn message(def: &str, name: &str) -> String {
    // A parameter named like a `target()` field passes it through: suggest the
    // alias that types it as `target()` accepts it.
    let fix = match types::for_param(name) {
        Some(alias) => format!("heph.{}.{}", types::NAMESPACE, alias.name),
        None => "<type>".to_string(),
    };
    format!("parameter `{name}` of `{def}` has no type; add `: {fix}`")
}

fn finding(ast: &AstModule, missing: Missing) -> Finding {
    let span = ast.file_span(missing.span);
    let begin = span.resolve_span().begin;
    Finding {
        file: span.filename().to_string(),
        line: begin.line + 1,
        col: begin.column + 1,
        kind: Kind::Annotation,
        message: missing.message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::parse_build_source;

    /// The findings for `src` in a file named `pkg/BUILD`, as report lines.
    fn lines(src: &str) -> Vec<String> {
        let ast = parse_build_source("pkg/BUILD", src.to_string()).expect("parses");
        check(&ast).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn missing_param_reported() {
        assert_eq!(
            lines("def make_lib(name: str, x):\n    pass\n"),
            ["pkg/BUILD:1:25: annotation: parameter `x` of `make_lib` has no type; add `: <type>`"]
        );
    }

    #[test]
    fn all_annotated_passes() {
        assert_eq!(
            lines(
                "def f(a: str, b: int = 1, *args: str, c: bool = False, **kwargs: int) -> None:\n    \
                 pass\n"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn star_args_and_kwargs_required() {
        assert_eq!(
            lines("def f(*args, **kwargs):\n    pass\n"),
            [
                "pkg/BUILD:1:7: annotation: parameter `*args` of `f` has no type; add `: <type>`",
                "pkg/BUILD:1:14: annotation: parameter `**kwargs` of `f` has no type; add `: <type>`",
            ]
        );
    }

    #[test]
    fn return_type_optional() {
        // No return type: nothing to report.
        assert_eq!(
            lines("def f(x: int):\n    return x\n"),
            Vec::<String>::new()
        );
        // An annotated return is still typechecked, by the lint check.
        let tmp =
            super::super::test_ws::ws(&[("pkg/BUILD", "def f(x: int) -> str:\n    return x\n")]);
        let findings = super::super::lint::tests::lint_pkg(tmp.path(), "pkg", true);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].kind, Kind::TypeError, "{findings:#?}");
    }

    #[test]
    fn reports_all_sorted() {
        let src = "\
def b(y, z: int):
    def inner(w):
        pass

def a(x):
    pass
";
        let ast = parse_build_source("pkg/BUILD", src.to_string()).expect("parses");
        let mut findings = check(&ast);
        findings.sort();
        let at: Vec<(usize, usize)> = findings.iter().map(|f| (f.line, f.col)).collect();
        assert_eq!(at, [(1, 7), (2, 15), (5, 7)]);
    }

    #[test]
    fn off_reports_nothing() {
        let tmp = super::super::test_ws::ws(&[("pkg/BUILD", "def f(x, *args):\n    pass\n")]);
        assert!(super::super::lint::tests::lint_pkg(tmp.path(), "pkg", false).is_empty());
        assert_eq!(
            super::super::lint::tests::lint_pkg(tmp.path(), "pkg", true).len(),
            2
        );
    }

    #[test]
    fn suggests_alias() {
        let found = lines(
            "def lib(name: str, deps = None, labels = None, env = None, pass_env = None, \
             runtime_pass_env = None, tools = None, other = None):\n    pass\n",
        );
        let fixes: Vec<&str> = found
            .iter()
            .map(|l| l.rsplit_once("add `: ").expect("a fix").1)
            .collect();
        assert_eq!(
            fixes,
            [
                "heph.types.Deps`",
                "heph.types.Labels`",
                "heph.types.Env`",
                "heph.types.PassEnv`",
                "heph.types.RuntimePassEnv`",
                "heph.types.Tools`",
                "<type>`",
            ]
        );
        // `*deps` is not a field pass-through: no alias.
        assert!(lines("def f(*deps):\n    pass\n")[0].ends_with("add `: <type>`"));
    }

    /// One case per row of the spec's adversarial table for this checker.
    #[test]
    fn adversarial() {
        let none = Vec::<String>::new();
        // A bare `*` is a marker.
        assert_eq!(lines("def f(a: int, *, b: int) -> None:\n    pass\n"), none);
        // `/` is a marker. heph's dialect refuses it at parse, so the checker
        // is held to it over a dialect that has it: an upgrade that enables it
        // must not turn the marker into a finding.
        let slash = "def f(a: int, /, b: int) -> None:\n    pass\n";
        assert!(parse_build_source("pkg/BUILD", slash.to_string()).is_err());
        let dialect = starlark::syntax::Dialect {
            enable_positional_only_arguments: true,
            ..starlark::syntax::Dialect::Extended
        };
        let ast = AstModule::parse("pkg/BUILD", slash.to_string(), &dialect).expect("parses");
        assert_eq!(check(&ast), []);
        // A literal default types the parameter; `None` does not (more rows in
        // `literal_default_typed`).
        assert_eq!(lines("def f(x = 1) -> None:\n    pass\n"), none);
        assert_eq!(
            lines("def f(x = None) -> None:\n    pass\n"),
            ["pkg/BUILD:1:7: annotation: parameter `x` of `f` has no type; add `: <type>`"]
        );
        assert_eq!(lines("def f(x: int = 1) -> None:\n    pass\n"), none);
        assert_eq!(lines("def f() -> None:\n    pass\n"), none);
        // The return type is optional.
        assert_eq!(lines("def f():\n    pass\n"), none);
        // Nested in a def, an if and a for body: each reported.
        let nested = lines(
            "\
def outer() -> None:
    def in_def(a):
        pass

if True:
    def in_if(b):
        pass

for i in []:
    def in_for(c):
        pass
",
        );
        assert_eq!(
            nested,
            [
                "pkg/BUILD:2:16: annotation: parameter `a` of `in_def` has no type; add `: <type>`",
                "pkg/BUILD:6:15: annotation: parameter `b` of `in_if` has no type; add `: <type>`",
                "pkg/BUILD:10:16: annotation: parameter `c` of `in_for` has no type; add `: <type>`",
            ]
        );
        // Lambdas are exempt.
        assert_eq!(lines("f = lambda x: x\n"), none);
        // Columns count characters, as an editor does: `é` is one column, two
        // bytes. The lexer takes ASCII identifiers only (`def f(été)` does not
        // parse), so the non-ASCII text sits in a default before the finding.
        assert!(parse_build_source("pkg/BUILD", "def f(été):\n    pass\n".into()).is_err());
        assert_eq!(
            lines("def f(a: str = \"été\", b = None):\n    pass\n"),
            ["pkg/BUILD:1:23: annotation: parameter `b` of `f` has no type; add `: <type>`"]
        );
        // A multi-line signature: each finding on its own parameter's line.
        let multi = lines("def f(\n    a,\n    b: int,\n    c,\n):\n    pass\n");
        assert_eq!(
            multi,
            [
                "pkg/BUILD:2:5: annotation: parameter `a` of `f` has no type; add `: <type>`",
                "pkg/BUILD:4:5: annotation: parameter `c` of `f` has no type; add `: <type>`",
            ]
        );
        // An empty file, and one of comments only.
        assert_eq!(lines(""), none);
        assert_eq!(lines("# a comment\n# another\n"), none);
        // A file that fails to parse: the parse error, not a findings list.
        let tmp = super::super::test_ws::ws(&[("pkg/BUILD", "def f(x:\n")]);
        let err = super::super::lint::tests::lint_pkg_err(tmp.path(), "pkg", true);
        assert!(err.contains("pkg/BUILD"), "{err}");
    }

    /// Defaults for `def f(x = <default>)`, after an optional module prelude,
    /// and whether the default types `x`: literals and the near misses
    /// around them.
    const DEFAULTS: &[(&str, &str, bool)] = &[
        ("", "1", true),
        ("", "-1", true),
        ("", "+2", true),
        ("", "1.5", true),
        ("", "-1.5", true),
        ("", "\"s\"", true),
        ("", "\"\"", true),
        ("", "True", true),
        ("", "False", true),
        ("", "[]", true),
        ("", "[\"a\", 1]", true),
        ("", "{}", true),
        ("", "{\"k\": 1}", true),
        ("", "()", true),
        ("", "(1, \"a\")", true),
        // Parentheses are not a node: still the literal.
        ("", "(1)", true),
        ("", "None", false),
        // A name, even one bound to a literal.
        ("D = 1\n", "D", false),
        ("D = 1\n", "-D", false),
        ("", "1 + 1", false),
        ("", "\"a\" + \"b\"", false),
        ("", "not True", false),
        ("", "len(\"a\")", false),
        ("", "[i for i in []]", false),
        ("", "{k: 1 for k in []}", false),
        ("", "1 if True else 2", false),
        ("", "lambda: 1", false),
        // A rebound `True` is not the global.
        ("True = 0\n", "True", false),
        ("False = 0\n", "True", true),
        ("False = 0\n", "False", false),
    ];

    fn src(prelude: &str, default: &str) -> String {
        format!("{prelude}def f(x = {default}) -> None:\n    pass\n")
    }

    #[test]
    fn literal_default_typed() {
        for (prelude, default, typed) in DEFAULTS {
            let found = lines(&src(prelude, default));
            assert_eq!(found.is_empty(), *typed, "default `{default}`: {found:?}");
        }
        // `*args` and `**kwargs` have no default to type them.
        assert_eq!(lines("def f(*args, **kwargs):\n    pass\n").len(), 2);
    }

    /// The rule and heph's starlark agree on every row: a parameter the rule
    /// passes is one the typechecker does not see as `Any`.
    #[test]
    fn rule_agrees_with_the_typechecker() {
        use crate::pluginbuildfile::run_file::heph_globals;
        use starlark::typing::AstModuleTypecheck;
        use std::collections::HashMap;

        let globals = heph_globals(|_| {});
        for (prelude, default, typed) in DEFAULTS {
            let ast = parse_build_source("pkg/BUILD", src(prelude, default)).expect("parses");
            let (errors, _, interface, _) = ast.typecheck(&globals, &HashMap::new());
            assert!(errors.is_empty(), "default `{default}`: {errors:?}");
            let ty = interface.get("f").expect("f in the interface").to_string();
            assert_eq!(
                !ty.contains("x: typing.Any"),
                *typed,
                "default `{default}`: starlark typed f as `{ty}`"
            );
        }
    }
}
