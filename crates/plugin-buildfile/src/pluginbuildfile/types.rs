//! `heph.types.*`: type aliases for `target()`'s fields, so a wrapper `def`
//! types the parameters it passes through (`deps: heph.types.Deps = None`)
//! instead of falling back to `**kwargs`.
//!
//! **The contract.** `heph.types.X` means what `target()` accepts for the field
//! it is built from, at this heph version, **`None` included**: every field is
//! optional, so `def f(deps: heph.types.Deps = None)` must evaluate. An alias
//! widens with its field and never narrows; a name, once shipped, is removed
//! only through a deprecation. The flat aliases are built from
//! [`target_base_fields`] only.
//!
//! **Per-driver `Kwargs`.** `heph.types.<driver>.Kwargs` (`heph.types.bash.Kwargs`,
//! `heph.types.oci.image.Kwargs`) types a wrapper's `**kwargs` per key, closed:
//! one key per `target()` field but `driver`, plus the driver's own schema
//! fields ([`DriverKwargs`]). Built from the registered drivers' schemas, so a
//! lint verdict depends on them: [`DriverKwargs::digest`] enters the lint key,
//! and nothing about them reaches any other target's def.
//!
//! Only fields whose schema a Starlark type expresses exactly get an alias. A
//! struct-shaped field (`transitive`, `approval`) does not: Starlark has no
//! fixed-key dict type, so its alias would accept keys `target()` refuses.
//!
//! One alias is not built from a field: `heph.types.Number` is `int | float`,
//! the type a float default (`scale = 1.0`) gives an unannotated parameter. It
//! does not include `None`, and no parameter name suggests it.
//!
//! Each alias is a small [`StarlarkValue`] wrapping a [`Ty`], whose
//! `eval_type()` returns it: the evaluator checks it at runtime and the AST
//! typechecker statically, from the same value.

use crate::pluginbuildfile::run_file::{closest, param_type_to_ty, target_base_fields};
use allocative::Allocative;
use hcore::htvalue::signature::ParamType;
use hplugin::driver::DriverSchemas;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::typing::{Ty, TyTypedKwargs};
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, OnceLock};

/// The `heph.<name>` namespace the aliases live in.
pub(crate) const NAMESPACE: &str = "types";

/// One shipped alias: its name and what it stands for.
struct AliasDef {
    name: &'static str,
    source: Source,
    doc: &'static str,
}

/// What an alias stands for.
enum Source {
    /// A field of `target()`'s schema.
    Field {
        /// The field, then the struct field within it (`["transitive", "deps"]`).
        path: &'static [&'static str],
    },
    /// A fixed type, not a field: no `None`, and no parameter name suggests it.
    Fixed(fn() -> Ty),
}

const fn field(path: &'static [&'static str]) -> Source {
    Source::Field { path }
}

/// Sorted by name; `types::alias_names_snapshot` pins the list.
const DEFS: &[AliasDef] = &[
    AliasDef {
        name: "Deps",
        source: field(&["transitive", "deps"]),
        doc: "A `deps` value: an address, a list of them, or a `{group: addr | [addr]}` map.",
    },
    AliasDef {
        name: "Env",
        source: field(&["transitive", "env"]),
        doc: "An `env` value: a `{name: value}` map of environment variables.",
    },
    AliasDef {
        name: "Labels",
        source: field(&["labels"]),
        doc: "A `labels` value: a label, or a list of them.",
    },
    AliasDef {
        name: "Number",
        source: Source::Fixed(|| Ty::union2(Ty::int(), Ty::float())),
        doc: "A number: an `int` or a `float`. The type a float default (`scale = 1.0`) gives \
              an unannotated parameter. `None` is not a `Number`.",
    },
    AliasDef {
        name: "PassEnv",
        source: field(&["transitive", "pass_env"]),
        doc: "A `pass_env` value: a variable name, or a list of them.",
    },
    AliasDef {
        name: "RuntimePassEnv",
        source: field(&["transitive", "runtime_pass_env"]),
        doc: "A `runtime_pass_env` value: a variable name, or a list of them.",
    },
    AliasDef {
        name: "Tools",
        source: field(&["transitive", "tools"]),
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
fn field_type(path: &[&str]) -> Option<ParamType> {
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
    Some(ty)
}

/// What a field alias accepts, as the field's schema says it: the field's type
/// or `None`.
fn field_param(path: &[&str]) -> Option<ParamType> {
    let field = field_type(path)?;
    Some(ParamType::union(vec![field, ParamType::Null]))
}

/// What the alias `d` accepts, as a Starlark type.
fn alias_ty(d: &AliasDef) -> Option<Ty> {
    match &d.source {
        Source::Field { path } => Some(param_type_to_ty(&field_param(path)?)),
        Source::Fixed(ty) => Some(ty()),
    }
}

static ALIASES: LazyLock<Vec<Alias>> = LazyLock::new(|| {
    DEFS.iter()
        .filter_map(|d| {
            Some(Alias {
                name: d.name,
                ty: alias_ty(d)?,
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
    let def = DEFS.iter().find(|d| match &d.source {
        Source::Field { path } => path.last() == Some(&param),
        Source::Fixed(_) => false,
    })?;
    get(def.name)
}

/// The member of a driver's namespace that is its kwargs type:
/// `heph.types.<driver>.Kwargs`.
pub(crate) const KWARGS: &str = "Kwargs";

/// One driver's `heph.types.<driver>.Kwargs`.
#[derive(Debug)]
pub(crate) struct Kwargs {
    /// The driver's registry name, as `target(driver = …)` writes it.
    pub driver: String,
    /// Each key and its type, sorted by key.
    pub keys: Vec<(String, Ty)>,
    /// `keys` as one closed typed-kwargs type, every key optional.
    pub ty: Ty,
    /// The keys the driver's schema declares `required`, sorted. Display only
    /// (the hover marks them): never in [`Self::ty`] or the digest.
    pub required: Vec<String>,
}

impl Kwargs {
    /// The driver's name, by segment.
    fn segments(&self) -> impl Iterator<Item = &str> {
        self.driver.split('.')
    }

    /// Hover / completion markdown: the type, what it is for, and each key
    /// with its type, marked when the driver requires it.
    pub(crate) fn markdown(&self) -> String {
        let mut md = format!(
            "**`heph.{NAMESPACE}.{}.{KWARGS}`**\n\n\
             The keyword arguments `target(driver = \"{}\")` takes, for a wrapper's \
             `**kwargs`. Closed: any other key is a type error. Every key is optional \
             and accepts `None`.\n",
            self.driver, self.driver
        );
        for (key, ty) in &self.keys {
            md.push_str(&format!("\n- `{key}`: `{ty}`"));
            if self.required.binary_search(key).is_ok() {
                md.push_str(" (required by the driver)");
            }
        }
        md
    }
}

/// Every registered driver's `heph.types.<driver>.Kwargs`, built once from
/// the engine's sealed driver schemas.
///
/// A key is each `target()` field but `driver` (the alias fixes it), plus
/// each driver field not named like one (`target()` consumes those). Its type
/// is the field's ([`key_ty`]), or `None`: never narrower than what the field
/// accepts, so lint may miss what evaluation catches, never the reverse.
#[derive(Debug)]
pub(crate) struct DriverKwargs {
    /// Sorted by driver name.
    drivers: Vec<Kwargs>,
    /// [`Self::digest`], built on first use.
    digest: OnceLock<String>,
}

/// Why the driver `name` cannot be written as an attribute path under
/// `heph.types`, or `None` when it can: an empty segment, a `Kwargs` segment,
/// or a first segment that is a flat alias (`heph.types.Deps`), which keeps
/// its meaning.
fn unwritable(name: &str) -> Option<&'static str> {
    for (i, s) in name.split('.').enumerate() {
        if s.is_empty() {
            return Some("an empty name segment");
        }
        if s == KWARGS {
            return Some("a `Kwargs` name segment");
        }
        if i == 0 && get(s).is_some() {
            return Some("a first segment named like a heph.types alias");
        }
    }
    None
}

impl Default for DriverKwargs {
    /// No driver: every `heph.types.<driver>` fails as unknown.
    fn default() -> Self {
        Self::from_schemas(&DriverSchemas::new())
    }
}

impl DriverKwargs {
    /// The `Kwargs` of every driver in `schemas`.
    ///
    /// A driver whose name cannot be written as an attribute path under
    /// `heph.types` is left out, with a warning ([`unwritable`]). Registry
    /// names follow the engine's `PLUGIN_NAME_RULE`, so none is in practice.
    pub(crate) fn from_schemas(schemas: &DriverSchemas) -> Self {
        let base = target_base_fields();
        let base_keys: Vec<(String, Ty)> = base
            .iter()
            .filter(|f| f.name != "driver")
            .map(|f| (f.name.clone(), key_ty(&f.ty)))
            .collect();
        let drivers: Vec<Kwargs> = schemas
            .iter()
            .filter(|(driver, _)| match unwritable(driver) {
                None => true,
                Some(why) => {
                    tracing::warn!(
                        driver = driver.as_str(),
                        reason = why,
                        "driver has no heph.types.<driver>.Kwargs: its name is not an \
                         attribute path under heph.types"
                    );
                    false
                }
            })
            .map(|(driver, schema)| {
                let mut keys: BTreeMap<String, Ty> = base_keys.iter().cloned().collect();
                for f in &schema.fields {
                    if !base.iter().any(|b| b.name == f.name) {
                        keys.insert(f.name.clone(), key_ty(&f.ty));
                    }
                }
                let keys: Vec<(String, Ty)> = keys.into_iter().collect();
                // A required field named like a `target()` field is still a
                // key the driver needs; only `driver` is not a key.
                let mut required: Vec<String> = schema
                    .fields
                    .iter()
                    .filter(|f| f.required && f.name != "driver")
                    .map(|f| f.name.clone())
                    .collect();
                required.sort_unstable();
                required.dedup();
                Kwargs {
                    driver: driver.clone(),
                    ty: TyTypedKwargs::new(keys.iter().cloned()),
                    keys,
                    required,
                }
            })
            .collect();
        Self {
            drivers,
            digest: OnceLock::new(),
        }
    }

    /// No driver, shared: what a loader built without the engine's drivers sees.
    pub(crate) fn empty() -> Arc<Self> {
        static EMPTY: LazyLock<Arc<DriverKwargs>> = LazyLock::new(Arc::default);
        Arc::clone(&EMPTY)
    }

    /// The lint key's `driverKwargsDigest`: every `Kwargs` the checker sees,
    /// by driver name, then each key and its Starlark type. Only what reaches
    /// the checker: a schema's docs or `required` never move it.
    /// Computed on the first call: only the lint check's spec and run read it.
    pub(crate) fn digest(&self) -> &str {
        self.digest.get_or_init(|| kwargs_digest(&self.drivers))
    }

    /// The `Kwargs` of the driver whose name is `segments` joined by dots.
    pub(crate) fn at(&self, segments: &[&str]) -> Option<&Kwargs> {
        self.drivers
            .iter()
            .find(|k| k.segments().eq(segments.iter().copied()))
    }

    /// The driver namespaces directly under `heph.types.<prefix>`: the next
    /// segment of every driver name that continues `prefix`, sorted.
    pub(crate) fn children(&self, prefix: &[&str]) -> BTreeSet<&str> {
        self.drivers
            .iter()
            .filter(|k| k.segments().take(prefix.len()).eq(prefix.iter().copied()))
            .filter_map(|k| k.segments().nth(prefix.len()))
            .collect()
    }

    /// Fill `heph.types` with one nested namespace per driver name segment,
    /// holding `Kwargs` where a driver's name ends.
    fn register(&self, builder: &mut GlobalsBuilder) {
        #[derive(Default)]
        struct Node<'a> {
            kwargs: Option<&'a Kwargs>,
            children: BTreeMap<&'a str, Node<'a>>,
        }
        fn emit(b: &mut GlobalsBuilder, node: &Node<'_>) {
            if let Some(k) = node.kwargs {
                b.set(
                    KWARGS,
                    TypeAlias {
                        name: format!("{}.{KWARGS}", k.driver),
                        ty: k.ty.clone(),
                    },
                );
            }
            for (seg, child) in &node.children {
                b.namespace(seg, |nb| emit(nb, child));
            }
        }
        let mut root = Node::default();
        for k in &self.drivers {
            let node = k
                .segments()
                .fold(&mut root, |n, seg| n.children.entry(seg).or_default());
            node.kwargs = Some(k);
        }
        emit(builder, &root);
    }

    /// Why `heph.types.<path>` names nothing, naming what exists where it
    /// stops; `None` when it is an alias, a `Kwargs`, a driver namespace, or
    /// an attribute of one of those (evaluation's to judge).
    pub(crate) fn unknown_member(&self, path: &[&str]) -> Option<String> {
        let is_driver = |p: &[&str]| self.at(p).is_some();
        let is_namespace = |p: &[&str]| {
            self.drivers
                .iter()
                .any(|k| k.segments().take(p.len()).eq(p.iter().copied()))
        };
        for (i, &name) in path.iter().enumerate() {
            let prefix = path.get(..i).unwrap_or_default();
            let alias = match prefix {
                [] => get(name).is_some(),
                _ => name == KWARGS && is_driver(prefix),
            };
            if alias {
                return None;
            }
            if !is_namespace(path.get(..=i).unwrap_or_default()) {
                return Some(self.not_a_member(prefix, name, path.len() == 1));
            }
        }
        None
    }

    /// `name` is not a member of `heph.types.<prefix>`. `alone` when the path
    /// is `heph.types.<name>`, read as a flat alias rather than a driver.
    fn not_a_member(&self, prefix: &[&str], name: &str, alone: bool) -> String {
        let depth = prefix.len();
        let next = self.children(prefix);
        let hint = |candidates: &[&str]| {
            closest(name, candidates.iter().copied())
                .map(|c| format!(" (did you mean \"{c}\"?)"))
                .unwrap_or_default()
        };
        if depth == 0 {
            let aliases: Vec<&str> = aliases().iter().map(|a| a.name).collect();
            let roots: Vec<&str> = next.into_iter().collect();
            if alone {
                let mut candidates = aliases.clone();
                candidates.extend(&roots);
                let drivers = if roots.is_empty() {
                    String::new()
                } else {
                    format!("; driver namespaces: {}", roots.join(", "))
                };
                return format!(
                    "no type alias named \"{name}\"{}; heph.{NAMESPACE} has: {}{drivers}",
                    hint(&candidates),
                    aliases.join(", ")
                );
            }
            let drivers: Vec<&str> = self.drivers.iter().map(|k| k.driver.as_str()).collect();
            return format!(
                "no driver named \"{name}\"{}; heph.{NAMESPACE}.<driver>.{KWARGS} exists for: {}; \
                 drivers come from the plugins listed in .hephconfig (`plugins:`): is \"{name}\" \
                 named there?",
                hint(&roots),
                if drivers.is_empty() {
                    "(no driver is registered)".to_string()
                } else {
                    drivers.join(", ")
                }
            );
        }
        let mut members: Vec<&str> = Vec::with_capacity(next.len() + 1);
        if self.at(prefix).is_some() {
            members.push(KWARGS);
        }
        members.extend(next);
        format!(
            "heph.{NAMESPACE}.{} has no member \"{name}\"{}; it has: {}",
            prefix.join("."),
            hint(&members),
            members.join(", ")
        )
    }
}

/// The type of a `Kwargs` key for a field of type `t`: the field's type or
/// `None`. A struct-shaped field (`transitive`, `approval`) is a
/// `dict[str, typing.Any]`, at any depth: no Starlark type has fixed keys, and
/// the key must accept everything the field does. A float is `int | float`, at
/// any depth: heph takes an int where a float is expected (`heph.types.Number`).
fn key_ty(t: &ParamType) -> Ty {
    fn field(t: &ParamType) -> Ty {
        match t {
            ParamType::Struct(_) => Ty::dict(Ty::string(), Ty::any()),
            ParamType::Float => Ty::union2(Ty::int(), Ty::float()),
            ParamType::Union(members) => Ty::unions(members.iter().map(field).collect()),
            ParamType::List(inner) => Ty::list(field(inner)),
            ParamType::Map(value) => Ty::dict(Ty::string(), field(value)),
            leaf => param_type_to_ty(leaf),
        }
    }
    Ty::union2(field(t), Ty::none())
}

fn kwargs_digest(drivers: &[Kwargs]) -> String {
    use std::hash::Hash;
    let mut h = xxhash_rust::xxh3::Xxh3Default::new();
    drivers.len().hash(&mut h);
    for k in drivers {
        k.driver.hash(&mut h);
        k.keys.len().hash(&mut h);
        for (key, ty) in &k.keys {
            key.hash(&mut h);
            ty.to_string().hash(&mut h);
        }
    }
    format!("{:032x}", h.digest128())
}

/// Fill the `heph.types` namespace: the flat aliases, then one namespace per
/// driver holding its `Kwargs`.
pub(crate) fn register(builder: &mut GlobalsBuilder, kwargs: &DriverKwargs) {
    for a in aliases() {
        builder.set(
            a.name,
            TypeAlias {
                name: a.name.to_string(),
                ty: a.ty.clone(),
            },
        );
    }
    kwargs.register(builder);
}

/// The Starlark value behind `heph.types.<name>`: a type, usable wherever a
/// type is (an annotation, `isinstance`), evaluating to its [`Ty`].
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct TypeAlias {
    /// The path under `heph.types`: `Deps`, `oci.image.Kwargs`.
    name: String,
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
pub(crate) mod tests {
    use super::*;
    use crate::pluginbuildfile::run_file::{build_globals, eval_source, starlark_to_rust};
    use hplugin::function::FunctionRegistry;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::typing::{AstModuleTypecheck, Interface};
    use std::collections::HashMap;

    fn globals() -> starlark::environment::Globals {
        build_globals(&FunctionRegistry::default(), &DriverKwargs::default())
    }

    /// The drivers the tests register: plain, dotted, a namespace that is
    /// also a driver (`oci` and `oci.image`), a field named like a `target()`
    /// field, a struct field, and the buildfile plugin's own check drivers.
    pub(crate) fn test_schemas() -> DriverSchemas {
        use crate::pluginbuildfile::checks::drivers::CheckDriver;
        use crate::pluginbuildfile::checks::targets::CheckKind;
        use hcore::htvalue::signature::StructField;
        use hdriver_support::driver_managed::ManagedDriver;
        use hplugin::driver::{DriverField, DriverSchema};
        let f = |name: &str, ty: ParamType| DriverField {
            name: name.to_string(),
            ty,
            doc: format!("{name}'s doc"),
            required: false,
        };
        let schema = |fields| DriverSchema {
            fields,
            accepts_deferred: false,
        };
        let mut s = DriverSchemas::from([
            (
                "bash".to_string(),
                schema(vec![
                    f(
                        "run",
                        ParamType::union(vec![
                            ParamType::String,
                            ParamType::list(ParamType::String),
                        ]),
                    ),
                    f("env", ParamType::map(ParamType::String)),
                    f("cache", ParamType::Bool),
                    // `target()` consumes `labels`: its type wins.
                    f("labels", ParamType::Int),
                ]),
            ),
            ("oci".to_string(), schema(vec![f("push", ParamType::Bool)])),
            (
                "oci.image".to_string(),
                schema(vec![
                    f("base", ParamType::String),
                    f("scale", ParamType::Float),
                    f("count", ParamType::Uint),
                    f("os", ParamType::literal("linux")),
                    f(
                        "config",
                        ParamType::Struct(vec![StructField {
                            name: "entrypoint".to_string(),
                            ty: ParamType::list(ParamType::String),
                        }]),
                    ),
                ]),
            ),
        ]);
        for k in CheckKind::ALL {
            let d = CheckDriver::new(
                k,
                crate::pluginbuildfile::provider::FunctionSource::default(),
                crate::pluginbuildfile::provider::DriverSource::default(),
            );
            s.insert(k.driver(), d.schema());
        }
        s
    }

    /// [`test_schemas`]' kwargs, built once.
    pub(crate) fn test_kwargs() -> Arc<DriverKwargs> {
        static KWARGS: LazyLock<Arc<DriverKwargs>> =
            LazyLock::new(|| Arc::new(DriverKwargs::from_schemas(&test_schemas())));
        Arc::clone(&KWARGS)
    }

    fn kwargs_loader() -> (
        tempfile::TempDir,
        crate::pluginbuildfile::run_file::BuildFileLoader,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let loader = crate::pluginbuildfile::run_file::tests::source_loader_with(
            tmp.path().to_path_buf(),
            test_kwargs(),
        );
        (tmp, loader)
    }

    /// The field each key is built from, by driver: `target()`'s, else the
    /// driver's.
    fn key_fields(driver: &str) -> BTreeMap<String, ParamType> {
        key_fields_in(&test_schemas(), driver)
    }

    /// [`key_fields`], for `driver` in `schemas`.
    fn key_fields_in(schemas: &DriverSchemas, driver: &str) -> BTreeMap<String, ParamType> {
        let base = target_base_fields();
        let mut fields: BTreeMap<String, ParamType> = schemas[driver]
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();
        fields.extend(
            base.into_iter()
                .filter(|f| f.name != "driver")
                .map(|f| (f.name, f.ty)),
        );
        fields
    }

    #[test]
    fn kwargs_alias_keys_match_schema() {
        let kwargs = test_kwargs();
        let schemas = test_schemas();
        let drivers: Vec<&str> = kwargs.drivers.iter().map(|k| k.driver.as_str()).collect();
        let expected: Vec<&str> = schemas.keys().map(String::as_str).collect();
        assert_eq!(drivers, expected, "one Kwargs per driver, sorted");
        for k in &kwargs.drivers {
            let fields = key_fields(&k.driver);
            let keys: Vec<&str> = k.keys.iter().map(|(n, _)| n.as_str()).collect();
            let want: Vec<&str> = fields.keys().map(String::as_str).collect();
            assert_eq!(
                keys, want,
                "{}: target fields - driver + driver fields",
                k.driver
            );
            assert!(!keys.contains(&"driver"), "{}", k.driver);
            for (name, ty) in &k.keys {
                assert_eq!(ty, &key_ty(&fields[name]), "{}.{name}", k.driver);
            }
            assert_eq!(
                k.ty,
                TyTypedKwargs::new(k.keys.iter().cloned()),
                "{}",
                k.driver
            );
        }
        // `labels` is target()'s, not bash's `int`; a struct field is a dict.
        let bash = &kwargs.drivers[0];
        let labels = bash
            .keys
            .iter()
            .find(|(n, _)| n == "labels")
            .expect("labels");
        assert_eq!(labels.1.to_string(), "None | str | list[str]");
        let image = kwargs
            .drivers
            .iter()
            .find(|k| k.driver == "oci.image")
            .expect("oci.image");
        let config = image
            .keys
            .iter()
            .find(|(n, _)| n == "config")
            .expect("config");
        assert_eq!(config.1.to_string(), "None | dict[str, typing.Any]");
        // Each resolves under heph.types, dots included.
        let (_tmp, loader) = kwargs_loader();
        let src = "a = heph.types.bash.Kwargs\nb = heph.types.oci.Kwargs\n\
                   c = heph.types.oci.image.Kwargs\nd = heph.types.buildfile.lint_check.Kwargs\n";
        eval_source("BUILD", src.to_string(), "pkg", &loader).expect("evaluates");
    }

    /// What heph accepts for a field of type `t`: the schema, with an int
    /// wherever a float is expected (`value_matches` in the LSP's diagnostics,
    /// `heph.types.Number`).
    fn heph_accepts(t: &ParamType) -> ParamType {
        match t {
            ParamType::Float => ParamType::union(vec![ParamType::Int, ParamType::Float]),
            ParamType::List(inner) => ParamType::list(heph_accepts(inner)),
            ParamType::Map(value) => ParamType::map(heph_accepts(value)),
            ParamType::Union(members) => {
                ParamType::union(members.iter().map(heph_accepts).collect())
            }
            ParamType::Struct(fields) => ParamType::Struct(
                fields
                    .iter()
                    .map(|f| hcore::htvalue::signature::StructField {
                        name: f.name.clone(),
                        ty: heph_accepts(&f.ty),
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Every value a field accepts passes its key, at runtime: a key is never
    /// narrower than its field (I3).
    #[test]
    fn kwargs_key_never_narrower_than_field() {
        let kwargs = test_kwargs();
        assert_never_narrower(&kwargs, &test_schemas());
    }

    /// [`kwargs_key_never_narrower_than_field`] over `kwargs`, built from
    /// `schemas`.
    fn assert_never_narrower(kwargs: &DriverKwargs, schemas: &DriverSchemas) {
        let near_misses = [
            "1",
            "1.5",
            "True",
            "[1]",
            "[1, 1.5]",
            "{\"k\": 1}",
            "\"s\"",
            "(\"a\",)",
        ];
        let globals = build_globals(&FunctionRegistry::default(), kwargs);
        let eval_with = |src: &str| {
            Module::with_temp_heap(|module| {
                let mut ev = Evaluator::new(&module);
                ev.eval_module(parse("BUILD", src), &globals)
                    .map(drop)
                    .map_err(|e| format!("{e:#}"))
            })
        };
        for k in &kwargs.drivers {
            for (name, field) in key_fields_in(schemas, &k.driver) {
                let field = heph_accepts(&field);
                let mut candidates = samples(&field);
                candidates.extend(near_misses.iter().map(|s| s.to_string()));
                candidates.push("None".to_string());
                for lit in candidates {
                    let field_accepts = Module::with_temp_heap(|module| {
                        let mut ev = Evaluator::new(&module);
                        let v = ev
                            .eval_module(parse("v", &lit), &globals)
                            .map_err(|e| format!("{e:#}"))
                            .expect("literal evaluates");
                        starlark_to_rust(&v).is_ok_and(|v| field.matches(&v))
                    });
                    if !field_accepts {
                        continue;
                    }
                    let src = format!(
                        "def f(**kwargs: heph.types.{}.Kwargs):\n    pass\nf({name} = {lit})\n",
                        k.driver
                    );
                    if let Err(e) = eval_with(&src) {
                        panic!(
                            "{}.{name} refuses {lit}, which the field accepts: {e}",
                            k.driver
                        );
                    }
                }
                let src = format!(
                    "def f(**kwargs: heph.types.{}.Kwargs):\n    pass\nf({name} = None)\n",
                    k.driver
                );
                eval_with(&src).unwrap_or_else(|e| panic!("{}.{name} = None: {e}", k.driver));
            }
        }
    }

    #[test]
    fn unknown_driver_kwargs() {
        let (_tmp, loader) = kwargs_loader();
        let fails = |src: &str| match eval_source("BUILD", src.to_string(), "pkg", &loader) {
            Ok(_) => panic!("evaluated: {src}"),
            Err(e) => format!("{e:#}"),
        };
        let msg = fails("x = heph.types.nope.Kwargs\n");
        assert!(
            msg.contains(
                "BUILD:1:5-27: heph.types.nope.Kwargs: no driver named \"nope\"; \
                 heph.types.<driver>.Kwargs exists for: bash, buildfile.format, \
                 buildfile.format_check, buildfile.lint_check, oci, oci.image; drivers come \
                 from the plugins listed in .hephconfig (`plugins:`): is \"nope\" named there?"
            ),
            "{msg}"
        );
        // A namespace with no driver at that path has no `Kwargs`.
        let msg = fails("x = heph.types.buildfile.Kwargs\n");
        assert!(
            msg.contains(
                "heph.types.buildfile.Kwargs: heph.types.buildfile has no member \"Kwargs\"; \
                 it has: format, format_check, lint_check"
            ),
            "{msg}"
        );
        let msg = fails("def f(**kwargs: heph.types.bash.Nope):\n    pass\n");
        assert!(
            msg.contains(
                "heph.types.bash.Nope: heph.types.bash has no member \"Nope\"; it has: Kwargs"
            ),
            "{msg}"
        );
        let msg = fails("x = heph.types.oci.imag.Kwargs\n");
        assert!(
            msg.contains(
                "heph.types.oci.imag.Kwargs: heph.types.oci has no member \"imag\" \
                 (did you mean \"image\"?); it has: Kwargs, image"
            ),
            "{msg}"
        );
        let msg = fails("x = heph.types.Nope\n");
        assert!(
            msg.contains(
                "heph.types.Nope: no type alias named \"Nope\"; heph.types has: Deps, Env, \
                 Labels, Number, PassEnv, RuntimePassEnv, Tools; driver namespaces: bash, \
                 buildfile, oci"
            ),
            "{msg}"
        );
        // A driver namespace is a value; an attribute of a type is evaluation's.
        let src = "x = heph.types.oci\ny = heph.types.oci.image\n";
        eval_source("BUILD", src.to_string(), "pkg", &loader).expect("namespaces evaluate");
        // No driver at all: the same hint.
        let msg = DriverKwargs::default()
            .unknown_member(&["nope", "Kwargs"])
            .expect("unknown");
        assert_eq!(
            msg,
            "no driver named \"nope\"; heph.types.<driver>.Kwargs exists for: (no driver is \
             registered); drivers come from the plugins listed in .hephconfig (`plugins:`): is \
             \"nope\" named there?"
        );
    }

    fn schema_of(fields: Vec<(&str, ParamType)>) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema {
            fields: fields
                .into_iter()
                .map(|(name, ty)| hplugin::driver::DriverField {
                    name: name.to_string(),
                    ty,
                    doc: String::new(),
                    required: false,
                })
                .collect(),
            accepts_deferred: false,
        }
    }

    /// A float field's key takes an int too, bare or nested (I3): heph takes
    /// an int wherever it expects a float.
    #[test]
    fn kwargs_float_key_takes_int() {
        let schemas = DriverSchemas::from([(
            "num".to_string(),
            schema_of(vec![
                ("scale", ParamType::Float),
                ("ratios", ParamType::list(ParamType::Float)),
                ("weights", ParamType::map(ParamType::Float)),
            ]),
        )]);
        let kwargs = DriverKwargs::from_schemas(&schemas);
        let num = kwargs.at(&["num"]).expect("num");
        let ty = |key: &str| {
            num.keys
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, t)| t.to_string())
                .expect("key")
        };
        assert_eq!(ty("scale"), "None | float | int");
        assert_eq!(ty("ratios"), "None | list[float | int]");
        assert_eq!(ty("weights"), "None | dict[str, float | int]");
        assert_never_narrower(&kwargs, &schemas);
        let globals = build_globals(&FunctionRegistry::default(), &kwargs);
        let src = "def f(**kwargs: heph.types.num.Kwargs):\n    pass\n\
                   f(scale = 1, ratios = [1, 1.5], weights = {\"k\": 2})\nf(scale = 1.5)\n";
        Module::with_temp_heap(|module| {
            let mut ev = Evaluator::new(&module);
            ev.eval_module(parse("BUILD", src), &globals)
                .map(drop)
                .map_err(|e| format!("{e:#}"))
        })
        .expect("an int passes a float key");
    }

    /// A driver with no schema field: its keys are `target()`'s, but `driver`.
    #[test]
    fn kwargs_zero_field_driver() {
        let schemas = DriverSchemas::from([("bare".to_string(), schema_of(vec![]))]);
        let kwargs = DriverKwargs::from_schemas(&schemas);
        let keys: Vec<&str> = kwargs
            .at(&["bare"])
            .expect("bare")
            .keys
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        let mut base: Vec<String> = target_base_fields()
            .into_iter()
            .map(|f| f.name)
            .filter(|n| n != "driver")
            .collect();
        base.sort();
        assert_eq!(keys, base);
    }

    /// Each reason a driver gets no `Kwargs`, alone in a set with a driver
    /// that does: it is left out, and the rest are kept.
    #[test]
    fn kwargs_unwritable_drivers_skipped() {
        for (name, why) in [
            ("", "an empty name segment"),
            ("a..b", "an empty name segment"),
            ("a.", "an empty name segment"),
            ("Kwargs", "a `Kwargs` name segment"),
            ("oci.Kwargs", "a `Kwargs` name segment"),
            ("Deps", "a first segment named like a heph.types alias"),
            ("Labels.x", "a first segment named like a heph.types alias"),
        ] {
            assert_eq!(unwritable(name), Some(why), "{name:?}");
            let schemas = DriverSchemas::from([
                (name.to_string(), schema_of(vec![])),
                ("bash".to_string(), schema_of(vec![])),
            ]);
            let kwargs = DriverKwargs::from_schemas(&schemas);
            let drivers: Vec<&str> = kwargs.drivers.iter().map(|k| k.driver.as_str()).collect();
            assert_eq!(drivers, ["bash"], "{name:?} is skipped");
        }
        // A later segment may be named like an alias: only the first is read
        // as one.
        assert_eq!(unwritable("oci.Deps"), None);
        assert_eq!(unwritable("buildfile.lint_check"), None);
    }

    /// The engine's `PLUGIN_NAME_RULE` (`crates/engine/src/engine/engine.rs`),
    /// which every driver's name segments follow: duplicated, since this crate
    /// does not depend on the engine. Keep the two in step.
    const PLUGIN_NAME_RULE: &str = "[a-z_][a-z0-9_]*";

    /// Whether `s` matches [`PLUGIN_NAME_RULE`].
    fn matches_name_rule(s: &str) -> bool {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    /// No real driver name hits [`unwritable`]: no flat alias, and not
    /// `Kwargs`, can be a segment the naming rule allows, nor can an empty one.
    #[test]
    fn unwritable_names_break_the_name_rule() {
        assert!(matches_name_rule("bash") && matches_name_rule("_x1"));
        assert!(!matches_name_rule(""));
        assert!(
            !matches_name_rule(KWARGS),
            "{KWARGS} matches `{PLUGIN_NAME_RULE}`"
        );
        for a in aliases() {
            assert!(
                !matches_name_rule(a.name),
                "{} matches `{PLUGIN_NAME_RULE}`",
                a.name
            );
        }
    }

    /// `heph.types.<driver>.Kwargs` on an ordinary (not `**`) parameter is a
    /// dict type at evaluation: known keys pass, an unknown key fails.
    #[test]
    fn kwargs_alias_on_plain_param() {
        let (_tmp, loader) = kwargs_loader();
        let def = "def f(k: heph.types.bash.Kwargs):\n    pass\n\n";
        let ok = format!("{def}f({{\"run\": \"echo\", \"labels\": [\"l\"]}})\nf({{}})\n");
        eval_source("BUILD", ok, "pkg", &loader).expect("known keys evaluate");
        let bad = format!("{def}f({{\"rnu\": \"echo\"}})\n");
        let Err(err) = eval_source("BUILD", bad, "pkg", &loader) else {
            panic!("an unknown key evaluated");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("`k`"), "names the parameter: {msg}");
    }

    /// The Goal example at evaluation: an unknown key and a wrong value each
    /// fail the call, naming the key (D8).
    #[test]
    fn kwargs_evaluation_checks_keys() {
        let (_tmp, loader) = kwargs_loader();
        let lib = "def lib(name: str, **kwargs: heph.types.bash.Kwargs):\n    \
                   return target(name = name, driver = \"bash\", **kwargs)\n\n";
        let ok = format!("{lib}lib(name = \"a\", run = \"echo\", labels = [\"l\"])\n");
        let result = eval_source("BUILD", ok, "pkg", &loader).expect("evaluates");
        assert_eq!(result.targets().len(), 1);
        for (call, key) in [("rnu = \"echo\"", "`rnu`"), ("run = 1", "`run`")] {
            let src = format!("{lib}lib(name = \"a\", {call})\n");
            let Err(err) = eval_source("BUILD", src, "pkg", &loader) else {
                panic!("{call} evaluated");
            };
            let msg = format!("{err:#}");
            assert!(msg.contains(key), "{call}: {msg}");
        }
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
                "Deps",
                "Env",
                "Labels",
                "Number",
                "PassEnv",
                "RuntimePassEnv",
                "Tools"
            ],
            "adding an alias is an API addition; removing one needs a deprecation"
        );
        // Every definition resolved against the schema: none silently dropped.
        assert_eq!(names.len(), DEFS.len());
    }

    /// The aliases built from a `target()` field.
    fn field_aliases() -> impl Iterator<Item = &'static Alias> {
        aliases().iter().filter(|a| {
            DEFS.iter()
                .any(|d| d.name == a.name && matches!(d.source, Source::Field { .. }))
        })
    }

    /// `heph.types.Number` is `int | float`: both numbers pass, `None` and a
    /// string do not, statically and at runtime; no parameter name suggests it.
    #[test]
    fn number_alias() {
        let (errors, _) = typecheck(
            "def f(x: heph.types.Number):\n    pass\n\ndef ok():\n    f(1)\n    f(1.5)\n\n\
             def bad():\n    f(\"s\")\n",
            &HashMap::new(),
        );
        assert_eq!(errors.len(), 1, "exactly the str call: {errors:#?}");
        assert!(errors[0].contains("str"), "{errors:#?}");
        // A float default infers the same type: the default fits the alias.
        let (errors, _) = typecheck(
            "def g(scale = 1.0):\n    n: heph.types.Number = scale\n",
            &HashMap::new(),
        );
        assert!(errors.is_empty(), "{errors:#?}");

        let (_tmp, loader) = source_loader();
        let src = "def f(x: heph.types.Number = 1):\n    return x\n\nf()\nf(2)\nf(1.5)\n\
                   y = heph.types.Number\n";
        eval_source("BUILD", src.to_string(), "pkg", &loader).expect("evaluates");
        for arg in ["\"s\"", "None"] {
            let src = format!("def f(x: heph.types.Number = 1):\n    pass\nf({arg})\n");
            assert!(
                eval_source("BUILD", src, "pkg", &loader).is_err(),
                "{arg} is not a Number"
            );
        }
        for name in ["number", "Number", "n", "scale"] {
            assert!(for_param(name).is_none(), "{name}");
        }
    }

    #[test]
    fn aliases_typecheck() {
        for a in field_aliases() {
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
        // `Number` is no field: nothing to agree with.
        let params: Vec<(&str, ParamType)> = DEFS
            .iter()
            .filter_map(|d| match &d.source {
                Source::Field { path } => Some((d.name, field_param(path).expect("derives"))),
                Source::Fixed(_) => None,
            })
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
        for a in field_aliases() {
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
        assert_eq!(result.targets().len(), 2);
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

    /// The wrapper docs/BUILDFILE_CHECKS.md shows under `heph.types`, read
    /// verbatim from the doc, evaluates and passes its params through.
    #[test]
    fn doc_example_evaluates() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/BUILDFILE_CHECKS.md");
        let doc = std::fs::read_to_string(&path).expect("read the doc");
        let (_, section) = doc.split_once("## `heph.types`").expect("the section");
        let (_, block) = section.split_once("```python\n").expect("its example");
        let (example, _) = block.split_once("```").expect("the example's end");
        let (_tmp, loader) = source_loader();
        let src =
            format!("{example}\nlib(\"a\")\nlib(\"b\", deps = [\"//x:y\"], labels = \"l\")\n");
        let result = eval_source("BUILD", src, "pkg", &loader)
            .unwrap_or_else(|e| panic!("the doc's example: {e:#}\n{example}"));
        let names: Vec<&str> = result.targets().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        let b = &result.targets()[1];
        assert_eq!(b.labels, ["l"], "{b:#?}");
        let transitive = format!("{:?}", b.transitive);
        assert!(transitive.contains("//x:y"), "deps passed through: {b:#?}");
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
                    "heph.types.Nope: no type alias named \"Nope\"; heph.types has: Deps, \
                     Env, Labels, Number, PassEnv, RuntimePassEnv, Tools"
                ),
                "{msg}"
            );
            assert!(msg.contains("BUILD:1:"), "names the file and line: {msg}");
        }
        let none = DriverKwargs::default();
        let msg = none.unknown_member(&["Lables"]).expect("a typo");
        assert!(msg.contains("(did you mean \"Labels\"?)"), "{msg}");
        assert_eq!(none.unknown_member(&["Labels"]), None);
    }
}
