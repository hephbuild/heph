//! `build-lint-check`'s checker: Starlark's static typechecker over a
//! package's files, without executing any of them (I4), plus the
//! `requireTypeAnnotations` rule ([`annotations`](super::annotations)).
//!
//! Every file of the package's [`LoadWalk`] is parsed by its workspace-relative
//! name and typechecked after the files it loads, each fed the `Interface`s of
//! what it loads, keyed by the literal `load()` string of *that* file
//! (`./defs.star` in two packages names two files). The checker never resolves
//! a load: the walk's edges are the only answer (I8), and a load string they do
//! not name fails rather than typechecking against an empty interface.
//!
//! Findings in a file the package does not own are discarded (I1); its
//! interface is still used by the files that load it.

use super::annotations;
use super::loads::{LoadEdge, LoadWalk, owns};
use super::report::{Finding, Kind};
use crate::pluginbuildfile::ast::bound_names;
use crate::pluginbuildfile::run_file::{heph_globals, is_exported, parse_build_source};
use crate::pluginbuildfile::types::DriverKwargs;
use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::environment::Globals;
use starlark::typing::{AstModuleTypecheck, Interface, Ty};
use starlark::values::typing::{StarlarkCallableParamAny, StarlarkCallableParamSpec};
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// Check `walk`'s package: typecheck every file in dependency order and, when
/// `require_annotations`, apply the annotation rule to the owned ones.
///
/// - `read` returns a file's content by workspace-relative path; the driver
///   reads its sandbox inputs ([`read_under`]). A file it cannot read fails
///   the check, naming the file: there is no empty-interface fallback.
/// - `plugin_fns` are the `(plugin, fn)` names callable as
///   `heph.<plugin>.<fn>`. The checker sees each as an opaque callable taking
///   anything and returning anything, so a plugin function's signature cannot
///   move the verdict, and only the names reach the def hash.
/// - `kwargs` are every driver's `heph.types.<driver>.Kwargs`, typed per key
///   as evaluation types them; their digest is in the def hash.
///
/// Returns the owned files' findings, sorted. An error means the check could
/// not run (an unreadable or unparseable file, a load the walk does not name),
/// never a finding.
pub(crate) fn lint(
    walk: &LoadWalk,
    read: &mut dyn FnMut(&str) -> anyhow::Result<String>,
    plugin_fns: &[(&str, &str)],
    kwargs: &DriverKwargs,
    require_annotations: bool,
) -> anyhow::Result<Vec<Finding>> {
    let globals = check_globals(plugin_fns, kwargs);
    let mut checked: HashMap<&str, Checked> = HashMap::with_capacity(walk.files.len());
    let mut findings = Vec::new();

    for path in walk.dependency_order()? {
        let file = walk
            .files
            .get(path)
            .with_context(|| format!("load graph orders {path}, which it does not record"))?;
        let src = read(path)?;
        let ast = parse_build_source(path, src)?;

        let mut loads = HashMap::new();
        for load in ast.loads() {
            let edge = file.loads.get(load.module_id).with_context(|| {
                format!(
                    "{}: load(\"{}\") is not in the package's load graph",
                    load.span, load.module_id
                )
            })?;
            loads.insert(load.module_id.to_owned(), interface_of(edge, &checked)?);
        }

        let owned = owns(&walk.pkg, file);
        if owned && require_annotations {
            findings.extend(annotations::check(&ast));
        }
        // The fork's `Interface` has `new` and `get` but no way to list its
        // names (adding `iter` upstream was blocked), so a directory load's
        // merge enumerates each file's exported names from its syntax tree:
        // every top-level binding, looked up in the interface. Names bound
        // only inside a def are not in the interface and drop out there.
        let exported: BTreeSet<String> = bound_names(&ast)
            .into_iter()
            .filter(|n| is_exported(n))
            .map(str::to_owned)
            .collect();

        // Approximations mark where the checker gave up, not wrong code.
        let (errors, _types, iface, _approximations) = ast.typecheck(&globals, &loads);
        // An internal error is the checker failing, not the code: the check
        // could not run, whichever file it is in.
        if let Some(e) = errors
            .iter()
            .find(|e| matches!(e.kind(), starlark::ErrorKind::Internal(_)))
        {
            anyhow::bail!(
                "the typechecker failed on {path}: {}",
                e.without_diagnostic()
            );
        }
        if owned {
            findings.extend(errors.iter().map(|e| type_error(path, e)));
        }
        checked.insert(path, Checked { iface, exported });
    }

    findings.sort();
    Ok(findings)
}

/// Read workspace-relative paths under `root` (the driver's sandbox). An
/// error names the workspace-relative path, never the sandbox's.
pub(crate) fn read_under(root: &Path) -> impl FnMut(&str) -> anyhow::Result<String> + '_ {
    move |rel| std::fs::read_to_string(root.join(rel)).with_context(|| format!("reading {rel}"))
}

/// A typechecked file: its interface, and the exported names it binds at the
/// top level.
struct Checked {
    iface: Interface,
    exported: BTreeSet<String>,
}

/// The interface one `load()` sees. A file load is the file's own; a
/// directory load merges its files' interfaces as evaluation merges their
/// modules (`merge_pkg_results`): in order, a later file's name wins, and only
/// exported names ([`is_exported`]) are re-exported.
fn interface_of(edge: &LoadEdge, checked: &HashMap<&str, Checked>) -> anyhow::Result<Interface> {
    let get = |path: &str| {
        checked
            .get(path)
            .with_context(|| format!("{path} is loaded before it was checked"))
    };
    match edge {
        LoadEdge::File { path, .. } => Ok(get(path)?.iface.clone()),
        LoadEdge::Dir { files, .. } => {
            let mut merged = HashMap::new();
            for path in files {
                let file = get(path)?;
                for name in &file.exported {
                    if let Some(ty) = file.iface.get(name) {
                        merged.insert(name.clone(), ty.clone());
                    }
                }
            }
            Ok(Interface::new(merged))
        }
    }
}

/// A typechecker error as a `type-error:` finding in `path`, at its span's
/// start (columns in characters), its message's first line.
fn type_error(path: &str, e: &starlark::Error) -> Finding {
    let (line, col) = e.span().map_or((1, 1), |span| {
        let begin = span.resolve_span().begin;
        (begin.line + 1, begin.column + 1)
    });
    let message = e.without_diagnostic().to_string();
    Finding {
        file: path.to_owned(),
        line,
        col,
        kind: Kind::TypeError,
        message: message.lines().next().unwrap_or_default().to_owned(),
    }
}

/// The globals the checker typechecks against: evaluation's static surface
/// (`target()` and the other builtins, `heph.core`, `heph.types` with each
/// driver's `Kwargs`) plus one [`OpaquePluginFn`] per plugin function name.
fn check_globals(plugin_fns: &[(&str, &str)], kwargs: &DriverKwargs) -> Globals {
    let mut by_plugin: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for &(plugin, func) in plugin_fns {
        by_plugin.entry(plugin).or_default().insert(func);
    }
    heph_globals(kwargs, |hb| {
        for (plugin, fns) in &by_plugin {
            hb.namespace(plugin, |nb| {
                for func in fns {
                    nb.set(
                        func,
                        OpaquePluginFn {
                            display: format!("heph.{plugin}.{func}"),
                        },
                    );
                }
            });
        }
    })
}

/// A `heph.<plugin>.<fn>` stand-in for the typechecker: callable with any
/// arguments, returning anything. Never evaluated.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct OpaquePluginFn {
    display: String,
}

impl std::fmt::Display for OpaquePluginFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

starlark::starlark_simple_value!(OpaquePluginFn);

#[starlark_value(type = "provider_function")]
impl<'v> StarlarkValue<'v> for OpaquePluginFn {
    fn typechecker_ty(&self) -> Option<Ty> {
        // `*args: Any, **kwargs: Any`.
        Some(Ty::callable(StarlarkCallableParamAny::params(), Ty::any()))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::pluginbuildfile::checks::test_ws::{chain, walk, ws};

    /// `lint` over `pkg` in the workspace at `root`, with `heph.fs.glob` and
    /// the test drivers' `Kwargs` known.
    fn run(root: &Path, pkg: &str, require: bool) -> anyhow::Result<Vec<Finding>> {
        let walk = walk(root, pkg)?;
        let kwargs = crate::pluginbuildfile::types::tests::test_kwargs();
        lint(
            &walk,
            &mut read_under(root),
            &[("fs", "glob")],
            &kwargs,
            require,
        )
    }

    /// The findings of `pkg`, which must check.
    pub(in crate::pluginbuildfile::checks) fn lint_pkg(
        root: &Path,
        pkg: &str,
        require: bool,
    ) -> Vec<Finding> {
        run(root, pkg, require).unwrap_or_else(|e| panic!("lint {pkg}: {}", chain(&e)))
    }

    /// Why `pkg` cannot be checked.
    pub(in crate::pluginbuildfile::checks) fn lint_pkg_err(
        root: &Path,
        pkg: &str,
        require: bool,
    ) -> String {
        match run(root, pkg, require) {
            Ok(findings) => panic!("lint {pkg} checked: {findings:#?}"),
            Err(e) => chain(&e),
        }
    }

    fn lines(findings: &[Finding]) -> Vec<String> {
        findings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn type_errors_all_reported() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "def a():\n    x: int = \"s\"\n    y: str = 1\n\ndef b():\n    z: bool = 1\n",
            ),
            ("pkg/more.BUILD", "def c():\n    w: int = \"s\"\n"),
        ]);
        let found = lint_pkg(tmp.path(), "pkg", false);
        let at: Vec<(&str, usize)> = found.iter().map(|f| (f.file.as_str(), f.line)).collect();
        assert_eq!(
            at,
            [
                ("pkg/BUILD", 2),
                ("pkg/BUILD", 3),
                ("pkg/BUILD", 6),
                ("pkg/more.BUILD", 2)
            ],
            "{found:#?}"
        );
        assert!(found.iter().all(|f| f.kind == Kind::TypeError));
        assert_eq!(
            found[0].to_string(),
            "pkg/BUILD:2:8: type-error: Expected type `int` but got `str`"
        );
    }

    #[test]
    fn loaded_signature_checked() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"make\")\n\ndef ok():\n    make(1)\n\n\
                 def bad():\n    make(\"s\")\n",
            ),
            ("lib/defs.star", "def make(n: int) -> None:\n    pass\n"),
        ]);
        let found = lint_pkg(tmp.path(), "pkg", false);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!((found[0].file.as_str(), found[0].line), ("pkg/BUILD", 7));
    }

    /// A literal default types its parameter, across a load: a caller passing
    /// another type is a type error, and the parameter needs no annotation.
    #[test]
    fn literal_default_types_param() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"make\")\n\ndef ok():\n    make(n = 2)\n\n\
                 def bad():\n    make(n = \"s\")\n",
            ),
            (
                "lib/defs.star",
                "def make(n = 1, opt = None) -> None:\n    pass\n",
            ),
            // Makes `lib` a package owning `defs.star`.
            ("lib/BUILD", "load(\"//lib/defs.star\", \"make\")\n"),
        ]);
        let found = lint_pkg(tmp.path(), "pkg", false);
        assert_eq!(
            lines(&found),
            ["pkg/BUILD:7:10: type-error: Expected type `int` but got `str`"]
        );
        // With annotations required, `lib` reports only `opt`, whose `None`
        // default types nothing.
        let lib = lint_pkg(tmp.path(), "lib", true);
        assert_eq!(
            lines(&lib),
            [
                "lib/defs.star:1:17: annotation: parameter `opt` of `make` has no type; add `: <type>`"
            ]
        );
    }

    #[test]
    fn approximation_ignored() {
        // Assigning through an indexed expression is beyond the checker: it
        // records an approximation, which must not fail the check.
        let src = "def f(d: dict[str, list[int]]) -> None:\n    d[\"k\"][0] = 1\n";
        let ast = parse_build_source("pkg/BUILD", src.to_string()).expect("parses");
        let (errors, _, _, approximations) =
            ast.typecheck(&check_globals(&[], &none()), &HashMap::new());
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!approximations.is_empty(), "the fixture must approximate");

        let tmp = ws(&[("pkg/BUILD", src)]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    #[test]
    fn unresolved_load_fails() {
        // The walk itself refuses a load it cannot resolve...
        let tmp = ws(&[("pkg/BUILD", "load(\"//lib/nope.star\", \"X\")\n")]);
        assert!(walk(tmp.path(), "pkg").is_err());
        // ...and an edge naming a file the walk does not hold fails the check
        // rather than typechecking against an empty interface.
        let tmp = ws(&[
            ("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
            ("lib/defs.star", "X = 1\n"),
        ]);
        let mut graph = walk(tmp.path(), "pkg").expect("walk");
        graph.files.remove("lib/defs.star");
        let err =
            lint(&graph, &mut read_under(tmp.path()), &[], &none(), false).expect_err("fails");
        assert!(chain(&err).contains("lib/defs.star"), "{}", chain(&err));
    }

    #[test]
    fn missing_sandbox_file_fails() {
        let tmp = ws(&[
            ("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
            ("lib/defs.star", "X = 1\n"),
        ]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        // The sandbox holds the package's file but not the one it loads.
        let sandbox = ws(&[("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n")]);
        let err = lint(&graph, &mut read_under(sandbox.path()), &[], &none(), false)
            .expect_err("a missing loaded file fails");
        assert!(
            chain(&err).contains("reading lib/defs.star"),
            "{}",
            chain(&err)
        );
    }

    #[test]
    fn verdict_independent_of_runtime() {
        // Host builtins and plugin functions are typed, never called: the
        // verdict is the same on any host, whatever they would return.
        let src = "\
def f() -> None:
    os: str = heph.core.os()
    pkgs = heph.core.packages(\"//...\")
    srcs = heph.fs.glob(\"*.go\")
    bad: int = heph.core.os()

TOP = heph.core.os()
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        let found = lint_pkg(tmp.path(), "pkg", true);
        assert_eq!(
            lines(&found),
            ["pkg/BUILD:5:10: type-error: Expected type `int` but got `str`"]
        );
    }

    /// T10: `heph.core.matrix` has a typed signature.
    #[test]
    fn matrix_signature_is_typed() {
        let src = "\
def f() -> None:
    ok = heph.core.matrix(axes = {\"go-version\": [\"1.22\", 1, None], \"if\": [[1], {\"a\": 1}]}, cell = lambda **kw: kw, default = {\"go-version\": 1, \"if\": [1]}, exclude = [{\"if\": {\"a\": 1}}], include = [{\"go-version\": (1,), \"if\": True}])
    bad_axes = heph.core.matrix(axes = 1)
    bad_name = heph.core.matrix(axis = {\"a\": [\"b\"]})
    bad_default = heph.core.matrix(axes = {\"a\": [\"b\"]}, default = 1)
    string_default = heph.core.matrix(axes = {\"a\": [\"b\"]}, default = \"b\")
    bad_values = heph.core.matrix(axes = {\"a\": \"b\"})
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        let found = lint_pkg(tmp.path(), "pkg", true);
        let got = lines(&found);
        assert_eq!(got.len(), 5, "{got:#?}");
        for (line, finding) in [3, 4, 5, 6, 7].iter().zip(&got) {
            assert!(
                finding.starts_with(&format!("pkg/BUILD:{line}:"))
                    && finding.contains("type-error"),
                "{got:#?}"
            );
        }
    }

    #[test]
    fn non_owned_findings_discarded() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"make\")\n\ndef f(x):\n    make(\"s\")\n",
            ),
            (
                "lib/defs.star",
                "def make(n: int, untyped) -> None:\n    bad: int = \"s\"\n",
            ),
        ]);
        let found = lint_pkg(tmp.path(), "pkg", true);
        // The helper's own type error and missing annotation are not pkg's;
        // its interface still types the call.
        assert!(found.iter().all(|f| f.file == "pkg/BUILD"), "{found:#?}");
        let kinds: Vec<Kind> = found.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, [Kind::Annotation, Kind::TypeError], "{found:#?}");
    }

    #[test]
    fn unknown_load_string_fails() {
        let tmp = ws(&[
            ("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
            ("lib/defs.star", "X = 1\n"),
        ]);
        let mut graph = walk(tmp.path(), "pkg").expect("walk");
        let edge = graph
            .files
            .get_mut("pkg/BUILD")
            .expect("pkg/BUILD")
            .loads
            .remove("//lib/defs.star")
            .expect("edge");
        graph
            .files
            .get_mut("pkg/BUILD")
            .expect("pkg/BUILD")
            .loads
            .insert("//lib/other.star".to_string(), edge);
        let err =
            lint(&graph, &mut read_under(tmp.path()), &[], &none(), false).expect_err("fails");
        let text = chain(&err);
        assert!(
            text.contains("load(\"//lib/defs.star\") is not in the package's load graph"),
            "{text}"
        );
        assert!(text.starts_with("pkg/BUILD:1:"), "{text}");
    }

    #[test]
    fn per_file_load_map() {
        // `./defs.star` written in two packages names two files with two
        // different signatures: each call is checked against its own.
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"./defs.star\", \"f\")\nload(\"//lib/lib.star\", \"g\")\n\n\
                 def use() -> None:\n    f(1)\n    g(\"s\")\n",
            ),
            ("pkg/defs.star", "def f(x: int) -> None:\n    pass\n"),
            (
                "lib/lib.star",
                "load(\"./defs.star\", \"f\")\n\ndef g(x: str) -> None:\n    f(x)\n",
            ),
            ("lib/defs.star", "def f(x: str) -> None:\n    pass\n"),
        ]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        assert_eq!(
            graph.files["pkg/BUILD"].loads["./defs.star"]
                .files()
                .collect::<Vec<_>>(),
            ["pkg/defs.star"]
        );
        assert_eq!(
            graph.files["lib/lib.star"].loads["./defs.star"]
                .files()
                .collect::<Vec<_>>(),
            ["lib/defs.star"]
        );
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);

        // Swap the signatures: both calls are now wrong, each reported where
        // its own map sends it (the lib's is not owned, so only pkg's shows).
        std::fs::write(
            tmp.path().join("pkg/defs.star"),
            "def f(x: str) -> None:\n    pass\n",
        )
        .expect("write");
        let found = lint_pkg(tmp.path(), "pkg", true);
        assert_eq!(
            found
                .iter()
                .map(|f| (f.file.as_str(), f.line))
                .collect::<Vec<_>>(),
            [("pkg/BUILD", 5)],
            "{found:#?}"
        );
    }

    #[test]
    fn dir_load_merges_like_eval() {
        let lib = "A = 1\n_P = 1\n\ndef B(x: int) -> None:\n    inner = x\n";
        let more = "def B(x: str) -> None:\n    pass\n\nC = [1]\n_Q = \"q\"\n";
        let pkg = |arg: &str| {
            format!(
                "load(\"//lib\", \"A\", \"B\", \"C\")\n\nB({arg})\n\n\
                 def use() -> None:\n    B({arg})\n"
            )
        };
        let tmp = ws(&[
            ("pkg/BUILD", &pkg("\"s\"")),
            ("lib/BUILD", lib),
            ("lib/more.BUILD", more),
        ]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        let edge = &graph.files["pkg/BUILD"].loads["//lib"];
        let LoadEdge::Dir { files, .. } = edge else {
            panic!("a directory load");
        };
        assert_eq!(files, &["lib/BUILD", "lib/more.BUILD"]);

        // The interface the checker builds for `load("//lib")`...
        let mut read = read_under(tmp.path());
        let globals = check_globals(&[], &none());
        let mut checked = HashMap::new();
        let mut universe: BTreeSet<String> = BTreeSet::new();
        for path in files {
            let ast = parse_build_source(path, read(path).expect("read")).expect("parse");
            universe.extend(bound_names(&ast).into_iter().map(str::to_owned));
            let exported = bound_names(&ast)
                .into_iter()
                .filter(|n| is_exported(n))
                .map(str::to_owned)
                .collect();
            let (_, _, iface, _) = ast.typecheck(&globals, &HashMap::new());
            checked.insert(path.as_str(), Checked { iface, exported });
        }
        let merged = interface_of(edge, &checked).expect("merge");

        // ...exports exactly the names evaluation's merged module does, over
        // every name either file binds (private and def-local ones included).
        let evaluated = crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "lib")
            .expect("evaluates");
        let eval_names: BTreeSet<String> = evaluated
            .module
            .names()
            .map(|n| n.as_str().to_owned())
            .collect();
        assert_eq!(
            eval_names,
            BTreeSet::from(["A", "B", "C"].map(str::to_owned))
        );
        for name in &universe {
            assert_eq!(
                merged.get(name).is_some(),
                eval_names.contains(name),
                "{name}: the checker's merge and evaluation's disagree"
            );
        }

        // The later file's `B` wins in both: a `str` call passes evaluation
        // (top level, runtime-checked) and the checker (in the def)...
        crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "pkg")
            .expect("evaluation took more.BUILD's B");
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
        // ...and an `int` call fails both.
        std::fs::write(tmp.path().join("pkg/BUILD"), pkg("1")).expect("write");
        assert!(crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "pkg").is_err());
        let found = lint_pkg(tmp.path(), "pkg", true);
        assert_eq!(
            found.iter().map(|f| (f.line, f.kind)).collect::<Vec<_>>(),
            [(6, Kind::TypeError)],
            "{found:#?}"
        );
    }

    #[test]
    fn top_level_not_statically_checked() {
        // A top-level type error is left to evaluation's runtime checks: the
        // static checker only checks def bodies. This pins that limit.
        let tmp = ws(&[(
            "pkg/BUILD",
            "x: int = \"s\"\n\ndef f(n: int) -> None:\n    pass\n\nf(\"s\")\n",
        )]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    #[test]
    fn inferred_return_used_at_call_site() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"name_of\")\n\ndef use() -> None:\n    n: int = name_of(1)\n",
            ),
            ("lib/defs.star", "def name_of(n: int):\n    return str(n)\n"),
        ]);
        assert_eq!(
            lines(&lint_pkg(tmp.path(), "pkg", true)),
            ["pkg/BUILD:4:8: type-error: Expected type `int` but got `str`"]
        );
    }

    #[test]
    fn plugin_fns_hashed_by_name_only() {
        // The verdict half (the def-hash half is the driver's): a plugin
        // function is an opaque callable known by name, so no call to it, with
        // any arguments, used as any type, can be judged against a signature.
        let src = "\
def f() -> None:
    a: int = heph.p.make(\"s\")
    b: str = heph.p.make(1, 2, named = [True])
    c: list[str] = heph.p.make()
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        let fns = [("p", "make")];
        let found = lint(&graph, &mut read_under(tmp.path()), &fns, &none(), true).expect("checks");
        assert_eq!(found, []);
        // The name is what the checker knows: an unknown plugin is reported.
        let found = lint(&graph, &mut read_under(tmp.path()), &[], &none(), true).expect("checks");
        assert!(!found.is_empty(), "heph.p is unknown without its name");

        // The hash half: only the names reach the def hash, sorted and
        // deduplicated, so nothing about a function but its name can move it.
        let hash = |fns: &[(&str, &str)]| lint_hash(&graph, false, "v1", fns, &patterns());
        let base = hash(&[("p", "make"), ("q", "other")]);
        assert_eq!(base, hash(&[("q", "other"), ("p", "make"), ("p", "make")]));
        assert_ne!(base, hash(&[("p", "make")]));
        assert_ne!(base, hash(&[("p", "make2"), ("q", "other")]));

        // The spec carries one digest, not the names: its size does not grow
        // with the number of plugin functions.
        use crate::pluginbuildfile::checks::targets::{ChecksOptions, lint_keys, lint_spec};
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            "build-lint-check".to_string(),
            Default::default(),
        );
        let many: Vec<(String, String)> = (0..500)
            .map(|i| (format!("p{i}"), format!("f{i}")))
            .collect();
        let spec = lint_spec(
            addr,
            &graph,
            ChecksOptions::default(),
            &patterns(),
            many.iter().map(|(p, f)| (p.as_str(), f.as_str())),
            none().digest(),
        )
        .expect("spec");
        let Some(hcore::htvalue::Value::String(digest)) =
            spec.config.get(lint_keys::PLUGIN_FNS_DIGEST)
        else {
            panic!("a digest string: {:?}", spec.config);
        };
        assert_eq!(digest.len(), 32, "{digest}");
        assert!(
            !spec
                .config
                .values()
                .any(|v| format!("{v:?}").contains("p499"))
        );
    }

    fn patterns() -> Vec<glob::Pattern> {
        crate::pluginbuildfile::default_build_file_patterns()
    }

    /// No driver `Kwargs`.
    fn none() -> std::sync::Arc<DriverKwargs> {
        DriverKwargs::empty()
    }

    /// The def hash of `walk`'s `build-lint-check`, as the provider's spec and
    /// the driver's parse give it.
    fn lint_hash(
        walk: &LoadWalk,
        require: bool,
        version: &str,
        fns: &[(&str, &str)],
        patterns: &[glob::Pattern],
    ) -> Vec<u8> {
        lint_hash_with(walk, require, version, fns, patterns, none().digest())
    }

    /// [`lint_hash`], with the driver kwargs' digest.
    fn lint_hash_with(
        walk: &LoadWalk,
        require: bool,
        version: &str,
        fns: &[(&str, &str)],
        patterns: &[glob::Pattern],
        kwargs_digest: &str,
    ) -> Vec<u8> {
        use crate::pluginbuildfile::checks::targets::{ChecksOptions, lint_spec};
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from(walk.pkg.as_str()),
            "build-lint-check".to_string(),
            Default::default(),
        );
        let opts = ChecksOptions {
            require_type_annotations: require,
            ..ChecksOptions::default()
        };
        let spec = lint_spec(
            addr,
            walk,
            opts,
            patterns,
            fns.iter().copied(),
            kwargs_digest,
        )
        .expect("spec");
        crate::pluginbuildfile::checks::drivers::parse_spec(spec, version).hash
    }

    #[test]
    fn def_hash_inputs() {
        use crate::pluginbuildfile::checks::targets::{ChecksOptions, lint_spec};
        let tmp = ws(&[
            ("pkg/BUILD", "load(\"//lib/defs.star\", \"X\")\n"),
            ("lib/defs.star", "X = 1\n"),
        ]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        let fns = [("p", "f")];
        let base = lint_hash(&graph, false, "v1", &fns, &patterns());
        assert_eq!(base, lint_hash(&graph, false, "v1", &fns, &patterns()));
        assert_ne!(
            base,
            lint_hash(&graph, true, "v1", &fns, &patterns()),
            "option"
        );
        assert_ne!(
            base,
            lint_hash(&graph, false, "v2", &fns, &patterns()),
            "version"
        );
        assert_ne!(
            base,
            lint_hash(&graph, false, "v1", &[("p", "g")], &patterns()),
            "fns"
        );
        let build_only = [glob::Pattern::new("BUILD").expect("pattern")];
        assert_ne!(
            base,
            lint_hash(&graph, false, "v1", &fns, &build_only),
            "patterns"
        );

        // The edges: the same files, a load resolved elsewhere.
        let mut moved = graph.clone();
        let edge = moved
            .files
            .get_mut("pkg/BUILD")
            .expect("pkg/BUILD")
            .loads
            .get_mut("//lib/defs.star")
            .expect("edge");
        *edge = LoadEdge::File {
            path: "lib/defs.star".to_string(),
            pkg: "elsewhere".to_string(),
        };
        assert_ne!(
            base,
            lint_hash(&moved, false, "v1", &fns, &patterns()),
            "edges"
        );

        // Not with `indent`, which only the formatter reads.
        let addr = hmodel::htaddr::Addr::new(
            hmodel::htpkg::PkgBuf::from("pkg"),
            "build-lint-check".to_string(),
            Default::default(),
        );
        let wide = ChecksOptions {
            indent: 8,
            ..ChecksOptions::default()
        };
        let spec = lint_spec(
            addr,
            &graph,
            wide,
            &patterns(),
            fns.iter().copied(),
            none().digest(),
        )
        .expect("spec");
        assert_eq!(
            base,
            crate::pluginbuildfile::checks::drivers::parse_spec(spec, "v1").hash,
            "indent"
        );
    }

    /// The spec's Goal example: a known key passes, a wrong value and an
    /// unknown key are each a finding.
    #[test]
    fn driver_kwargs_typed() {
        let src = "\
def lib(name: str, **kwargs: heph.types.bash.Kwargs):
    target(name = name, driver = \"bash\", **kwargs)

def app():
    lib(name = \"a\", run = \"echo\")
    lib(name = \"a\", run = 1)
    lib(name = \"a\", rnu = \"echo\")
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        assert_eq!(
            lines(&lint_pkg(tmp.path(), "pkg", false)),
            [
                "pkg/BUILD:6:21: type-error: Expected type `None | str | list[str]` but got `int`",
                "pkg/BUILD:7:21: type-error: Unexpected parameter named `rnu`",
            ]
        );
    }

    /// Forwarding `**kwargs` to `target()` typechecks, annotations required.
    #[test]
    fn driver_kwargs_forwarding_clean() {
        let src = "\
def lib(name: str, **kwargs: heph.types.oci.image.Kwargs) -> str:
    return target(name = name, driver = \"oci.image\", **kwargs)

def app() -> None:
    lib(name = \"a\", base = \"b\", labels = [\"l\"], scale = 1.5)
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    /// A wrapper in a loaded `.star` file of another package: its `Kwargs`
    /// cross the load, so both findings land at the caller's lines.
    #[test]
    fn driver_kwargs_through_load() {
        let lib = "\
def lib(name: str, **kwargs: heph.types.bash.Kwargs) -> None:
    target(name = name, driver = \"bash\", **kwargs)
";
        let caller = "\
load(\"//lib/defs.star\", \"lib\")

def app() -> None:
    lib(name = \"a\", run = 1)
    lib(name = \"a\", rnu = \"echo\")
";
        let tmp = ws(&[
            ("pkg/BUILD", caller),
            ("lib/defs.star", lib),
            // Makes `lib` a package owning `defs.star`.
            ("lib/BUILD", "load(\"//lib/defs.star\", \"lib\")\n"),
        ]);
        assert_eq!(
            lines(&lint_pkg(tmp.path(), "pkg", true)),
            [
                "pkg/BUILD:4:21: type-error: Expected type `None | str | list[str]` but got `int`",
                "pkg/BUILD:5:21: type-error: Unexpected parameter named `rnu`",
            ]
        );
        assert_eq!(lint_pkg(tmp.path(), "lib", true), []);
    }

    /// Evaluation through the same loaded wrapper: a known key builds the
    /// target, an unknown one fails the call naming the key.
    #[test]
    fn driver_kwargs_eval_through_load() {
        use crate::pluginbuildfile::checks::test_ws::eval_pkg;
        let lib = "\
def lib(name: str, **kwargs: heph.types.bash.Kwargs) -> None:
    target(name = name, driver = \"bash\", **kwargs)
";
        let call = |args: &str| {
            ws(&[
                (
                    "pkg/BUILD",
                    &format!("load(\"//lib/defs.star\", \"lib\")\n\nlib(name = \"t\", {args})\n"),
                ),
                ("lib/defs.star", lib),
            ])
        };
        let ok = call("run = \"echo\"");
        let result = eval_pkg(ok.path(), "pkg").unwrap_or_else(|e| panic!("{}", chain(&e)));
        assert_eq!(result.targets().len(), 1);
        let bad = call("rnu = \"echo\"");
        let err = chain(&eval_pkg(bad.path(), "pkg").expect_err("an unknown key"));
        assert!(err.contains("`rnu`"), "names the key: {err}");
    }

    /// An explicit parameter beside `**kwargs` wins its key: forwarding it is
    /// clean, and a call is judged by the parameter's own type.
    #[test]
    fn driver_kwargs_explicit_param() {
        let src = "\
def lib(name: str, run: str, **kwargs: heph.types.bash.Kwargs) -> None:
    target(name = name, driver = \"bash\", run = run, **kwargs)

def app() -> None:
    lib(name = \"a\", run = \"echo\", labels = [\"l\"])
    lib(name = \"a\", run = 1)
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        assert_eq!(
            lines(&lint_pkg(tmp.path(), "pkg", true)),
            ["pkg/BUILD:6:21: type-error: Expected type `str` but got `int`"]
        );
    }

    /// Outside `**kwargs`, a `Kwargs` value types as a dict: membership,
    /// union and `.get` are clean.
    #[test]
    fn driver_kwargs_as_a_dict() {
        let src = "\
def f(k: heph.types.bash.Kwargs):
    if \"run\" in k:
        pass
    m = k | {}
    r = k.get(\"run\")
    return [m, r]
";
        let tmp = ws(&[("pkg/BUILD", src)]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    /// The lint key covers the derived kwargs: a key's type change moves it,
    /// a doc or `required` change does not, and the run refuses kwargs whose
    /// digest is not the def's. An ordinary target's spec, and so its def
    /// hash, is the same whatever the drivers' kwargs (I2).
    #[test]
    fn driver_kwargs_digest_in_lint_key() {
        use crate::pluginbuildfile::types::tests::test_schemas;
        use hcore::htvalue::signature::ParamType;
        let base = test_schemas();
        let digest =
            |s: &hplugin::driver::DriverSchemas| DriverKwargs::from_schemas(s).digest().to_string();
        let edit = |f: &dyn Fn(&mut hplugin::driver::DriverField)| {
            let mut s = base.clone();
            let run = s
                .get_mut("bash")
                .and_then(|d| d.fields.iter_mut().find(|f| f.name == "run"))
                .expect("bash.run");
            f(run);
            s
        };
        let d0 = digest(&base);
        assert_eq!(d0, digest(&base), "stable");
        let retyped = digest(&edit(&|f| f.ty = ParamType::Int));
        assert_ne!(d0, retyped, "a key's type");
        assert_eq!(d0, digest(&edit(&|f| f.doc = "other".into())), "doc");
        assert_eq!(d0, digest(&edit(&|f| f.required = true)), "required");

        let tmp = ws(&[("pkg/BUILD", "X = 1\n")]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        let hash = |d: &str| lint_hash_with(&graph, false, "v1", &[], &patterns(), d);
        assert_ne!(hash(&d0), hash(&retyped), "the digest is in the lint key");

        // An ordinary target: the same spec with or without the kwargs.
        let src = "target(name = \"a\", driver = \"bash\", run = \"echo\")\n";
        let eval = |kwargs: std::sync::Arc<DriverKwargs>| {
            let dir = tempfile::tempdir().expect("tempdir");
            let loader = crate::pluginbuildfile::run_file::tests::source_loader_with(
                dir.path().to_path_buf(),
                kwargs,
            );
            let r = crate::pluginbuildfile::run_file::eval_source(
                "BUILD",
                src.to_string(),
                "pkg",
                &loader,
            )
            .expect("evaluates");
            format!("{:?}", r.targets())
        };
        assert_eq!(
            eval(none()),
            eval(crate::pluginbuildfile::types::tests::test_kwargs())
        );
    }

    #[test]
    fn patterns_order_irrelevant() {
        let tmp = ws(&[("pkg/BUILD", "X = 1\n")]);
        let graph = walk(tmp.path(), "pkg").expect("walk");
        let p = |s: &str| glob::Pattern::new(s).expect("pattern");
        let base = lint_hash(&graph, false, "v1", &[], &[p("BUILD"), p("*.BUILD")]);
        assert_eq!(
            base,
            lint_hash(
                &graph,
                false,
                "v1",
                &[],
                &[p("*.BUILD"), p("BUILD"), p("BUILD")]
            )
        );
    }
}
