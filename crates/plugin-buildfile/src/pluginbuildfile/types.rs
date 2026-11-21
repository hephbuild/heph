//! `heph.types.*`: type aliases for `target()`'s fields, so a wrapper `def`
//! types the parameters it passes through (`deps: heph.types.Deps = None`)
//! instead of falling back to `**kwargs`.
//!
//! **The contract.** `heph.types.X` means what `target()` accepts for the field
//! it is built from, at this heph version, **`None` included**: every field is
//! optional, so `def f(deps: heph.types.Deps = None)` must evaluate. An alias
//! widens with its field and never narrows; a name, once shipped, is removed
//! only through a deprecation. Aliases are built from [`target_base_fields`]
//! only: one built from a driver's schema would make a lint verdict depend on
//! that driver, and so have to enter the lint key.
//!
//! Only fields whose schema a Starlark type expresses exactly get an alias. A
//! struct-shaped field (`transitive`, `approval`) does not: Starlark has no
//! fixed-key dict type, so its alias would accept keys `target()` refuses.
//!
//! Each alias is a small [`StarlarkValue`] wrapping a [`Ty`], whose
//! `eval_type()` returns it: the evaluator checks it at runtime and the AST
//! typechecker statically, from the same value.

use crate::pluginbuildfile::run_file::{param_type_to_ty, target_base_fields};
use allocative::Allocative;
use hcore::htvalue::signature::ParamType;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::typing::Ty;
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};
use std::sync::LazyLock;

/// The `heph.<name>` namespace the aliases live in.
pub(crate) const NAMESPACE: &str = "types";

/// One shipped alias: its name and where in `target()`'s schema it comes from.
struct AliasDef {
    name: &'static str,
    /// The field, then the struct field within it (`["transitive", "deps"]`).
    path: &'static [&'static str],
    /// Take the bare-string member of the field's union rather than the whole
    /// field: one address out of `deps`.
    single: bool,
    doc: &'static str,
}

/// Sorted by name; `types::alias_names_snapshot` pins the list.
const DEFS: &[AliasDef] = &[
    AliasDef {
        name: "Addr",
        path: &["transitive", "deps"],
        single: true,
        doc: "One target address, `//pkg:name`: what `target()` returns, and one entry of \
              `deps` or `tools`.",
    },
    AliasDef {
        name: "Deps",
        path: &["transitive", "deps"],
        single: false,
        doc: "A `deps` value: an address, a list of them, or a `{group: addr | [addr]}` map.",
    },
    AliasDef {
        name: "Env",
        path: &["transitive", "env"],
        single: false,
        doc: "An `env` value: a `{name: value}` map of environment variables.",
    },
    AliasDef {
        name: "Labels",
        path: &["labels"],
        single: false,
        doc: "A `labels` value: a label, or a list of them.",
    },
    AliasDef {
        name: "PassEnv",
        path: &["transitive", "pass_env"],
        single: false,
        doc: "A `pass_env` value: a variable name, or a list of them.",
    },
    AliasDef {
        name: "RuntimePassEnv",
        path: &["transitive", "runtime_pass_env"],
        single: false,
        doc: "A `runtime_pass_env` value: a variable name, or a list of them.",
    },
    AliasDef {
        name: "Tools",
        path: &["transitive", "tools"],
        single: false,
        doc: "A `tools` value: an address, a list of them, or a `{group: addr | [addr]}` map.",
    },
];

/// A `heph.types.<name>` alias, resolved against the current `target()` schema.
#[derive(Debug)]
pub(crate) struct Alias {
    pub name: &'static str,
    /// What the alias accepts, as a Starlark type: [`alias_param`] converted.
    pub ty: Ty,
    pub doc: &'static str,
}

impl Alias {
    /// The Starlark rendering of what the alias stands for, e.g.
    /// `list[str] | str | None`.
    pub(crate) fn expansion(&self) -> String {
        self.ty.to_string()
    }

    /// Hover / completion markdown: the alias, its expansion and its doc.
    pub(crate) fn markdown(&self) -> String {
        format!(
            "**`heph.{NAMESPACE}.{}`** = `{}`\n\n{}",
            self.name,
            self.expansion(),
            self.doc
        )
    }
}

/// The type of `target()`'s field at `path`, or `None` when the schema no
/// longer has it (the snapshot test then fails on the missing alias).
fn field_type(path: &[&str], single: bool) -> Option<ParamType> {
    let (first, rest) = path.split_first()?;
    let mut ty = target_base_fields()
        .into_iter()
        .find(|f| f.name == *first)?
        .ty;
    for name in rest {
        let ParamType::Struct(fields) = ty else {
            return None;
        };
        ty = fields.into_iter().find(|f| f.name == *name)?.ty;
    }
    if single {
        let members = match ty {
            ParamType::Union(members) => members,
            other => vec![other],
        };
        ty = members.into_iter().find(|t| *t == ParamType::String)?;
    }
    Some(ty)
}

/// What the alias `d` accepts, as the field's schema says it: the field's type
/// or `None`.
fn alias_param(d: &AliasDef) -> Option<ParamType> {
    let field = field_type(d.path, d.single)?;
    Some(ParamType::union(vec![field, ParamType::Null]))
}

static ALIASES: LazyLock<Vec<Alias>> = LazyLock::new(|| {
    DEFS.iter()
        .filter_map(|d| {
            Some(Alias {
                name: d.name,
                ty: param_type_to_ty(&alias_param(d)?),
                doc: d.doc,
            })
        })
        .collect()
});

/// Every alias, sorted by name.
pub(crate) fn aliases() -> &'static [Alias] {
    &ALIASES
}

/// The alias named `name`.
pub(crate) fn get(name: &str) -> Option<&'static Alias> {
    aliases().iter().find(|a| a.name == name)
}

/// The alias for a parameter named like a `target()` field (`deps`, `labels`,
/// `pass_env`, …): the one built from that whole field. The annotation check
/// suggests it on an unannotated parameter of that name.
pub(crate) fn for_param(param: &str) -> Option<&'static Alias> {
    let def = DEFS
        .iter()
        .find(|d| !d.single && d.path.last() == Some(&param))?;
    get(def.name)
}

/// Why `heph.types.<name>` names nothing, naming the aliases that exist; `None`
/// when it is an alias.
pub(crate) fn unknown_alias(name: &str) -> Option<String> {
    if get(name).is_some() {
        return None;
    }
    let known: Vec<&str> = aliases().iter().map(|a| a.name).collect();
    let hint = crate::pluginbuildfile::run_file::closest(name, known.iter().copied())
        .map(|c| format!(" (did you mean \"{c}\"?)"))
        .unwrap_or_default();
    Some(format!(
        "no type alias named \"{name}\"{hint}; heph.{NAMESPACE} has: {}",
        known.join(", ")
    ))
}

/// Fill the `heph.types` namespace.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    for a in aliases() {
        builder.set(
            a.name,
            TypeAlias {
                name: a.name,
                ty: a.ty.clone(),
            },
        );
    }
}

/// The Starlark value behind `heph.types.<name>`: a type, usable wherever a
/// type is (an annotation, `isinstance`), evaluating to its [`Ty`].
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct TypeAlias {
    name: &'static str,
    ty: Ty,
}

impl std::fmt::Display for TypeAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "heph.{NAMESPACE}.{}", self.name)
    }
}

starlark::starlark_simple_value!(TypeAlias);

#[starlark_value(type = "type")]
impl<'v> StarlarkValue<'v> for TypeAlias {
    fn eval_type(&self) -> Option<Ty> {
        Some(self.ty.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::{build_globals, eval_source, starlark_to_rust};
    use hplugin::function::FunctionRegistry;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::typing::{AstModuleTypecheck, Interface};
    use std::collections::HashMap;

    fn globals() -> starlark::environment::Globals {
        build_globals(&FunctionRegistry::default())
    }

    fn parse(name: &str, src: &str) -> AstModule {
        AstModule::parse(name, src.to_string(), &Dialect::Extended).expect("parses")
    }

    /// The static typechecker's errors over `src`, given loaded interfaces.
    fn typecheck(src: &str, loads: &HashMap<String, Interface>) -> (Vec<String>, Interface) {
        let (errors, _, iface, _) = parse("BUILD", src).typecheck(&globals(), loads);
        (errors.iter().map(|e| format!("{e:#}")).collect(), iface)
    }

    /// Evaluate `src` with heph's globals (no `target()` calls), returning the
    /// error text.
    fn eval(src: &str) -> Result<(), String> {
        let globals = globals();
        Module::with_temp_heap(|module| {
            let mut ev = Evaluator::new(&module);
            ev.eval_module(parse("BUILD", src), &globals)
                .map(drop)
                .map_err(|e| format!("{e:#}"))
        })
    }

    #[test]
    fn alias_names_snapshot() {
        let names: Vec<&str> = aliases().iter().map(|a| a.name).collect();
        assert_eq!(
            names,
            [
                "Addr",
                "Deps",
                "Env",
                "Labels",
                "PassEnv",
                "RuntimePassEnv",
                "Tools"
            ],
            "adding an alias is an API addition; removing one needs a deprecation"
        );
        // Every definition resolved against the schema: none silently dropped.
        assert_eq!(names.len(), DEFS.len());
    }

    #[test]
    fn aliases_typecheck() {
        for a in aliases() {
            let name = a.name;
            let (errors, _) = typecheck(
                &format!(
                    "def f(x: heph.types.{name}):\n    pass\n\ndef ok():\n    f(None)\n\n\
                     def bad():\n    f(1.5)\n"
                ),
                &HashMap::new(),
            );
            assert_eq!(errors.len(), 1, "{name}: exactly the bad call: {errors:#?}");
            let e = errors.first().map(String::as_str).unwrap_or_default();
            assert!(e.contains("float"), "{name}: names the wrong type: {e}");
        }
        // A well-typed argument for a structured alias passes.
        let (errors, _) = typecheck(
            "def f(deps: heph.types.Deps, env: heph.types.Env):\n    pass\n\n\
             def ok():\n    f({\"g\": [\"//a:b\"]}, {\"K\": \"v\"})\n    f(\"//a:b\", {})\n",
            &HashMap::new(),
        );
        assert!(errors.is_empty(), "{errors:#?}");
        let (errors, _) = typecheck(
            "def f(env: heph.types.Env):\n    pass\n\ndef bad():\n    f({\"K\": 1})\n",
            &HashMap::new(),
        );
        assert_eq!(errors.len(), 1, "dict value type is checked: {errors:#?}");
    }

    /// The literals a value of `t` may be built from, one per shape.
    fn samples(t: &ParamType) -> Vec<String> {
        match t {
            ParamType::String => vec!["\"//a:b\"".into()],
            ParamType::Null => vec!["None".into()],
            ParamType::Bool => vec!["True".into()],
            ParamType::Int | ParamType::Uint => vec!["1".into()],
            ParamType::Float => vec!["1.5".into()],
            ParamType::Literal(l) => vec![format!("{l:?}")],
            ParamType::List(inner) => std::iter::once("[]".to_string())
                .chain(samples(inner).into_iter().map(|s| format!("[{s}]")))
                .collect(),
            ParamType::Map(inner) => std::iter::once("{}".to_string())
                .chain(
                    samples(inner)
                        .into_iter()
                        .map(|s| format!("{{\"k\": {s}}}")),
                )
                .collect(),
            ParamType::Union(members) => members.iter().flat_map(samples).collect(),
            ParamType::Struct(fields) => fields
                .iter()
                .flat_map(|f| {
                    samples(&f.ty)
                        .into_iter()
                        .map(move |s| format!("{{{:?}: {s}}}", f.name))
                })
                .collect(),
        }
    }

    /// Each alias accepts exactly what its field's schema accepts: every value
    /// built from the field's shape, every other alias's values, and a set of
    /// near misses, each judged by the schema (`ParamType::matches` over what
    /// `target()` converts the value to) and by the alias at runtime.
    #[test]
    fn alias_matches_target_field() {
        let near_misses = [
            "1",
            "1.5",
            "True",
            "[1]",
            "[None]",
            "[\"a\", 1]",
            "[[\"a\"]]",
            "(\"a\",)",
            "{\"k\": 1}",
            "{\"k\": None}",
            "{\"k\": [1]}",
            "{\"k\": {\"k\": \"a\"}}",
            "{1: \"a\"}",
            "{\"k\": [\"a\", None]}",
        ];
        let params: Vec<(&str, ParamType)> = DEFS
            .iter()
            .map(|d| (d.name, alias_param(d).expect("derives")))
            .collect();
        let mut candidates: Vec<String> = params.iter().flat_map(|(_, p)| samples(p)).collect();
        candidates.extend(near_misses.iter().map(|s| s.to_string()));
        candidates.sort();
        candidates.dedup();

        let globals = globals();
        for (name, param) in &params {
            for lit in &candidates {
                // What `target()` sees: the value converted, then the schema.
                let schema_accepts = Module::with_temp_heap(|module| {
                    let mut ev = Evaluator::new(&module);
                    let v = ev
                        .eval_module(parse("v", lit), &globals)
                        .map_err(|e| format!("{e:#}"))
                        .expect("literal evaluates");
                    starlark_to_rust(&v).is_ok_and(|v| param.matches(&v))
                });
                let alias_accepts = eval(&format!(
                    "def f(x: heph.types.{name} = None):\n    pass\nf({lit})\n"
                ))
                .is_ok();
                assert_eq!(
                    alias_accepts, schema_accepts,
                    "heph.types.{name} on {lit}: alias accepts {alias_accepts}, the field \
                     {schema_accepts}"
                );
            }
        }
    }

    fn source_loader() -> (
        tempfile::TempDir,
        crate::pluginbuildfile::run_file::BuildFileLoader,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let loader =
            crate::pluginbuildfile::run_file::tests::source_loader(tmp.path().to_path_buf());
        (tmp, loader)
    }

    #[test]
    fn aliases_evaluate() {
        let (_tmp, loader) = source_loader();
        for a in aliases() {
            let name = a.name;
            let src = format!(
                "def f(x: heph.types.{name} = None):\n    return x\n\nf()\nf(None)\n\
                 y = heph.types.{name}\n"
            );
            eval_source("BUILD", src, "pkg", &loader)
                .unwrap_or_else(|e| panic!("heph.types.{name}: {e:#}"));
        }
        // A wrapper typing its pass-through params, in a real BUILD file.
        let src = "\
def lib(name: str, deps: heph.types.Deps = None, labels: heph.types.Labels = None):
    return target(name = name, driver = \"exec\", labels = labels, transitive = {\"deps\": deps})

lib(\"a\")
lib(\"b\", deps = [\"//x:y\"], labels = \"l\")
";
        let result = eval_source("BUILD", src.to_string(), "pkg", &loader).expect("evaluates");
        assert_eq!(result.targets.len(), 2);
        // A wrong value fails at runtime, naming the parameter.
        let Err(err) = eval_source(
            "BUILD",
            "def f(x: heph.types.Labels = None):\n    pass\nf(1)\n".to_string(),
            "pkg",
            &loader,
        ) else {
            panic!("int is not a Labels");
        };
        assert!(format!("{err:#}").contains("`x`"), "{err:#}");
    }

    #[test]
    fn alias_across_load() {
        let (errors, lib) = typecheck(
            "def wrap(labels: heph.types.Labels = None):\n    pass\n",
            &HashMap::new(),
        );
        assert!(errors.is_empty(), "{errors:#?}");
        let loads = HashMap::from([("//lib/defs.star".to_string(), lib)]);
        let (errors, _) = typecheck(
            "load(\"//lib/defs.star\", \"wrap\")\n\n\
             def ok():\n    wrap([\"a\"])\n\ndef bad():\n    wrap({\"a\": 1})\n",
            &loads,
        );
        assert_eq!(
            errors.len(),
            1,
            "the bad call, through the Interface: {errors:#?}"
        );
    }

    #[test]
    fn unknown_alias_error() {
        let (_tmp, loader) = source_loader();
        for src in [
            "x = heph.types.Nope\n",
            "def f(x: heph.types.Nope = None):\n    pass\n",
        ] {
            let Err(err) = eval_source("BUILD", src.to_string(), "pkg", &loader) else {
                panic!("Nope is no alias: {src}");
            };
            let msg = format!("{err:#}");
            assert!(
                msg.contains(
                    "heph.types.Nope: no type alias named \"Nope\"; heph.types has: Addr, Deps, \
                     Env, Labels, PassEnv, RuntimePassEnv, Tools"
                ),
                "{msg}"
            );
            assert!(msg.contains("BUILD:1:"), "names the file and line: {msg}");
        }
        let msg = unknown_alias("Lables").expect("a typo");
        assert!(msg.contains("(did you mean \"Labels\"?)"), "{msg}");
        assert_eq!(unknown_alias("Labels"), None);
    }
}
