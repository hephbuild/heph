use crate::pluginbuildfile::provider::Provider;
use anyhow::Context;
use enclose::enclose;
use hcore::hmemoizer::unwrap_arc_err;
use hcore::htvalue::signature::{FnSignature, ParamType};
use hcore::htvalue::{self, parse_map_string_string, parse_map_string_strings, parse_strings};
use hmodel::htaddr;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::TargetAddr;
use hplugin::driver::sandbox::{Dep, Env, EnvValue, Mode, Sandbox, Tool};
use hplugin::provider::{
    FnArgs, FnCallContext, ProvenanceFrame, ProviderFn, ProviderFunctionRegistry,
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

        // No public accessor returns an arbitrary positional slice; read up to a
        // fixed cap and let the signature validator enforce the real arity. Eight
        // is far beyond any provider function (the widest takes one positional);
        // more than that trips Starlark's own too-many-args error first.
        let (_, optional) =
            starlark::__derive_refs::parse_args::parse_positional::<0, 8>(args, eval.heap())?;
        let positional: Vec<htvalue::Value> =
            optional.iter().flatten().map(starlark_to_rust).collect();
        let named: HashMap<String, htvalue::Value> = args
            .names_map()?
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), starlark_to_rust(v)))
            .collect();

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

        let result = futures::executor::block_on(self.func.call(&ctx, fn_args))
            .map_err(starlark::Error::new_other)?;

        // Native `return_type` is documentation-only for native fns, so validate
        // the actual return value here.
        self.signature
            .validate_return(&self.display, &result)
            .map_err(starlark::Error::new_other)?;

        Ok(rust_to_starlark(eval.heap(), &result))
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
    pub config: HashMap<String, htvalue::Value>,
    /// Source call sites that produced this target (innermost `target()` call
    /// first). See [`hplugin::provider::ProvenanceFrame`].
    pub provenance: Vec<ProvenanceFrame>,
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
        for (i, (k, ss)) in parsed.iter().enumerate() {
            for s in ss {
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
        for (i, (k, ss)) in parsed.iter().enumerate() {
            for s in ss {
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
    /// Capture each target's source call-stack provenance. Off on the normal
    /// build path (walking `eval.call_stack()` per `target()` call is needless
    /// overhead there); on only for the LSP, which needs it to map a source
    /// position back to the targets a symbol produced.
    pub capture_provenance: bool,
}

// Unsupported starlark value types (not str/bool/int/float/list/dict) are a programming error
#[expect(
    clippy::panic,
    reason = "caller must only pass supported starlark value types; any other type is a programming error"
)]
fn starlark_to_rust(v: &Value) -> htvalue::Value {
    if v.is_none() {
        return htvalue::Value::Null();
    }

    if let Some(s) = v.unpack_str() {
        return htvalue::Value::String(s.to_string());
    }

    if let Some(b) = v.unpack_bool() {
        return htvalue::Value::Bool(b);
    }

    if let Some(i) = v.unpack_i32() {
        return htvalue::Value::Int(i as i64);
    }

    if let Ok(Some(UnpackFloat(f))) = UnpackFloat::unpack_value(*v) {
        return htvalue::Value::Float(f);
    }

    if let Ok(Some(l)) = UnpackList::<Value>::unpack_value(*v) {
        return htvalue::Value::List(l.items.iter().map(starlark_to_rust).collect());
    }

    if let Some(d) = DictRef::from_value(*v) {
        let map = d
            .iter()
            .filter_map(|(k, val)| {
                k.unpack_str()
                    .map(|s| (s.to_string(), starlark_to_rust(&val)))
            })
            .collect();
        return htvalue::Value::Map(map);
    }

    panic!(
        "starlark_to_rust: Unsupported starlark value type: {}",
        v.get_type()
    );
}

/// Returns `path` prefixed with the current package (e.g. `"src/main.rs"` from pkg
/// `"foo/bar"` becomes `"foo/bar/src/main.rs"`). If `abs` is true, or the current
/// package is empty (workspace root), returns `path` unchanged.
fn resolve_fs_path(eval: &Evaluator, path: &str, abs: bool) -> String {
    if abs {
        return path.to_string();
    }
    let extra = eval
        .extra
        .expect("evaluator extra must be set")
        .downcast_ref::<Extra>()
        .expect("evaluator extra must be of type Extra");
    if extra.pkg.is_empty() {
        path.to_string()
    } else {
        format!("{}/{}", extra.pkg, path)
    }
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
                        transitive = sandbox_from(starlark_to_rust(e.1), &PkgBuf::from(extra.pkg))
                            .with_context(|| "transitive")?;
                        Ok(None)
                    }
                    _ => Ok(Some((e.0.as_str().to_string(), starlark_to_rust(e.1)))),
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
        let resolved = resolve_fs_path(eval, path, abs);
        Ok(hbuiltins::pluginfs::file_addr(&resolved).format())
    }

    /// Reference files matching a glob `pattern` (with optional `exclude`) as a
    /// dependency address. Package-relative unless `abs = True`. Returns an `fs`
    /// provider address.
    fn glob<'v>(
        eval: &mut Evaluator<'v, '_, '_>,
        pattern: &str,
        exclude: Option<Value<'v>>,
        #[starlark(require = named, default = false)] abs: bool,
    ) -> starlark::Result<String> {
        let resolved = resolve_fs_path(eval, pattern, abs);
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
            .filter_map(|e| match e.0.as_str() {
                "provider" => {
                    if let Some(s) = e.1.unpack_str() {
                        provider = s.to_string();
                    }
                    None
                }
                _ => Some((e.0.as_str().to_string(), starlark_to_rust(e.1))),
            })
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

    /// The package currently being evaluated.
    fn pkg<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<String> {
        let extra = eval
            .extra
            .expect("evaluator extra must be set before calling heph.core.pkg()")
            .downcast_ref::<Extra>()
            .expect("evaluator extra must be of type Extra");
        Ok(extra.pkg.to_string())
    }
}

impl Provider {
    pub(crate) async fn run_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
        let key = pkg.to_string();
        let root = self.root.clone();
        let patterns = self.build_file_patterns.clone();
        let file_cache = self.file_cache.clone();
        let dir_cache = self.dir_cache.clone();
        let registry = self.function_registry.get().cloned().unwrap_or_default();
        let globals = self.globals.clone();
        let walker = self.walker.clone();
        self.pkg_cache
            .once(
                key.clone(),
                enclose!((key) move || async move {
                    hproc::process_supervisor::block_or_inline(move || -> anyhow::Result<Arc<RunResult>> {
                        let loader =
                            BuildFileLoader::new(root, patterns, file_cache, dir_cache, registry, globals, walker);
                        loader
                            .load_pkg(&key)
                            .with_context(|| format!("pkg: `{}`", key))
                    })
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
}

impl BuildFileLoader {
    pub(crate) fn new(
        root: PathBuf,
        patterns: Vec<glob::Pattern>,
        file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
        dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
        registry: Arc<ProviderFunctionRegistry>,
        globals: Arc<OnceLock<Globals>>,
        walker: Arc<CachedWalker>,
    ) -> Self {
        Self {
            root,
            patterns,
            file_cache,
            dir_cache,
            in_flight: Mutex::new(HashSet::new()),
            registry,
            globals,
            walker,
        }
    }

    /// The Starlark globals for this loader, built once from the function registry.
    fn globals(&self) -> &Globals {
        self.globals.get_or_init(|| build_globals(&self.registry))
    }

    /// Top-level entry: evaluate every BUILD file in `pkg`'s directory matching the
    /// configured patterns and merge their targets/states/symbols. Returns an empty
    /// result if the directory is missing or has no matching file — this is the path
    /// used by `Provider::list`/`get`/`probe`, where unknown packages should surface
    /// as empty/`NotFound` rather than hard errors. Use `load_dir` directly (via the
    /// `FileLoader` impl) for the `load(...)` path, which is strict.
    fn load_pkg(&self, pkg: &str) -> anyhow::Result<Arc<RunResult>> {
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
        if let Some(cached) = self
            .file_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
            .get(file_path)
        {
            return Ok(cached.clone());
        }

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

        let result = Arc::new(
            eval_file(file_path, pkg, self)
                .with_context(|| format!("evaluating {}", file_path.display()))?,
        );
        self.file_cache
            .lock()
            .map_err(|_e| anyhow::anyhow!("file_cache lock poisoned"))?
            .insert(file_path.to_path_buf(), result.clone());
        Ok(result)
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

/// Resolve a `load()` path argument to an absolute (logically normalized) filesystem path.
///
/// Accepts:
///   `//pkg/...`     absolute, relative to workspace root
///   `./rel/...`     relative to `current_pkg`'s directory
///   `../rel/...`    relative to `current_pkg`'s directory (walks up via `..`)
pub(crate) fn resolve_load_target(
    root: &Path,
    current_pkg: &str,
    path: &str,
) -> anyhow::Result<PathBuf> {
    let raw = if let Some(rel) = path.strip_prefix("//") {
        if rel.is_empty() {
            anyhow::bail!("load() path must not be empty after `//`");
        }
        root.join(rel)
    } else if path.starts_with("./") || path.starts_with("../") {
        root.join(current_pkg).join(path)
    } else {
        anyhow::bail!("load() path must start with `//`, `./`, or `../`, got `{path}`");
    };
    Ok(normalize_path(&raw))
}

/// Logically collapse `.` and `..` components in an absolute path without touching the
/// filesystem. Used so cache keys for `//a/sub` and `./sub` (from `a/BUILD`) match.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out: Vec<std::path::Component> = Vec::with_capacity(path.components().count());
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(
                    out.last(),
                    Some(std::path::Component::Normal(_)) | Some(std::path::Component::CurDir)
                ) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().map(|c| c.as_os_str()).collect()
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
    use std::fs;
    use tempfile::tempdir;

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
        BuildFileLoader::new(
            root,
            vec![glob::Pattern::new("BUILD").unwrap()],
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(ProviderFunctionRegistry::default()),
            Arc::new(OnceLock::new()),
            Arc::new(CachedWalker::disabled()),
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

    #[test]
    fn test_starlark_glob_with_exclude_pkg_relative() {
        let content = r#"
target(
    name = "t",
    driver = "d",
    srcs = glob("**/*.go", exclude = ["vendor/**", "gen/**"]),
)
"#;
        let config = run_target_config(content);
        let expected =
            hbuiltins::pluginfs::glob_addr("mypkg/**/*.go", &["vendor/**", "gen/**"]).format();
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
        async fn call(
            &self,
            ctx: &FnCallContext<'_>,
            args: FnArgs,
        ) -> anyhow::Result<htvalue::Value> {
            let arg = match args.positional.first() {
                Some(htvalue::Value::String(s)) => s.clone(),
                _ => anyhow::bail!("echo expects a string"),
            };
            Ok(htvalue::Value::String(format!("{}:{}", ctx.pkg, arg)))
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
