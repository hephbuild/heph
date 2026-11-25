//! Typed member completion: what `x.│` lists when `x` is a name the file
//! binds, or a literal (`docs/LSP.md`, "Typed members").
//!
//! The types come from lint's checker, run over the document's last good
//! parse with lint's globals and lint's load interfaces (I9), and are looked
//! up by the cursor's def path and the receiver's name, never by position
//! (I8). A member list is exactly what the checker accepts on the receiver's
//! type ([`starlark::typing::members`], I7).

use crate::pluginbuildfile::run_file::parse_build_source;
use serde_json::{Map, Value, json};
use starlark::environment::Globals;
use starlark::typing::{
    AstModuleTypecheck, BindingKind, DeclarationApproximation, Interface, Ty, TyBasic, members,
};
use std::collections::HashMap;
use std::sync::Arc;

use super::site::{Literal, Receiver, Site};

/// One module-level binding's type and where it came from.
#[derive(Debug)]
struct ModuleType {
    ty: Ty,
    /// The `load()` string that bound it, for a loaded name.
    load: Option<String>,
    /// `path:line` of the record or enum declaration it names.
    declaration: Option<String>,
    /// Why the checker typed the declaration this binding holds as `Any`.
    approximation: Option<DeclarationApproximation>,
}

/// One binding of a name inside a def.
#[derive(Debug)]
struct DefBinding {
    kind: BindingKind,
    ty: Ty,
    /// Why the checker typed the declaration this binding holds as `Any`.
    approximation: Option<DeclarationApproximation>,
}

/// One def's bindings by name: every binding of the name, with how it is
/// bound.
type DefBindings = HashMap<String, Vec<DefBinding>>;

/// The types of one document's bindings, from one typecheck of its last good
/// parse.
#[derive(Debug, Default)]
pub(crate) struct DocTypes {
    /// Def bindings by owner def path.
    defs: HashMap<Vec<String>, DefBindings>,
    module: HashMap<String, ModuleType>,
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
        let (_errors, types, _iface, _approximations) = ast.typecheck(globals, &interfaces);

        let mut defs: HashMap<Vec<String>, DefBindings> = HashMap::new();
        for (owner, name, kind, ty, approximation) in types.bindings() {
            defs.entry(owner.to_vec())
                .or_default()
                .entry(name.to_owned())
                .or_default()
                .push(DefBinding {
                    kind,
                    ty: ty.clone(),
                    approximation,
                });
        }
        let module = types
            .module_bindings()
            .map(|b| {
                let declaration = b
                    .declaration
                    .map(|(_, span)| format!("{path}:{}", span.resolve_span().begin.line + 1));
                (
                    b.name.to_owned(),
                    ModuleType {
                        ty: b.ty.clone(),
                        load: loaded.get(b.name).cloned(),
                        declaration,
                        approximation: b.approximation,
                    },
                )
            })
            .collect();
        Some(DocTypes { defs, module })
    }
}

/// What a member answer needs from where it is asked: the editor's server,
/// or the CLI's `--explain`. One dispatch ([`member_answer`]) serves both, so
/// the two cannot answer the same site differently but for what the host
/// holds.
pub(crate) trait MemberHost {
    /// Whether the registry has landed.
    fn registry_ready(&mut self) -> bool;
    /// `name` is a global the typing globals hold, or a namespace.
    fn is_global(&mut self, name: &str) -> bool;
    /// `name` is bound by a comprehension or lambda in the cursor's bracket
    /// group or line ([`super::site::bound_in_group`]); `None` when the document
    /// is gone.
    fn shadowed(&mut self, name: &str) -> Option<bool>;
    /// The document's types (`None`: it has never parsed), whether they are
    /// from an earlier text than the current one, and whether the registry
    /// had landed when they were checked.
    fn types(&mut self) -> (Option<Arc<DocTypes>>, bool, bool);
}

/// The typed answer at a member site whose receiver is a name the file
/// binds, a name that is no global, or a literal, and whether it is a
/// literal's. `None` at any other site: a global namespace's members are not
/// typed.
pub(crate) fn member_answer(site: &Site, host: &mut impl MemberHost) -> Option<(Answer, bool)> {
    let Site::Member {
        receiver,
        scope,
        path,
        ..
    } = site
    else {
        return None;
    };
    let (root, rest) = match receiver {
        Receiver::Literal { kind } => {
            return Some((Answer::literal(*kind, host.registry_ready()), true));
        }
        Receiver::Expression => return None,
        Receiver::Rebound { name, rest } => (name.as_str(), rest.as_slice()),
        // A root that is no global, and that the last good parse does not
        // bind: bound in text that has not parsed yet (`no_types_yet`, E4),
        // or by nothing (`unknown_binding`). The answer is empty either way;
        // the lookup says which.
        Receiver::Names => {
            let (root, rest) = path.as_deref()?.split_first()?;
            if host.is_global(root) {
                return None;
            }
            (root.as_str(), rest)
        }
    };
    let shadowed = host.shadowed(root)?;
    let (types, stale, registry_ready) = host.types();
    let lookup = Lookup {
        root,
        rest,
        scope,
        shadowed,
        stale,
        registry_ready,
    };
    Some((Answer::of(types.as_deref(), &lookup), false))
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
        // The reason is the found binding's own: a parameter `R` shadowing a
        // conditionally declared module `R` has none.
        let (binding, root_ty, source, root_approximation) =
            match find(types, lookup.root, lookup.scope) {
                Found::Def(DefBinding {
                    kind: BindingKind::Comprehension,
                    ..
                }) => {
                    return Answer::empty("comprehension", "ambiguous_binding", lookup);
                }
                Found::Def(DefBinding {
                    kind: BindingKind::Lambda,
                    ..
                }) => {
                    return Answer::empty("lambda", "ambiguous_binding", lookup);
                }
                Found::Ambiguous(kind) => {
                    return Answer::empty(kind.as_str(), "ambiguous_binding", lookup);
                }
                Found::Missing => return Answer::empty("none", "unknown_binding", lookup),
                Found::Def(b) => (b.kind.as_str(), b.ty.clone(), None, b.approximation),
                Found::Module(m) => match &m.load {
                    Some(load) => ("loaded", m.ty.clone(), Some(load.clone()), m.approximation),
                    None => (
                        "module",
                        m.ty.clone(),
                        m.declaration.clone(),
                        m.approximation,
                    ),
                },
            };
        // `l.mode.│`: each segment is a member of the type before it.
        // An open type's attribute is unknown (`Any`); a type that lists its
        // members and not the segment is not a receiver the checker accepts.
        let mut ty = root_ty;
        for seg in lookup.rest {
            let listed = members(&ty);
            let open = listed.is_empty()
                && matches!(
                    empty_because(&ty),
                    "any" | "open_type" | "members_not_listed"
                );
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
        // A chain through an approximated root stays `Any` for its reason.
        let approximation = root_approximation
            .filter(|_| lookup.rest.is_empty() || ty == Ty::any())
            .map(|a| a.as_str().to_owned());
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
            json!(if self.registry_ready {
                "ready"
            } else {
                "pending"
            }),
        );
        site.insert("empty_because".to_string(), json!(self.empty_because));
    }
}

/// Where a name resolved.
enum Found<'t> {
    Def(&'t DefBinding),
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
            [binding] => Found::Def(binding),
            [first, ..] => Found::Ambiguous(first.kind),
            [] => continue,
        };
    }
    types.module.get(name).map_or(Found::Missing, Found::Module)
}

/// Why a type that is not `Any` lists no members:
/// - `open_type` when it accepts any name;
/// - `members_not_listed` for a struct or a namespace, whose fields the
///   checker knows (and holds an access to) but completion does not list;
/// - `no_members` when it accepts none.
fn empty_because(ty: &Ty) -> &'static str {
    if *ty == Ty::any() {
        return "any";
    }
    let mut unlisted = false;
    let open = ty.iter_union().iter().any(|basic| match basic {
        TyBasic::Custom(_) if matches!(basic.as_name(), Some("struct" | "namespace")) => {
            unlisted = true;
            false
        }
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
    if open {
        "open_type"
    } else if unlisted {
        "members_not_listed"
    } else {
        "no_members"
    }
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
        let t = types(
            "n = typing\n\
             def f(i: int, c: typing.Callable, a):\n    pass\n",
        );
        let because = |name| answer(&t, name, &[], &["f"]).empty_because;
        assert_eq!(because("i"), Some("no_members"));
        assert_eq!(because("c"), Some("open_type"));
        assert_eq!(because("a"), Some("any"));
        // A namespace is closed, but its fields are not listed. (heph's
        // globals have no starlark `struct`; a struct classifies the same.)
        let a = answer(&t, "n", &[], &["f"]);
        assert_eq!(a.empty_because, Some("members_not_listed"), "{a:?}");
        // A field of one is still a receiver: the chain folds through it.
        let a = answer(&t, "n", &["Any"], &["f"]);
        assert_ne!(a.empty_because, Some("unknown_binding"), "{a:?}");
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
