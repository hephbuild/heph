//! Typed member completion: what `x.│` lists when `x` is a name the file
//! binds, or a literal (`docs/LSP.md`, "Typed members").
//!
//! The types come from lint's checker, run over the document's last good
//! parse with lint's globals and lint's load interfaces (I9), and are looked
//! up by the cursor's def path and the receiver's name, never by position
//! (I8). A member list is exactly what the checker accepts on the receiver's
//! type ([`starlark::typing::members`], I7).

use crate::pluginbuildfile::checks::lint::{self, interface_of};
use crate::pluginbuildfile::checks::loads::walk_loads;
use crate::pluginbuildfile::run_file::parse_build_source;
use serde_json::{Map, Value, json};
use starlark::environment::Globals;
use starlark::typing::{AstModuleTypecheck, BindingKind, Interface, Ty, TyBasic, members};
use std::collections::HashMap;
use std::path::Path;

use super::site::Literal;

/// The interface each of a document's `load()` strings sees: lint's walk and
/// lint's merge (I9), the loaded files read from disk under `root`.
///
/// `file` is the document's absolute path (`root` joined with its workspace
/// path), `pkg` its package, `modules` its load strings. A load that does not
/// resolve is missing from the map, so the names it binds are untyped.
pub(crate) fn loaded_interfaces<'m>(
    root: &Path,
    pkg: &str,
    file: &Path,
    modules: impl IntoIterator<Item = &'m str>,
    patterns: &[glob::Pattern],
    globals: &Globals,
) -> HashMap<String, Interface> {
    let walker = hwalk::CachedWalker::disabled();
    let (edges, walk) = walk_loads(root, &walker, pkg, file, modules, patterns);
    let mut read = lint::read_under(root);
    let checked = lint::interfaces(&walk, &mut read, globals);
    edges
        .into_iter()
        .filter_map(|(module, edge)| Some((module, interface_of(&edge, &checked).ok()?)))
        .collect()
}

/// One module-level binding's type and where it came from.
#[derive(Debug)]
struct ModuleType {
    ty: Ty,
    /// The `load()` string that bound it, for a loaded name.
    load: Option<String>,
    /// `path:line` of the record or enum declaration it names.
    declaration: Option<String>,
}

/// One def's bindings by name: every binding of the name, with how it is
/// bound.
type DefBindings = HashMap<String, Vec<(BindingKind, Ty)>>;

/// The types of one document's bindings, from one typecheck of its last good
/// parse.
#[derive(Debug, Default)]
pub(crate) struct DocTypes {
    /// Def bindings by owner def path.
    defs: HashMap<Vec<String>, DefBindings>,
    module: HashMap<String, ModuleType>,
    /// The checker's reason for typing a declaration as `Any`, by name.
    approximations: HashMap<String, String>,
}

impl DocTypes {
    /// Typecheck `source` (the document at workspace path `path`) against
    /// `globals`, its loads seeing what `loads` returns for its load strings.
    /// `None` when it does not parse.
    pub(crate) fn check(
        path: &str,
        source: &str,
        globals: &Globals,
        loads: impl FnOnce(&[&str]) -> HashMap<String, Interface>,
    ) -> Option<DocTypes> {
        let ast = parse_build_source(path, source.to_owned()).ok()?;
        // Read before the typecheck consumes the AST.
        let mut loaded: HashMap<String, String> = HashMap::new();
        let mut modules: Vec<&str> = Vec::new();
        let ast_loads = ast.loads();
        for load in &ast_loads {
            modules.push(load.module_id);
            for local in load.symbols.keys() {
                loaded.insert((*local).to_owned(), load.module_id.to_owned());
            }
        }
        let interfaces = loads(&modules);
        drop(ast_loads);
        let (_errors, types, _iface, approximations) = ast.typecheck(globals, &interfaces);

        let mut defs: HashMap<Vec<String>, DefBindings> = HashMap::new();
        for (owner, name, kind, ty) in types.bindings() {
            defs.entry(owner.to_vec())
                .or_default()
                .entry(name.to_owned())
                .or_default()
                .push((kind, ty.clone()));
        }
        let module = types
            .module_bindings()
            .map(|b| {
                let declaration = b.declaration.map(|(_, span)| {
                    format!("{path}:{}", span.resolve_span().begin.line + 1)
                });
                (
                    b.name.to_owned(),
                    ModuleType {
                        ty: b.ty.clone(),
                        load: loaded.get(b.name).cloned(),
                        declaration,
                    },
                )
            })
            .collect();
        let approximations = approximations
            .iter()
            .filter(|a| a.category == "Declaration typed as Any")
            .filter_map(|a| {
                let message = a.message.trim_matches('"');
                let (name, reason) = message.split_once(": ")?;
                Some((name.to_owned(), reason.to_owned()))
            })
            .collect();
        Some(DocTypes {
            defs,
            module,
            approximations,
        })
    }
}

/// A typed member answer: the items, and the keys `--explain` adds to a
/// member site (D5).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Answer {
    /// `param`, `local`, `comprehension`, `lambda`, `module`, `loaded` or
    /// `none`; an open set.
    pub binding: &'static str,
    /// The receiver's type, once the chain is folded; `None` when unknown.
    pub ty: Option<Ty>,
    /// The declaration (`path:line`) or the load string the receiver's root
    /// came from; `None` for a parameter or a local.
    pub source: Option<String>,
    /// The checker's reason for typing the receiver `Any`.
    pub approximation: Option<String>,
    /// The types are from an earlier version of the text than the current.
    pub stale: bool,
    /// Whether the registry had landed when the types were checked.
    pub registry_ready: bool,
    /// Why `items` is empty; `None` when it is not.
    pub empty_because: Option<&'static str>,
    /// The members the checker accepts, sorted, each with its type.
    pub items: Vec<(String, Ty)>,
}

/// What a lookup needs besides the types.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lookup<'a> {
    /// The receiver chain's root and the segments after it.
    pub root: &'a str,
    pub rest: &'a [String],
    /// The cursor's def path, outermost first.
    pub scope: &'a [String],
    /// The root is bound by a comprehension or lambda in the cursor's bracket
    /// group or line ([`super::site::bound_in_group`]).
    pub shadowed: bool,
    pub stale: bool,
    pub registry_ready: bool,
}

impl Answer {
    fn empty(binding: &'static str, because: &'static str, lookup: &Lookup<'_>) -> Answer {
        Answer {
            binding,
            ty: None,
            source: None,
            approximation: None,
            stale: lookup.stale,
            registry_ready: lookup.registry_ready,
            empty_because: Some(because),
            items: Vec::new(),
        }
    }

    /// The members of a literal receiver (D9): no parse, no registry.
    pub(crate) fn literal(kind: Literal, registry_ready: bool) -> Answer {
        let ty = match kind {
            Literal::Str => Ty::string(),
            Literal::List => Ty::list(Ty::any()),
            Literal::Dict => Ty::dict(Ty::any(), Ty::any()),
        };
        let items = members(&ty);
        Answer {
            binding: "none",
            empty_because: items.is_empty().then(|| empty_because(&ty)),
            ty: Some(ty),
            source: None,
            approximation: None,
            stale: false,
            registry_ready,
            items,
        }
    }

    /// The members of `lookup`'s receiver in `types` (`None`: the document
    /// has never parsed).
    pub(crate) fn of(types: Option<&DocTypes>, lookup: &Lookup<'_>) -> Answer {
        let Some(types) = types else {
            return Answer::empty("none", "no_types_yet", lookup);
        };
        if lookup.shadowed {
            return Answer::empty("comprehension", "ambiguous_binding", lookup);
        }
        let (binding, root_ty, source) = match find(types, lookup.root, lookup.scope) {
            Found::Def(BindingKind::Comprehension) => {
                return Answer::empty("comprehension", "ambiguous_binding", lookup);
            }
            Found::Def(BindingKind::Lambda) => {
                return Answer::empty("lambda", "ambiguous_binding", lookup);
            }
            Found::Ambiguous(kind) => {
                return Answer::empty(kind.as_str(), "ambiguous_binding", lookup);
            }
            Found::Missing => return Answer::empty("none", "unknown_binding", lookup),
            Found::Def(kind) => (kind.as_str(), def_ty(types, lookup), None),
            Found::Module(m) => match &m.load {
                Some(load) => ("loaded", Some(m.ty.clone()), Some(load.clone())),
                None => ("module", Some(m.ty.clone()), m.declaration.clone()),
            },
        };
        let Some(root_ty) = root_ty else {
            return Answer::empty(binding, "unknown_binding", lookup);
        };
        // `l.mode.│`: each segment is a member of the type before it.
        // An open type's attribute is unknown (`Any`); a type that lists its
        // members and not the segment is not a receiver the checker accepts.
        let mut ty = root_ty;
        for seg in lookup.rest {
            let listed = members(&ty);
            let open = listed.is_empty() && matches!(empty_because(&ty), "any" | "open_type");
            ty = match listed.into_iter().find(|(name, _)| name == seg) {
                Some((_, member)) => member,
                None if open => Ty::any(),
                None => {
                    let mut answer = Answer::empty(binding, "unknown_binding", lookup);
                    answer.source = source;
                    return answer;
                }
            };
        }
        let items = members(&ty);
        let approximation = (lookup.rest.is_empty() && ty == Ty::any())
            .then(|| types.approximations.get(lookup.root).cloned())
            .flatten();
        let empty_because = items.is_empty().then(|| {
            if approximation.is_some() {
                "approximation"
            } else {
                empty_because(&ty)
            }
        });
        Answer {
            binding,
            ty: Some(ty),
            source,
            approximation,
            stale: lookup.stale,
            registry_ready: lookup.registry_ready,
            empty_because,
            items,
        }
    }

    /// Add the D5 keys to a member site's `--explain` object.
    pub(crate) fn explain_into(&self, site: &mut Map<String, Value>, literal: bool) {
        if !literal {
            site.insert("binding".to_string(), json!(self.binding));
            site.insert(
                "type".to_string(),
                json!(self.ty.as_ref().map(ToString::to_string)),
            );
            site.insert("source".to_string(), json!(self.source));
            site.insert("approximation".to_string(), json!(self.approximation));
            site.insert("stale".to_string(), json!(self.stale));
        }
        site.insert(
            "registry".to_string(),
            json!(if self.registry_ready { "ready" } else { "pending" }),
        );
        site.insert("empty_because".to_string(), json!(self.empty_because));
    }
}

/// Where a name resolved.
enum Found<'t> {
    Def(BindingKind),
    /// Bound more than once under the innermost def that binds it.
    Ambiguous(BindingKind),
    Module(&'t ModuleType),
    Missing,
}

/// Look `name` up from the def path `scope` outward, then at module level.
fn find<'t>(types: &'t DocTypes, name: &str, scope: &[String]) -> Found<'t> {
    for depth in (1..=scope.len()).rev() {
        let Some(entries) = scope
            .get(..depth)
            .and_then(|owner| types.defs.get(owner))
            .and_then(|names| names.get(name))
        else {
            continue;
        };
        return match entries.as_slice() {
            [(kind, _)] => Found::Def(*kind),
            [(kind, _), ..] => Found::Ambiguous(*kind),
            [] => continue,
        };
    }
    types.module.get(name).map_or(Found::Missing, Found::Module)
}

/// The type of the def binding [`find`] found for `lookup`.
fn def_ty(types: &DocTypes, lookup: &Lookup<'_>) -> Option<Ty> {
    (1..=lookup.scope.len()).rev().find_map(|depth| {
        let entries = types.defs.get(lookup.scope.get(..depth)?)?.get(lookup.root)?;
        entries.first().map(|(_, ty)| ty.clone())
    })
}

/// Why a type that is not `Any` lists no members: `open_type` when it accepts
/// names it cannot list, `no_members` when it accepts none.
fn empty_because(ty: &Ty) -> &'static str {
    if *ty == Ty::any() {
        return "any";
    }
    let open = ty.iter_union().iter().any(|basic| match basic {
        TyBasic::Any
        | TyBasic::Callable(_)
        | TyBasic::Iter(_)
        | TyBasic::Type
        | TyBasic::Custom(_) => true,
        TyBasic::StarlarkValue(v) => {
            !matches!(v.to_string().as_str(), "int" | "float" | "bool" | "None")
        }
        TyBasic::List(_) | TyBasic::Tuple(_) | TyBasic::Dict(..) | TyBasic::Set(_) => false,
    });
    if open { "open_type" } else { "no_members" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::heph_globals;
    use crate::pluginbuildfile::types::DriverKwargs;

    fn types(src: &str) -> DocTypes {
        let globals = heph_globals(&DriverKwargs::default(), |_| {});
        DocTypes::check("p/BUILD", src, &globals, |_| HashMap::new()).expect("parses")
    }

    fn answer(types: &DocTypes, root: &str, rest: &[&str], scope: &[&str]) -> Answer {
        let rest: Vec<String> = rest.iter().map(|s| (*s).to_string()).collect();
        let scope: Vec<String> = scope.iter().map(|s| (*s).to_string()).collect();
        Answer::of(
            Some(types),
            &Lookup {
                root,
                rest: &rest,
                scope: &scope,
                shadowed: false,
                stale: false,
                registry_ready: true,
            },
        )
    }

    fn names(a: &Answer) -> Vec<&str> {
        a.items.iter().map(|(n, _)| n.as_str()).collect()
    }

    const LIB: &str = "Lib = record(name = str, mode = str)\n\
                       Mode = enum(\"debug\", \"release\")\n";

    #[test]
    fn typed_lookup_by_scope_and_name() {
        let t = types(&format!(
            "{LIB}cfg = Lib(name = \"x\", mode = \"y\")\n\
             def f(l: Lib, m: Mode, **kw):\n    c = Lib(name = \"a\", mode = \"b\")\n    pass\n\
             def g(l: Mode):\n    pass\n"
        ));
        let a = answer(&t, "l", &[], &["f"]);
        assert_eq!((a.binding, names(&a)), ("param", vec!["mode", "name"]));
        assert_eq!(a.ty.as_ref().map(ToString::to_string), Some("Lib".into()));
        // Same name in another def: that def's binding, never this one's.
        let a = answer(&t, "l", &[], &["g"]);
        assert_eq!(names(&a), ["index", "value"]);
        let a = answer(&t, "c", &[], &["f"]);
        assert_eq!((a.binding, names(&a)), ("local", vec!["mode", "name"]));
        let a = answer(&t, "cfg", &[], &["f"]);
        assert_eq!((a.binding, names(&a)), ("module", vec!["mode", "name"]));
        let a = answer(&t, "Mode", &[], &[]);
        assert!(names(&a).contains(&"debug"), "{a:?}");
        assert_eq!(a.source.as_deref(), Some("p/BUILD:2"));
        let a = answer(&t, "l", &["mode"], &["f"]);
        assert!(names(&a).contains(&"upper"), "{a:?}");
        let a = answer(&t, "l", &["nosuch"], &["f"]);
        assert_eq!(a.empty_because, Some("unknown_binding"));
        let a = answer(&t, "nosuch", &[], &["f"]);
        assert_eq!(a.empty_because, Some("unknown_binding"));
    }

    #[test]
    fn same_name_twice_under_one_owner_is_ambiguous() {
        let t = types(&format!(
            "{LIB}def f(l: Lib):\n    pass\ndef f(l: Mode):\n    pass\n"
        ));
        let a = answer(&t, "l", &[], &["f"]);
        assert_eq!(a.empty_because, Some("ambiguous_binding"), "{a:?}");
        assert!(a.items.is_empty());
    }

    #[test]
    fn empty_reasons() {
        let t = types("def f(i: int, c: typing.Callable, a):\n    pass\n");
        let because = |name| answer(&t, name, &[], &["f"]).empty_because;
        assert_eq!(because("i"), Some("no_members"));
        assert_eq!(because("c"), Some("open_type"));
        assert_eq!(because("a"), Some("any"));
        assert_eq!(
            Answer::of(
                None,
                &Lookup {
                    root: "x",
                    rest: &[],
                    scope: &[],
                    shadowed: false,
                    stale: false,
                    registry_ready: false,
                }
            )
            .empty_because,
            Some("no_types_yet")
        );
    }

    #[test]
    fn literal_members_are_the_checker_s() {
        let a = Answer::literal(Literal::Str, false);
        assert!(names(&a).contains(&"upper"), "{a:?}");
        let a = Answer::literal(Literal::List, false);
        assert!(names(&a).contains(&"append"), "{a:?}");
        let a = Answer::literal(Literal::Dict, false);
        assert!(names(&a).contains(&"items"), "{a:?}");
    }
}
