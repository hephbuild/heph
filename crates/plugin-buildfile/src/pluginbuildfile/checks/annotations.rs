//! The `requireTypeAnnotations` rule: every `def` parameter carries a type,
//! `*args` and `**kwargs` included; the return type may be left out (heph's
//! starlark infers it from the def's returns). A parameter whose default is a
//! scalar literal may leave its type out too: heph's starlark types it from
//! the literal (`n = 1` is an `int`), see [`typed_by_default`]. A container
//! display does not exempt its parameter: heph's starlark types `srcs = []` as
//! a `list` of `Any`, which describes nothing, so the rule asks for a type
//! with no `Any` in it. Lambdas are exempt, as Starlark syntax cannot annotate
//! them.
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
                    message: message(&def.name.node.ident, &name, container_default(param)),
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

/// Whether `default` types an unannotated parameter well enough that the rule
/// exempts it.
///
/// This is the fork's rule (`literal_default_ty` in
/// `starlark/src/typing/bindings.rs`), restated over the parse tree, which
/// carries no name resolution: `1`, `-1`, `1.5`, `"s"`, `True` and `False`.
/// `None` and every other expression leave the parameter `Any`, so it still
/// needs an annotation.
///
/// Two places are stricter than the fork, on purpose. A list, dict or tuple
/// display types its parameter as a container of `Any` there; here it exempts
/// nothing, since `srcs = []` does not say what goes in it (see
/// [`container_default`]). And the fork takes `True`/`False` only when they
/// resolve to the globals, while here a module that binds either name anywhere
/// ([`bound_names`], flat) loses the exemption: erring toward a finding keeps
/// the rule from passing a parameter the typechecker sees as `Any`.
/// `rule_agrees_with_the_typechecker` holds the two to the same answers, and
/// to the one-way agreement on the stricter rows.
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
        _ => false,
    }
}

/// A container display default heph's starlark types its parameter from.
#[derive(Debug, Clone, Copy)]
struct Container {
    /// The type the default gives, as the typechecker prints it.
    ty: &'static str,
    /// What that type holds, in words.
    of: &'static str,
    /// A type with no `Any`, to show the shape of the fix.
    example: &'static str,
}

/// The container an unannotated parameter's default is a display of, if any:
/// `[]`/`["a"]`, `{}`/`{"k": 1}`, `()`/`(1, 2)`.
fn container_default(param: &AstParameter) -> Option<Container> {
    let ParameterP::Normal(_, None, Some(default)) = &param.node else {
        return None;
    };
    let (ty, of, example) = match default.node {
        Expr::List(_) => ("list", "a list of `Any`", "list[str]"),
        Expr::Dict(_) => ("dict", "a dict of `Any` to `Any`", "dict[str, str]"),
        Expr::Tuple(_) => ("tuple", "a tuple of `Any`", "tuple[str, ...]"),
        _ => return None,
    };
    Some(Container { ty, of, example })
}

fn message(def: &str, name: &str, container: Option<Container>) -> String {
    // A parameter named like a `target()` field passes it through: suggest the
    // alias that types it as `target()` accepts it.
    let alias =
        types::for_param(name).map(|alias| format!("heph.{}.{}", types::NAMESPACE, alias.name));
    match (container, alias) {
        (None, alias) => {
            let fix = alias.as_deref().unwrap_or("<type>");
            format!("parameter `{name}` of `{def}` has no type; add `: {fix}`")
        }
        (Some(c), alias) => {
            let typed = format!(
                "parameter `{name}` of `{def}` is typed `{}` by its default, {}",
                c.ty, c.of
            );
            match alias {
                Some(alias) => format!("{typed}; add `: {alias}`"),
                None => format!("{typed}; add a type with no `Any`, e.g. `: {}`", c.example),
            }
        }
    }
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

    /// A container display default is reported, empty or not, with the type it
    /// gives and a fix with no `Any`: the alias for a `target()` field, an
    /// example of the container's shape otherwise.
    #[test]
    fn container_default_finding() {
        let found = |param: &str| lines(&format!("def f({param}) -> None:\n    pass\n"));
        assert_eq!(
            found("srcs = []"),
            [
                "pkg/BUILD:1:7: annotation: parameter `srcs` of `f` is typed `list` by its \
                 default, a list of `Any`; add a type with no `Any`, e.g. `: list[str]`"
            ]
        );
        assert_eq!(
            found("xs = [\"a\"]"),
            [
                "pkg/BUILD:1:7: annotation: parameter `xs` of `f` is typed `list` by its \
                 default, a list of `Any`; add a type with no `Any`, e.g. `: list[str]`"
            ]
        );
        assert_eq!(
            found("creds = {}"),
            [
                "pkg/BUILD:1:7: annotation: parameter `creds` of `f` is typed `dict` by its \
                 default, a dict of `Any` to `Any`; add a type with no `Any`, e.g. \
                 `: dict[str, str]`"
            ]
        );
        assert_eq!(
            found("t = ()"),
            [
                "pkg/BUILD:1:7: annotation: parameter `t` of `f` is typed `tuple` by its \
                 default, a tuple of `Any`; add a type with no `Any`, e.g. \
                 `: tuple[str, ...]`"
            ]
        );
        assert_eq!(
            found("deps = []"),
            [
                "pkg/BUILD:1:7: annotation: parameter `deps` of `f` is typed `list` by its \
                 default, a list of `Any`; add `: heph.types.Deps`"
            ]
        );
        // Annotated, a container default is fine, `Any` in the annotation or
        // not: only the type a default gives is held to the rule.
        assert_eq!(
            found("srcs: list[str] = [], kv: dict[str, typing.Any] = {}"),
            Vec::<String>::new()
        );
        // Parentheses are not a tuple: `(1)` is the int literal.
        assert_eq!(found("n = (1)"), Vec::<String>::new());
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
        // `+`/`-` only on a number literal, and only once.
        ("", "--1", false),
        ("", "-\"a\"", false),
        ("", "-True", false),
        ("", "-[]", false),
        ("", "+1.5", true),
        ("", "-(1)", true),
        ("", "0x10", true),
        ("", "10000000000000000000000", true),
        // A literal, but not one that types: heph's dialect has types, so `...`
        // parses. (It has no f-strings: see `literal_default_typed`.)
        ("", "...", false),
        // A rebound `True` is not the global.
        ("True = 0\n", "True", false),
        ("True = 0\n", "False", true),
        ("False = 0\n", "True", true),
        ("False = 0\n", "False", false),
    ];

    fn src(prelude: &str, default: &str) -> String {
        format!("{prelude}def f(x = {default}) -> None:\n    pass\n")
    }

    /// Other parameter shapes for `x` in a top-level `def f`, and whether its
    /// default types it.
    const SHAPES: &[(&str, bool)] = &[
        ("def f(*, x = 1) -> None:\n    pass\n", true),
        ("def f(*, x = None) -> None:\n    pass\n", false),
        ("def f(*args: int, x = 1) -> None:\n    pass\n", true),
        ("def f(*args: int, x = None) -> None:\n    pass\n", false),
        ("def f(x = 1, **kwargs: int) -> None:\n    pass\n", true),
    ];

    /// Modules that bind `True` somewhere before `def f(x = True)`: the rule
    /// reports `x` in each. In the last, the typechecker still types `x`
    /// (the rebinding is local to `g`), so the rule is stricter than it by
    /// design: `bound_names` is flat.
    const STRICTER: &[&str] = &[
        "load(\"//lib/defs.star\", True = \"t\")\n",
        "for True in []:\n    pass\n",
        "True, a = 0, 1\n",
        "def True():\n    pass\n",
        "def g():\n    True = 0\n",
    ];

    fn stricter_src(prelude: &str) -> String {
        src(prelude, "True")
    }

    /// Container display defaults, as `def f(<param> = <default>)`: the rule
    /// reports each, although heph's starlark types the parameter (a list,
    /// dict or tuple of `Any`). Stricter than the typechecker by design: a
    /// display does not say what goes in it.
    const CONTAINER_DEFAULTS: &[(&str, &str)] = &[
        ("x", "[]"),
        ("x", "[\"a\", 1]"),
        ("x", "{}"),
        ("x", "{\"k\": 1}"),
        ("x", "()"),
        ("x", "(1, \"a\")"),
        ("srcs", "[]"),
        ("creds", "{}"),
        ("xs", "[\"a\"]"),
        ("t", "()"),
        ("deps", "[]"),
    ];

    fn container_src(param: &str, default: &str) -> String {
        format!("def f({param} = {default}) -> None:\n    pass\n")
    }

    #[test]
    fn literal_default_typed() {
        for (param, default) in CONTAINER_DEFAULTS {
            let found = lines(&container_src(param, default));
            let typed = format!("parameter `{param}` of `f` is typed");
            assert!(
                matches!(found.as_slice(), [one] if one.contains(&typed)),
                "`{param} = {default}`: {found:?}"
            );
        }
        for (prelude, default, typed) in DEFAULTS {
            let found = lines(&src(prelude, default));
            assert_eq!(found.is_empty(), *typed, "default `{default}`: {found:?}");
        }
        for (src, typed) in SHAPES {
            let found = lines(src);
            assert_eq!(found.is_empty(), *typed, "{src}: {found:?}");
        }
        for prelude in STRICTER {
            let found = lines(&stricter_src(prelude));
            assert!(
                matches!(found.as_slice(), [one] if one.contains("parameter `x` of `f`")),
                "{prelude}: {found:?}"
            );
        }
        // A nested def follows the same rule.
        let none = Vec::<String>::new();
        let nested = |default: &str| {
            lines(&format!(
                "def outer() -> None:\n    def f(x = {default}) -> None:\n        pass\n"
            ))
        };
        assert_eq!(nested("1"), none);
        assert_eq!(nested("None").len(), 1);
        // heph's dialect has no f-strings; a dialect that adds them needs a row.
        assert!(parse_build_source("pkg/BUILD", "def f(x = f\"a\"):\n    pass\n".into()).is_err());
        // `*args` and `**kwargs` have no default to type them.
        assert_eq!(lines("def f(*args, **kwargs):\n    pass\n").len(), 2);
    }

    /// Whether heph's starlark types `x` of the top-level `f` in `src`. A row
    /// the rule exempts must typecheck cleanly.
    fn typechecker_types_x(src: &str, exempt: bool) -> bool {
        typechecker_types(src, "x", exempt)
    }

    /// Whether heph's starlark types `param` of the top-level `f` in `src`.
    fn typechecker_types(src: &str, param: &str, exempt: bool) -> bool {
        use crate::pluginbuildfile::run_file::heph_globals;
        use starlark::typing::AstModuleTypecheck;
        use std::collections::HashMap;

        let ast = parse_build_source("pkg/BUILD", src.to_string()).expect("parses");
        let globals = heph_globals(
            &crate::pluginbuildfile::types::DriverKwargs::default(),
            |_| {},
        );
        let (errors, _, interface, _) = ast.typecheck(&globals, &HashMap::new());
        assert!(!exempt || errors.is_empty(), "{src}: {errors:?}");
        let ty = interface.get("f").expect("f in the interface").to_string();
        !ty.contains(&format!("{param}: typing.Any"))
    }

    /// The rule exempts `x` exactly when heph's starlark types it, on every
    /// `DEFAULTS` and `SHAPES` row. On the `STRICTER` and `CONTAINER_DEFAULTS`
    /// rows only one direction holds: the rule never exempts a parameter the
    /// typechecker sees as `Any`. On the container rows the typechecker does
    /// type the parameter, and the rule reports it anyway.
    #[test]
    fn rule_agrees_with_the_typechecker() {
        let exact = DEFAULTS
            .iter()
            .map(|(prelude, default, typed)| (src(prelude, default), *typed))
            .chain(SHAPES.iter().map(|(s, typed)| (s.to_string(), *typed)));
        for (src, typed) in exact {
            assert_eq!(typechecker_types_x(&src, typed), typed, "{src}");
        }
        for prelude in STRICTER {
            let src = stricter_src(prelude);
            let exempt = lines(&src).is_empty();
            if exempt {
                assert!(typechecker_types_x(&src, true), "{src}");
            }
        }
        for (param, default) in CONTAINER_DEFAULTS {
            let src = container_src(param, default);
            assert!(!lines(&src).is_empty(), "{src}");
            assert!(typechecker_types(&src, param, false), "{src}");
        }
    }
}
