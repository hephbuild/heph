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
//!
//! A `type-error` finding about a record or enum a walked file declares ends
//! with where it was declared (`(Lib declared at lib/defs.star:3)`), matched
//! by the type's identity in the error, never by its text. Only the finding's
//! text changes; nothing about it reaches a hash.

use super::annotations;
use super::loads::{LoadEdge, LoadWalk, WalkedFile, owns};
use super::report::{DeclarationSite, Finding, Kind};
use crate::pluginbuildfile::ast::bound_names;
use crate::pluginbuildfile::run_file::{heph_globals, is_exported, parse_build_source};
use crate::pluginbuildfile::types::DriverKwargs;
use allocative::Allocative;
use anyhow::Context;
use starlark::any::ProvidesStaticType;
use starlark::environment::Globals;
use starlark::typing::{AstModuleTypecheck, Interface, Ty, TypeMap, error_types};
use starlark::values::typing::{
    StarlarkCallableParamAny, StarlarkCallableParamSpec, TypeInstanceId,
};
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
    // Every record and enum the walked files declare, by identity, and the
    // owned findings whose errors mention one (index into `findings`): a
    // finding names the declarations once every file is checked (D10).
    let mut declarations: HashMap<TypeInstanceId, DeclarationSite> = HashMap::new();
    let mut mentions: Vec<(usize, Vec<TypeInstanceId>)> = Vec::new();

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
        let (errors, types, iface, _approximations) = ast.typecheck(&globals, &loads);
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
        record_declarations(path, &types, &mut declarations);
        if owned {
            for e in &errors {
                let ids = mentioned_user_types(e);
                if !ids.is_empty() {
                    mentions.push((findings.len(), ids));
                }
                findings.push(type_error(path, e));
            }
        }
        checked.insert(path, Checked { iface, exported });
    }

    for (at, ids) in mentions {
        if let Some(finding) = findings.get_mut(at) {
            finding.name_declarations(ids.iter().filter_map(|id| declarations.get(id)));
        }
    }
    findings.sort();
    Ok(findings)
}

/// Record where each record and enum type `path` declares was declared, by
/// the type's identity: the path is the walk's key, never the name the
/// codemap carries, and the line is the declaring binding's. An alias in the
/// same file (`Lib2 = Lib`) reports the same declaration, kept once.
fn record_declarations(
    path: &str,
    types: &TypeMap,
    declarations: &mut HashMap<TypeInstanceId, DeclarationSite>,
) {
    for binding in types.module_bindings() {
        let Some((id, span)) = binding.declaration else {
            continue;
        };
        declarations.entry(id).or_insert_with(|| DeclarationSite {
            name: binding
                .as_type
                .map_or_else(|| binding.name.to_owned(), ToString::to_string),
            file: path.to_owned(),
            line: span.resolve_span().begin.line + 1,
        });
    }
}

/// The user types (records, enums) a typechecker error mentions, by
/// identity, in the order its message mentions them. Read from the error's
/// typed payload, never its text: an error built from a plain message
/// mentions none.
fn mentioned_user_types(e: &starlark::Error) -> Vec<TypeInstanceId> {
    error_types(e).iter().flat_map(Ty::user_type_ids).collect()
}

/// Read workspace-relative paths under `root` (the driver's sandbox). An
/// error names the workspace-relative path, never the sandbox's.
pub(crate) fn read_under(root: &Path) -> impl FnMut(&str) -> anyhow::Result<String> + '_ {
    move |rel| std::fs::read_to_string(root.join(rel)).with_context(|| format!("reading {rel}"))
}

/// A typechecked file: its interface, and the exported names it binds at the
/// top level.
pub(crate) struct Checked {
    pub(crate) iface: Interface,
    pub(crate) exported: BTreeSet<String>,
}

/// One walked file (`path`, its source `src`, its walk entry `file`)
/// typechecked against `globals` as [`lint`] checks it: each of its loads sees
/// [`interface_with`] over what `checked` holds of the files it loads, which
/// were checked before it. Best effort, for the LSP (I9): `None` when the file
/// does not parse, and a load whose files are not in `checked` is left out, so
/// a loader sees its names as `Any`. Errors and approximations are not kept.
pub(crate) fn check_loaded<'c>(
    path: &str,
    src: String,
    file: &WalkedFile,
    checked: impl Fn(&str) -> Option<&'c Checked>,
    globals: &Globals,
) -> Option<Checked> {
    let ast = parse_build_source(path, src).ok()?;
    let loads = ast
        .loads()
        .into_iter()
        .filter_map(|load| {
            let edge = file.loads.get(load.module_id)?;
            let iface = interface_with(edge, &checked).ok()?;
            Some((load.module_id.to_owned(), iface))
        })
        .collect();
    let exported = bound_names(&ast)
        .into_iter()
        .filter(|n| is_exported(n))
        .map(str::to_owned)
        .collect();
    let (_errors, _types, iface, _approximations) = ast.typecheck(globals, &loads);
    Some(Checked { iface, exported })
}

/// The interface one `load()` sees. A file load is the file's own; a
/// directory load merges its files' interfaces as evaluation merges their
/// modules (`merge_pkg_results`): in order, a later file's name wins, and only
/// exported names ([`is_exported`]) are re-exported.
///
/// A name carries two things, its value's type (`get`) and, when it is a type,
/// the type it denotes in an annotation (`get_type`). The later file wins
/// both at once, per name: a later `X = 1` drops an earlier file's record `X`
/// as a type too, as evaluation's later value does.
pub(crate) fn interface_of(
    edge: &LoadEdge,
    checked: &HashMap<&str, Checked>,
) -> anyhow::Result<Interface> {
    interface_with(edge, |path| checked.get(path))
}

/// [`interface_of`], each loaded file's [`Checked`] looked up by `checked`.
pub(crate) fn interface_with<'c>(
    edge: &LoadEdge,
    checked: impl Fn(&str) -> Option<&'c Checked>,
) -> anyhow::Result<Interface> {
    let get = |path: &str| {
        checked(path).with_context(|| format!("{path} is loaded before it was checked"))
    };
    match edge {
        LoadEdge::File { path, .. } => Ok(get(path)?.iface.clone()),
        LoadEdge::Dir { files, .. } => {
            let mut values = HashMap::new();
            let mut types = HashMap::new();
            for path in files {
                let file = get(path)?;
                for name in &file.exported {
                    let Some(ty) = file.iface.get(name) else {
                        continue;
                    };
                    values.insert(name.clone(), ty.clone());
                    match file.iface.get_type(name) {
                        Some(as_type) => types.insert(name.clone(), as_type.clone()),
                        None => types.remove(name),
                    };
                }
            }
            Ok(Interface::new_with_types(values, types))
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
pub(crate) fn check_globals(plugin_fns: &[(&str, &str)], kwargs: &DriverKwargs) -> Globals {
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

    /// Types a BUILD file declares (`record`, `field`, `enum`, an alias,
    /// `typing`), here and across a load, check clean with annotations
    /// required: the checker's globals are evaluation's.
    #[test]
    fn user_declared_types_check_clean() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"Lib\", \"Mode\")\n\n\
                 Names = list[str]\n\n\
                 def names(l: Lib, extra: typing.Any = None) -> Names:\n    \
                 return [l.name, l.mode.value]\n\n\
                 def use(f: typing.Callable) -> Names:\n    \
                 return names(Lib(name = \"a\", mode = Mode(\"debug\")))\n",
            ),
            (
                "lib/defs.star",
                "Mode = enum(\"debug\", \"release\")\n\
                 Lib = record(name = str, mode = Mode, srcs = field(list[str], []))\n",
            ),
        ]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    /// A type a file declares (an alias, a record, an enum) is checked where
    /// it is declared and in every file that loads it (I5). This inverts
    /// #653's `user_declared_record_and_enum_checked_at_evaluation_only`,
    /// where a record and an enum were `Any`, and so was a loaded alias.
    #[test]
    fn loaded_alias_checked() {
        let decls = [
            "Names = list[str]",
            "R = record(a = str)",
            "E = enum(\"a\", \"b\")",
        ];
        // The checker reports one error per call, so one wrong argument each.
        let uses = [
            "def f(n: Names, r: R, e: E) -> None:",
            "    pass",
            "",
            "def g() -> None:",
            "    f(1, R(a = \"x\"), E(\"a\"))",
            "    f([], \"r\", E(\"a\"))",
            "    f([], R(a = \"x\"), 1)",
        ];
        let here: Vec<&str> = decls.iter().chain([&""]).chain(&uses).copied().collect();
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &src(&here))]),
            [
                "pkg/BUILD:9:7: type-error: Expected type `list[str]` but got `int`",
                "pkg/BUILD:10:11: type-error: Expected type `R` but got `str` \
                 (R declared at pkg/BUILD:2)",
                "pkg/BUILD:11:23: type-error: Expected type `E` but got `int` \
                 (E declared at pkg/BUILD:3)",
            ]
        );

        let loader: Vec<&str> = ["load(\"//lib/defs.star\", \"Names\", \"R\", \"E\")", ""]
            .iter()
            .chain(&uses)
            .copied()
            .collect();
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &src(&loader)),
                ("lib/defs.star", &src(&decls))
            ]),
            [
                "pkg/BUILD:7:7: type-error: Expected type `list[str]` but got `int`",
                "pkg/BUILD:8:11: type-error: Expected type `R` but got `str` \
                 (R declared at lib/defs.star:2)",
                "pkg/BUILD:9:23: type-error: Expected type `E` but got `int` \
                 (E declared at lib/defs.star:3)",
            ]
        );
    }

    /// A file that binds a name the type extensions add keeps its own binding
    /// in the checker, as in evaluation (`run_file`'s
    /// `user_bindings_shadow_type_globals`).
    #[test]
    fn user_bindings_shadow_type_globals() {
        let tmp = ws(&[
            (
                "pkg/BUILD",
                "load(\"//lib/defs.star\", \"field\")\n\
                 enum = \"e\"\n\
                 typing = {\"a\": \"t\"}\n\n\
                 def eval_type(x: str) -> str:\n    return x\n\n\
                 def name(record: str = \"r\") -> str:\n    \
                 n: str = typing[\"a\"]\n    \
                 return \"-\".join([record, enum, n, eval_type(field)])\n",
            ),
            ("lib/defs.star", "field = \"f\"\n"),
        ]);
        assert_eq!(lint_pkg(tmp.path(), "pkg", true), []);
    }

    /// A file of `lines`, each ended by a newline: line `n` is `lines[n - 1]`.
    fn src(lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    /// `//pkg`'s findings in a workspace of `files`, as report lines.
    fn lint_ws(files: &[(&str, &str)]) -> Vec<String> {
        let tmp = ws(files);
        lines(&lint_pkg(tmp.path(), "pkg", false))
    }

    /// Why evaluating `//pkg` in a workspace of `files` fails, if it does.
    fn eval_err(files: &[(&str, &str)]) -> Option<String> {
        let tmp = ws(files);
        crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "pkg")
            .err()
            .map(|e| chain(&e))
    }

    /// The checker's view of one file's module-level names, with a plugin
    /// function `heph.p.fn`: each name's value type and, when it is a type,
    /// the type it denotes, as the checker prints them.
    fn module_types(lines: &[&str]) -> BTreeMap<String, (String, Option<String>)> {
        let ast = parse_build_source("pkg/BUILD", src(lines)).expect("parses");
        let (_, types, _, _) =
            ast.typecheck(&check_globals(&[("p", "fn")], &none()), &HashMap::new());
        types
            .module_bindings()
            .map(|b| {
                (
                    b.name.to_owned(),
                    (b.ty.to_string(), b.as_type.map(ToString::to_string)),
                )
            })
            .collect()
    }

    /// The value type the checker gives module-level `name` in `lines`.
    fn type_of(lines: &[&str], name: &str) -> String {
        let types = module_types(lines);
        types
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not bound: {types:#?}"))
            .0
            .clone()
    }

    /// The approximations the checker reports for one file, as
    /// `category: message`.
    fn approximations(lines: &[&str]) -> Vec<String> {
        let ast = parse_build_source("pkg/BUILD", src(lines)).expect("parses");
        let (_, _, _, approximations) =
            ast.typecheck(&check_globals(&[], &none()), &HashMap::new());
        approximations
            .iter()
            .map(|a| format!("{}: {}", a.category, a.message))
            .collect()
    }

    #[test]
    fn record_field_types_checked() {
        let pkg = src(&[
            "Lib = record(name = str, srcs = field(list[str], []))",
            "Count = field(int)",
            "Pt = record(n = Count)",
            "",
            "def wrong_type() -> None:",
            "    Lib(name = 1)",
            "",
            "def missing() -> None:",
            "    Lib()",
            "",
            "def optional_omitted() -> None:",
            "    Lib(name = \"a\")",
            "",
            "def bound_field() -> None:",
            "    Pt(n = \"s\")",
            "    Pt(n = 1)",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            [
                "pkg/BUILD:6:9: type-error: Expected type `str` but got `int`",
                "pkg/BUILD:9:5: type-error: Missing required parameter `name`",
                "pkg/BUILD:15:8: type-error: Expected type `int` but got `str`",
            ]
        );
    }

    #[test]
    fn record_attrs_checked() {
        let pkg = src(&[
            "Lib = record(name = str, srcs = field(list[str], []))",
            "",
            "def typo(l: Lib) -> None:",
            "    l.nme",
            "",
            "def op(l: Lib) -> None:",
            "    l.name - 1",
            "",
            "def ok(l: Lib) -> None:",
            "    l.srcs.append(\"x\")",
            // The checker accepts `str + int` (starlark types `str`'s `+`
            // loosely), so this is not a finding with or without records.
            "    l.name + 1",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            [
                "pkg/BUILD:4:7: type-error: The attribute `nme` is not available on the type \
                 `Lib` (Lib declared at pkg/BUILD:1)",
                "pkg/BUILD:7:5: type-error: Binary operator `-` is not available on the types \
                 `str` and `int`",
            ]
        );
    }

    #[test]
    fn enum_checked() {
        let pkg = src(&[
            "Mode = enum(\"debug\", \"release\")",
            "",
            "def f(m: Mode) -> None:",
            "    pass",
            "",
            "def bad() -> None:",
            "    f(\"debug\")",
            "",
            "def ok(m: Mode) -> str:",
            "    f(Mode(\"debug\"))",
            "    f(Mode.debug)",
            "    return m.value",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            [
                "pkg/BUILD:7:7: type-error: Expected type `Mode` but got `str` \
              (Mode declared at pkg/BUILD:1)"
            ]
        );
    }

    #[test]
    fn compound_types_of_declared() {
        let pkg = src(&[
            "load(\"//lib/defs.star\", \"Lib\")",
            "",
            "Mode = enum(\"a\", \"b\")",
            "Libs = list[Lib]",
            "Opt = Lib | None",
            "ByMode = dict[str, Mode]",
            "Wrap = record(lib = Lib, later = Later)",
            "Later = record(x = str)",
            "",
            "def f(xs: Libs, o: Opt, d: ByMode, w: Wrap) -> None:",
            "    pass",
            "",
            "def bad(w: Wrap) -> None:",
            "    f([1], None, {}, w)",
            // `later` is a forward reference: `Any`, as evaluation cannot
            // know it either when it builds `Wrap`.
            "    Wrap(lib = 1, later = 1)",
            "",
            "def ok(l: Lib, m: Mode) -> None:",
            "    f([l], None, {\"a\": m}, Wrap(lib = l, later = 1))",
            "    f([], l, {}, Wrap(lib = l, later = \"x\"))",
            "",
            "def bad2(l: Lib, w: Wrap) -> None:",
            "    f([], 1, {}, w)",
            "    f([], l, {\"a\": \"x\"}, w)",
            "    f([], l, {}, 1)",
        ]);
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &pkg),
                ("lib/defs.star", "Lib = record(name = str)\n")
            ]),
            [
                "pkg/BUILD:14:7: type-error: Expected type `list[Lib]` but got `list[int]` \
                 (Lib declared at lib/defs.star:1)",
                "pkg/BUILD:15:10: type-error: Expected type `Lib` but got `int` \
                 (Lib declared at lib/defs.star:1)",
                "pkg/BUILD:22:11: type-error: Expected type `None | Lib` but got `int` \
                 (Lib declared at lib/defs.star:1)",
                "pkg/BUILD:23:14: type-error: Expected type `dict[str, Mode]` but got \
                 `dict[str, str]` (Mode declared at pkg/BUILD:3)",
                "pkg/BUILD:24:18: type-error: Expected type `Wrap` but got `int` \
                 (Wrap declared at pkg/BUILD:7)",
            ]
        );
    }

    #[test]
    fn dir_load_carries_types() {
        let pkg = src(&[
            "load(\"//lib\", \"Lib\", \"Names\")",
            "",
            "def f(l: Lib, n: Names) -> None:",
            "    pass",
            "",
            "def g(l: Lib) -> None:",
            "    f(1, [])",
            "    f(l, [1])",
        ]);
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &pkg),
                ("lib/BUILD", "Lib = record(name = str)\n"),
                ("lib/more.BUILD", "Names = list[str]\n"),
            ]),
            [
                "pkg/BUILD:7:7: type-error: Expected type `Lib` but got `int` \
                 (Lib declared at lib/BUILD:1)",
                "pkg/BUILD:8:10: type-error: Expected type `list[str]` but got `list[int]`",
            ]
        );
    }

    /// The later file's `X = "x"` wins the name in both of the interface's
    /// maps: `X` is a `str`, and no longer the earlier file's record type.
    #[test]
    fn dir_load_later_non_type_clears_type() {
        let pkg = src(&[
            "load(\"//lib\", \"X\")",
            "",
            "def f(x: X) -> None:",
            "    pass",
            "",
            "def g() -> int:",
            "    f(1)",
            "    return X",
        ]);
        let lib = ("lib/BUILD", "X = record(a = str)\n");
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg), lib, ("lib/more.BUILD", "X = \"x\"\n")]),
            ["pkg/BUILD:8:5: type-error: Expected type `int` but got `str`"]
        );
        // The control: with no later `X`, the record is the type.
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg), lib, ("lib/more.BUILD", "Y = \"y\"\n")]),
            [
                "pkg/BUILD:7:7: type-error: Expected type `X` but got `int` \
                 (X declared at lib/BUILD:1)",
                "pkg/BUILD:8:5: type-error: Expected type `int` but got `record[X]` \
                 (X declared at lib/BUILD:1)",
            ]
        );
    }

    /// One record reached by two loads, one directly and one through a file
    /// that loads it, is one type; `Lib2 = Lib` is that type too, and prints
    /// as `Lib`.
    #[test]
    fn same_record_through_two_loads_is_one_type() {
        let pkg = src(&[
            "load(\"//lib/defs.star\", \"Lib\", \"Lib2\")",
            "load(\"//lib/take.star\", \"take\")",
            "",
            "def ok(l: Lib, l2: Lib2) -> None:",
            "    take(l)",
            "    take(l2)",
            "    x: Lib2 = l",
            "",
            "def bad() -> None:",
            "    take(1)",
            "    y: Lib2 = 1",
        ]);
        let take = src(&[
            "load(\"//lib/defs.star\", \"Lib\")",
            "",
            "def take(l: Lib) -> None:",
            "    pass",
        ]);
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &pkg),
                ("lib/defs.star", "Lib = record(name = str)\nLib2 = Lib\n"),
                ("lib/take.star", &take),
            ]),
            [
                "pkg/BUILD:10:10: type-error: Expected type `Lib` but got `int` \
                 (Lib declared at lib/defs.star:1)",
                "pkg/BUILD:11:8: type-error: Expected type `Lib` but got `int` \
                 (Lib declared at lib/defs.star:1)",
            ]
        );
    }

    #[test]
    fn diamond_load_is_one_type() {
        let left = src(&[
            "load(\"//lib/base.star\", \"Lib\")",
            "",
            "def make() -> Lib:",
            "    return Lib(name = \"a\")",
        ]);
        let right = src(&[
            "load(\"//lib/base.star\", \"Lib\")",
            "",
            "def take(l: Lib) -> None:",
            "    pass",
        ]);
        let pkg = src(&[
            "load(\"//lib/left.star\", \"make\")",
            "load(\"//lib/right.star\", \"take\")",
            "",
            "def use() -> None:",
            "    take(make())",
            "    take(\"s\")",
        ]);
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &pkg),
                ("lib/base.star", "Lib = record(name = str)\n"),
                ("lib/left.star", &left),
                ("lib/right.star", &right),
            ]),
            [
                "pkg/BUILD:6:10: type-error: Expected type `Lib` but got `str` \
              (Lib declared at lib/base.star:1)"
            ]
        );
    }

    /// `use(x: X)` called with an `int`: a finding only when `X` is a type.
    const USE_X: [&str; 6] = [
        "",
        "def use(x: X) -> None:",
        "    pass",
        "",
        "def call() -> None:",
        "    use(1)",
    ];

    /// `decl` followed by [`USE_X`].
    fn using_x(decl: &[&str]) -> String {
        src(&decl.iter().chain(&USE_X).copied().collect::<Vec<_>>())
    }

    /// I3: a declaration the pre-pass cannot type exactly is `Any`, never a
    /// finding. It denotes no type, so a wrong argument passes, and the
    /// checker says why in an approximation when there is a typed reason.
    #[test]
    fn unresolvable_declarations_are_any() {
        let invalid = Some("X: invalid_declaration");
        let rows: &[(&str, &[&str], Option<&str>)] = &[
            (
                "under if",
                &["c = True", "if c:", "    X = record(a = str)"],
                Some("X: conditional_binding"),
            ),
            (
                "bound twice",
                &["X = record(a = str)", "X = record(a = int)"],
                Some("X: rebound"),
            ),
            (
                "a variant that is not a literal",
                &["V = \"a\"", "X = enum(V, \"b\")"],
                Some("X: non_literal_variant"),
            ),
            (
                "**kwargs",
                &["kw = {\"a\": str}", "X = record(**kw)"],
                Some("X: star_args"),
            ),
            (
                "*args",
                &["vs = [\"a\"]", "X = enum(*vs)"],
                Some("X: star_args"),
            ),
            ("a positional field", &["X = record(str)"], invalid),
            ("a duplicate variant", &["X = enum(\"a\", \"a\")"], invalid),
            ("no fields", &["X = record()"], invalid),
            ("no variants", &["X = enum()"], invalid),
            (
                "a tuple binding",
                &["X, Y = record(a = str), enum(\"a\")"],
                None,
            ),
        ];
        for &(row, decl, reason) in rows {
            let types = module_types(decl);
            // Not a type (a rebound name's value is the union of its values).
            assert_eq!(types["X"].1, None, "{row}");
            if let Some(reason) = reason {
                let approx = approximations(decl);
                let want = format!("Declaration typed as Any: {reason:?}");
                assert!(approx.contains(&want), "{row}: {approx:#?}");
            }
            assert_eq!(
                lint_ws(&[("pkg/BUILD", &using_x(decl))]),
                Vec::<String>::new(),
                "{row}"
            );
        }

        // Inside a def.
        assert!(
            approximations(&["def mk():", "    X = record(a = str)", "    return X"])
                .contains(&format!("Declaration typed as Any: {:?}", "X: in_def"))
        );

        // A default or a whole argument the pre-pass cannot evaluate (both
        // loaded) makes that field an optional `Any`; the record keeps its
        // other fields.
        let pkg = src(&[
            "load(\"//lib/defs.star\", \"D\", \"SRCS\")",
            "",
            "X = record(a = field(str, D), srcs = SRCS, b = str)",
            "",
            "def g() -> None:",
            "    X(b = \"\")",
            "    X(a = 1, srcs = 1, b = \"\")",
            "    X(a = 1)",
        ]);
        assert_eq!(
            lint_ws(&[
                ("pkg/BUILD", &pkg),
                ("lib/defs.star", "D = \"d\"\nSRCS = [\"a\"]\n"),
            ]),
            ["pkg/BUILD:8:5: type-error: Missing required parameter `b`"]
        );

        // A private name is typed like any other.
        let types = module_types(&["_Lib = record(a = str)"]);
        assert_eq!(types["_Lib"].1.as_deref(), Some("_Lib"));

        // A parameter named like a record is the parameter inside its def.
        let pkg = src(&[
            "Lib = record(a = str)",
            "",
            "def f(Lib: str) -> None:",
            "    x: int = Lib",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            ["pkg/BUILD:4:8: type-error: Expected type `int` but got `str`"]
        );
    }

    /// I1: only starlark's own `record`/`enum`/`field` natives build a type,
    /// recognized by identity. A user `def record`, a loaded one, and a
    /// loaded re-export of the native are not synthesized.
    #[test]
    fn shadowed_constructor_not_synthesized() {
        let decl = ["X = record(a = str)"];
        let local: Vec<&str> = ["def record(**kw):", "    return 1", ""]
            .iter()
            .chain(&decl)
            .copied()
            .collect();
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &using_x(&local))]),
            Vec::<String>::new()
        );

        let loaded: Vec<&str> = ["load(\"//lib/defs.star\", \"record\")", ""]
            .iter()
            .chain(&decl)
            .copied()
            .collect();
        for lib in ["def record(**kw):\n    return 1\n", "record = record\n"] {
            assert_eq!(
                lint_ws(&[("pkg/BUILD", &using_x(&loaded)), ("lib/defs.star", lib)]),
                Vec::<String>::new(),
                "{lib}"
            );
        }
    }

    /// I1: an alias of the native is the native.
    #[test]
    fn aliased_constructor_synthesized() {
        let decl = [
            "r = record",
            "e = enum",
            "f = field",
            "X = r(a = f(int))",
            "Mode = e(\"a\")",
            "",
            "def g(m: Mode) -> None:",
            "    X(a = \"s\")",
            "    g(\"a\")",
        ];
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &using_x(&decl))]),
            [
                "pkg/BUILD:8:7: type-error: Expected type `int` but got `str`",
                "pkg/BUILD:9:7: type-error: Expected type `Mode` but got `str` \
                 (Mode declared at pkg/BUILD:5)",
                "pkg/BUILD:15:9: type-error: Expected type `X` but got `int` \
                 (X declared at pkg/BUILD:4)",
            ]
        );
    }

    /// E1: what is wrong inside a declaration's own arguments is left to
    /// evaluation, which rejects it.
    #[test]
    fn declaration_errors_left_to_evaluation() {
        for decl in [
            "X = record(a = 1)",
            "X = enum(\"a\", \"a\")",
            "X = record(a = field(str, 1))",
        ] {
            let pkg = src(&[decl]);
            assert_eq!(
                lint_ws(&[("pkg/BUILD", &pkg)]),
                Vec::<String>::new(),
                "{decl}"
            );
            assert!(
                eval_err(&[("pkg/BUILD", &pkg)]).is_some(),
                "{decl} evaluates"
            );
        }
    }

    /// D4: a declared type is a real value at the top level, so an
    /// unconditional statement that fails on it is a finding, as it fails
    /// evaluation. Under `if`, and in a call's arguments (E2), it is not.
    #[test]
    fn top_level_value_errors_reported() {
        let pkg = src(&["Mode = enum(\"debug\")", "X = Mode.nope"]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            ["pkg/BUILD:2:5: type-error: Operation `.nope` not supported on type `function`"]
        );
        let err = eval_err(&[("pkg/BUILD", &pkg)]).expect("evaluation fails too");
        assert!(err.contains("no attribute `nope`"), "{err}");

        let pkg = src(&[
            "Mode = enum(\"debug\")",
            "if hasattr(Mode, \"nope\"):",
            "    X = Mode.nope",
        ]);
        assert_eq!(lint_ws(&[("pkg/BUILD", &pkg)]), Vec::<String>::new());
        assert_eq!(eval_err(&[("pkg/BUILD", &pkg)]), None);

        let pkg = src(&["Lib = record(name = str)", "x = Lib(name = 1)"]);
        assert_eq!(lint_ws(&[("pkg/BUILD", &pkg)]), Vec::<String>::new());
        assert!(eval_err(&[("pkg/BUILD", &pkg)]).is_some());
    }

    /// The library every gate row may load: an alias, a record and a def
    /// whose return type is inferred.
    const GATE_LIB: &str = "Names = list[str]\nDep = record(name = str)\n\n\
                            def mk():\n    return \"s\"\n";

    /// I2's gate, held row by row: `pkg` has a finding, evaluates, and fails
    /// to evaluate once it calls `bad()`, the def the finding is in.
    fn assert_lint_errors_are_evaluation_errors(rows: &[&[&str]]) {
        for &row in rows {
            let pkg = src(row);
            let files = [("pkg/BUILD", pkg.as_str()), ("lib/defs.star", GATE_LIB)];
            let found = lint_ws(&files);
            assert!(!found.is_empty(), "{row:#?}: no finding");
            assert_eq!(eval_err(&files), None, "{row:#?}: fails before bad() runs");
            let called = format!("{pkg}\nbad()\n");
            assert!(
                eval_err(&[("pkg/BUILD", &called), ("lib/defs.star", GATE_LIB)]).is_some(),
                "{row:#?}: lint found {found:#?}, evaluation accepts bad()"
            );
        }
    }

    #[test]
    fn lint_errors_are_evaluation_errors() {
        let lib = "Lib = record(name = str, srcs = field(list[str], []))";
        let mode = "Mode = enum(\"debug\", \"release\")";
        assert_lint_errors_are_evaluation_errors(&[
            &[lib, "def bad() -> None:", "    Lib(name = 1)"],
            &[lib, "def bad() -> None:", "    Lib()"],
            &[
                lib,
                "def bad() -> None:",
                "    Lib(name = \"a\", nme = \"b\")",
            ],
            &[
                lib,
                "def bad() -> None:",
                "    l = Lib(name = \"a\")",
                "    l.nme",
            ],
            &[lib, "def bad() -> None:", "    Lib(name = \"a\").name - 1"],
            &[
                lib,
                "def bad() -> None:",
                "    Lib(name = \"a\", srcs = [1])",
            ],
            &[
                lib,
                "def f(xs: list[Lib]) -> None:",
                "    pass",
                "def bad() -> None:",
                "    f([1])",
            ],
            &[
                "A = record(x = str)",
                "B = record(x = str)",
                "def f(a: A) -> None:",
                "    pass",
                "def bad() -> None:",
                "    f(B(x = \"s\"))",
            ],
            &[
                mode,
                "def f(m: Mode) -> None:",
                "    pass",
                "def bad() -> None:",
                "    f(\"debug\")",
            ],
            &[mode, "def bad() -> None:", "    Mode(\"debug\").nope"],
            &[mode, "def bad() -> None:", "    Mode.nope"],
            &[
                "load(\"//lib/defs.star\", \"Names\", \"Dep\")",
                "def f(n: Names, d: Dep) -> None:",
                "    pass",
                "def bad() -> None:",
                "    f(1, Dep(name = \"a\"))",
            ],
            &[
                "load(\"//lib/defs.star\", \"Dep\")",
                "def bad() -> None:",
                "    Dep(name = \"a\").nme",
            ],
        ]);

        // The right operand of a short-circuiting `and`/`or` the left decides
        // never runs at evaluation, so the pre-pass must not evaluate it
        // either: an attribute error there is no evaluation error.
        let short_circuited: &[&[&str]] = &[
            &["x = False and \"s\".nope()"],
            &["x = False and \"s\".nope"],
            &["x = True or \"s\".nope()"],
            &["Mode = enum(\"a\")", "x = False and Mode.nope()"],
            &["Mode = enum(\"a\")", "x = True or Mode.nope"],
        ];
        for &row in short_circuited {
            let pkg = src(row);
            let files = [("pkg/BUILD", pkg.as_str())];
            assert_eq!(lint_ws(&files), Vec::<String>::new(), "{row:#?}");
            assert_eq!(eval_err(&files), None, "{row:#?}: evaluation fails");
        }
        // The same operand on its own is a finding and an evaluation error,
        // so the rows above are not clean for want of a check.
        for row in [
            &["x = \"s\".nope()"][..],
            &["x = \"s\".nope"],
            &["Mode = enum(\"a\")", "x = Mode.nope()"],
            &["Mode = enum(\"a\")", "x = Mode.nope"],
        ] {
            let pkg = src(row);
            let files = [("pkg/BUILD", pkg.as_str())];
            assert_ne!(lint_ws(&files), Vec::<String>::new(), "{row:#?}");
            assert!(eval_err(&files).is_some(), "{row:#?}: evaluation succeeds");
        }
    }

    #[test]
    fn module_call_results_typed() {
        // A record's instance, an enum's value, a native's declared return
        // type, a def's annotated one.
        let decl = [
            "Lib = record(name = str)",
            "Mode = enum(\"debug\")",
            "",
            "def ann() -> str:",
            "    return \"\"",
            "",
            "def local():",
            "    return \"\"",
            "",
            "cfg = Lib(name = \"x\")",
            "m = Mode(\"debug\")",
            "srcs = glob(\"*.c\")",
            "a = ann()",
            "l = local()",
            "p = heph.p.fn()",
            "X = heph.core.os()",
            "s = \"a,b\".split(\",\")",
            "v = fail(\"no\")",
        ];
        let types = module_types(&decl);
        let ty = |name: &str| types[name].clone();
        let value = |t: &str| (t.to_owned(), None);
        assert_eq!(ty("cfg"), value("Lib"));
        assert_eq!(ty("m"), value("Mode"));
        assert_eq!(ty("srcs"), value("str"));
        assert_eq!(ty("a"), value("str"));
        // Inference solves after the pre-pass, against its output.
        assert_eq!(ty("l"), value("typing.Any"));
        // A plugin function is opaque.
        assert_eq!(ty("p"), value("typing.Any"));
        // A host-dependent value never becomes a type.
        assert_eq!(ty("X"), value("str"));
        assert_eq!(ty("s"), value("list[str]"));
        assert_eq!(ty("v"), value("typing.Never"));

        // Two unconditional bindings give the union; a conditional one gives
        // `Any`.
        let both = [
            "def a() -> str:",
            "    return \"\"",
            "",
            "def b() -> int:",
            "    return 1",
            "",
            "x = a()",
            "x = b()",
            "c = True",
            "if c:",
            "    y = a()",
            "else:",
            "    y = b()",
        ];
        assert_eq!(type_of(&both, "x"), "int | str");
        assert_eq!(type_of(&both, "y"), "typing.Any");

        // A call before its def, and a callee rebound after the call.
        let order = [
            "x = f()",
            "",
            "def f() -> str:",
            "    return \"\"",
            "",
            "def h() -> int:",
            "    return 1",
            "",
            "y = f()",
            "f = h",
        ];
        assert_eq!(type_of(&order, "x"), "typing.Any");
        assert_eq!(type_of(&order, "y"), "str");

        // What the pre-pass does not evaluate stays `Any`.
        let shapes = [
            "def g() -> str:",
            "    return \"\"",
            "",
            "y = g().upper",
            "z = [g()]",
            "w = g() or g()",
        ];
        for name in ["y", "z", "w"] {
            assert_eq!(type_of(&shapes, name), "typing.Any", "{name}");
        }

        // `*args` and `**kwargs` at the call.
        let star = [
            "def f(*a, **k) -> str:",
            "    return \"\"",
            "",
            "xs = [1]",
            "kw = {}",
            "x = f(*xs)",
            "y = f(**kw)",
        ];
        assert_eq!(type_of(&star, "x"), "str");
        assert_eq!(type_of(&star, "y"), "str");

        // In a def, a result's type is checked; `X` is no annotation.
        let pkg = src(&[
            "Lib = record(name = str)",
            "cfg = Lib(name = \"x\")",
            "srcs = glob(\"*.c\")",
            "X = heph.core.os()",
            "",
            "def g(x: X) -> str:",
            "    cfg.nme",
            "    srcs.foo",
            "    return cfg.name",
            "",
            "def h() -> None:",
            "    g(1)",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            [
                "pkg/BUILD:7:9: type-error: The attribute `nme` is not available on the type \
                 `Lib` (Lib declared at pkg/BUILD:1)",
                "pkg/BUILD:8:10: type-error: The attribute `foo` is not available on the type \
                 `str`",
            ]
        );

        // E2: a module-level call's arguments are not checked.
        let pkg = src(&[
            "Lib = record(name = str)",
            "x = Lib(name = 1)",
            "",
            "def f(s: str) -> str:",
            "    return s",
            "",
            "y = f(1)",
        ]);
        assert_eq!(lint_ws(&[("pkg/BUILD", &pkg)]), Vec::<String>::new());

        // A loaded unannotated def gives its inferred return type, and a
        // loader sees a module-level result through the interface.
        let pkg = src(&[
            "load(\"//lib/defs.star\", \"cfg\", \"local\")",
            "",
            "x = local()",
            "",
            "def g() -> None:",
            "    y: int = x",
            "    cfg.nme",
        ]);
        let lib = src(&[
            "Lib = record(name = str)",
            "cfg = Lib(name = \"x\")",
            "",
            "def local():",
            "    return \"\"",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg), ("lib/defs.star", &lib)]),
            [
                "pkg/BUILD:6:8: type-error: Expected type `int` but got `str`",
                "pkg/BUILD:7:9: type-error: The attribute `nme` is not available on the type \
                 `Lib` (Lib declared at lib/defs.star:1)",
            ]
        );
    }

    /// I11 held to I2's gate: each finding a module-level call result adds in
    /// a def is an error evaluation raises when that def runs.
    #[test]
    fn module_call_results_never_stricter() {
        assert_lint_errors_are_evaluation_errors(&[
            &[
                "Lib = record(name = str)",
                "cfg = Lib(name = \"x\")",
                "def bad() -> None:",
                "    cfg.nme",
            ],
            &[
                "Mode = enum(\"debug\")",
                "m = Mode(\"debug\")",
                "def bad() -> None:",
                "    m.nope",
            ],
            &["srcs = glob(\"*.c\")", "def bad() -> None:", "    srcs.foo"],
            &["x = heph.core.os()", "def bad() -> None:", "    x - 1"],
            &[
                "xs = \"a,b\".split(\",\")",
                "def bad() -> None:",
                "    xs.nope",
            ],
            &[
                "def f() -> str:",
                "    return \"\"",
                "x = f()",
                "def bad() -> None:",
                "    x - 1",
            ],
            &[
                "load(\"//lib/defs.star\", \"mk\")",
                "x = mk()",
                "def bad() -> None:",
                "    x - 1",
            ],
            &[
                "def a() -> str:",
                "    return \"\"",
                "def b() -> int:",
                "    return 1",
                "x = a()",
                "x = b()",
                "def bad() -> None:",
                "    x.nope",
            ],
        ]);
    }

    /// The messages of `//pkg`'s findings, in a workspace where `pkg/BUILD`
    /// is [`SITE_HEADER`] followed by `body` (its line `k` is the file's line
    /// `k + 5`), with the libraries it loads.
    fn site_messages(body: &[&str]) -> Vec<String> {
        let pkg: Vec<&str> = SITE_HEADER.iter().chain(body).copied().collect();
        let tmp = ws(&[
            ("pkg/BUILD", &src(&pkg)),
            (
                "lib/defs.star",
                &src(&[
                    "Lib = record(name = str)",
                    "Mode = enum(\"a\", \"b\")",
                    "LibConfig = record(lib = Lib)",
                    "Names = list[str]",
                    "Lib2 = Lib",
                ]),
            ),
            (
                "lib/other.star",
                "# Another record named Lib.\nLib = record(other = str)\n",
            ),
            (
                "lib/multi.star",
                "# Declared over three lines.\n\nMulti = record(\n    a = str,\n)\n",
            ),
            ("dir/BUILD", "A = \"a\"\n"),
            ("dir/more.BUILD", "\nViaDir = record(a = str)\n"),
            ("lib/é/defs.star", "Accent = record(a = str)\n"),
        ]);
        lint_pkg(tmp.path(), "pkg", false)
            .into_iter()
            .map(|f| f.message)
            .collect()
    }

    const SITE_HEADER: [&str; 5] = [
        "load(\"//lib/defs.star\", \"Lib\", \"Mode\", \"LibConfig\", \"Names\", \"Lib2\")",
        "load(\"//lib/other.star\", OLib = \"Lib\")",
        "load(\"//lib/multi.star\", \"Multi\")",
        "load(\"//dir\", \"ViaDir\")",
        "load(\"//lib/é/defs.star\", \"Accent\")",
    ];

    /// D10: a finding about a declared type names where it was declared,
    /// matched by identity in the error, from real lint runs.
    #[test]
    fn declaration_site_named() {
        let rows: &[(&str, &[&str], &[&str])] = &[
            // One row per message kind.
            (
                "argument mismatch",
                &[
                    "def f(l: Lib) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `Lib` but got `int` (Lib declared at lib/defs.star:1)"],
            ),
            (
                "missing attribute",
                &["def g(l: Lib) -> None:", "    l.nme"],
                &["The attribute `nme` is not available on the type `Lib` \
                   (Lib declared at lib/defs.star:1)"],
            ),
            (
                "return type",
                &["def g() -> Lib:", "    return 1"],
                &["Expected type `Lib` but got `int` (Lib declared at lib/defs.star:1)"],
            ),
            (
                "binary operator",
                &["def g(l: Lib) -> None:", "    l + 1"],
                &[
                    "Binary operator `+` is not available on the types `Lib` and `int` \
                   (Lib declared at lib/defs.star:1)",
                ],
            ),
            (
                "unary operator",
                &["def g(l: Lib) -> None:", "    -l"],
                &["Unary operator `-` is not available on the type `Lib` \
                   (Lib declared at lib/defs.star:1)"],
            ),
            (
                "index",
                &["def g(l: Lib) -> None:", "    l[0]"],
                &[
                    "Type `Lib` does not have [] operator or [] cannot accept `int` \
                   (Lib declared at lib/defs.star:1)",
                ],
            ),
            (
                "iteration",
                &["def g(l: Lib) -> None:", "    for x in l:", "        pass"],
                &["Type `Lib` is not iterable (Lib declared at lib/defs.star:1)"],
            ),
            // The checker reports a non-callable record instance as its base
            // type, from an error with no typed payload: no site.
            (
                "call",
                &["def g(l: Lib) -> None:", "    l()"],
                &["Type `record` is not callable"],
            ),
            // The adversarial inputs.
            (
                "through a container",
                &[
                    "def f(xs: list[Lib]) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f([1])",
                ],
                &["Expected type `list[Lib]` but got `list[int]` \
                   (Lib declared at lib/defs.star:1)"],
            ),
            (
                "two types, in the message's order",
                &[
                    "def g(o: Lib | None) -> None:",
                    "    y: dict[str, Mode] = o",
                ],
                &["Expected type `dict[str, Mode]` but got `None | Lib` \
                   (Mode declared at lib/defs.star:2; Lib declared at lib/defs.star:1)"],
            ),
            (
                "two records named Lib, the message about one",
                &[
                    "def f(l: OLib) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `Lib` but got `int` (Lib declared at lib/other.star:2)"],
            ),
            (
                "two records named Lib, the message about both",
                &[
                    "def f(l: OLib) -> None:",
                    "    pass",
                    "def g(l: Lib) -> None:",
                    "    f(l)",
                ],
                &["Expected type `Lib` but got `Lib` \
                   (Lib declared at lib/other.star:2; Lib declared at lib/defs.star:1)"],
            ),
            (
                "an attribute named like a record",
                &["def g(l: Lib) -> None:", "    l.Mode"],
                &["The attribute `Mode` is not available on the type `Lib` \
                   (Lib declared at lib/defs.star:1)"],
            ),
            (
                "a parameter named like a record",
                &[
                    "Name = record(a = str)",
                    "def g() -> None:",
                    "    Lib(Name = \"x\")",
                ],
                &["Unexpected parameter named `Name`"],
            ),
            (
                "a name that extends another",
                &[
                    "def f(c: LibConfig) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `LibConfig` but got `int` \
                   (LibConfig declared at lib/defs.star:3)"],
            ),
            (
                "an alias of a record",
                &[
                    "def f(l: Lib2) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `Lib` but got `int` (Lib declared at lib/defs.star:1)"],
            ),
            (
                "the same type twice",
                &["def g(l: Lib) -> None:", "    y: dict[Lib, Lib] = l"],
                &["Expected type `dict[Lib, Lib]` but got `Lib` \
                   (Lib declared at lib/defs.star:1)"],
            ),
            (
                "a type's own type",
                &["def g() -> None:", "    y: int = Lib", "    z: int = Mode"],
                &[
                    "Expected type `int` but got `record[Lib]` (Lib declared at lib/defs.star:1)",
                    "Expected type `int` but got `enum[Mode]` (Mode declared at lib/defs.star:2)",
                ],
            ),
            (
                "a builtin, an alias, a plain-message error",
                &[
                    "def f(n: Names) -> None:",
                    "    pass",
                    "def g(k: heph.types.bash.Kwargs) -> None:",
                    "    y: int = \"s\"",
                    "    f(1)",
                    "    k()",
                ],
                &[
                    "Expected type `int` but got `str`",
                    "Expected type `list[str]` but got `int`",
                    "Value of type `typed_kwargs(approval = None | bool | dict[str, typing.Any], \
                     cache = None | bool, env = None | dict[str, str], labels = None | str | \
                     list[str], name = None | str, run = None | str | list[str], transitive = \
                     None | dict[str, typing.Any])` is not callable",
                ],
            ),
            (
                "re-exported by a directory load",
                &[
                    "def f(v: ViaDir) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `ViaDir` but got `int` (ViaDir declared at dir/more.BUILD:2)"],
            ),
            (
                "declared over several lines",
                &[
                    "def f(m: Multi) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `Multi` but got `int` (Multi declared at lib/multi.star:3)"],
            ),
            (
                "declared under a non-ASCII directory",
                &[
                    "def f(a: Accent) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `Accent` but got `int` (Accent declared at lib/é/defs.star:1)"],
            ),
            (
                "a private record",
                &[
                    "_Lib = record(a = str)",
                    "def f(l: _Lib) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &["Expected type `_Lib` but got `int` (_Lib declared at pkg/BUILD:6)"],
            ),
            (
                "more than three types",
                &[
                    "A = record(a = str)",
                    "B = record(a = str)",
                    "C = record(a = str)",
                    "D = record(a = str)",
                    "E = record(a = str)",
                    "def f(x: A | B | C | D | E) -> None:",
                    "    pass",
                    "def g() -> None:",
                    "    f(1)",
                ],
                &[
                    "Expected type `A | B | C | D | E` but got `int` (A declared at \
                   pkg/BUILD:6; B declared at pkg/BUILD:7; C declared at pkg/BUILD:8; \
                   and 2 more)",
                ],
            ),
            // No row for non-ASCII text: starlark identifiers are ASCII, so no
            // typechecker message carries any.
        ];
        for &(row, body, want) in rows {
            assert_eq!(site_messages(body), want, "{row}");
        }
    }

    /// A 1,000-field record and a 10,000-variant enum check in bounded time.
    #[test]
    fn large_declarations() {
        let fields: Vec<String> = (0..1_000).map(|i| format!("f{i} = str")).collect();
        let variants: Vec<String> = (0..10_000).map(|i| format!("\"v{i}\"")).collect();
        let pkg = src(&[
            &format!("Big = record({})", fields.join(", ")),
            &format!("Many = enum({})", variants.join(", ")),
            "",
            "def g(b: Big, m: Many) -> None:",
            "    b.f999 - 1",
            "    m.nope",
        ]);
        let started = std::time::Instant::now();
        let found = lint_ws(&[("pkg/BUILD", &pkg)]);
        let took = started.elapsed();
        assert_eq!(
            found,
            [
                "pkg/BUILD:5:5: type-error: Binary operator `-` is not available on the types \
                 `str` and `int`",
                "pkg/BUILD:6:7: type-error: The attribute `nope` is not available on the type \
                 `Many` (Many declared at pkg/BUILD:2)",
            ]
        );
        assert!(took < std::time::Duration::from_secs(20), "took {took:?}");
    }

    #[test]
    fn distinct_records_are_distinct() {
        let pkg = src(&[
            "A = record(x = str)",
            "B = record(x = str)",
            "",
            "def f(a: A) -> None:",
            "    pass",
            "",
            "def g(b: B) -> None:",
            "    f(b)",
        ]);
        assert_eq!(
            lint_ws(&[("pkg/BUILD", &pkg)]),
            ["pkg/BUILD:8:7: type-error: Expected type `A` but got `B` \
              (A declared at pkg/BUILD:1; B declared at pkg/BUILD:2)"]
        );
    }

    /// One row of [`module_call_results_sound_against_runtime`]: the global
    /// the row calls (its dotted path) and the module's lines, the last of
    /// which binds `x` to the call. A global can have several rows.
    const SOUND_ROWS: &[(&str, &[&str])] = &[
        ("abs", &["x = abs(-1)"]),
        ("abs", &["x = abs(-1.5)"]),
        ("all", &["x = all([1, 0])"]),
        ("any", &["x = any([])"]),
        ("bool", &["x = bool(1)"]),
        ("bytes", &["x = bytes(\"a\")"]),
        ("chr", &["x = chr(65)"]),
        ("dict", &["x = dict(a = 1)"]),
        ("dir", &["x = dir(\"\")"]),
        ("enum", &["x = enum(\"a\", \"b\")"]),
        ("enumerate", &["x = enumerate([\"a\"])"]),
        ("eval_type", &["x = eval_type(int)"]),
        ("fail", &["x = fail(\"no\")"]),
        ("field", &["x = field(str)"]),
        ("field", &["x = field(list[str], [])"]),
        ("file", &["x = file(\"a.txt\")"]),
        ("float", &["x = float(1)"]),
        ("getattr", &["x = getattr(\"\", \"upper\")"]),
        ("glob", &["x = glob(\"*.c\")"]),
        ("hasattr", &["x = hasattr(\"\", \"upper\")"]),
        ("hash", &["x = hash(\"a\")"]),
        ("heph.core.arch", &["x = heph.core.arch()"]),
        ("heph.core.arch_raw", &["x = heph.core.arch_raw()"]),
        ("heph.core.num_cpu", &["x = heph.core.num_cpu()"]),
        ("heph.core.os", &["x = heph.core.os()"]),
        ("heph.core.os_raw", &["x = heph.core.os_raw()"]),
        ("heph.core.packages", &["x = heph.core.packages(\"//...\")"]),
        ("heph.core.pkg", &["x = heph.core.pkg()"]),
        ("heph.core.resource", &["x = heph.core.resource(\"cpu\")"]),
        (
            "heph.core.resource",
            &["x = heph.core.resource(\"memory\")"],
        ),
        ("int", &["x = int(\"1\")"]),
        ("isinstance", &["x = isinstance(1, int)"]),
        ("len", &["x = len([1])"]),
        ("list", &["x = list((1, 2))"]),
        ("max", &["x = max(1, 2)"]),
        ("min", &["x = min([1, 2])"]),
        ("ord", &["x = ord(\"a\")"]),
        (
            "provider_state",
            &["x = provider_state(provider = \"p\", a = 1)"],
        ),
        ("query", &["x = query(\"//...\")"]),
        ("range", &["x = range(3)"]),
        ("record", &["x = record(name = str)"]),
        ("repr", &["x = repr(1)"]),
        ("reversed", &["x = reversed([1, 2])"]),
        ("sorted", &["x = sorted([2, 1])"]),
        ("str", &["x = str(1)"]),
        ("struct", &["x = struct(a = 1)"]),
        ("target", &["x = target(name = \"t\", driver = \"bash\")"]),
        ("tuple", &["x = tuple([1])"]),
        ("type", &["x = type(1)"]),
        ("zip", &["x = zip([1], [\"a\"])"]),
    ];

    /// Callable globals [`module_call_results_sound_against_runtime`] cannot
    /// call in a unit test, each with why. Checked: an entry that is not a
    /// callable global, or that also has a row, fails the test.
    const SOUND_EXEMPT: &[(&str, &str)] = &[];

    /// Every callable global the checker sees ([`check_globals`] with no
    /// plugin function: a plugin function is opaque and typed `Any`), by
    /// dotted path, namespaces walked.
    fn callable_globals() -> BTreeSet<String> {
        use starlark::environment::Module;
        use starlark::values::{Heap, Value};

        fn walk<'v>(path: String, v: Value<'v>, heap: Heap<'v>, out: &mut BTreeSet<String>) {
            match v.get_type() {
                "namespace" => {
                    for name in v.dir_attr() {
                        let member =
                            v.get_attr(&name, heap).ok().flatten().unwrap_or_else(|| {
                                panic!("{path}.{name} is listed but not readable")
                            });
                        walk(format!("{path}.{name}"), member, heap, out);
                    }
                }
                "function" => {
                    out.insert(path);
                }
                _ => {}
            }
        }

        let globals = check_globals(&[], &crate::pluginbuildfile::types::tests::test_kwargs());
        Module::with_temp_heap(|module| {
            let mut out = BTreeSet::new();
            for (name, v) in globals.iter() {
                walk(name.to_owned(), v.to_value(), module.heap(), &mut out);
            }
            out
        })
    }

    /// A type as a value, so `TypeCompiled::new` compiles it exactly as the
    /// evaluator compiles an annotation of that type
    /// (`TypeCompiled::from_ty`, which is crate-private).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
    struct TyValue(Ty);

    impl std::fmt::Display for TyValue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&self.0, f)
        }
    }

    starlark::starlark_simple_value!(TyValue);

    #[starlark_value(type = "ty_value")]
    impl<'v> StarlarkValue<'v> for TyValue {
        fn eval_type(&self) -> Option<Ty> {
            Some(self.0.clone())
        }
    }

    /// I11's gate, exhaustive over the globals: for each row, the pre-pass's
    /// type of `x = <call>` at module level matches the value evaluation
    /// binds `x` to. A call that fails at evaluation must be typed `Never`.
    /// A callable global with no row and no exemption fails, so a new native
    /// cannot skip the sweep; a wrong declared return type fails here. Method
    /// calls are typed through `dot` (D11), so every member of the
    /// [`METHOD_RECEIVERS`] is held to the same rule, by [`METHOD_ROWS`].
    #[test]
    fn module_call_results_sound_against_runtime() {
        use starlark::environment::Module;
        use starlark::values::FrozenHeap;
        use starlark::values::typing::TypeCompiled;

        let mut callables = callable_globals();
        let members = receiver_members();
        assert!(
            callables.is_disjoint(&members),
            "a global and a member share a key: {:?}",
            callables.intersection(&members).collect::<Vec<_>>()
        );
        callables.extend(members);
        let rowed: BTreeSet<String> = SOUND_ROWS
            .iter()
            .chain(METHOD_ROWS)
            .map(|(g, _)| (*g).to_owned())
            .collect();
        let exempt: BTreeSet<String> = SOUND_EXEMPT.iter().map(|(g, _)| (*g).to_owned()).collect();
        let missing: Vec<&String> = callables
            .iter()
            .filter(|g| !rowed.contains(*g) && !exempt.contains(*g))
            .collect();
        assert!(
            missing.is_empty(),
            "callable globals or receiver members with no row: {missing:?}"
        );
        let stale: Vec<&String> = rowed
            .iter()
            .chain(&exempt)
            .filter(|g| !callables.contains(*g))
            .collect();
        assert!(
            stale.is_empty(),
            "rows or exemptions for no callable global or receiver member: {stale:?}"
        );
        let both: Vec<&String> = exempt.intersection(&rowed).collect();
        assert!(both.is_empty(), "exempt and rowed: {both:?}");

        let mut failures = Vec::new();
        let rows = SOUND_ROWS
            .iter()
            .map(|row| (row, None))
            .chain(METHOD_ROWS.iter().map(|row| (row, row.0.split_once('.'))));
        for ((global, lines), method) in rows {
            let (inferred, verdict) = sound_against_runtime(lines, |inferred, value| {
                let frozen = FrozenHeap::new();
                Module::with_temp_heap(|module| {
                    let heap = module.heap();
                    TypeCompiled::new(heap.alloc_simple(TyValue(inferred.clone())), heap)
                        .expect("a type compiles")
                        .to_frozen(&frozen)
                })
                .matches(value)
            });
            if let Err(why) = verdict {
                failures.push(format!("{global} {lines:?}: typed `{inferred}`, {why}"));
            }
            // A method row typed `Any` checks nothing: it must be on a
            // receiver the pre-pass never types, and a row on such a
            // receiver that becomes typed means the list is stale.
            if let Some((receiver, _)) = method {
                let untyped = UNTYPED_METHOD_RECEIVERS.contains(&receiver);
                let any = inferred == Ty::any();
                if any && !untyped {
                    failures.push(format!(
                        "{global} {lines:?}: typed `Any`, so the row checks nothing"
                    ));
                } else if !any && untyped {
                    failures.push(format!(
                        "{global} {lines:?}: typed `{inferred}` on a receiver \
                         UNTYPED_METHOD_RECEIVERS says is never typed"
                    ));
                }
            }
        }
        // A record or enum the module declares has one identity in the
        // pre-pass and another in each evaluation, so its constructor's result
        // cannot match by identity: it is held to the type evaluation gives
        // the value, by name (the fork's `export_as_unchanged` pins that the
        // two name the same type).
        for lines in DECLARED_ROWS {
            let (inferred, verdict) = sound_against_runtime(lines, |inferred, value| {
                !inferred.user_type_ids().is_empty()
                    && Ty::of_value(value).to_string() == inferred.to_string()
            });
            if let Err(why) = verdict {
                failures.push(format!("{lines:?}: typed `{inferred}`, {why}"));
            }
        }
        assert!(
            failures.is_empty(),
            "unsound call result types:\n{}",
            failures.join("\n")
        );
    }

    /// The receivers whose members [`module_call_results_sound_against_runtime`]
    /// sweeps, each with a BUILD expression evaluating to one. A method call
    /// is typed through `dot` (D11), so a member's declared type moves
    /// verdicts exactly as a global's return type does. The dialect has no
    /// `set` value (no `set` global, no literal), so there is none to sweep.
    const METHOD_RECEIVERS: &[(&str, &str)] = &[
        ("str", "\"\""),
        ("list", "[]"),
        ("dict", "{}"),
        ("bytes", "bytes(\"\")"),
    ];

    /// Receivers on which the pre-pass never types a method call: `dot` takes
    /// a member's declared type only when the receiver has a value (D11), and
    /// the pre-pass has no list, dict or bytes value (a display or a call
    /// result is typed at most), so `x = l.pop()` at module level is `Any`.
    /// Their rows still run and pin that; a `str` row typed `Any` fails, as
    /// it would check nothing, and a row here that becomes typed fails too.
    const UNTYPED_METHOD_RECEIVERS: &[&str] = &["list", "dict", "bytes"];

    /// One row of the method half of
    /// [`module_call_results_sound_against_runtime`]: `<receiver>.<member>`
    /// and the module's lines, the last of which binds `x` to the call. A
    /// member can have several rows.
    const METHOD_ROWS: &[(&str, &[&str])] = &[
        ("str.capitalize", &["x = \"ab\".capitalize()"]),
        ("str.codepoints", &["x = \"ab\".codepoints()"]),
        ("str.count", &["x = \"aba\".count(\"a\")"]),
        ("str.count", &["x = \"aba\".count(\"a\", 1, 3)"]),
        ("str.elems", &["x = \"ab\".elems()"]),
        ("str.endswith", &["x = \"ab\".endswith(\"b\")"]),
        ("str.endswith", &["x = \"ab\".endswith((\"a\", \"b\"))"]),
        ("str.find", &["x = \"ab\".find(\"z\")"]),
        ("str.format", &["x = \"{}-{a}\".format(1, a = 2)"]),
        ("str.index", &["x = \"ab\".index(\"b\")"]),
        ("str.isalnum", &["x = \"a1\".isalnum()"]),
        ("str.isalpha", &["x = \"a1\".isalpha()"]),
        ("str.isdigit", &["x = \"1\".isdigit()"]),
        ("str.islower", &["x = \"a\".islower()"]),
        ("str.isspace", &["x = \" \".isspace()"]),
        ("str.istitle", &["x = \"Ab\".istitle()"]),
        ("str.isupper", &["x = \"A\".isupper()"]),
        ("str.join", &["x = \",\".join([\"a\", \"b\"])"]),
        ("str.lower", &["x = \"A\".lower()"]),
        ("str.lstrip", &["x = \" a\".lstrip()"]),
        ("str.lstrip", &["x = \"xa\".lstrip(\"x\")"]),
        ("str.partition", &["x = \"a=b\".partition(\"=\")"]),
        ("str.partition", &["x = \"ab\".partition(\"=\")"]),
        ("str.removeprefix", &["x = \"ab\".removeprefix(\"a\")"]),
        ("str.removesuffix", &["x = \"ab\".removesuffix(\"b\")"]),
        ("str.replace", &["x = \"aa\".replace(\"a\", \"b\", 1)"]),
        ("str.rfind", &["x = \"aba\".rfind(\"a\")"]),
        ("str.rindex", &["x = \"aba\".rindex(\"a\")"]),
        ("str.rpartition", &["x = \"a=b=c\".rpartition(\"=\")"]),
        ("str.rsplit", &["x = \"a,b,c\".rsplit(\",\", 1)"]),
        ("str.rsplit", &["x = \"a b\".rsplit()"]),
        ("str.rstrip", &["x = \"a \".rstrip()"]),
        ("str.split", &["x = \"a,b\".split(\",\")"]),
        ("str.split", &["x = \"a b\".split()"]),
        ("str.splitlines", &["x = \"a\\nb\".splitlines()"]),
        ("str.splitlines", &["x = \"a\\nb\".splitlines(True)"]),
        ("str.startswith", &["x = \"ab\".startswith(\"a\")"]),
        ("str.strip", &["x = \" a \".strip()"]),
        ("str.title", &["x = \"ab cd\".title()"]),
        ("str.upper", &["x = \"a\".upper()"]),
        ("str.split", &["S = \"a,b\"", "x = S.split(\",\")"]),
        ("bytes.elems", &["b = bytes(\"ab\")", "x = b.elems()"]),
        ("list.append", &["l = list([1])", "x = l.append(2)"]),
        ("list.clear", &["l = list([1])", "x = l.clear()"]),
        ("list.extend", &["l = list([1])", "x = l.extend([2])"]),
        (
            "list.index",
            &["l = list([\"a\", \"b\"])", "x = l.index(\"b\")"],
        ),
        ("list.insert", &["l = list([1])", "x = l.insert(0, 2)"]),
        ("list.pop", &["l = list([\"a\", \"b\"])", "x = l.pop()"]),
        ("list.pop", &["l = list([1, \"a\"])", "x = l.pop(0)"]),
        ("list.pop", &["l = \"a,b\".split(\",\")", "x = l.pop()"]),
        ("list.remove", &["l = list([1])", "x = l.remove(1)"]),
        ("dict.clear", &["d = dict(a = 1)", "x = d.clear()"]),
        ("dict.get", &["d = dict(a = 1)", "x = d.get(\"a\")"]),
        ("dict.get", &["d = dict(a = 1)", "x = d.get(\"b\")"]),
        ("dict.get", &["d = dict(a = 1)", "x = d.get(\"b\", \"z\")"]),
        ("dict.items", &["d = dict(a = 1)", "x = d.items()"]),
        ("dict.keys", &["d = dict(a = 1)", "x = d.keys()"]),
        ("dict.pop", &["d = dict(a = 1)", "x = d.pop(\"a\")"]),
        ("dict.pop", &["d = dict(a = 1)", "x = d.pop(\"b\", \"z\")"]),
        ("dict.popitem", &["d = dict(a = 1)", "x = d.popitem()"]),
        (
            "dict.setdefault",
            &["d = dict(a = 1)", "x = d.setdefault(\"b\")"],
        ),
        (
            "dict.setdefault",
            &["d = dict(a = 1)", "x = d.setdefault(\"a\", 2)"],
        ),
        ("dict.update", &["d = dict(a = 1)", "x = d.update(b = 2)"]),
        (
            "dict.update",
            &["d = dict(a = 1)", "x = d.update([(\"b\", 2)])"],
        ),
        ("dict.values", &["d = dict(a = 1)", "x = d.values()"]),
    ];

    /// Every member the checker types on a [`METHOD_RECEIVERS`] receiver, as
    /// `<receiver>.<member>`, with each receiver's typed members held equal
    /// to the attributes evaluation gives the value: a member evaluation has
    /// and lint lacks makes lint stricter than evaluation (I2), and one lint
    /// has and evaluation lacks is a declared type no call can be checked
    /// against.
    ///
    /// Every member is kept, callable or not: today every member of these
    /// receivers is a method, and a non-method attribute's type is the same
    /// declared surface, held to the same rule (its row reads it).
    fn receiver_members() -> BTreeSet<String> {
        let lines: Vec<String> = METHOD_RECEIVERS
            .iter()
            .map(|(name, expr)| format!("r_{name} = {expr}"))
            .collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let tmp = ws(&[("pkg/BUILD", &src(&lines))]);
        let result = crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "pkg")
            .unwrap_or_else(|e| panic!("the receivers evaluate: {}", chain(&e)));

        let mut out = BTreeSet::new();
        let mut drift = Vec::new();
        for (name, _) in METHOD_RECEIVERS {
            let value = result
                .module
                .get(&format!("r_{name}"))
                .unwrap_or_else(|e| panic!("r_{name} is bound: {e}"));
            let value = value.value();
            let typed: BTreeSet<String> = starlark::typing::members(&Ty::of_value(value))
                .into_iter()
                .map(|(member, _)| member)
                .collect();
            let runtime: BTreeSet<String> = value.dir_attr().into_iter().collect();
            if typed != runtime {
                drift.push(format!(
                    "{name}: typed only {:?}, runtime only {:?}",
                    typed.difference(&runtime).collect::<Vec<_>>(),
                    runtime.difference(&typed).collect::<Vec<_>>(),
                ));
            }
            out.extend(typed.into_iter().map(|member| format!("{name}.{member}")));
        }
        assert!(
            drift.is_empty(),
            "typed members differ from evaluation's attributes:\n{}",
            drift.join("\n")
        );
        out
    }

    /// Constructor calls of a record and an enum the module declares, for
    /// [`module_call_results_sound_against_runtime`]: the last line binds `x`.
    const DECLARED_ROWS: &[&[&str]] = &[
        &["Lib = record(name = str)", "x = Lib(name = \"a\")"],
        &[
            "Lib = record(name = str, srcs = field(list[str], []))",
            "x = Lib(name = \"a\")",
        ],
        &["Mode = enum(\"a\", \"b\")", "x = Mode(\"a\")"],
    ];

    /// The pre-pass's type of module-level `x` in `lines`, and whether it is
    /// sound against evaluation of the same file as `//pkg`: `matches` holds
    /// for the value evaluation binds, or the type is `Never` and evaluation
    /// fails.
    fn sound_against_runtime(
        lines: &[&str],
        matches: impl FnOnce(&Ty, starlark::values::Value<'_>) -> bool,
    ) -> (Ty, Result<(), String>) {
        let file = src(lines);
        let ast = parse_build_source("pkg/BUILD", file.clone()).expect("parses");
        let globals = check_globals(&[], &crate::pluginbuildfile::types::tests::test_kwargs());
        let (_, types, _, _) = ast.typecheck(&globals, &HashMap::new());
        let inferred = types
            .module_bindings()
            .find(|b| b.name == "x")
            .unwrap_or_else(|| panic!("`x` is not bound in {lines:?}"))
            .ty
            .clone();

        let tmp = ws(&[("pkg/BUILD", &file), ("pkg/a.txt", "")]);
        let verdict = match crate::pluginbuildfile::checks::test_ws::eval_pkg(tmp.path(), "pkg") {
            Err(_) if inferred == Ty::never() => Ok(()),
            Err(e) => Err(format!("evaluation failed: {}", chain(&e))),
            Ok(_) if inferred == Ty::never() => Err("evaluates, typed `Never`".to_owned()),
            Ok(result) => {
                let value = result.module.get("x").expect("x is bound");
                let value = value.value();
                if matches(&inferred, value) {
                    Ok(())
                } else {
                    Err(format!("value `{value}` of type `{}`", value.get_type()))
                }
            }
        };
        (inferred, verdict)
    }
}
