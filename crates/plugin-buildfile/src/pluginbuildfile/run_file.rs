use crate::pluginbuildfile::provider::{PackageList, Provider};
use anyhow::Context;
use enclose::enclose;
use hcore::hmemoizer::unwrap_arc_err;
use hcore::htvalue::signature::{FnSignature, ParamType};
use hcore::htvalue::{self, parse_map_string_string, parse_map_string_strings, parse_strings};
use hmodel::htaddr;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use hplugin::driver::{DriverSchema, TargetAddr};
use hplugin::provider::{
    Approval, FnArgs, FnCallContext, ProvenanceFrame, ProviderFn, ProviderFunctionRegistry,
};
use hwalk::{CachedWalker, EntryKind};
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Arguments, Evaluator, FileLoader};
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::float::UnpackFloat;
use starlark::values::list::{AllocList, UnpackList};
use starlark::values::{UnpackValue, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Build the Starlark globals: the static `starlark_module` builtins, the static
/// `heph.core` host/platform namespace, plus a dynamic `heph.<provider>.<fn>`
/// namespace for every function the providers expose. Built once per
/// buildfile-provider lifetime (the registry is fixed after the engine injects it).
/// One `heph.core.<fn>` builtin, for LSP member completion/hover: bare name, a
/// one-line signature for the completion detail, and the full rendered doc.
pub(crate) struct CoreMember {
    pub name: String,
    pub detail: String,
    pub doc: String,
}

/// Enumerate the `heph.core` namespace's functions from the globals doc. These
/// are static `#[starlark_module]` builtins (not provider-registry functions),
/// so the proxy can't get them from the registry — it reads this list instead.
pub(crate) fn heph_core_members(globals_doc: &starlark::docs::DocModule) -> Vec<CoreMember> {
    use starlark::docs::markdown::render_doc_item_no_link;
    use starlark::docs::{DocItem, DocMember};

    // Navigate `heph` → `core`; both are namespaces (nested `DocModule`s).
    let nested = |item: &DocItem, key: &str| -> Option<DocItem> {
        match item {
            DocItem::Module(m) => m.members.get(key).cloned(),
            _ => None,
        }
    };
    let Some(heph) = globals_doc.members.get("heph") else {
        return vec![];
    };
    let core = match nested(heph, "core") {
        Some(DocItem::Module(m)) => m.members,
        _ => return vec![],
    };

    core.iter()
        .map(|(name, item)| {
            let detail = match item {
                DocItem::Member(DocMember::Function(f)) => {
                    let params = f
                        .params
                        .pos_only
                        .iter()
                        .chain(f.params.pos_or_named.iter())
                        .chain(f.params.named_only.iter())
                        .map(|p| format!("{}: {}", p.name, p.typ))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{name}({params}) -> {}", f.ret.typ)
                }
                _ => String::new(),
            };
            CoreMember {
                name: name.clone(),
                detail,
                doc: render_doc_item_no_link(name, item),
            }
        })
        .collect()
}

/// Hover markdown for the `target` and `provider_state` builtins, rendered on
/// demand so each prototype can be narrowed to what the call site selects.
///
/// Both builtins take a raw `args: &Arguments`, so the stock server can only
/// render the meaningless `def name(*args, **kwargs)` prototype. We render a
/// meaningful one instead: `target`'s recognized base arguments (from
/// [`target_base_fields`], so the two never drift) and `provider_state`'s
/// `provider`, keeping each builtin's real docstring.
///
/// The point of rendering per call rather than once at startup is the rest: when
/// the call names a `driver` (or a `provider`) whose schema resolves, that
/// schema's fields *replace* the `**config` / `**state` catch-all, each with its
/// type, its required-ness and its doc. The hover then describes the keys this
/// particular target accepts and nothing else — the generic prototype could only
/// say "and some more keyword arguments". Only the docstrings are held here;
/// rendering is a handful of allocations and runs once per hover.
pub(crate) struct BuiltinHovers {
    target_doc: Option<starlark::docs::DocString>,
    provider_state_doc: Option<starlark::docs::DocString>,
}

/// One schema field as the hover renderer needs it — the shape a driver's
/// `DriverField` and a provider's `StateField` have in common.
struct HoverField<'a> {
    name: &'a str,
    ty: &'a ParamType,
    doc: &'a str,
    required: bool,
}

impl BuiltinHovers {
    pub(crate) fn new(globals_doc: &starlark::docs::DocModule) -> BuiltinHovers {
        use starlark::docs::{DocItem, DocMember};
        // The builtin's real docstring, pulled from the globals doc so it never
        // drifts from the `#[starlark_module]` definition.
        let docstring = |name: &str| match globals_doc.members.get(name) {
            Some(DocItem::Member(DocMember::Function(f))) => f.docs.clone(),
            _ => None,
        };
        BuiltinHovers {
            target_doc: docstring("target"),
            provider_state_doc: docstring("provider_state"),
        }
    }

    /// `target(name, driver, …, **config) -> str`, or — with `driver` resolved to
    /// its schema — that driver's config fields in place of the catch-all.
    pub(crate) fn target(&self, driver: Option<(&str, &DriverSchema)>) -> String {
        let fields = driver.map(|(name, schema)| {
            let fields: Vec<HoverField<'_>> = schema
                .fields
                .iter()
                .map(|f| HoverField {
                    name: &f.name,
                    ty: &f.ty,
                    doc: &f.doc,
                    required: f.required,
                })
                .collect();
            (name, fields, "driver", "config")
        });
        let base = target_base_fields();
        let base: Vec<HoverField<'_>> = base
            .iter()
            .map(|f| HoverField {
                name: &f.name,
                ty: &f.ty,
                doc: &f.doc,
                required: f.required,
            })
            .collect();
        render_builtin_hover(
            "target",
            &self.target_doc,
            &base,
            fields
                .as_ref()
                .map(|(n, f, k, c)| (*n, f.as_slice(), *k, *c)),
            "config",
            starlark::typing::Ty::string(),
        )
    }

    /// `provider_state(provider, **state) -> None`, narrowed the same way once
    /// the named provider's state schema resolves.
    pub(crate) fn provider_state(
        &self,
        provider: Option<(&str, &hplugin::provider::StateSchema)>,
    ) -> String {
        let fields = provider.map(|(name, schema)| {
            let fields: Vec<HoverField<'_>> = schema
                .fields
                .iter()
                .map(|f| HoverField {
                    name: &f.name,
                    ty: &f.ty,
                    doc: &f.doc,
                    required: f.required,
                })
                .collect();
            (name, fields, "provider", "state")
        });
        let base = [HoverField {
            name: "provider",
            ty: &ParamType::String,
            doc: "Provider whose state this sets.",
            required: true,
        }];
        render_builtin_hover(
            "provider_state",
            &self.provider_state_doc,
            &base,
            fields
                .as_ref()
                .map(|(n, f, k, c)| (*n, f.as_slice(), *k, *c)),
            "state",
            starlark::typing::Ty::none(),
        )
    }
}

/// Render one builtin's hover: its prototype, docstring, and a `#### Parameters`
/// section for every field that documents itself.
///
/// `schema` is the resolved driver/provider — `(name, its fields, what it is,
/// what the fields are called)`. Present, its fields take the place of the
/// `**{catch_all}` parameter and a closing line attributes them, so a reader who
/// wonders where `cmd` came from can see it. Absent, the catch-all stands: the
/// remaining keys exist but nothing here knows them.
fn render_builtin_hover(
    name: &str,
    docs: &Option<starlark::docs::DocString>,
    base: &[HoverField<'_>],
    schema: Option<(&str, &[HoverField<'_>], &str, &str)>,
    catch_all: &str,
    ret: starlark::typing::Ty,
) -> String {
    use starlark::docs::markdown::render_doc_item_no_link;
    use starlark::docs::{
        DocFunction, DocItem, DocMember, DocParam, DocParams, DocReturn, DocString, DocStringKind,
    };

    // `required` controls whether a `= None` default — the optional convention —
    // is shown; the field's own doc becomes the parameter's, which is what puts
    // it in the rendered `#### Parameters` list.
    let param = |f: &HoverField<'_>| DocParam {
        name: f.name.to_string(),
        docs: DocString::from_docstring(DocStringKind::Starlark, f.doc),
        typ: param_type_to_ty(f.ty),
        default_value: (!f.required).then(|| "None".to_string()),
    };

    let params = DocParams {
        pos_only: Vec::new(),
        pos_or_named: Vec::new(),
        args: None,
        // Keyword-only, which is what these builtins actually are — they reject
        // positional arguments outright. It also keeps the prototype well-formed
        // once a schema is spliced in: a driver's required `cmd` lands after the
        // optional base args, which reads as invalid syntax without the `*`.
        named_only: base
            .iter()
            .chain(schema.iter().flat_map(|(_, fields, _, _)| fields.iter()))
            .map(param)
            .collect(),
        // A `**name` catch-all of arbitrary type, only while the schema behind it
        // is unknown.
        kwargs: schema.is_none().then(|| DocParam {
            name: catch_all.to_string(),
            docs: None,
            typ: starlark::typing::Ty::any(),
            default_value: None,
        }),
    };

    let item = DocItem::Member(DocMember::Function(DocFunction {
        docs: docs.clone(),
        params,
        ret: DocReturn {
            docs: None,
            typ: ret,
        },
    }));
    let mut md = render_doc_item_no_link(name, &item);

    // Say which half is which. The list above blends two sources — the
    // builtin's own spec-level fields, then the selected schema's — and a
    // reader who can't tell them apart can't tell what changes if they swap the
    // driver.
    if let Some((schema_name, fields, kind, field_kind)) = schema {
        md.push_str("\n\n");
        if fields.is_empty() {
            md.push_str(&format!(
                "*The fields above are `{name}`'s own; the `{schema_name}` {kind} takes no {field_kind}.*"
            ));
        } else {
            md.push_str(&format!(
                "*The fields above are `{name}`'s own, then the `{schema_name}` {kind}'s {field_kind}.*"
            ));
        }
    }
    md
}

pub(crate) fn build_globals(registry: &ProviderFunctionRegistry) -> Globals {
    let mut builder = GlobalsBuilder::standard();
    builder = builder.with(starlark_module);
    builder
        .with_namespace("heph", |hb| {
            // Static `heph.core` host/platform builtins.
            hb.namespace("core", heph_core_module);
            // One `heph.<provider>` namespace per provider; each function becomes a
            // native callable bridging into its async `ProviderFn`.
            for (provider, fns) in registry.providers() {
                hb.namespace(provider, |nb| {
                    for (name, rf) in fns {
                        // Each provider function carries per-fn state (its async
                        // `ProviderFn` + declared signature), so it's registered as a
                        // custom callable value rather than `set_function` (whose
                        // 0.14 `NativeFuncFn` is a stateless `fn` pointer). The
                        // engine-side validator in `invoke` is the canonical guard
                        // for arity/types/return; the LSP proxy renders hover and
                        // completion for these from the registry.
                        nb.set(
                            name.as_str(),
                            ProviderNativeFn {
                                display: format!("heph.{provider}.{name}"),
                                signature: Arc::clone(&rf.signature),
                                func: Arc::clone(&rf.func),
                            },
                        );
                    }
                });
            }
        })
        .build()
}

/// Map a [`ParamType`] to the Starlark `Ty` used for native param/return typing
/// (and, for the LSP, to render hover signatures with Starlark type names).
pub(crate) fn param_type_to_ty(t: &ParamType) -> starlark::typing::Ty {
    use starlark::typing::Ty;
    match t {
        ParamType::String => Ty::string(),
        ParamType::Bool => Ty::bool(),
        // htvalue distinguishes Int/Uint but Starlark has a single int type.
        ParamType::Int | ParamType::Uint => Ty::int(),
        ParamType::Float => Ty::float(),
        ParamType::Null => Ty::none(),
        ParamType::List(inner) => Ty::list(param_type_to_ty(inner)),
        ParamType::Map(value) => Ty::dict(Ty::string(), param_type_to_ty(value)),
        ParamType::Union(types) => Ty::unions(types.iter().map(param_type_to_ty).collect()),
        // Starlark has no record type; model a struct as a string-keyed dict
        // whose value type is the union of the field types.
        ParamType::Struct(fields) => Ty::dict(
            Ty::string(),
            Ty::unions(fields.iter().map(|f| param_type_to_ty(&f.ty)).collect()),
        ),
    }
}

/// A `heph.<provider>.<fn>` callable: a custom Starlark value holding the
/// provider's async [`ProviderFn`] and declared signature. Registered via
/// `GlobalsBuilder::set` — 0.14's `set_function` takes a stateless `fn` pointer
/// that can't carry this per-function state. The Starlark eval is synchronous,
/// so the async handler is driven with `futures::executor::block_on` —
/// runtime-agnostic, works under `#[test]`, inline eval, and `block_in_place`.
#[derive(ProvidesStaticType, starlark::values::NoSerialize, allocative::Allocative)]
struct ProviderNativeFn {
    /// `heph.<provider>.<fn>`, used in validation error messages.
    display: String,
    #[allocative(skip)]
    signature: Arc<FnSignature>,
    #[allocative(skip)]
    func: Arc<dyn ProviderFn>,
}

impl std::fmt::Debug for ProviderNativeFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderNativeFn")
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for ProviderNativeFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display)
    }
}

starlark::starlark_simple_value!(ProviderNativeFn);

#[starlark::values::starlark_value(type = "provider_function")]
impl<'v> starlark::values::StarlarkValue<'v> for ProviderNativeFn {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling a provider function")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");

        // Collect positional and named args verbatim, then let the declared
        // signature (`validate_args` below) enforce arity / names / types — it is
        // the canonical guard. `positions()` (rather than `parse_positional`, which
        // rejects *all* named args via `no_named_args`) is what lets a provider
        // function — a build-file plugin's rule — take named arguments like
        // `foo_codegen(name = …, srcs = …)`.
        let positional: Vec<htvalue::Value> = args
            .positions(eval.heap())?
            .map(|v| starlark_to_rust(&v))
            .collect::<anyhow::Result<_>>()
            .map_err(starlark::Error::new_other)?;
        let named: HashMap<String, htvalue::Value> = args
            .names_map()?
            .iter()
            .map(|(k, v)| Ok((k.as_str().to_string(), starlark_to_rust(v)?)))
            .collect::<anyhow::Result<_>>()
            .map_err(starlark::Error::new_other)?;

        // Enforce the declared signature: hard-fail on bad arity, missing
        // required, unknown named, or wrong type; substitute optional defaults.
        let (positional, named) = self
            .signature
            .validate_args(&self.display, positional, named)
            .map_err(starlark::Error::new_other)?;

        let ctx = FnCallContext {
            pkg: extra.pkg,
            root: extra.root,
        };
        let fn_args = FnArgs { positional, named };

        let outcome = futures::executor::block_on(self.func.call(&ctx, fn_args))
            .map_err(starlark::Error::new_other)?;

        // A provider function may declare targets / provider-state (a "build-file
        // plugin" wrapping a driver). Merge each into the calling package through
        // the same sinks the `target()` / `provider_state()` builtins use, so a
        // declared target is indistinguishable from a hand-written one. Capture the
        // call-site provenance once (when enabled) and stamp it on every declared
        // target, so tooling traces them back to the `heph.<plugin>.<fn>(…)` call.
        if !outcome.targets.is_empty() {
            let provenance = if extra.capture_provenance {
                capture_provenance(eval)
            } else {
                Vec::new()
            };
            for dt in outcome.targets {
                if dt.name.is_empty() {
                    return Err(starlark::Error::new_other(anyhow::anyhow!(
                        "{}: declared target name cannot be empty",
                        self.display
                    )));
                }
                (extra.on_target)(OnTargetPayload {
                    name: dt.name,
                    driver: dt.driver,
                    labels: dt.labels,
                    transitive: dt.transitive,
                    approval: dt.approval,
                    config: dt.config,
                    provenance: provenance.clone(),
                })
                .map_err(starlark::Error::new_other)?;
            }
        }
        for ds in outcome.states {
            if ds.provider.is_empty() {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "{}: declared provider_state is missing provider",
                    self.display
                )));
            }
            (extra.on_state)(OnStatePayload {
                provider: ds.provider,
                args: ds.args,
            })
            .map_err(starlark::Error::new_other)?;
        }

        // Native `return_type` is documentation-only for native fns, so validate
        // the actual return value here.
        self.signature
            .validate_return(&self.display, &outcome.value)
            .map_err(starlark::Error::new_other)?;

        Ok(rust_to_starlark(eval.heap(), &outcome.value))
    }
}

/// Convert a [`htvalue::Value`] back into a Starlark value — the inverse of
/// [`starlark_to_rust`].
fn rust_to_starlark<'v>(heap: starlark::values::Heap<'v>, v: &htvalue::Value) -> Value<'v> {
    match v {
        htvalue::Value::Null() => Value::new_none(),
        htvalue::Value::String(s) => heap.alloc(s.as_str()),
        htvalue::Value::Bool(b) => Value::new_bool(*b),
        htvalue::Value::Int(i) => heap.alloc(*i),
        htvalue::Value::Uint(u) => heap.alloc(*u),
        htvalue::Value::Float(f) => heap.alloc(*f),
        htvalue::Value::List(l) => {
            heap.alloc(AllocList(l.iter().map(|e| rust_to_starlark(heap, e))))
        }
        htvalue::Value::Map(m) => heap
            .alloc(starlark::values::dict::AllocDict(m.iter().map(
                |(k, val)| (heap.alloc(k.as_str()), rust_to_starlark(heap, val)),
            ))),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OnTargetPayload {
    pub name: String,
    pub driver: String,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
    pub approval: Approval,
    pub config: HashMap<String, htvalue::Value>,
    /// Source call sites that produced this target (innermost `target()` call
    /// first). See [`hplugin::provider::ProvenanceFrame`].
    pub provenance: Vec<ProvenanceFrame>,
}

/// Parse a BUILD-file `approval` value into an [`Approval`]. Two spellings:
/// - `approval = True` / `False` — bare required flag, no notice.
/// - `approval = {"required": True, "notice": ["group", ...]}` — explicit form.
///   `required` defaults to `False`; `notice` to `[]`. Unknown keys are an error
///   (typos must fail loudly, not silently disable the gate).
fn approval_from(v: htvalue::Value) -> anyhow::Result<Approval> {
    match v {
        htvalue::Value::Null() => Ok(Approval::default()),
        htvalue::Value::Bool(required) => Ok(Approval {
            required,
            notice: vec![],
        }),
        htvalue::Value::Map(mut m) => {
            let required = match m.remove("required") {
                Some(htvalue::Value::Bool(b)) => b,
                None => false,
                Some(other) => {
                    anyhow::bail!("approval `required` must be a bool, got {other:?}")
                }
            };
            let notice = match m.remove("notice") {
                Some(v) => parse_strings(&v).with_context(|| "approval `notice`")?,
                None => vec![],
            };
            if !m.is_empty() {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                anyhow::bail!("approval has unknown entries: {keys:?}");
            }
            Ok(Approval { required, notice })
        }
        other => anyhow::bail!("approval must be a bool or map, got {other:?}"),
    }
}

/// A parsed `{group: [dep]}` attribute, ordered by group name.
///
/// `parse_map_string_strings` returns a `HashMap`, whose iteration order is
/// randomized per instance — so anything derived from that order differs between
/// two parses of the same BUILD file, in the same process or across processes.
/// Everything built below reaches a dependent's def hash (dep/tool ids become
/// `Input::origin_id`), so it has to be derived from the content, never from the
/// map's order.
fn sorted_groups(m: &HashMap<String, Vec<String>>) -> Vec<(&String, &Vec<String>)> {
    let mut groups: Vec<(&String, &Vec<String>)> = m.iter().collect();
    groups.sort_by(|a, b| a.0.cmp(b.0));
    groups
}

/// Parse a BUILD-file `sandbox`/`transitive` value (Starlark → `htvalue`) into a
/// [`Sandbox`]. Free function rather than an inherent `impl` because `Sandbox`
/// now lives in the `heph-plugin` contract crate (orphan rule).
fn sandbox_from(m: htvalue::Value, pkg: &PkgBuf) -> anyhow::Result<Sandbox> {
    let m = match m {
        htvalue::Value::Map(m) => m,
        htvalue::Value::Null() => {
            return Ok(Default::default());
        }
        _ => anyhow::bail!("Expected map, got {:?}", m),
    };

    let mut m: HashMap<&str, &htvalue::Value> = m.iter().map(|(k, v)| (k.as_str(), v)).collect();

    let mut sandbox = Sandbox::default();

    if let Some(v) = m.remove("deps") {
        let parsed = parse_map_string_strings(v).with_context(|| "parse `deps`")?;
        for (k, ss) in sorted_groups(&parsed) {
            for (i, s) in ss.iter().enumerate() {
                sandbox.push_dep(Dep {
                    r#ref: TargetAddr::parse(s, pkg).with_context(|| "parse `deps`")?,
                    mode: Mode::None,
                    group: k.to_string(),
                    runtime: true,
                    hash: true,
                    id: format!("dep|{}|{}", k, i),
                });
            }
        }
    }

    if let Some(v) = m.remove("env") {
        sandbox.env = parse_map_string_string(v)
            .with_context(|| "parse `env`")?
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Env {
                        value: EnvValue::Literal(v),
                        hash: true,
                        append: false,
                        append_prefix: "".to_string(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
    }

    if let Some(v) = m.remove("tools") {
        let parsed = parse_map_string_strings(v).with_context(|| "parse `tools`")?;
        for (k, ss) in sorted_groups(&parsed) {
            for (i, s) in ss.iter().enumerate() {
                sandbox.push_tool(Tool {
                    r#ref: TargetAddr::parse(s, pkg).with_context(|| "parse `tools`")?,
                    group: k.to_string(),
                    hash: true,
                    id: format!("tool|{}|{}", k, i),
                });
            }
        }
    }

    if let Some(v) = m.remove("pass_env") {
        for name in parse_strings(v).with_context(|| "parse `pass_env`")? {
            sandbox.env.insert(
                name,
                Env {
                    value: EnvValue::Pass,
                    hash: true,
                    append: false,
                    append_prefix: "".to_string(),
                },
            );
        }
    }

    if let Some(v) = m.remove("runtime_pass_env") {
        for name in parse_strings(v).with_context(|| "parse `runtime_pass_env`")? {
            sandbox.env.insert(
                name,
                Env {
                    value: EnvValue::Pass,
                    hash: false,
                    append: false,
                    append_prefix: "".to_string(),
                },
            );
        }
    }

    if !m.is_empty() {
        let unknown_keys: Vec<&str> = m.into_keys().collect();
        anyhow::bail!("unknown entries found: {:?}", unknown_keys)
    }

    Ok(sandbox)
}

#[derive(Debug, Clone)]
pub(crate) struct OnStatePayload {
    pub provider: String,
    pub args: HashMap<String, htvalue::Value>,
}

#[derive(Debug)]
pub(crate) struct RunResult {
    pub targets: Vec<OnTargetPayload>,
    pub states: Vec<OnStatePayload>,
    /// Frozen Starlark module for this BUILD file. Exposes top-level symbols so that
    /// other BUILD files can import them via `load("//pkg", "sym")`. Empty when the
    /// package has no BUILD file matching the configured patterns.
    pub module: FrozenModule,
}

fn empty_run_result() -> anyhow::Result<RunResult> {
    let module = Module::with_temp_heap(|m| m.freeze()).map_err(anyhow::Error::from)?;
    Ok(RunResult {
        targets: vec![],
        states: vec![],
        module,
    })
}

fn merge_pkg_results(parts: &[Arc<RunResult>]) -> anyhow::Result<RunResult> {
    let mut targets = vec![];
    let mut states = vec![];
    for part in parts {
        targets.extend(part.targets.iter().cloned());
        states.extend(part.states.iter().cloned());
    }
    let frozen = Module::with_temp_heap(|module| -> anyhow::Result<FrozenModule> {
        // Re-export every public symbol of each frozen part so `load("//pkg", sym)`
        // sees the whole package's symbols. `import_public_symbols` can't be used —
        // it imports them *private* (not re-exported). The safe `owned_value` ties
        // the value to a borrow of `module`, which would block the later freeze, so
        // we reference the part's heap into this module's heap and take the owned
        // `FrozenValue` directly.
        for part in parts {
            module
                .frozen_heap()
                .add_reference(part.module.frozen_heap());
            for name in part.module.names() {
                let name_str = name.as_str();
                // Skip underscore-prefixed (private) symbols — never re-exportable.
                if name_str.starts_with('_') {
                    continue;
                }
                if let Some(owned) = part.module.get_option(name_str)? {
                    // SAFETY: the part's frozen heap is referenced into `module`'s
                    // frozen heap just above, so the value stays alive for the
                    // module's lifetime (and the resulting frozen module's).
                    let frozen_value = unsafe { owned.unchecked_frozen_value() };
                    module.set(name_str, frozen_value.to_value());
                }
            }
        }
        module.freeze().map_err(anyhow::Error::from)
    })?;
    Ok(RunResult {
        targets,
        states,
        module: frozen,
    })
}

#[derive(ProvidesStaticType)]
pub(crate) struct Extra<'a> {
    pub pkg: &'a str,
    pub root: &'a Path,
    pub on_state: Box<dyn Fn(OnStatePayload) -> anyhow::Result<()>>,
    pub on_target: Box<dyn Fn(OnTargetPayload) -> anyhow::Result<()>>,
    /// Every package in the workspace (dir with a matching BUILD file, plus its
    /// ancestors), pruning `fs.skip`ped subtrees, **sorted**. Backs
    /// `heph.core.packages()`; served from a shared [`PackageList`], so the tree
    /// is walked once rather than once per call and the order — which reaches the
    /// def hash — is the same for every caller. See [`PackageList`] for why that
    /// matters.
    pub list_packages: Box<dyn Fn() -> anyhow::Result<Arc<Vec<String>>>>,
    /// Capture each target's source call-stack provenance. Off on the normal
    /// build path (walking `eval.call_stack()` per `target()` call is needless
    /// overhead there); on only for the LSP, which needs it to map a source
    /// position back to the targets a symbol produced.
    pub capture_provenance: bool,
}

/// Unsupported starlark value types (not str/bool/int/float/list/dict) are a
/// BUILD-file authoring error (e.g. passing a namespace or a native function
/// value where a plain value is expected) — never a programming error, so
/// this must return an error rather than panic: it runs inside the LSP, which
/// must survive a malformed BUILD file rather than crash the whole server.
fn starlark_to_rust(v: &Value) -> anyhow::Result<htvalue::Value> {
    if v.is_none() {
        return Ok(htvalue::Value::Null());
    }

    if let Some(s) = v.unpack_str() {
        return Ok(htvalue::Value::String(s.to_string()));
    }

    if let Some(b) = v.unpack_bool() {
        return Ok(htvalue::Value::Bool(b));
    }

    if let Some(i) = v.unpack_i32() {
        return Ok(htvalue::Value::Int(i as i64));
    }

    if let Ok(Some(UnpackFloat(f))) = UnpackFloat::unpack_value(*v) {
        return Ok(htvalue::Value::Float(f));
    }

    if let Ok(Some(l)) = UnpackList::<Value>::unpack_value(*v) {
        return Ok(htvalue::Value::List(
            l.items
                .iter()
                .map(starlark_to_rust)
                .collect::<anyhow::Result<_>>()?,
        ));
    }

    if let Some(d) = DictRef::from_value(*v) {
        let map = d
            .iter()
            .filter_map(|(k, val)| k.unpack_str().map(|s| (s.to_string(), val)))
            .map(|(k, val)| Ok((k, starlark_to_rust(&val)?)))
            .collect::<anyhow::Result<_>>()?;
        return Ok(htvalue::Value::Map(map));
    }

    Err(anyhow::anyhow!(
        "unsupported starlark value type: {} (expected None/str/bool/int/float/list/dict)",
        v.get_type()
    ))
}

/// Returns `path` prefixed with the current package and lexically normalized
/// (e.g. `"./src/main.rs"` from pkg `"foo/bar"` becomes `"foo/bar/src/main.rs"`).
/// If `abs` is true, returns `path` unchanged; at the workspace root the package
/// prefix is empty but the path is still normalized. Errors when a `..` segment
/// would escape the workspace root.
fn resolve_fs_path(eval: &Evaluator, path: &str, abs: bool) -> anyhow::Result<String> {
    if abs {
        return Ok(path.to_string());
    }
    let extra = eval
        .extra
        .expect("evaluator extra must be set")
        .downcast_ref::<Extra>()
        .expect("evaluator extra must be of type Extra");
    hmodel::htpkg::join_rel_checked(extra.pkg, path)
        .with_context(|| format!("resolving fs path {path:?} in package {}", extra.pkg))
}

/// Snapshot the Starlark call stack at the moment `target()` runs into a chain of
/// [`ProvenanceFrame`]s — the innermost frame is the `target()` call site, outer
/// frames are the macro/loop call sites that led there. Frames without a source
/// location (native-only calls) are dropped. Lines/columns are converted from
/// starlark's 0-based positions to 1-based.
fn capture_provenance(eval: &Evaluator) -> Vec<ProvenanceFrame> {
    eval.call_stack()
        .frames
        .iter()
        .filter_map(|frame| {
            let span = frame.location.as_ref()?;
            let rs = span.resolve_span();
            Some(ProvenanceFrame {
                fn_name: frame.name.clone(),
                file: span.filename().to_string(),
                line_start: rs.begin.line as u32 + 1,
                col_start: rs.begin.column as u32 + 1,
                line_end: rs.end.line as u32 + 1,
                col_end: rs.end.column as u32 + 1,
            })
        })
        .collect()
}

/// The driver-independent keyword arguments the `target()` builtin always
/// accepts, for BUILD-file LSP completion. Kept next to the `target()` builtin
/// (below) so the two don't drift. Driver-specific config args come from the
/// driver's own schema and are merged in by the LSP.
pub(crate) fn target_base_fields() -> Vec<hplugin::driver::DriverField> {
    use hplugin::driver::DriverField;
    let f = |name: &str, ty: ParamType, doc: &str, required: bool| DriverField {
        name: name.to_string(),
        ty,
        doc: doc.to_string(),
        required,
    };
    vec![
        f("name", ParamType::String, "Target name (required).", true),
        f(
            "driver",
            ParamType::String,
            "Driver that builds this target; falls back to the provider's `defaultDriver`.",
            false,
        ),
        f(
            "labels",
            ParamType::union(vec![ParamType::String, ParamType::list(ParamType::String)]),
            "Labels for querying/filtering this target.",
            false,
        ),
        f(
            "transitive",
            ParamType::map(ParamType::union(vec![
                ParamType::list(ParamType::String),
                ParamType::map(ParamType::list(ParamType::String)),
            ])),
            "Sandbox applied transitively: `deps`, `tools`, `env`, `pass_env`, `runtime_pass_env`, `runtime_env`.",
            false,
        ),
        f(
            "approval",
            ParamType::union(vec![
                ParamType::Bool,
                ParamType::map(ParamType::union(vec![
                    ParamType::Bool,
                    ParamType::list(ParamType::String),
                ])),
            ]),
            "Require explicit approval before executing: `True`, or `{required, notice}` where `notice` lists input groups shown to the user.",
            false,
        ),
    ]
}

#[starlark_module]
fn starlark_module(builder: &mut GlobalsBuilder) {
    /// Declare a build target.
    ///
    /// `name` is required. `driver` selects which driver builds it (falls back to
    /// the provider's `defaultDriver` when omitted). `labels` and `transitive`
    /// (sandbox `deps`/`tools`/`env`) are recognized; every other keyword argument
    /// becomes driver-specific config. Returns the target's `//pkg:name` address.
    fn target<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<String> {
        args.no_positional_args(eval.heap())?;
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling target()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");

        let m = args.names_map()?;

        let mut name = String::new();
        let mut driver = String::new();
        let mut labels: Vec<String> = vec![];
        let mut transitive: Sandbox = Default::default();
        let mut approval: Approval = Default::default();
        let config = m
            .iter()
            .map(|e| -> anyhow::Result<Option<(String, htvalue::Value)>> {
                match e.0.as_str() {
                    "name" => {
                        if let Some(s) = e.1.unpack_str() {
                            name = s.to_string();
                        }
                        Ok(None)
                    }
                    "driver" => {
                        if let Some(s) = e.1.unpack_str() {
                            driver = s.to_string();
                        }
                        Ok(None)
                    }
                    "labels" => {
                        if let Some(s) = e.1.unpack_str() {
                            labels = vec![s.to_string()];
                        } else {
                            labels = UnpackList::<String>::unpack_value_err(*e.1)
                                .map_err(|e| anyhow::anyhow!("{e}"))?
                                .items;
                        }
                        Ok(None)
                    }
                    "transitive" => {
                        transitive = sandbox_from(starlark_to_rust(e.1)?, &PkgBuf::from(extra.pkg))
                            .with_context(|| "transitive")?;
                        Ok(None)
                    }
                    "approval" => {
                        approval =
                            approval_from(starlark_to_rust(e.1)?).with_context(|| "approval")?;
                        Ok(None)
                    }
                    _ => Ok(Some((e.0.as_str().to_string(), starlark_to_rust(e.1)?))),
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<HashMap<String, htvalue::Value>>();

        if name.is_empty() {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "target name cannot be empty"
            )));
        }
        // An empty driver is allowed here: the provider resolves it against the
        // configured `defaultDriver` (and errors if neither is set).

        let provenance = if extra.capture_provenance {
            capture_provenance(eval)
        } else {
            Vec::new()
        };

        let p = OnTargetPayload {
            name: name.clone(),
            driver,
            labels,
            transitive,
            approval,
            config,
            provenance,
        };

        (extra.on_target)(p)?;

        Ok(htaddr::Addr::new(PkgBuf::from(extra.pkg), name, Default::default()).format())
    }

    /// Reference a single file as a dependency address. Resolved relative to the
    /// current package unless `abs = True`. Returns an `fs` provider address.
    fn file<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        path: &str,
        #[starlark(require = named, default = false)] abs: bool,
    ) -> starlark::Result<String> {
        let resolved = resolve_fs_path(eval, path, abs)?;
        Ok(hbuiltins::pluginfs::file_addr(&resolved).format())
    }

    /// Reference files matching a glob `pattern` (with optional `exclude`) as a
    /// dependency address. Package-relative unless `abs = True`. Returns an `fs`
    /// provider address.
    ///
    /// `exclude` resolves exactly like `pattern` — the fs driver matches both
    /// against workspace-root-relative paths, so a package-relative pattern with
    /// a raw exclude would silently exclude nothing.
    fn glob<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        pattern: &str,
        exclude: Option<Value<'v>>,
        #[starlark(require = named, default = false)] abs: bool,
    ) -> starlark::Result<String> {
        let resolved = resolve_fs_path(eval, pattern, abs)?;
        let excludes: Vec<String> = match exclude {
            Some(v) => {
                if let Some(s) = v.unpack_str() {
                    vec![s.to_string()]
                } else {
                    UnpackList::<String>::unpack_value_err(v)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                        .items
                }
            }
            None => vec![],
        };
        let excludes = excludes
            .iter()
            .map(|e| resolve_fs_path(eval, e, abs))
            .collect::<anyhow::Result<Vec<String>>>()?;
        let excludes_ref: Vec<&str> = excludes.iter().map(String::as_str).collect();
        Ok(hbuiltins::pluginfs::glob_addr(&resolved, &excludes_ref).format())
    }

    /// Reference every target matching a query `expr` as a dependency address.
    /// Uses the heph query language (the same one `heph run -e` accepts):
    /// `&&`, `||`, `!`, parentheses, the `label()`/`tree_output()` functions,
    /// and `//pkg` / `//pkg/...` / `//pkg:name` patterns. Relative patterns
    /// (`./x`, `..`, `.`) resolve against the current package. Returns a
    /// `@heph/query` provider address that expands to the matched group.
    ///
    /// Opts out of the engine's auto-exclusion of the requesting provider so the
    /// query sees sibling BUILD-file targets (the common intent in a BUILD file).
    fn query<'v>(eval: &mut Evaluator<'v, '_, '_>, expr: &str) -> starlark::Result<String> {
        use hplugin_query::pluginquery;
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling query()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");
        Ok(
            pluginquery::query_addr(expr, extra.pkg, &[pluginquery::NO_PROVIDER_EXCLUSION])
                .format(),
        )
    }

    /// Build a struct (dict) from keyword arguments, for nested target config.
    fn r#struct<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let m = args.names_map()?;
        let pairs: Vec<(&str, Value<'v>)> = m.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        Ok(eval.heap().alloc(starlark::values::dict::AllocDict(pairs)))
    }

    /// Declare package-level provider state, read by the named `provider` when it
    /// resolves targets in this package. Remaining keyword arguments form the state.
    fn provider_state<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        args: &Arguments<'v, '_>,
    ) -> starlark::Result<starlark::values::none::NoneType> {
        args.no_positional_args(eval.heap())?;
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling provider_state()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");

        let m = args.names_map()?;

        let mut provider = String::new();
        let kwargs = m
            .iter()
            .map(|e| -> anyhow::Result<Option<(String, htvalue::Value)>> {
                match e.0.as_str() {
                    "provider" => {
                        if let Some(s) = e.1.unpack_str() {
                            provider = s.to_string();
                        }
                        Ok(None)
                    }
                    _ => Ok(Some((e.0.as_str().to_string(), starlark_to_rust(e.1)?))),
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<HashMap<String, htvalue::Value>>();

        if provider.is_empty() {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "provider_state: missing provider"
            )));
        }

        (extra.on_state)(OnStatePayload {
            provider,
            args: kwargs,
        })?;

        Ok(starlark::values::none::NoneType)
    }
}

#[starlark_module]
fn heph_core_module(builder: &mut GlobalsBuilder) {
    /// Host operating system in canonical (Go/OCI) naming, e.g. `linux`, `darwin`.
    fn os() -> starlark::Result<String> {
        Ok(hcore::htplatform::os().to_string())
    }

    /// Host architecture in canonical (Go/OCI) naming, e.g. `amd64`, `arm64`.
    fn arch() -> starlark::Result<String> {
        Ok(hcore::htplatform::arch().to_string())
    }

    /// Host operating system as Rust reports it, e.g. `linux`, `macos`.
    fn os_raw() -> starlark::Result<String> {
        Ok(hcore::htplatform::os_raw().to_string())
    }

    /// Host architecture as Rust reports it, e.g. `x86_64`, `aarch64`.
    fn arch_raw() -> starlark::Result<String> {
        Ok(hcore::htplatform::arch_raw().to_string())
    }

    /// The number of CPUs available to the process, falling back to 1 when the
    /// host count can't be determined.
    fn num_cpu() -> starlark::Result<i32> {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        // `available_parallelism` returns a `usize`; clamp into Starlark's `i32`
        // so an implausibly large host count can't overflow.
        Ok(i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// The package currently being evaluated.
    fn pkg<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<String> {
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling heph.core.pkg()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");
        Ok(extra.pkg.to_string())
    }

    /// The workspace packages matching `matcher`, as a sorted list of package
    /// paths (no leading `//`). `matcher` is a heph query string (`//foo`,
    /// `//foo/...`, combined with `&&`/`||`/`!`; relative `./x`/`..`/`.` resolve
    /// against the current package). It is evaluated per package, so only
    /// package-level matchers work — one that needs target/label info (e.g.
    /// `label(x)` or `//pkg:name`) errors rather than silently matching nothing.
    ///
    /// The result is a snapshot of the workspace taken once per run, and passing
    /// it to a `target(...)` puts it in that target's def hash: adding or
    /// removing a matched package rebuilds the target, and a package created
    /// while the run is already going is not seen until the next one.
    fn packages<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        matcher: &str,
    ) -> starlark::Result<Value<'v>> {
        use hmodel::htmatcher::MatchResult;
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling heph.core.packages()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");
        let base = PkgBuf::from(extra.pkg);
        let m = hmodel::htquery::parse(matcher, &base)
            .map_err(|e| anyhow::anyhow!("heph.core.packages: invalid matcher `{matcher}`: {e}"))?;
        // `.context`, not a formatted `map_err`: flattening the chain here drops
        // the cause, so an EACCES from the walk surfaces without its
        // "Permission denied".
        let pkgs = (extra.list_packages)()
            .context("heph.core.packages: enumerating packages")
            .map_err(starlark::Error::new_other)?;

        let mut matched: Vec<&str> = Vec::new();
        for pkg in pkgs.iter() {
            // A synthetic package-only addr (empty target name): package matchers
            // decide Yes/No; target-level matchers return Shrug, which we reject.
            let addr = htaddr::Addr::new(
                PkgBuf::from(pkg.as_str()),
                String::new(),
                Default::default(),
            );
            match m.matches_addr(&addr) {
                MatchResult::MatchYes => matched.push(pkg.as_str()),
                MatchResult::MatchNo => {}
                MatchResult::MatchShrug => {
                    return Err(anyhow::anyhow!(
                        "heph.core.packages: matcher `{matcher}` needs target-level info \
                         (labels/output paths); only package matchers (//pkg, //pkg/...) \
                         are supported"
                    )
                    .into());
                }
            }
        }

        // `pkgs` arrives sorted (see `PackageList`), so `matched` is too — and the
        // order this returns lands in the calling target's def hash.
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(matched.iter().map(|p| heap.alloc(*p)))))
    }
}

/// Concurrent whole-package Starlark evaluations.
///
/// [`hcore::blocking`] is `2 * cores` run slots behind one FIFO semaphore,
/// shared by four classes of work with no reserve between them:
/// sub-millisecond manifest reads, tar-and-copy into the cache, gzip (already
/// self-capped at the core count by the remote cache's `CODEC_SLOTS`) — and
/// this, the single heaviest synchronous unit in a build, at hundreds of
/// milliseconds per package.
///
/// The slots are arrival-fair and work-conserving, so an unbounded fan-out of
/// package evaluations does not *starve* the short jobs, it puts them behind a
/// queue of long ones. `run_pkg` is the only class that can occupy every slot
/// for that long, and it was the only one not bounded.
///
/// The core count, for the same reason `CODEC_SLOTS` uses it: the work is
/// CPU-bound, so more concurrent evaluations than cores buys nothing, and half
/// the slots stay free for the short jobs that were queueing behind them. It is
/// a cap on this class, not a reserve for the others — with gzip also at its own
/// core-count cap, a build that peaks on both at once can still fill the slots.
/// Guaranteeing a reserve is a change to `hcore::blocking` itself.
static PKG_EVAL_SLOTS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| {
        let slots = pkg_eval_slots();
        // `LoadRegistry`'s cross-chain claim parks its blocking job on a condvar
        // while the claim's holder evaluates inside a blocking job of its own.
        // That is deadlock-free only while every holder can actually be running —
        // i.e. while the number of concurrent evaluations stays strictly below
        // `hcore::blocking`'s concurrency limit, so a claim holder can always
        // take a run slot. (Thread supply is not the binding resource anymore —
        // tokio's blocking pool is capped at `8 * cores + 64` — the run slots
        // are.) The two constants live in different crates and nothing ties
        // their formulas together, so enforce the invariant where the slots are
        // minted.
        assert!(
            slots < hcore::blocking::concurrency_limit(),
            "PKG_EVAL_SLOTS ({slots}) must stay strictly below \
         hcore::blocking::concurrency_limit() ({}): a LoadRegistry claim waiter \
         parks its run slot while its holder needs one",
            hcore::blocking::concurrency_limit()
        );
        tokio::sync::Semaphore::new(slots)
    });

fn pkg_eval_slots() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8)
}

impl Provider {
    pub(crate) async fn run_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        // Answer a completed entry before building anything. Everything below
        // this point exists only to construct the closure `once` runs on a
        // *miss* — a `PathBuf` clone, a full clone of the function registry,
        // five `Arc` bumps and a `String` — and it was all paid on every call,
        // hit included.
        //
        // That is not a marginal saving here, because the call rate is not
        // bounded by the number of packages. `probe` calls this for every
        // package whose `provider_state` ancestry is consulted, and each
        // top-level target with a `codegen = in_place` output re-runs its whole
        // resolution under a *fresh* `RequestState` whose probe memo starts
        // empty (`Engine::new_hash_only_state`). Measured on a fully cached
        // `run lint //go/large/...` over 2000 Go packages: **12.27 million**
        // calls, 11.4M of them from `probe` — against 13 BUILD files in the
        // tree. The evaluation itself was already memoized; the preamble was
        // not, and it was ~4% of the run's CPU.
        if let Some(hit) = self.pkg_cache.peek(pkg) {
            return hit.map_err(unwrap_arc_err);
        }
        let key = pkg.to_string();
        let root = self.root.clone();
        let patterns = self.build_file_patterns.clone();
        let file_cache = self.file_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let registry = self.function_registry.get().cloned().unwrap_or_default();
        let globals = self.globals.clone();
        let walker = self.walker.clone();
        let packages = self.packages();
        let loads = self.loads.clone();
        self.pkg_cache
            .once(
                key.clone(),
                enclose!((key) move || async move {
                    // Bound the fan-out before queueing: see `PKG_EVAL_SLOTS`.
                    // Waited for here rather than inside the closure so the wait
                    // happens in async-land, not inside a blocking job — parking
                    // a run slot to wait for a run slot is the deadlock.
                    let slot = PKG_EVAL_SLOTS
                        .acquire()
                        .await
                        .context("acquiring a package-evaluation slot")?;
                    // Starlark evaluation of a whole package: the single heaviest
                    // synchronous unit in a build, and one per package. On a runtime
                    // worker it stops that worker polling anything at all — see
                    // `hcore::blocking`.
                    hcore::blocking::run(move || -> anyhow::Result<Arc<RunResult>> {
                        // The permit rides *into* the job rather than being held
                        // across the await. `PKG_EVAL_SLOTS` is a static, so the
                        // guard is already `'static`, and this costs nothing.
                        //
                        // It matters because a permit held across an await is
                        // released only by a poll of this future — and plain
                        // futures can stop being polled without being dropped
                        // (a `buffered(K)` walk whose consumer parks, the shape
                        // `Engine::query` used to have). Moved into the job's
                        // closure, the permit has exactly two fates, neither of
                        // which needs the caller polled: the job runs to
                        // completion even when its awaiter is gone (detached,
                        // `hcore::blocking::run`) and releases at job end, or
                        // the closure is dropped un-run with the future and
                        // releases then. This body is also a memoized *task*
                        // (`pkg_cache` spawns it), so it is either polled by the
                        // runtime or aborted-and-dropped — never parked forever
                        // mid-await holding the permit.
                        let _slot = slot;
                        let loader =
                            BuildFileLoader::new(root, patterns, file_cache, dir_cache, registry, globals, walker, packages, loads);
                        loader
                            .load_pkg(&key)
                            .with_context(|| format!("pkg: `{}`", key))
                    })
                    .await
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }
}

/// Resolves `load(...)` paths against the workspace root and recursively evaluates
/// referenced BUILD files. Shared caches ensure a file/dir is parsed at most once
/// even when reached via different load paths or via top-level `run_pkg`.
pub(crate) struct BuildFileLoader {
    root: PathBuf,
    patterns: Vec<glob::Pattern>,
    file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Files/dirs currently being evaluated on this call chain — guards against `load()` cycles.
    in_flight: Mutex<HashSet<PathBuf>>,
    /// Provider functions exposed as `heph.<provider>.<fn>`. Used to build `globals`.
    registry: Arc<ProviderFunctionRegistry>,
    /// Lazily-built, provider-lifetime Starlark globals (shared across loaders so
    /// the registry-driven namespace is built at most once).
    globals: Arc<OnceLock<Globals>>,
    /// Shared cross-run fs-walk cache. `find_build_files` lists each package dir
    /// through it, so an unchanged dir skips the `readdir` syscall.
    walker: Arc<CachedWalker>,
    /// The sorted workspace package list backing `heph.core.packages()`, bound to
    /// the root/patterns/skip/walker it is walked from. Scoped to whatever built
    /// it: the engine shares one per provider (a loader is built per package
    /// evaluation, so the tree is walked once for the whole run and both the
    /// Starlark and `Provider::list_packages` paths see one order), while the LSP
    /// deliberately builds a fresh one per loader.
    packages: Arc<PackageList>,
    /// Single-flights the evaluation of a `load()`ed file across concurrent
    /// package evaluations.
    loads: Arc<LoadRegistry>,
    /// This loader's identity in [`LoadRegistry`]'s wait-for graph.
    chain: u64,
}

impl BuildFileLoader {
    #[expect(clippy::too_many_arguments, reason = "loader threads provider fields")]
    pub(crate) fn new(
        root: PathBuf,
        patterns: Vec<glob::Pattern>,
        file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
        dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
        registry: Arc<ProviderFunctionRegistry>,
        globals: Arc<OnceLock<Globals>>,
        walker: Arc<CachedWalker>,
        packages: Arc<PackageList>,
        loads: Arc<LoadRegistry>,
    ) -> Self {
        let chain = loads.new_chain();
        Self {
            root,
            patterns,
            file_cache,
            dir_cache,
            in_flight: Mutex::new(HashSet::new()),
            registry,
            globals,
            walker,
            packages,
            loads,
            chain,
        }
    }

    /// The Starlark globals for this loader, built once from the function registry.
    fn globals(&self) -> &Globals {
        self.globals.get_or_init(|| build_globals(&self.registry))
    }

    /// Top-level entry: evaluate every BUILD file in `pkg`'s directory matching the
    /// configured patterns and merge their targets/states/symbols. Returns an empty
    /// result if the directory is missing, escapes the workspace root, or has no
    /// matching file — this is the path used by `Provider::list`/`get`/`probe`,
    /// where unknown packages should surface as empty/`NotFound` rather than hard
    /// errors. Use `load_dir` directly (via the `FileLoader` impl) for the
    /// `load(...)` path, which is strict.
    ///
    /// `pkg` reaches here from an `Addr`/`PkgBuf` that the address parser does not
    /// itself bound to the workspace (a package segment may contain `..`), so this
    /// is the chokepoint that keeps `root.join(pkg)` from walking outside the
    /// workspace for a crafted address like `//../../etc:x`.
    fn load_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        if hmodel::htpkg::join_rel_checked("", pkg).is_err() {
            return Ok(Arc::new(empty_run_result()?));
        }
        let dir = self.root.join(pkg);
        if !dir.is_dir() {
            return Ok(Arc::new(empty_run_result()?));
        }
        let files = find_build_files(&self.walker, &dir, &self.patterns)?;
        if files.is_empty() {
            return Ok(Arc::new(empty_run_result()?));
        }
        self.load_dir(&dir, pkg)
    }

    fn load_dir(&self, dir: &Path, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        if let Some(cached) = self
            .dir_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("dir_cache lock poisoned"))?
            .get(dir)
        {
            return Ok(cached.clone());
        }

        let files = find_build_files(&self.walker, dir, &self.patterns)?;
        if files.is_empty() {
            let pats: Vec<&str> = self.patterns.iter().map(glob::Pattern::as_str).collect();
            anyhow::bail!(
                "no BUILD file in {} matching patterns {:?}",
                dir.display(),
                pats
            );
        }

        // Cycle guard on the dir entry itself so that load("//a") -> load("//a") is caught
        // even before we recurse into the same files.
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .map_err(|_e| anyhow::anyhow!("in_flight lock poisoned"))?;
            if !in_flight.insert(dir.to_path_buf()) {
                anyhow::bail!("load() cycle detected at {}", dir.display());
            }
        }
        let _guard = InFlightGuard {
            set: &self.in_flight,
            path: dir.to_path_buf(),
        };

        let mut parts = Vec::with_capacity(files.len());
        for file in &files {
            parts.push(
                self.load_file(file, pkg)
                    .with_context(|| format!("file: {}", file.display()))?,
            );
        }

        let merged = Arc::new(merge_pkg_results(&parts)?);
        self.dir_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("dir_cache lock poisoned"))?
            .insert(dir.to_path_buf(), merged.clone());
        Ok(merged)
    }

    fn load_file(&self, file_path: &Path, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        loop {
            if let Some(cached) = self
                .file_cache
                .lock()
                .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
                .get(file_path)
            {
                return Ok(cached.clone());
            }

            // Own-chain cycle, checked before the cross-chain claim: this one is
            // local knowledge and needs no shared state.
            {
                let mut in_flight = self
                    .in_flight
                    .lock()
                    .map_err(|_e| anyhow::anyhow!("in_flight lock poisoned"))?;
                if !in_flight.insert(file_path.to_path_buf()) {
                    anyhow::bail!("load() cycle detected at {}", file_path.display());
                }
            }
            let _guard = InFlightGuard {
                set: &self.in_flight,
                path: file_path.to_path_buf(),
            };

            // Another package may be evaluating this very file — see
            // `LoadRegistry`. Wait for it rather than repeating the work.
            let _claim = match self.loads.claim(file_path, self.chain)? {
                Claim::Owned => ClaimGuard {
                    registry: &self.loads,
                    path: file_path.to_path_buf(),
                },
                // The other chain finished: re-check the cache. If it *failed*,
                // the cache is still empty and this chain takes its turn.
                Claim::Retry => continue,
            };

            let result = Arc::new(
                eval_file(file_path, pkg, self)
                    .with_context(|| format!("evaluating {}", file_path.display()))?,
            );
            self.file_cache
                .lock()
                .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
                .insert(file_path.to_path_buf(), result.clone());
            return Ok(result);
        }
    }
}

/// Single-flights the evaluation of a `load()`ed file across concurrently-evaluating
/// packages.
///
/// `file_cache` is a plain check-then-insert, so N packages that all `load()` the
/// same shared build-file helper each evaluate it: K-fold duplicate work on exactly
/// the file that is shared the most. Serial discovery hides this — there is only ever
/// one package in flight — so it is a latent bug that goes live the moment
/// discovery runs wide.
///
/// It has to block rather than merely deduplicate opportunistically: the whole
/// point is that K-1 chains *wait* instead of repeating the evaluation. And
/// blocking is what introduces the hazard this type exists to handle.
///
/// **The wait-for graph.** Each evaluating chain holds the paths it is inside and
/// blocks on at most one path held by another chain, so an edge in the wait-for
/// graph is always a real `load()` edge. A cycle in it is therefore exactly a
/// genuine `load()` cycle spread across two chains — which the per-loader
/// `in_flight` set cannot see, because neither chain is cyclic on its own. Left
/// alone that hangs; detected, it reports the same `load() cycle` error a
/// single-chain cycle already does.
#[derive(Default)]
pub(crate) struct LoadRegistry {
    state: Mutex<LoadRegistryState>,
    progress: std::sync::Condvar,
    next_chain: std::sync::atomic::AtomicU64,
    /// Evaluations actually started, i.e. claims granted. The number this type
    /// exists to keep at one per file however many packages want it.
    started: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct LoadRegistryState {
    /// Path → the chain currently evaluating it.
    holder: HashMap<PathBuf, u64>,
    /// Chain → the path it is blocked on, if any.
    waiting: HashMap<u64, PathBuf>,
}

/// Outcome of trying to take ownership of a path's evaluation.
enum Claim {
    /// This chain owns it; evaluate, then `release`.
    Owned,
    /// Another chain finished (or failed) it — re-check the cache and retry.
    Retry,
}

impl LoadRegistry {
    /// A fresh chain identity. One per [`BuildFileLoader`], which is one per
    /// package evaluation and stays on a single thread, so a chain is exactly a
    /// `load()` call stack.
    pub(crate) fn new_chain(&self) -> u64 {
        self.next_chain
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn claim(&self, path: &Path, chain: u64) -> anyhow::Result<Claim> {
        let mut state = self
            .state
            .lock()
            .map_err(|_e| anyhow::anyhow!("load registry lock poisoned"))?;
        loop {
            let Some(&owner) = state.holder.get(path) else {
                state.holder.insert(path.to_path_buf(), chain);
                state.waiting.remove(&chain);
                self.started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Ok(Claim::Owned);
            };
            // Follow the wait-for edges from the owner. Reaching this chain means
            // the two are waiting on each other, which can only happen through a
            // real `load()` cycle.
            let mut at = owner;
            loop {
                if at == chain {
                    state.waiting.remove(&chain);
                    anyhow::bail!("load() cycle detected at {}", path.display());
                }
                match state.waiting.get(&at).and_then(|p| state.holder.get(p)) {
                    Some(&next) => at = next,
                    None => break,
                }
            }
            state.waiting.insert(chain, path.to_path_buf());
            state = self
                .progress
                .wait(state)
                .map_err(|_e| anyhow::anyhow!("load registry condvar poisoned"))?;
            // The owner is done — cached on success, absent on failure. Either
            // way the caller re-checks the cache, so hand it back rather than
            // silently taking over.
            if !state.holder.contains_key(path) {
                state.waiting.remove(&chain);
                return Ok(Claim::Retry);
            }
        }
    }

    /// Evaluations started since this registry was created.
    ///
    /// Only the single-flight tests read this; the counter itself is always
    /// maintained so the two paths cannot drift.
    #[cfg(test)]
    pub(crate) fn started(&self) -> u64 {
        self.started.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn release(&self, path: &Path) {
        if let Ok(mut state) = self.state.lock() {
            state.holder.remove(path);
        }
        self.progress.notify_all();
    }
}

/// Releases a [`LoadRegistry`] claim however the evaluation ends — including a
/// Starlark evaluation that returns `Err` partway down a `load()` chain.
struct ClaimGuard<'a> {
    registry: &'a LoadRegistry,
    path: PathBuf,
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.registry.release(&self.path);
    }
}

struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<PathBuf>>,
    path: PathBuf,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.path);
        }
    }
}

/// Per-eval [`FileLoader`] view that knows which package the calling BUILD file lives in,
/// so `load("./foo.BUILD")` and `load("../other/foo.BUILD")` resolve relative to that
/// package rather than the workspace root.
struct ScopedLoader<'a> {
    inner: &'a BuildFileLoader,
    current_pkg: &'a str,
}

impl FileLoader for ScopedLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.inner
            .load_resolved(path, self.current_pkg)
            .map_err(starlark::Error::new_other)
    }
}

impl BuildFileLoader {
    fn load_resolved(&self, path: &str, current_pkg: &str) -> anyhow::Result<FrozenModule> {
        let candidate = resolve_load_target(&self.root, current_pkg, path)?;
        let meta = std::fs::metadata(&candidate)
            .with_context(|| format!("stat load path {}", candidate.display()))?;
        let result = if meta.is_file() {
            let pkg = candidate
                .parent()
                .and_then(|p| p.strip_prefix(&self.root).ok())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            self.load_file(&candidate, &pkg)?
        } else if meta.is_dir() {
            let pkg = candidate
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            self.load_dir(&candidate, &pkg)?
        } else {
            anyhow::bail!(
                "load() path {} is neither file nor directory",
                candidate.display()
            )
        };
        Ok(result.module.clone())
    }
}

/// Resolve a `load()` path argument to an absolute filesystem path, rejecting
/// any path that would resolve outside the workspace root.
///
/// Accepts:
///   `//pkg/...`     absolute, relative to workspace root
///   `./rel/...`     relative to `current_pkg`'s directory
///   `../rel/...`    relative to `current_pkg`'s directory (walks up via `..`)
///
/// Uses [`hmodel::htpkg::join_rel_checked`] — the same boundary check `fs.file`/
/// `fs.glob` apply — so a `..`-laden path (e.g. `load("../../../../etc/hosts")`)
/// errors instead of escaping the workspace and being parsed as Starlark.
pub(crate) fn resolve_load_target(
    root: &Path,
    current_pkg: &str,
    path: &str,
) -> anyhow::Result<PathBuf> {
    let rel_from_root = if let Some(rel) = path.strip_prefix("//") {
        if rel.is_empty() {
            anyhow::bail!("load() path must not be empty after `//`");
        }
        hmodel::htpkg::join_rel_checked("", rel)
    } else if path.starts_with("./") || path.starts_with("../") {
        hmodel::htpkg::join_rel_checked(current_pkg, path)
    } else {
        anyhow::bail!("load() path must start with `//`, `./`, or `../`, got `{path}`");
    }
    .with_context(|| format!("resolving load() path `{path}`"))?;
    // `join_rel_checked` preserves a trailing slash so `fs.glob` can tell a directory
    // path from a file path; `load()` has no such distinction, and a trailing slash on
    // a path that names a real file makes `std::fs::metadata` reject it as not-a-directory.
    Ok(root.join(rel_from_root.trim_end_matches('/')))
}

/// Enumerate every file in `dir` whose name matches any of `patterns`.
/// Result is sorted for deterministic ordering across runs. Propagates IO errors,
/// including "directory does not exist".
fn find_build_files(
    walker: &CachedWalker,
    dir: &Path,
    patterns: &[glob::Pattern],
) -> anyhow::Result<Vec<PathBuf>> {
    // Read through the shared walker: an unchanged package dir skips `readdir`.
    // The listing is already sorted by name, and only regular files are matched
    // (mirroring the prior `file_type().is_file()` — a symlinked BUILD is not a
    // build file here).
    let listing = walker
        .read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?;
    Ok(listing
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .filter(|e| patterns.iter().any(|p| p.matches(&e.name)))
        .map(|e| dir.join(&e.name))
        .collect())
}

fn eval_file(path: &Path, pkg: &str, loader: &BuildFileLoader) -> anyhow::Result<RunResult> {
    let ast: AstModule =
        AstModule::parse_file(path, &Dialect::Extended).map_err(starlark::Error::into_anyhow)?;
    // Normal build path: provenance capture off (walking the call stack per
    // target() is needless overhead unless tooling asks for it).
    eval_ast(ast, pkg, loader, false)
}

/// Parse `content` as a BUILD file named `filename` and evaluate it. Used by the
/// LSP to evaluate in-editor (possibly unsaved) buffers; `capture_provenance` is
/// enabled so each target records its source call sites.
pub(crate) fn eval_source(
    filename: &str,
    content: String,
    pkg: &str,
    loader: &BuildFileLoader,
) -> anyhow::Result<RunResult> {
    let ast: AstModule = AstModule::parse(filename, content, &Dialect::Extended)
        .map_err(starlark::Error::into_anyhow)?;
    eval_ast(ast, pkg, loader, true)
}

fn eval_ast(
    ast: AstModule,
    pkg: &str,
    loader: &BuildFileLoader,
    capture_provenance: bool,
) -> anyhow::Result<RunResult> {
    let globals = loader.globals();

    let targets = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let states = std::rc::Rc::new(std::cell::RefCell::new(vec![]));

    // 0.14 modules are scoped to a temp heap: eval into the module and freeze it
    // inside the closure, returning the frozen result. The `targets`/`states`
    // sinks are owned outside so they survive after the closure drops `extra`.
    let frozen = Module::with_temp_heap(|module| -> anyhow::Result<FrozenModule> {
        let extra = Extra {
            pkg,
            root: &loader.root,
            capture_provenance,
            on_target: {
                let targets = targets.clone();
                Box::new(move |p| {
                    targets.borrow_mut().push(p);

                    Ok(())
                })
            },
            on_state: {
                let states = states.clone();
                Box::new(move |p| {
                    states.borrow_mut().push(p);

                    Ok(())
                })
            },
            list_packages: {
                // Owned clone so the closure outlives this borrow of `loader`.
                let packages = Arc::clone(&loader.packages);
                Box::new(move || packages.get())
            },
        };
        let scoped = ScopedLoader {
            inner: loader,
            current_pkg: pkg,
        };
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&extra);
        eval.set_loader(&scoped);

        eval.eval_module(ast, globals)
            .map_err(starlark::Error::into_anyhow)?;
        // Drop the evaluator (and its borrow of `module`) before freezing.
        drop(eval);
        module
            .freeze()
            .map_err(anyhow::Error::from)
            .context("freezing starlark module")
    })?;

    let targets = std::rc::Rc::try_unwrap(targets)
        .map_err(|_rc| anyhow::anyhow!("targets Rc still has outstanding references after eval"))?
        .into_inner();
    let states = std::rc::Rc::try_unwrap(states)
        .map_err(|_rc| anyhow::anyhow!("states Rc still has outstanding references after eval"))?
        .into_inner();

    Ok(RunResult {
        targets,
        states,
        module: frozen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::htvalue::signature::Param;
    use hplugin::provider::{DeclaredState, DeclaredTarget, FnOutcome};
    use std::fs;
    use tempfile::tempdir;

    fn hovers() -> BuiltinHovers {
        BuiltinHovers::new(&build_globals(&ProviderFunctionRegistry::default()).documentation())
    }

    #[test]
    fn builtin_call_hovers_render_real_signatures() {
        let hovers = hovers();

        // With nothing selected: real args, the `**config` catch-all, and the
        // address return — not the stock `def target(*args, **kwargs) -> None`.
        let target = hovers.target(None);
        assert!(target.contains("name"), "names base arg: {target}");
        assert!(target.contains("driver"), "names driver arg: {target}");
        assert!(target.contains("**config"), "shows config kwargs: {target}");
        assert!(!target.contains("**kwargs"), "no raw kwargs: {target}");
        // The real docstring carries through.
        assert!(target.contains("Declare a build target"), "doc: {target}");
        // Base fields document themselves.
        assert!(
            target.contains("Target name (required)."),
            "base field doc: {target}"
        );

        let ps = hovers.provider_state(None);
        assert!(ps.contains("provider"), "names provider arg: {ps}");
        assert!(ps.contains("**state"), "shows state kwargs: {ps}");
        assert!(!ps.contains("**kwargs"), "no raw kwargs: {ps}");
    }

    #[test]
    fn builtin_call_hovers_narrow_to_the_selected_driver() {
        let schema = DriverSchema {
            fields: vec![
                hplugin::driver::DriverField {
                    name: "cmd".to_string(),
                    ty: ParamType::String,
                    doc: "Command line to run.".to_string(),
                    required: true,
                },
                hplugin::driver::DriverField {
                    name: "verbose".to_string(),
                    ty: ParamType::Bool,
                    doc: "Echo the command.".to_string(),
                    required: false,
                },
            ],
        };
        let md = hovers().target(Some(("exec", &schema)));

        // The driver's fields stand in for the catch-all, in the prototype and
        // in the parameter docs, with required-ness carried across.
        assert!(!md.contains("**config"), "catch-all replaced: {md}");
        assert!(md.contains("cmd"), "prototype names the field: {md}");
        assert!(md.contains("Command line to run."), "field doc: {md}");
        assert!(md.contains("Echo the command."), "optional field doc: {md}");
        assert!(md.contains("verbose"), "optional field: {md}");
        // A driver narrows the config keys; it never displaces `target`'s own
        // spec-level fields. Every one of them is still there, documented.
        for base in target_base_fields() {
            assert!(
                md.contains(&base.name),
                "keeps base arg {}: {md}",
                base.name
            );
            assert!(md.contains(&base.doc), "keeps base doc {}: {md}", base.name);
        }
        // Which half is which: the reader must be able to tell `name` (always
        // there) from `cmd` (there because this target picked `exec`).
        assert!(
            md.contains("*The fields above are `target`'s own, then the `exec` driver's config.*"),
            "attributes each half of the blend: {md}"
        );
    }

    #[test]
    fn builtin_call_hovers_say_so_when_a_driver_takes_no_config() {
        // A config-less driver (`DriverSchema::default()`, what several real
        // drivers return) must read as "no keys", not as a hover that broke —
        // and must still show `target`'s own fields.
        let md = hovers().target(Some(("bare", &DriverSchema::default())));
        assert!(!md.contains("**config"), "catch-all replaced: {md}");
        assert!(md.contains("name"), "keeps base args: {md}");
        assert!(
            md.contains("`bare` driver takes no config"),
            "says the driver takes nothing: {md}"
        );
    }

    #[test]
    fn builtin_call_hovers_narrow_to_the_selected_provider() {
        let schema = hplugin::provider::StateSchema {
            fields: vec![hplugin::provider::StateField {
                name: "go_codegen_root".to_string(),
                ty: ParamType::Bool,
                doc: "Treat this package as a codegen root.".to_string(),
                required: false,
            }],
        };
        let md = hovers().provider_state(Some(("go", &schema)));
        assert!(!md.contains("**state"), "catch-all replaced: {md}");
        assert!(md.contains("go_codegen_root"), "names the field: {md}");
        assert!(
            md.contains("Treat this package as a codegen root."),
            "field doc: {md}"
        );
        assert!(md.contains("provider"), "keeps the base arg: {md}");
        assert!(
            md.contains(
                "*The fields above are `provider_state`'s own, then the `go` provider's state.*"
            ),
            "attributes each half of the blend: {md}"
        );
    }

    #[test]
    fn approval_bool_shorthand() {
        let on = approval_from(htvalue::Value::Bool(true)).expect("parse");
        assert!(on.required);
        assert!(on.notice.is_empty());

        let off = approval_from(htvalue::Value::Bool(false)).expect("parse");
        assert!(!off.required);
    }

    #[test]
    fn approval_absent_defaults_off() {
        let a = approval_from(htvalue::Value::Null()).expect("parse");
        assert_eq!(a, Approval::default());
        assert!(!a.required);
    }

    #[test]
    fn approval_map_form_with_notice() {
        let v = htvalue::Value::Map(HashMap::from([
            ("required".to_string(), htvalue::Value::Bool(true)),
            (
                "notice".to_string(),
                htvalue::Value::List(vec![
                    htvalue::Value::String("plan".to_string()),
                    htvalue::Value::String("diff".to_string()),
                ]),
            ),
        ]));
        let a = approval_from(v).expect("parse");
        assert!(a.required);
        assert_eq!(a.notice, vec!["plan".to_string(), "diff".to_string()]);
    }

    #[test]
    fn approval_map_required_defaults_false() {
        let v = htvalue::Value::Map(HashMap::from([(
            "notice".to_string(),
            htvalue::Value::List(vec![htvalue::Value::String("plan".to_string())]),
        )]));
        let a = approval_from(v).expect("parse");
        assert!(!a.required);
        assert_eq!(a.notice, vec!["plan".to_string()]);
    }

    #[test]
    fn approval_unknown_key_is_rejected() {
        let v = htvalue::Value::Map(HashMap::from([(
            "requierd".to_string(), // typo must fail, not silently disable the gate
            htvalue::Value::Bool(true),
        )]));
        let err = approval_from(v).unwrap_err();
        assert!(format!("{err:#}").contains("unknown entries"), "{err:#}");
    }

    #[test]
    fn approval_required_wrong_type_is_rejected() {
        let v = htvalue::Value::Map(HashMap::from([(
            "required".to_string(),
            htvalue::Value::String("yes".to_string()),
        )]));
        assert!(approval_from(v).is_err());
    }

    /// Two packages loading the same shared file at once must evaluate it once
    /// between them.
    ///
    /// `file_cache` is check-then-insert, so before this every concurrent
    /// package re-evaluated every shared macro file — K-fold duplicate work on
    /// precisely the file that is shared the most. Serial discovery hid it: only
    /// one package was ever in flight.
    ///
    /// Counted at the registry, because the evaluation count is the whole claim
    /// — `file_cache` holds every file the run touched, so its size says nothing
    /// about how many times any one of them was evaluated.
    #[test]
    fn concurrent_packages_evaluate_a_shared_load_once() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // The shared file is slow enough that both packages are inside
        // `load_file` at the same time, which is the case being tested.
        fs::write(
            root.join("shared.BUILD"),
            "N = len([x for x in range(400000)])\n",
        )
        .unwrap();
        for pkg in ["a", "b"] {
            let p = root.join(pkg);
            fs::create_dir_all(&p).unwrap();
            fs::write(
                p.join("BUILD"),
                "load(\"//shared.BUILD\", \"N\")\ntarget(name = \"t\", driver = \"d\")\n",
            )
            .unwrap();
        }

        let provider = Arc::new(Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("BUILD").unwrap()],
            ..Provider::default()
        });

        // Both loaders share the provider's caches and load registry, exactly as
        // two concurrent `run_pkg` calls do.
        std::thread::scope(|s| {
            for pkg in ["a", "b"] {
                s.spawn(enclose!((provider) move || {
                    run_pkg_blocking(&provider, pkg).expect("evaluate package");
                }));
            }
        });

        assert!(
            provider
                .file_cache
                .lock()
                .unwrap()
                .contains_key(&root.join("shared.BUILD")),
            "the shared file must be cached",
        );
        // Three files exist — `a/BUILD`, `b/BUILD`, `shared.BUILD` — so three
        // evaluations is one apiece. Four would be the shared file evaluated
        // twice, which is the defect.
        assert_eq!(
            provider.loads.started(),
            3,
            "the shared file must be evaluated once, not once per package",
        );
    }

    /// A `load()` cycle split across two chains must still report a cycle.
    ///
    /// Neither chain is cyclic on its own, so the per-loader `in_flight` set
    /// cannot see it: `a.BUILD` loads `b.BUILD` and `b.BUILD` loads `a.BUILD`, with one
    /// chain starting at each. Without the wait-for graph check the two block on
    /// each other forever — a hang where the serial code reported an error.
    #[test]
    fn a_load_cycle_across_two_chains_reports_a_cycle_rather_than_hanging() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.BUILD"), "load(\"//b.BUILD\", \"B\")\nA = B\n").unwrap();
        fs::write(root.join("b.BUILD"), "load(\"//a.BUILD\", \"A\")\nB = A\n").unwrap();
        for (pkg, first) in [("pa", "a"), ("pb", "b")] {
            let p = root.join(pkg);
            fs::create_dir_all(&p).unwrap();
            fs::write(
                p.join("BUILD"),
                format!(
                    "load(\"//{first}.BUILD\", \"{}\")\ntarget(name = \"t\", driver = \"d\")\n",
                    first.to_uppercase()
                ),
            )
            .unwrap();
        }

        let provider = Arc::new(Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("BUILD").unwrap()],
            ..Provider::default()
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            for pkg in ["pa", "pb"] {
                s.spawn(enclose!((provider, tx) move || {
                    // The receiver outlives the scope, so a send error is
                    // impossible; ignore it rather than panicking in a thread.
                    let _sent = tx.send(run_pkg_blocking(&provider, pkg).is_err());
                }));
            }
            drop(tx);
            // Both must answer; a hang here is the bug.
            for _ in 0..2 {
                let errored = rx
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("a cross-chain load cycle must error, not deadlock");
                assert!(errored, "a load cycle must be reported as an error");
            }
        });
    }

    fn run_pkg_blocking(provider: &Provider, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        let registry = provider
            .function_registry
            .get()
            .cloned()
            .unwrap_or_default();
        let loader = BuildFileLoader::new(
            provider.root.clone(),
            provider.build_file_patterns.clone(),
            provider.file_cache.clone(),
            provider.dir_cache.clone(),
            registry,
            provider.globals.clone(),
            provider.walker.clone(),
            provider.packages(),
            // The provider's own registry, not a fresh one: two concurrent
            // `run_pkg` calls share it, and that sharing is the point.
            provider.loads.clone(),
        );
        loader.load_pkg(pkg)
    }

    /// Registry exposing the real `fs` provider functions, so `heph.fs.glob` works
    /// in these unit tests (which run the buildfile provider without an engine).
    fn fs_registry() -> Arc<ProviderFunctionRegistry> {
        use hplugin::provider::Provider as _;
        let mut reg = ProviderFunctionRegistry::default();
        reg.insert_provider("fs", hbuiltins::pluginfs::Provider::default().functions());
        Arc::new(reg)
    }

    fn source_loader(root: PathBuf) -> BuildFileLoader {
        let patterns = vec![glob::Pattern::new("BUILD").unwrap()];
        let walker = Arc::new(CachedWalker::disabled());
        BuildFileLoader::new(
            root.clone(),
            patterns.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(ProviderFunctionRegistry::default()),
            Arc::new(OnceLock::new()),
            Arc::clone(&walker),
            Arc::new(PackageList::new(
                root,
                patterns,
                Arc::new(hwalk::Ignore::default()),
                walker,
            )),
            Arc::default(),
        )
    }

    #[test]
    fn test_provenance_captures_macro_and_target_call_sites() {
        // A user macro that emits two targets, plus one direct target(). Each
        // produced target must record the chain of call sites that led to it:
        // the inner target() call and (for the macro ones) the macro call site.
        let tmp = tempdir().unwrap();
        let loader = source_loader(tmp.path().to_path_buf());
        let content = r#"
def my_macro(prefix):
    target(name = prefix + "_a", driver = "exec")
    target(name = prefix + "_b", driver = "exec")

my_macro("m")
target(name = "direct", driver = "exec")
"#;
        let result =
            eval_source("BUILD", content.to_string(), "pkg", &loader).expect("eval_source");

        let by_name: HashMap<&str, &OnTargetPayload> = result
            .targets
            .iter()
            .map(|t| (t.name.as_str(), t))
            .collect();
        assert_eq!(by_name.len(), 3);

        // Each target carries at least one provenance frame, all pointing at this BUILD.
        for t in &result.targets {
            assert!(
                !t.provenance.is_empty(),
                "target {} has no provenance",
                t.name
            );
            assert!(t.provenance.iter().all(|f| f.file == "BUILD"));
        }

        // The macro-produced targets carry a frame inside `my_macro` (the target()
        // call) AND a frame at module level (the `my_macro("m")` call site, line 6).
        let m_a = by_name["m_a"];
        assert!(
            m_a.provenance.iter().any(|f| f.fn_name == "my_macro"),
            "m_a missing my_macro frame: {:?}",
            m_a.provenance
        );
        assert!(
            m_a.provenance.iter().any(|f| f.line_start == 6),
            "m_a missing macro call site at line 6: {:?}",
            m_a.provenance
        );

        // The direct target's innermost frame is its own call site (line 7), and it
        // is NOT attributed to my_macro.
        let direct = by_name["direct"];
        assert!(direct.provenance.iter().any(|f| f.line_start == 7));
        assert!(direct.provenance.iter().all(|f| f.fn_name != "my_macro"));
    }

    #[test]
    fn test_provenance_off_on_normal_eval() {
        // eval_file (normal build path) must not pay for provenance capture.
        let tmp = tempdir().unwrap();
        let pkg_dir = tmp.path().join("p");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(
            pkg_dir.join("BUILD"),
            "target(name=\"t\", driver=\"exec\")\n",
        )
        .unwrap();
        let loader = source_loader(tmp.path().to_path_buf());
        let result = loader.load_pkg("p").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert!(result.targets[0].provenance.is_empty());
    }

    #[test]
    fn find_build_files_through_enabled_walker_finds_build() {
        // `find_build_files` now lists each package dir through the shared
        // CachedWalker. With a real (enabled) walker backing it, package
        // discovery must still find the BUILD file and evaluate its targets.
        let tmp = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let pkg = "p";
        let pkg_dir = tmp.path().join(pkg);
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("BUILD"), "target(name=\"t\", driver=\"d\")\n").unwrap();

        let provider = Provider {
            root: tmp.path().to_path_buf(),
            walker: Arc::new(CachedWalker::open(&dbdir.path().join("fswalk.db"))),
            ..Provider::default()
        };

        let result = run_pkg_blocking(&provider, pkg).unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "t");
    }

    #[test]
    fn test_run_file_inner() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
    labels = ["label1", "label2"],
    config_str = "hello",
    config_int = 42,
    config_bool = True,
    config_float = 1.5,
    config_list = ["a", 1],
)
"#;
        let filename = "BUILD".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let result = run_pkg_blocking(&provider, &pkg_name).unwrap();

        assert_eq!(result.targets.len(), 1);
        let target = &result.targets[0];
        assert_eq!(target.name, "mytarget");
        assert_eq!(target.driver, "mydriver");
        assert_eq!(
            target.labels,
            vec!["label1".to_string(), "label2".to_string()]
        );

        if let Some(htvalue::Value::String(s)) = target.config.get("config_str") {
            assert_eq!(s, "hello");
        } else {
            panic!(
                "Expected string for config_str, got {:?}",
                target.config.get("config_str")
            );
        }

        if let Some(htvalue::Value::Int(i)) = target.config.get("config_int") {
            assert_eq!(*i, 42);
        } else {
            panic!(
                "Expected int for config_int, got {:?}",
                target.config.get("config_int")
            );
        }

        if let Some(htvalue::Value::Bool(b)) = target.config.get("config_bool") {
            assert!(*b);
        } else {
            panic!(
                "Expected bool for config_bool, got {:?}",
                target.config.get("config_bool")
            );
        }

        if let Some(htvalue::Value::Float(f)) = target.config.get("config_float") {
            assert_eq!(*f, 1.5);
        } else {
            panic!(
                "Expected float for config_float, got {:?}",
                target.config.get("config_float")
            );
        }

        if let Some(htvalue::Value::List(l)) = target.config.get("config_list") {
            assert_eq!(l.len(), 2);
            if let htvalue::Value::String(s) = &l[0] {
                assert_eq!(s, "a");
            } else {
                panic!("Expected string in list");
            }
            if let htvalue::Value::Int(i) = &l[1] {
                assert_eq!(*i, 1);
            } else {
                panic!("Expected int in list");
            }
        } else {
            panic!("Expected list for config_list");
        }
    }

    fn make_provider(tmp_dir: &tempfile::TempDir) -> Provider {
        let p = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        // Wire the fs provider's functions so `heph.fs.glob` resolves in tests.
        assert!(p.function_registry.set(fs_registry()).is_ok());
        p
    }

    fn run_transitive(build_content: &str) -> anyhow::Result<Sandbox> {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, pkg_name)?;
        Ok(result
            .targets
            .first()
            .map(|t| t.transitive.clone())
            .unwrap_or_default())
    }

    #[test]
    fn starlark_eval_error_surfaces_location_and_cause() {
        // A Starlark evaluation error must convert (via into_anyhow) into an
        // error whose chain names the offending symbol and the BUILD file.
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        // Reference to an undefined symbol → eval error.
        fs::write(pkg.join("BUILD"), "X = undefined_symbol\n").unwrap();
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "p").unwrap_err();

        let chain = format!("{err:#}");
        assert!(
            chain.contains("undefined_symbol"),
            "eval error must name the offending symbol: {chain}"
        );
    }

    #[test]
    fn test_sandbox_empty_by_default() {
        let sandbox = run_transitive(r#"target(name="t", driver="d")"#).unwrap();
        assert!(sandbox.deps.is_empty());
        assert!(sandbox.tools.is_empty());
        assert!(sandbox.env.is_empty());
    }

    #[test]
    fn test_sandbox_null_transitive() {
        let sandbox = run_transitive(r#"target(name="t", driver="d", transitive=None)"#);
        // None is not a valid starlark value here; expect error or default
        // starlark doesn't have None by default in our globals, so this should error
        assert!(sandbox.is_err() || sandbox.unwrap().empty());
    }

    #[test]
    fn test_sandbox_deps_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"group1": ["//mypkg:other"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        let dep = &sandbox.deps[0];
        assert_eq!(dep.group, "group1");
        assert!(dep.runtime);
        assert!(dep.hash);
        assert_eq!(dep.r#ref.r#ref.name, "other");
        assert_eq!(dep.id, "dep|group1|0");
    }

    #[test]
    fn test_sandbox_multiple_deps() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"g": ["//mypkg:a", "//mypkg:b"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 2);
        let names: Vec<_> = sandbox
            .deps
            .iter()
            .map(|d| d.r#ref.r#ref.name.as_str())
            .collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        // Each dep in the group gets its own id. The id names the per-input list
        // file the exec driver reads back to build `$SRC_<group>`, and that
        // lookup takes the *first* input with a matching id — so two deps
        // sharing one id makes both answer with the same (shared) list file and
        // every path lands in `$SRC_G` twice.
        let ids: Vec<&str> = sandbox.deps.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["dep|g|0", "dep|g|1"], "ids must be per-dep");
    }

    /// `deps`/`tools` parse into a `HashMap` whose iteration order is randomized
    /// per instance, so an id derived from that order differs between two parses
    /// of the same BUILD file — and every id reaches a dependent's def hash
    /// through `Input::origin_id`. Two groups are enough to fire it; one is
    /// always index 0, which is why the single-group fixtures above never did.
    #[test]
    fn test_sandbox_ids_stable_across_group_order() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"a": ["//mypkg:a"], "b": ["//mypkg:b"], "c": ["//mypkg:c"],
                 "d": ["//mypkg:d"], "e": ["//mypkg:e"], "f": ["//mypkg:f"]},
        "tools": {"ta": ["//mypkg:ta"], "tb": ["//mypkg:tb"], "tc": ["//mypkg:tc"],
                  "td": ["//mypkg:td"], "te": ["//mypkg:te"], "tf": ["//mypkg:tf"]},
    },
)
"#;
        // Keyed by ref so the comparison is order-independent: what must be
        // stable is the id each dep gets, not the order they were pushed in.
        let ids = |sb: &Sandbox| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = sb
                .deps()
                .iter()
                .map(|d| (d.r#ref.r#ref.name.clone(), d.id.clone()))
                .chain(
                    sb.tools()
                        .iter()
                        .map(|t| (t.r#ref.r#ref.name.clone(), t.id.clone())),
                )
                .collect();
            v.sort();
            v
        };

        let first = ids(&run_transitive(content).unwrap());
        for i in 0..8 {
            let again = ids(&run_transitive(content).unwrap());
            assert_eq!(
                first,
                again,
                "ids differ between parse 0 and parse {}",
                i + 1
            );
        }
    }

    #[test]
    fn test_sandbox_tools_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "tools": {"toolgroup": ["//mypkg:mytool"]},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.tools.len(), 1);
        let tool = &sandbox.tools[0];
        assert_eq!(tool.group, "toolgroup");
        assert!(tool.hash);
        assert_eq!(tool.r#ref.r#ref.name, "mytool");
        assert_eq!(tool.id, "tool|toolgroup|0");
    }

    #[test]
    fn test_sandbox_env_parsed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "env": {"MY_VAR": "my_value"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.env.len(), 1);
        let env = sandbox.env.get("MY_VAR").unwrap();
        assert!(env.hash);
        assert!(!env.append);
        assert!(env.append_prefix.is_empty());
        match &env.value {
            EnvValue::Literal(s) => assert_eq!(s, "my_value"),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn test_sandbox_all_fields() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "deps": {"dg": ["//mypkg:dep1"]},
        "tools": {"tg": ["//mypkg:tool1"]},
        "env": {"K": "V"},
    },
)
"#;
        let sandbox = run_transitive(content).unwrap();
        assert_eq!(sandbox.deps.len(), 1);
        assert_eq!(sandbox.tools.len(), 1);
        assert_eq!(sandbox.env.len(), 1);
    }

    #[test]
    fn test_sandbox_unknown_key_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = {
        "unknown_key": "value",
    },
)
"#;
        assert!(run_transitive(content).is_err());
    }

    #[test]
    fn test_sandbox_not_map_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    transitive = "bad",
)
"#;
        assert!(run_transitive(content).is_err());
    }

    /// Whole-package Starlark evaluation is the heaviest synchronous unit in a
    /// build and it shares one unbounded FIFO with sub-millisecond manifest
    /// reads. Fanned out without a cap it fills every thread of
    /// `hcore::blocking` with hundreds-of-milliseconds jobs, and the short work
    /// queued behind them — the pool is arrival-fair, so this is head-of-line
    /// latency rather than starvation.
    ///
    /// Asserted by holding every slot: an evaluation must wait for one, so it
    /// cannot be reaching the pool while the cap is exhausted.
    ///
    /// Note it borrows a process-wide semaphore, so it briefly delays any other
    /// test in this binary that evaluates a package.
    #[tokio::test]
    async fn package_evaluation_waits_for_a_slot() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(
            pkg_path.join("BUILD"),
            r#"target(name = "t", driver = "d")"#,
        )
        .unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("BUILD").unwrap()],
            ..Provider::default()
        };

        let held = PKG_EVAL_SLOTS
            .acquire_many(u32::try_from(pkg_eval_slots()).unwrap())
            .await
            .expect("hold every evaluation slot");

        let eval = provider.run_pkg(&pkg_name);
        tokio::pin!(eval);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut eval)
                .await
                .is_err(),
            "an evaluation must not reach a blocking job while every slot is held",
        );

        drop(held);
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), eval)
            .await
            .expect("releasing the slots must let the evaluation through")
            .unwrap();
        assert_eq!(result.targets.len(), 1);
    }

    /// `LoadRegistry::claim` parks its blocking job on a condvar until the
    /// claim's holder — another evaluation, in another blocking job — finishes
    /// the file. Every holder must therefore be able to run, which requires
    /// strictly fewer concurrent evaluations than `hcore::blocking` run slots.
    /// The formulas live in different crates (`pkg_eval_slots` here,
    /// `concurrency_limit` in `hcore`), so pin the inequality; the
    /// `PKG_EVAL_SLOTS` initializer asserts it at runtime for non-test
    /// binaries.
    #[test]
    fn eval_slots_stay_strictly_below_the_blocking_limit() {
        assert!(
            pkg_eval_slots() < hcore::blocking::concurrency_limit(),
            "pkg_eval_slots ({}) must stay strictly below hcore::blocking::concurrency_limit ({})",
            pkg_eval_slots(),
            hcore::blocking::concurrency_limit()
        );
    }

    #[tokio::test]
    async fn test_run_pkg_inner_multiple_patterns() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "mytarget",
    driver = "mydriver",
)
"#;
        let filename = "BUILD.heph".to_string();
        fs::write(pkg_path.join(&filename), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("BUILD.heph").unwrap(),
            ],
            ..Provider::default()
        };

        let result = provider.run_pkg(&pkg_name).await.unwrap();

        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "mytarget");
    }

    fn run_target_config(build_content: &str) -> HashMap<String, htvalue::Value> {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, pkg_name).unwrap();
        result.targets.first().unwrap().config.clone()
    }

    /// [`run_target_config`] for a BUILD file expected to fail: returns the full
    /// error chain, so a test can assert on the cause rather than the top frame.
    fn run_target_config_err(build_content: &str) -> String {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();
        let provider = make_provider(&tmp_dir);
        let err =
            run_pkg_blocking(&provider, pkg_name).expect_err("expected BUILD evaluation to fail");
        format!("{err:#}")
    }

    #[test]
    fn test_starlark_file_pkg_relative_by_default() {
        // run_target_config runs in pkg "mypkg" — file("src/main.rs") should resolve
        // to "mypkg/src/main.rs".
        let content = r#"
target(
    name = "t",
    driver = "d",
    src = file("src/main.rs"),
)
"#;
        let config = run_target_config(content);
        let expected = hbuiltins::pluginfs::file_addr("mypkg/src/main.rs").format();
        match config.get("src") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected file addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_file_abs_skips_pkg_prefix() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    src = file("vendor/x.rs", abs = True),
)
"#;
        let config = run_target_config(content);
        let expected = hbuiltins::pluginfs::file_addr("vendor/x.rs").format();
        match config.get("src") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected file addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_glob_pkg_relative_by_default() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("src/**/*.rs"),
)
"#;
        let config = run_target_config(content);
        let expected = hbuiltins::pluginfs::glob_addr("mypkg/src/**/*.rs", &[]).format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    // The fs driver matches pattern and excludes against the same
    // workspace-root-relative paths, so a package-relative `exclude` must be
    // prefixed exactly like the pattern — left raw, `vendor/**` in `//mypkg`
    // matches `vendor/x.go` at the root and excludes nothing from the glob.
    #[test]
    fn test_starlark_glob_with_exclude_pkg_relative() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = ["vendor/**", "./gen/**"]),
)
"#;
        let config = run_target_config(content);
        let expected =
            hbuiltins::pluginfs::glob_addr("mypkg/**/*.go", &["mypkg/vendor/**", "mypkg/gen/**"])
                .format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    // A bare-string `exclude` takes the same path as the list form.
    #[test]
    fn test_starlark_glob_with_string_exclude_pkg_relative() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = "vendor/**"),
)
"#;
        let config = run_target_config(content);
        let expected =
            hbuiltins::pluginfs::glob_addr("mypkg/**/*.go", &["mypkg/vendor/**"]).format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    // `**/…` keeps matching anywhere under the package: the prefixed
    // `mypkg/**/*.pb.go` still matches `mypkg/x.pb.go` (`**` spans zero
    // components), so the common idiom is unaffected.
    #[test]
    fn test_starlark_glob_exclude_doublestar_prefixed() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = ["**/*.pb.go"]),
)
"#;
        let config = run_target_config(content);
        let expected =
            hbuiltins::pluginfs::glob_addr("mypkg/**/*.go", &["mypkg/**/*.pb.go"]).format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    #[test]
    fn test_starlark_glob_abs_skips_pkg_prefix() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.rs", abs = True),
)
"#;
        let config = run_target_config(content);
        let expected = hbuiltins::pluginfs::glob_addr("**/*.rs", &[]).format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    // `abs = True` governs both sides: neither pattern nor excludes are
    // prefixed, so a workspace-rooted exclude stays writable.
    #[test]
    fn test_starlark_glob_abs_skips_pkg_prefix_for_exclude() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.rs", exclude = ["vendor/**"], abs = True),
)
"#;
        let config = run_target_config(content);
        let expected = hbuiltins::pluginfs::glob_addr("**/*.rs", &["vendor/**"]).format();
        match config.get("srcs") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, &expected),
            other => panic!("expected glob addr string, got {:?}", other),
        }
    }

    // A `..` in an exclude escapes the workspace root exactly as it does in the
    // pattern, rather than being silently accepted and matching nothing.
    #[test]
    fn test_starlark_glob_exclude_escaping_root_errors() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = ["../../../etc/**"]),
)
"#;
        let err = run_target_config_err(content);
        assert!(
            err.contains("escapes workspace root"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn test_load_package() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("BUILD"),
            r#"
MY_DRIVER = "shared_driver"
MY_LABELS = ["a", "b"]
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "MY_DRIVER", "MY_LABELS")
target(
    name = "t",
    driver = MY_DRIVER,
    labels = MY_LABELS,
)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "shared_driver");
        assert_eq!(result.targets[0].labels, vec!["a", "b"]);
    }

    #[test]
    fn test_load_file_explicit() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("macros.BUILD"),
            r#"
def make_name():
    return "from_macro"
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib/macros.BUILD", "make_name")
target(
    name = make_name(),
    driver = "d",
)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "from_macro");
    }

    #[test]
    fn test_load_missing_symbol_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), "X = 1\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//lib", "DOES_NOT_EXIST")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_missing_package_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//nonexistent", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_cycle_detected() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(
            a.join("BUILD"),
            r#"load("//b", "Y")
X = Y
"#,
        )
        .unwrap();

        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(
            b.join("BUILD"),
            r#"load("//a", "X")
Y = X
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "a").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("cycle"), "{chain}");
    }

    #[test]
    fn test_pkg_with_multiple_build_files_merged() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let pkg = root.join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
target(name = "a", driver = "d")
SHARED_A = "from_a"
"#,
        )
        .unwrap();
        fs::write(
            pkg.join("more.BUILD"),
            r#"
target(name = "b", driver = "d")
SHARED_B = "from_b"
"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("*.BUILD").unwrap(),
            ],
            ..Provider::default()
        };
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let names: Vec<&str> = result.targets.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"a"), "{names:?}");
        assert!(names.contains(&"b"), "{names:?}");
        assert_eq!(result.targets.len(), 2);
    }

    #[test]
    fn test_load_merged_symbols_from_multi_file_pkg() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), "FOO = \"foo\"\n").unwrap();
        fs::write(lib.join("more.BUILD"), "BAR = \"bar\"\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "FOO", "BAR")
target(name = "t", driver = FOO + BAR)
"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("*.BUILD").unwrap(),
            ],
            ..Provider::default()
        };
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "foobar");
    }

    #[test]
    fn test_load_missing_dir_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//does/not/exist", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "app").is_err());
    }

    #[test]
    fn test_load_dir_with_no_matching_pattern_errors() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        // lib dir exists but contains no file matching the patterns.
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("README"), "not a build file").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("BUILD"), r#"load("//lib", "X")"#).unwrap();

        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "app").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("no BUILD file"), "{chain}");
    }

    #[test]
    fn test_run_pkg_missing_dir_returns_empty() {
        let tmp_dir = tempdir().unwrap();
        let provider = make_provider(&tmp_dir);
        let r = run_pkg_blocking(&provider, "nope").unwrap();
        assert!(r.targets.is_empty());
        assert!(r.states.is_empty());
    }

    /// `pkg` comes from an `Addr`'s package segment, which the address parser does
    /// not itself bound to the workspace root — a package like `"../secret"` reaches
    /// `Provider::get`/`list`/`probe` (and therefore `load_pkg`) unchecked. Without a
    /// boundary check here, `root.join(pkg)` would walk outside the workspace and
    /// parse whatever BUILD file lives there, the same class of escape `load()`
    /// resolution is guarded against elsewhere in this file.
    #[test]
    fn test_run_pkg_escaping_package_addr_returns_empty() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let secret = tmp_dir.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        fs::write(
            secret.join("BUILD"),
            r#"target(name = "t", driver = "leaked")"#,
        )
        .unwrap();

        let provider = Provider {
            root,
            ..Provider::default()
        };
        let r = run_pkg_blocking(&provider, "../secret").unwrap();
        assert!(r.targets.is_empty(), "{:?}", r.targets);
    }

    #[test]
    fn test_run_pkg_dir_without_match_returns_empty() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("empty");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("README"), "").unwrap();
        let provider = make_provider(&tmp_dir);
        let r = run_pkg_blocking(&provider, "empty").unwrap();
        assert!(r.targets.is_empty());
    }

    #[test]
    fn test_stdlib_type_builtin() {
        let content = r#"
deps_kind = type(["a", "b"])
str_kind = type("x")
target(name = "t", driver = "d", deps_kind = deps_kind, str_kind = str_kind)
"#;
        let config = run_target_config(content);
        match config.get("deps_kind") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "list"),
            other => panic!("expected list type string, got {other:?}"),
        }
        match config.get("str_kind") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "string"),
            other => panic!("expected string type string, got {other:?}"),
        }
    }

    #[test]
    fn test_stdlib_type_used_in_conditional() {
        let content = r#"
def coerce(deps):
    if type(deps) != "list":
        return [deps]
    return deps

target(name = "t", driver = "d", deps = coerce("single"))
"#;
        let config = run_target_config(content);
        match config.get("deps") {
            Some(htvalue::Value::List(l)) => {
                assert_eq!(l.len(), 1);
                match &l[0] {
                    htvalue::Value::String(s) => assert_eq!(s, "single"),
                    other => panic!("expected string in list, got {other:?}"),
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn test_heph_core_pkg_returns_current_pkg() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("some").join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", here = heph.core.pkg())"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "some/pkg").unwrap();
        assert_eq!(result.targets.len(), 1);
        match result.targets[0].config.get("here") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "some/pkg"),
            other => panic!("expected pkg string, got {other:?}"),
        }
    }

    #[test]
    fn test_heph_core_pkg_in_loaded_file_reports_loader_pkg() {
        // load("//lib", ...) evaluates lib's BUILD under pkg "lib" — heph.core.pkg()
        // there returns "lib", not the caller's package.
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("BUILD"), r#"WHERE = heph.core.pkg()"#).unwrap();
        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "WHERE")
target(name = "t", driver = "d", loaded_from = WHERE, here = heph.core.pkg())
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        let cfg = &result.targets[0].config;
        match cfg.get("loaded_from") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "lib"),
            other => panic!("expected lib, got {other:?}"),
        }
        match cfg.get("here") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "app"),
            other => panic!("expected app, got {other:?}"),
        }
    }

    #[test]
    fn test_provider_state_records_payload() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
provider_state(provider = "go", root = "src", strict = True)
target(name = "t", driver = "d")
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "p").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.states.len(), 1);
        let s = &result.states[0];
        assert_eq!(s.provider, "go");
        assert_eq!(
            s.args.get("root"),
            Some(&htvalue::Value::String("src".to_string()))
        );
        assert_eq!(s.args.get("strict"), Some(&htvalue::Value::Bool(true)));
        assert!(!s.args.contains_key("provider"));
    }

    #[test]
    fn test_provider_state_requires_provider_kwarg() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), "provider_state(root = \"x\")\n").unwrap();
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "p").expect_err("must error");
        assert!(format!("{err:#}").contains("missing provider"), "{err:#}");
    }

    #[test]
    fn test_struct_builtin_returns_dict() {
        let content = r#"
s = struct(name = "n", driver = "d", count = 3)
target(
    name = s["name"],
    driver = s["driver"],
    cfg = s,
)
"#;
        let config = run_target_config(content);
        match config.get("cfg") {
            Some(htvalue::Value::Map(m)) => {
                match m.get("name") {
                    Some(htvalue::Value::String(s)) => assert_eq!(s, "n"),
                    other => panic!("expected name string, got {other:?}"),
                }
                match m.get("driver") {
                    Some(htvalue::Value::String(s)) => assert_eq!(s, "d"),
                    other => panic!("expected driver string, got {other:?}"),
                }
                match m.get("count") {
                    Some(htvalue::Value::Int(i)) => assert_eq!(*i, 3),
                    other => panic!("expected count int, got {other:?}"),
                }
            }
            other => panic!("expected dict, got {other:?}"),
        }
    }

    #[test]
    fn test_heph_core_host_builtins() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    os = heph.core.os(),
    arch = heph.core.arch(),
    os_raw = heph.core.os_raw(),
    arch_raw = heph.core.arch_raw(),
)
"#;
        let config = run_target_config(content);
        let expect = |key: &str, want: &str| match config.get(key) {
            Some(htvalue::Value::String(s)) => assert_eq!(s, want, "for {key}"),
            other => panic!("expected {key} string, got {other:?}"),
        };
        expect("os", hcore::htplatform::os());
        expect("arch", hcore::htplatform::arch());
        expect("os_raw", std::env::consts::OS);
        expect("arch_raw", std::env::consts::ARCH);
    }

    #[test]
    fn test_heph_core_num_cpu() {
        let content = r#"target(name = "t", driver = "d", n = heph.core.num_cpu())"#;
        let config = run_target_config(content);
        match config.get("n") {
            // Host CPU count is machine-dependent, so assert only the invariant:
            // it's a positive integer.
            Some(htvalue::Value::Int(i)) => assert!(*i >= 1, "num_cpu should be >= 1, got {i}"),
            other => panic!("expected num_cpu int, got {other:?}"),
        }
    }

    /// Lay down `foo`, `foo/bar`, `other` packages (each a dir with a BUILD) and
    /// evaluate `heph.core.packages("//foo/...")` from `foo`.
    #[test]
    fn test_heph_core_packages_prefix() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for p in ["foo", "foo/bar", "other"] {
            let d = root.join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("BUILD"), "").unwrap();
        }
        fs::write(
            root.join("foo/BUILD"),
            r#"target(name = "t", driver = "d", pkgs = heph.core.packages("//foo/..."))"#,
        )
        .unwrap();
        let provider = make_provider(&tmp);
        let result = run_pkg_blocking(&provider, "foo").expect("eval foo");
        let cfg = &result.targets[0].config;
        let names: Vec<&str> = match cfg.get("pkgs").unwrap() {
            htvalue::Value::List(v) => v
                .iter()
                .map(|e| match e {
                    htvalue::Value::String(s) => s.as_str(),
                    other => panic!("expected string pkg, got {other:?}"),
                })
                .collect(),
            other => panic!("expected pkgs list, got {other:?}"),
        };
        // `//foo/...` = the prefix `foo`: matches `foo` and `foo/bar`, not `other`
        // (nor the root package).
        assert_eq!(names, vec!["foo", "foo/bar"]);
    }

    /// The `pkgs` config value a BUILD file built from `heph.core.packages(...)`.
    fn packages_config(result: &RunResult) -> Vec<String> {
        match result.targets[0].config.get("pkgs").expect("pkgs config") {
            htvalue::Value::List(v) => v
                .iter()
                .map(|e| match e {
                    htvalue::Value::String(s) => s.clone(),
                    other => panic!("expected string pkg, got {other:?}"),
                })
                .collect(),
            other => panic!("expected pkgs list, got {other:?}"),
        }
    }

    /// `heph.core.packages()` must return the same packages in the same order on
    /// every run: the list goes straight into the calling target's config and so
    /// into its **def hash**. The walk accumulates into a `HashSet` whose
    /// iteration order is reseeded per instance, so the sort in `PackageList` is
    /// the only thing between an unchanged tree and a definition that changes
    /// identity run to run.
    ///
    /// Asserted against the exact expected vector, not "is sorted": the point is
    /// that the order is *this* one — byte-lexicographic, the same one shipped
    /// today — so nobody's cache is invalidated by this change either.
    #[test]
    fn test_heph_core_packages_order_is_stable_across_runs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // Enough packages that a `HashSet` happening to iterate in sorted order
        // is not an outcome worth worrying about (40! orderings).
        let names: Vec<String> = (0..40).map(|i| format!("w/p{i:02}")).collect();
        for p in &names {
            let d = root.join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("BUILD"), "").unwrap();
        }
        fs::write(
            root.join("w/BUILD"),
            r#"target(name = "t", driver = "d", pkgs = heph.core.packages("//w/..."))"#,
        )
        .unwrap();

        let mut expected = vec!["w".to_string()];
        expected.extend(names.iter().cloned());

        for run in 0..5 {
            // A fresh provider per run means a fresh `HashSet` seed — which is
            // exactly what differs between two real runs over the same tree.
            let provider = make_provider(&tmp);
            let result = run_pkg_blocking(&provider, "w").expect("eval w");
            assert_eq!(packages_config(&result), expected, "run {run}");
        }
    }

    /// The package list is walked once and then frozen for the provider's
    /// lifetime. Two packages evaluated in the same run must observe the same
    /// set even if the tree changes in between (a codegen target writing into the
    /// workspace, say) — otherwise a target's def hash would depend on the order
    /// packages happened to be evaluated in, which under concurrent discovery is
    /// not even stable within one machine.
    #[test]
    fn test_heph_core_packages_frozen_within_a_run() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let build = r#"target(name = "t", driver = "d", pkgs = heph.core.packages("//w/..."))"#;
        for p in ["w/a", "w/b"] {
            let d = root.join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("BUILD"), build).unwrap();
        }
        // A fully bypassing walker, so what this test observes is the package
        // list's own freeze and not the walker's mtime-keyed listing cache.
        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        }
        .with_walker(Arc::new(CachedWalker::bypassing()));

        let first = packages_config(&run_pkg_blocking(&provider, "w/a").expect("eval w/a"));
        assert_eq!(first, vec!["w", "w/a", "w/b"]);

        // A package appearing mid-run must not change what a later evaluation
        // sees.
        let late = root.join("w/c");
        fs::create_dir_all(&late).unwrap();
        fs::write(late.join("BUILD"), "").unwrap();

        let second = packages_config(&run_pkg_blocking(&provider, "w/b").expect("eval w/b"));
        assert_eq!(second, first);
    }

    /// A target-level matcher (`label(...)`) can't be decided from a package path,
    /// so `heph.core.packages` errors rather than silently returning nothing.
    #[test]
    fn test_heph_core_packages_rejects_target_level_matcher() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), r#"X = heph.core.packages("label(foo)")"#).unwrap();
        let provider = make_provider(&tmp);
        let err = run_pkg_blocking(&provider, "p").unwrap_err();
        assert!(
            err.to_string().contains("target-level info")
                || format!("{err:#}").contains("target-level info"),
            "expected target-level-info error, got: {err:#}"
        );
    }

    #[test]
    fn test_struct_builtin_rejects_positional() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
s = struct("positional")
target(name = "t", driver = "d")
"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        assert!(run_pkg_blocking(&provider, "p").is_err());
    }

    #[test]
    fn test_load_relative_dot_file_same_pkg() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let pkg = root.join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("go.BUILD2"), "go_install = \"installed\"\n").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"
load("./go.BUILD2", "go_install")
target(name = "t", driver = go_install)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "installed");
    }

    #[test]
    fn test_load_relative_parent_dir() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("shared.BUILD"), "FOO = \"shared\"\n").unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("../lib/shared.BUILD", "FOO")
target(name = "t", driver = FOO)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].driver, "shared");
    }

    #[test]
    fn test_load_relative_parent_dir_escaping_root_is_rejected() {
        // A workspace root at `tmp/root`, with a secret file just outside it at
        // `tmp/secret.BUILD`. `../secret.BUILD` from the root package's directory
        // resolves (on disk) to that outside file — load() must refuse to follow
        // it rather than parsing whatever lives outside the workspace.
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(tmp_dir.path().join("secret.BUILD"), "SECRET = \"leaked\"\n").unwrap();
        fs::write(
            root.join("BUILD"),
            r#"
load("../secret.BUILD", "SECRET")
target(name = "t", driver = SECRET)
"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.clone(),
            ..Provider::default()
        };
        let err = run_pkg_blocking(&provider, "").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("escapes workspace root"), "{chain}");
    }

    #[test]
    fn test_load_absolute_path_escaping_root_is_rejected() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("BUILD"),
            r#"load("//../../../../etc/hosts", "X")"#,
        )
        .unwrap();

        let provider = Provider {
            root,
            ..Provider::default()
        };
        let err = run_pkg_blocking(&provider, "").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("escapes workspace root"), "{chain}");
    }

    #[test]
    fn resolve_load_target_rejects_escape_from_nested_package() {
        let root = Path::new("/workspace");
        let err = resolve_load_target(root, "a/b", "../../../etc/hosts").unwrap_err();
        assert!(
            format!("{err:#}").contains("escapes workspace root"),
            "{err:#}"
        );
    }

    #[test]
    fn resolve_load_target_allows_in_bounds_paths() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_load_target(root, "a/b", "../c/d.BUILD").unwrap(),
            root.join("a/c/d.BUILD")
        );
        assert_eq!(
            resolve_load_target(root, "a", "//lib/shared.BUILD").unwrap(),
            root.join("lib/shared.BUILD")
        );
        // A trailing slash is meaningless for load() (unlike fs.glob's dir-vs-file
        // distinction) and must not survive into the filesystem path, or `stat` on a
        // path naming a real file fails with "not a directory".
        assert_eq!(
            resolve_load_target(root, "a", "./sub/").unwrap(),
            root.join("a/sub")
        );
    }

    #[test]
    fn resolve_load_target_contains_leading_slash_smuggling() {
        // `//` + a leading `/` in the remainder (e.g. `load("///etc/passwd")`) must not
        // let `Path::join` treat the joined path as absolute and discard the root.
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_load_target(root, "", "///etc/passwd").unwrap(),
            root.join("etc/passwd")
        );
    }

    #[test]
    fn test_load_bare_path_rejected() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let pkg = root.join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), r#"load("foo.BUILD", "X")"#).unwrap();
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "p").unwrap_err();
        let chain = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("must start with"), "{chain}");
    }

    #[test]
    fn test_load_target_in_other_package_registers_there() {
        // Loading another package's BUILD file evaluates it; any target() calls
        // it makes register against THAT package, not the loader's package.
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            lib.join("BUILD"),
            r#"
target(name = "t_in_lib", driver = "d")
SHARED = "s"
"#,
        )
        .unwrap();

        let app = root.join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("BUILD"),
            r#"
load("//lib", "SHARED")
target(name = "t_in_app", driver = SHARED)
"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let app_res = run_pkg_blocking(&provider, "app").unwrap();
        assert_eq!(app_res.targets.len(), 1);
        assert_eq!(app_res.targets[0].name, "t_in_app");
        assert_eq!(app_res.targets[0].driver, "s");

        // Lib was parsed via the load() and cached; re-querying lib should return
        // the targets defined there without re-eval.
        let lib_res = run_pkg_blocking(&provider, "lib").unwrap();
        assert_eq!(lib_res.targets.len(), 1);
        assert_eq!(lib_res.targets[0].name, "t_in_lib");
    }

    #[test]
    fn test_target_unsupported_kwarg_type_errors_instead_of_panicking() {
        // Passing a namespace value (e.g. the bare `heph.fs` provider namespace,
        // rather than calling one of its functions) as a target() kwarg used to
        // panic in starlark_to_rust, which aborts the whole LSP process instead
        // of reporting a BUILD-file error.
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", bad = heph.fs)"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "p").expect_err("must error, not panic");
        assert!(
            format!("{err:#}").contains("unsupported starlark value type"),
            "{err:#}"
        );
    }

    fn expect_string_list(v: Option<&htvalue::Value>) -> Vec<String> {
        match v {
            Some(htvalue::Value::List(l)) => l
                .iter()
                .map(|e| match e {
                    htvalue::Value::String(s) => s.clone(),
                    other => panic!("expected string in list, got {other:?}"),
                })
                .collect(),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn test_heph_fs_glob_returns_pkg_relative_paths() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("a.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("b.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("c.txt"), "").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("src/*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let mut srcs = expect_string_list(result.targets[0].config.get("srcs"));
        srcs.sort();
        assert_eq!(
            srcs,
            vec!["src/a.yaml".to_string(), "src/b.yaml".to_string()]
        );
    }

    #[test]
    fn test_heph_fs_glob_no_matches_returns_empty_list() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("src/*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let srcs = expect_string_list(result.targets[0].config.get("srcs"));
        assert!(srcs.is_empty(), "expected empty list, got {srcs:?}");
    }

    #[test]
    fn test_heph_fs_glob_skips_directories() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(pkg.join("nested")).unwrap();
        fs::write(pkg.join("a.txt"), "").unwrap();
        fs::write(pkg.join("nested").join("b.txt"), "").unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        let srcs = expect_string_list(result.targets[0].config.get("srcs"));
        // BUILD + a.txt; nested/ is a directory and must be filtered out.
        assert!(srcs.contains(&"a.txt".to_string()), "{srcs:?}");
        assert!(srcs.contains(&"BUILD".to_string()), "{srcs:?}");
        assert!(!srcs.iter().any(|s| s == "nested"), "{srcs:?}");
    }

    #[test]
    fn test_heph_fs_glob_at_workspace_root_returns_unprefixed() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        fs::write(root.join("a.yaml"), "").unwrap();
        fs::write(root.join("b.yaml"), "").unwrap();
        fs::write(
            root.join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*.yaml"))"#,
        )
        .unwrap();

        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "").unwrap();
        let mut srcs = expect_string_list(result.targets[0].config.get("srcs"));
        srcs.sort();
        assert_eq!(srcs, vec!["a.yaml".to_string(), "b.yaml".to_string()]);
    }

    struct EchoFn;
    #[async_trait::async_trait]
    impl ProviderFn for EchoFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
            let arg = match args.positional.first() {
                Some(htvalue::Value::String(s)) => s.clone(),
                _ => anyhow::bail!("echo expects a string"),
            };
            Ok(htvalue::Value::String(format!("{}:{}", ctx.pkg, arg)).into())
        }
    }

    #[test]
    fn test_provider_function_exposed_as_heph_namespace() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", v = heph.myprov.echo("hi"))"#,
        )
        .unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        let mut reg = ProviderFunctionRegistry::default();
        reg.insert_provider(
            "myprov",
            vec![hplugin::provider::ProviderFunctionDef {
                name: "echo".to_string(),
                signature: FnSignature {
                    positional: vec![Param::required("v", ParamType::String)],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: String::new(),
                func: Arc::new(EchoFn),
            }],
        );
        assert!(provider.function_registry.set(Arc::new(reg)).is_ok());

        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        match result.targets[0].config.get("v") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "mypkg:hi"),
            other => panic!("expected echoed string, got {other:?}"),
        }
    }

    /// A "build-file plugin" function: called from a BUILD file, it declares a
    /// fully-configured `exec` target plus package provider-state, and returns the
    /// new target's address. This is the wrapper pattern a tool author ships instead
    /// of a cdylib.
    struct CodegenFn;
    #[async_trait::async_trait]
    impl ProviderFn for CodegenFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
            let name = match args.named.get("name") {
                Some(htvalue::Value::String(s)) => s.clone(),
                _ => anyhow::bail!("codegen expects a string `name`"),
            };
            let addr = format!("//{}:{}", ctx.pkg, name);
            let mut config = HashMap::new();
            config.insert(
                "run".to_string(),
                htvalue::Value::List(vec![
                    htvalue::Value::String("gen".to_string()),
                    htvalue::Value::String("$OUT".to_string()),
                ]),
            );
            let outcome = FnOutcome {
                value: htvalue::Value::String(addr),
                targets: vec![DeclaredTarget {
                    name,
                    driver: "exec".to_string(),
                    config,
                    ..Default::default()
                }],
                states: vec![DeclaredState {
                    provider: "codegen".to_string(),
                    args: HashMap::from([(
                        "toolchain".to_string(),
                        htvalue::Value::String("v1".to_string()),
                    )]),
                }],
            };
            Ok(outcome)
        }
    }

    fn provider_with_fn(
        tmp_dir: &tempfile::TempDir,
        name: &str,
        f: Arc<dyn ProviderFn>,
    ) -> Provider {
        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        let mut reg = ProviderFunctionRegistry::default();
        reg.insert_provider(
            "codegen",
            vec![hplugin::provider::ProviderFunctionDef {
                name: name.to_string(),
                signature: FnSignature {
                    positional: vec![],
                    named: vec![Param::required("name", ParamType::String)],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: String::new(),
                func: f,
            }],
        );
        assert!(provider.function_registry.set(Arc::new(reg)).is_ok());
        provider
    }

    #[test]
    fn test_provider_function_declares_target() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        // The BUILD file only calls the plugin function — no `target()` of its own.
        fs::write(
            pkg.join("BUILD"),
            r#"a = heph.codegen.rule(name = "gen_a")"#,
        )
        .unwrap();

        let provider = provider_with_fn(&tmp_dir, "rule", Arc::new(CodegenFn));
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();

        // The declared target lands in the calling package as if hand-written.
        assert_eq!(result.targets.len(), 1, "one declared target");
        let t = &result.targets[0];
        assert_eq!(t.name, "gen_a");
        assert_eq!(t.driver, "exec");
        assert_eq!(
            expect_string_list(t.config.get("run")),
            vec!["gen".to_string(), "$OUT".to_string()]
        );

        // The declared provider_state lands in the package too.
        assert_eq!(result.states.len(), 1, "one declared state");
        assert_eq!(result.states[0].provider, "codegen");
        assert_eq!(
            result.states[0].args.get("toolchain"),
            Some(&htvalue::Value::String("v1".to_string()))
        );
    }

    /// A plugin function returning an empty target name must fail loudly, mirroring
    /// the `target()` builtin's own guard — a wrapper bug must not emit a nameless
    /// target.
    struct EmptyNameFn;
    #[async_trait::async_trait]
    impl ProviderFn for EmptyNameFn {
        async fn call(&self, _ctx: &FnCallContext<'_>, _args: FnArgs) -> anyhow::Result<FnOutcome> {
            Ok(FnOutcome {
                value: htvalue::Value::Null(),
                targets: vec![DeclaredTarget {
                    name: String::new(),
                    driver: "exec".to_string(),
                    ..Default::default()
                }],
                states: vec![],
            })
        }
    }

    #[test]
    fn test_provider_function_empty_target_name_errors() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), r#"heph.codegen.rule(name = "x")"#).unwrap();

        let provider = provider_with_fn(&tmp_dir, "rule", Arc::new(EmptyNameFn));
        let err = run_pkg_blocking(&provider, "mypkg").unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("name cannot be empty"), "{chain}");
    }

    /// A provider function reads a *named* argument. Guards that provider
    /// functions accept named args at all — the `positions()`/`names_map()` path
    /// in `invoke`, not `parse_positional` (which rejects every named arg).
    struct NamedEchoFn;
    #[async_trait::async_trait]
    impl ProviderFn for NamedEchoFn {
        async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
            let msg = match args.named.get("msg") {
                Some(htvalue::Value::String(s)) => s.clone(),
                _ => anyhow::bail!("echo expects a string `msg`"),
            };
            Ok(htvalue::Value::String(msg).into())
        }
    }

    #[test]
    fn test_provider_function_accepts_named_arg() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", v = heph.codegen.rule(msg = "hey"))"#,
        )
        .unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        let mut reg = ProviderFunctionRegistry::default();
        reg.insert_provider(
            "codegen",
            vec![hplugin::provider::ProviderFunctionDef {
                name: "rule".to_string(),
                signature: FnSignature {
                    positional: vec![],
                    named: vec![Param::required("msg", ParamType::String)],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: String::new(),
                func: Arc::new(NamedEchoFn),
            }],
        );
        assert!(provider.function_registry.set(Arc::new(reg)).is_ok());

        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        match result.targets[0].config.get("v") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "hey"),
            other => panic!("expected named-arg echo, got {other:?}"),
        }
    }

    #[test]
    fn test_unknown_provider_function_errors() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", v = heph.nope.bar())"#,
        )
        .unwrap();

        // make_provider wires only the `fs` namespace, so `heph.nope` is undefined.
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "mypkg").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("nope"),
            "expected error to name `nope`: {chain}"
        );
    }

    /// Evaluate a BUILD whose `target` reads `expr`, returning the eval error chain.
    fn eval_expr_err(call: &str) -> String {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            format!(r#"target(name = "t", driver = "d", v = {call})"#),
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let err = run_pkg_blocking(&provider, "mypkg").unwrap_err();
        format!("{err:#}")
    }

    #[test]
    fn provider_fn_missing_required_arg_errors() {
        let msg = eval_expr_err("heph.fs.glob()");
        assert!(msg.contains("pattern"), "{msg}");
    }

    #[test]
    fn provider_fn_wrong_arg_type_errors() {
        let msg = eval_expr_err("heph.fs.glob(123)");
        assert!(msg.contains("heph.fs.glob"), "{msg}");
        assert!(msg.contains("expected string"), "{msg}");
    }

    #[test]
    fn provider_fn_too_many_positional_errors() {
        // Two positionals for a one-arg function.
        let msg = eval_expr_err(r#"heph.fs.glob("a", "b")"#);
        assert!(
            msg.contains("at most 1 positional") || msg.contains("too many"),
            "{msg}"
        );
    }

    #[test]
    fn provider_fn_unknown_kwarg_errors() {
        let msg = eval_expr_err(r#"heph.fs.glob("*.rs", bogus = 1)"#);
        assert!(msg.contains("bogus"), "{msg}");
    }

    #[test]
    fn provider_fn_join_rejects_non_string_variadic() {
        // `join` is variadic over strings; a non-string element is rejected.
        let msg = eval_expr_err(r#"heph.fs.join("a", 1)"#);
        assert!(msg.contains("expected string"), "{msg}");
    }

    #[test]
    fn provider_fn_join_accepts_variadic() {
        let tmp_dir = tempdir().unwrap();
        let pkg = tmp_dir.path().join("mypkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("BUILD"),
            r#"target(name = "t", driver = "d", v = heph.fs.join("a", "b", "c"))"#,
        )
        .unwrap();
        let provider = make_provider(&tmp_dir);
        let result = run_pkg_blocking(&provider, "mypkg").unwrap();
        match result.targets[0].config.get("v") {
            Some(htvalue::Value::String(s)) => assert_eq!(s, "a/b/c"),
            other => panic!("expected joined path, got {other:?}"),
        }
    }
}
