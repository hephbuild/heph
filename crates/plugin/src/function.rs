//! BUILD-file functions: `heph.<plugin>.<fn>(…)`.
//!
//! A function is a component of a plugin, declared next to its provider,
//! drivers, hooks and runners — not a method on a `Provider`. A plugin can ship
//! functions with no provider at all (a codegen wrapper around `exec`). The
//! engine gathers every plugin's functions into one [`FunctionRegistry`] keyed
//! by `(plugin, fn)`; see `docs/PLUGIN_FUNCTIONS.md`.

use crate::provider::Approval;
use async_trait::async_trait;
use hcore::htvalue::Value;
use hcore::htvalue::signature::FnSignature;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};

/// A function a plugin exposes to BUILD files, surfaced as the Starlark symbol
/// `heph.<plugin name>.<function name>`. Args and the return value are the loose
/// dynamic [`Value`] type so calls can cross plugin boundaries — in-process,
/// out to a cdylib plugin, and from one plugin into another plugin's function.
///
/// A function may also *declare* targets and provider-state as a side effect of
/// being called — see [`FnOutcome`]. This is what turns a plugin function into a
/// "build-file plugin": a convenience wrapper that a BUILD file calls to emit fully
/// configured `target(...)`/`provider_state(...)` declarations, instead of the
/// author writing a provider or driver of their own. The host (the buildfile
/// provider) merges the declared targets into the calling package exactly as if
/// the BUILD file had written them, wherever the function ran.
///
/// # A function is pure
///
/// Its outcome — the value **and** the targets and states it declares — depends
/// only on:
/// - the arguments, [`FnCallContext::pkg`] and [`FnCallContext::root`];
/// - the plugin's identity, its YAML `options:` and what the host handed it at
///   construction (`root`, `skip_dirs`, `skip_globs`);
/// - the host OS and arch;
/// - for a function documented as tree-reading (`heph.fs.glob`,
///   `heph.fs.parent`), the workspace tree under `root` as the plugin's walker
///   sees it.
///
/// It must not read the environment, the cwd, `$HOME`, the clock, the network,
/// or any path outside `root`. Its outputs never contain an absolute path:
/// values and declarations are workspace- or package-relative, because they land
/// in `TargetSpec.config` and so in the def hash of every consumer — an absolute
/// path makes that hash per-machine.
///
/// The host may replay a whole outcome instead of calling again, within one
/// engine's lifetime (the lifetime of the buildfile provider's package cache).
/// Nobody checks any of this at runtime: it is the author's contract. A function
/// that reads `$USER` or the time of day produces a def hash that differs per
/// machine or per run, and a stale value within one engine — see
/// `docs/PLUGIN_FUNCTIONS.md` for that worked bad example.
#[async_trait]
pub trait PluginFn: Send + Sync {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome>;
}

/// A target a plugin function declares when called from a BUILD file. Mirrors the
/// arguments of the `target()` builtin; the host merges each declaration into the
/// caller's package ([`FnCallContext::pkg`]) as if the BUILD file had written the
/// `target(...)` call itself. An empty `driver` falls back to the declaring
/// package's provider default, same as `target()`.
#[derive(Debug, PartialEq)]
pub struct DeclaredTarget {
    pub name: String,
    pub driver: String,
    pub labels: Vec<String>,
    /// The same map `target(transitive = {...})` takes (`deps`, `env`, …), or
    /// `Null` for none. Deliberately untyped: the host parses it exactly as it
    /// parses the builtin's argument — resolving relative addresses against the
    /// calling package, hashing every dep, assigning deterministic ids — so a
    /// declared target cannot carry a transitive sandbox a BUILD file could not.
    pub transitive: Value,
    pub approval: Approval,
    /// Driver options. The keys `target()` consumes itself (`name`, `driver`,
    /// `labels`, `transitive`, `approval`) are rejected here — they have fields.
    pub config: HashMap<String, Value>,
}

impl Default for DeclaredTarget {
    fn default() -> Self {
        Self {
            name: String::new(),
            driver: String::new(),
            labels: Vec::new(),
            transitive: Value::Null(),
            approval: Approval::default(),
            config: HashMap::new(),
        }
    }
}

/// Package-level provider state a plugin function declares, mirroring the
/// `provider_state(provider=…, **args)` builtin.
#[derive(Debug, Default, PartialEq)]
pub struct DeclaredState {
    pub provider: String,
    pub args: HashMap<String, Value>,
}

/// What a [`PluginFn`] returns: the [`Value`] substituted at the call site, plus
/// any targets and provider-state the call declared. Value-only functions (the
/// common case — `glob`, `join`, …) build this with `Value::into`; a wrapper function that stands up a target also pushes
/// [`DeclaredTarget`]s / [`DeclaredState`]s.
///
/// Declarations cross the out-of-process plugin ABI, so a cdylib plugin
/// function can stand up a target too (`CallFunctionRequest.accepts_declarations`,
/// ABI 0.15.0). A caller that predates the capability does not advertise it, and
/// then a declaring function **fails the call** rather than answering with its
/// value alone — a dropped declaration would otherwise configure the build
/// differently from what the author wrote, and hash as if that were intended.
///
/// A function that calls **another** plugin's function is the one hop the host
/// cannot police: the inner outcome's declarations reach the package only if
/// the outer function passes them on. That is why the fields are private and
/// [`absorb`](FnOutcome::absorb) is the consuming path that keeps them. It is
/// not airtight — reading [`value`](FnOutcome::value) and dropping the rest
/// still discards them — so relaying remains something an author does on
/// purpose.
///
/// Know which half of that has a safety net. Losing a relayed **target** fails
/// loudly: the value names a target nobody created, and the build stops at
/// "target not found". Losing a relayed **state** is silent — the package keeps
/// building, every target in it parses with provider defaults, and the def hash
/// records that as if the author had asked for it. Relay with `absorb`.
///
/// What a declaring function owes, since it runs unsandboxed while BUILD files
/// are evaluated:
/// - **Order does not matter, and ambiguity is refused.** The host sorts
///   declared targets by name and states by provider before merging, because
///   package order reaches downstream def hashes — iterating a `HashMap` must
///   not flap cache keys. One call declaring the same target name, or two states
///   for one provider, is an error rather than a coin flip: state is read
///   last-wins, so the winner would be whichever the function happened to yield
///   first. Declare the state once, with the values it should have.
/// - **No host-specific values.** [`FnCallContext::root`] is an absolute path;
///   putting it (or anything derived from the machine) into a declaration puts
///   it into the def hash, and the cache is never shared across machines.
/// - **Whatever it reads is captured only through what it declares.** Package
///   evaluation is not cached beyond one engine, so that is enough today; a
///   cache that outlived the engine would have to track what these functions
///   read.
#[derive(Debug)]
#[must_use = "a plugin function's outcome carries what it declared; dropping it drops them"]
pub struct FnOutcome {
    value: Value,
    targets: Vec<DeclaredTarget>,
    states: Vec<DeclaredState>,
}

impl FnOutcome {
    /// Declare a target, which the host merges into the package being evaluated.
    pub fn declare_target(&mut self, target: DeclaredTarget) -> &mut Self {
        self.targets.push(target);
        self
    }

    /// Declare package-level provider state.
    pub fn declare_state(&mut self, state: DeclaredState) -> &mut Self {
        self.states.push(state);
        self
    }

    /// The value substituted at the call site.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Set the value substituted at the call site — for a function that decides
    /// it only after relaying another call (see [`absorb`](Self::absorb)).
    pub fn set_value(&mut self, value: Value) -> &mut Self {
        self.value = value;
        self
    }

    pub fn targets(&self) -> &[DeclaredTarget] {
        &self.targets
    }

    pub fn states(&self) -> &[DeclaredState] {
        &self.states
    }

    /// Split into value and declarations, for the host merging them into a
    /// package and for the codec putting them on the wire. A plugin function
    /// relaying another function's outcome wants [`absorb`](Self::absorb): this
    /// hands over the declarations and leaves honoring them to the caller.
    pub fn into_parts(self) -> (Value, Vec<DeclaredTarget>, Vec<DeclaredState>) {
        (self.value, self.targets, self.states)
    }

    /// Rebuild from parts — the decoding half of [`into_parts`](Self::into_parts).
    pub fn from_parts(
        value: Value,
        targets: Vec<DeclaredTarget>,
        states: Vec<DeclaredState>,
    ) -> Self {
        Self {
            value,
            targets,
            states,
        }
    }

    /// The return value alone, for a caller that cannot carry declarations.
    /// Errors when the call declared anything, since answering with the value
    /// would drop them where no one could notice.
    ///
    /// Two kinds of caller: a wire handler whose peer did not advertise the
    /// capability — it adds which side is old and what to do about it — and a
    /// value-only call site asserting that the function it called declares
    /// nothing. So the message stays about the outcome, not the ABI.
    pub fn into_value_only(self) -> anyhow::Result<Value> {
        if !self.targets.is_empty() || !self.states.is_empty() {
            anyhow::bail!(
                "declared {} target(s) and {} provider_state(s), but this caller \
                 cannot carry declarations",
                self.targets.len(),
                self.states.len()
            );
        }
        Ok(self.value)
    }

    /// Take `inner`'s declarations into this outcome and return its value — how
    /// a plugin function reads the outcome of **another** plugin's function it
    /// called, so declarations made down the chain reach the host instead of
    /// stopping at this hop.
    ///
    /// The only *consuming* path that keeps declarations. Private fields put
    /// this one first, but they do not make it the only one: `value()` and then
    /// a drop, or [`into_parts`](Self::into_parts) ignoring two bindings, still
    /// discard them — silently, for a state (see the type's docs).
    ///
    /// ```ignore
    /// let mut out = FnOutcome::from(Value::Null());
    /// let inner_addr = out.absorb(functions.call("other", "rule", ctx, args).await?);
    /// ```
    pub fn absorb(&mut self, inner: FnOutcome) -> Value {
        self.targets.extend(inner.targets);
        self.states.extend(inner.states);
        inner.value
    }
}

impl From<Value> for FnOutcome {
    fn from(value: Value) -> Self {
        Self {
            value,
            targets: Vec::new(),
            states: Vec::new(),
        }
    }
}

/// One exposed function: its bare name (no `heph.<plugin>.` prefix), its
/// declarative signature, a one-line doc string, and its handler. The engine
/// enforces `signature` against every call (see
/// [`hcore::htvalue::signature::FnSignature`]); `doc` is surfaced by the
/// BUILD-file LSP on hover over `heph.<plugin>.<name>`.
pub struct PluginFnDef {
    pub name: String,
    pub signature: FnSignature,
    /// Human-readable description shown in LSP hover. Empty for undocumented
    /// functions (the hover then shows just the rendered signature).
    pub doc: String,
    pub func: Arc<dyn PluginFn>,
}

/// A function as held in the [`FunctionRegistry`]: its signature (shared, so
/// the Starlark bridge can both enforce it and derive a native param spec from
/// it), its hover doc, plus the handler.
#[derive(Clone)]
pub struct RegisteredFn {
    pub signature: Arc<FnSignature>,
    pub doc: String,
    pub func: Arc<dyn PluginFn>,
}

/// Context handed to a [`PluginFn`] at call time.
///
/// Intentionally minimal — `pkg` + `root` is what filesystem helpers like `glob`
/// need. A `ProviderExecutor`/cancellation token is deliberately absent: a function
/// that resolves targets through the engine would also need the buildfile provider's
/// cross-request `pkg_cache` reworked (it caches BUILD eval per provider lifetime,
/// not per request), so engine-calling functions are out of scope for now.
pub struct FnCallContext<'a> {
    /// Package the calling BUILD file lives in (e.g. `"foo/bar"`, empty at root).
    pub pkg: &'a str,
    /// Workspace root.
    pub root: &'a Path,
}

/// Positional + named arguments passed from the Starlark call site.
#[derive(Default)]
pub struct FnArgs {
    pub positional: Vec<Value>,
    pub named: HashMap<String, Value>,
}

/// Calls a function of any plugin by `(plugin, fn)` — how one plugin's function
/// reaches another's. A builtin gets one as [`FunctionSlot`] (in its
/// `PluginInit`); a cdylib gets one as the registry handle passed to its create
/// entry. Either way it is per engine: two engines in one process never see
/// each other's functions.
///
/// Relay what the inner call declared with [`FnOutcome::absorb`].
#[async_trait]
pub trait FunctionCaller: Send + Sync {
    async fn call(
        &self,
        plugin: &str,
        name: &str,
        ctx: &FnCallContext<'_>,
        args: FnArgs,
    ) -> anyhow::Result<FnOutcome>;
}

/// Every plugin's exposed functions: plugin name → function name → handler.
/// Built by the engine while plugins register, then sealed (see
/// [`FunctionSlot`]). Ordered maps, so every listing is sorted by plugin, then
/// function.
#[derive(Default)]
pub struct FunctionRegistry {
    map: BTreeMap<String, BTreeMap<String, RegisteredFn>>,
}

impl FunctionRegistry {
    /// Insert all of `plugin`'s functions under its name. All or nothing: a
    /// function name used twice within `defs`, or a plugin that already has
    /// functions here, is refused and inserts nothing.
    pub fn insert(&mut self, plugin: &str, defs: Vec<PluginFnDef>) -> anyhow::Result<()> {
        if defs.is_empty() {
            return Ok(());
        }
        if self.map.contains_key(plugin) {
            anyhow::bail!("plugin {plugin:?} registered its functions twice");
        }
        let mut fns = BTreeMap::new();
        for def in defs {
            if fns.contains_key(&def.name) {
                anyhow::bail!(
                    "plugin {plugin:?} exports two functions named {:?}; a function name is \
                     unique within its plugin",
                    def.name
                );
            }
            fns.insert(
                def.name,
                RegisteredFn {
                    signature: Arc::new(def.signature),
                    doc: def.doc,
                    func: def.func,
                },
            );
        }
        self.map.insert(plugin.to_string(), fns);
        Ok(())
    }

    /// Look up a single function by plugin + function name.
    pub fn get(&self, plugin: &str, func: &str) -> Option<&RegisteredFn> {
        self.map.get(plugin).and_then(|m| m.get(func))
    }

    /// Iterate `(plugin, function name, function)` over every registered
    /// function, sorted by plugin, then function.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &RegisteredFn)> {
        self.map.iter().flat_map(|(p, fns)| {
            fns.iter()
                .map(move |(name, rf)| (p.as_str(), name.as_str(), rf))
        })
    }

    /// Iterate `(plugin name, its functions)` — one entry per plugin that has
    /// functions, sorted by name.
    pub fn plugins(&self) -> impl Iterator<Item = (&str, &BTreeMap<String, RegisteredFn>)> {
        self.map.iter().map(|(p, fns)| (p.as_str(), fns))
    }

    /// Resolve `(plugin, fn)` or say why not, naming what does exist — the text
    /// an agent recovers from.
    pub fn resolve(&self, plugin: &str, name: &str) -> anyhow::Result<&RegisteredFn> {
        let Some(fns) = self.map.get(plugin) else {
            let known: Vec<&str> = self.map.keys().map(String::as_str).collect();
            anyhow::bail!(
                "no plugin named {plugin:?} has functions; plugins with functions: {}",
                list_or_none(&known)
            );
        };
        fns.get(name).ok_or_else(|| {
            let known: Vec<&str> = fns.keys().map(String::as_str).collect();
            anyhow::anyhow!(
                "plugin {plugin:?} has no function {name:?}; available: {}",
                list_or_none(&known)
            )
        })
    }
}

fn list_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

#[async_trait]
impl FunctionCaller for FunctionRegistry {
    async fn call(
        &self,
        plugin: &str,
        name: &str,
        ctx: &FnCallContext<'_>,
        args: FnArgs,
    ) -> anyhow::Result<FnOutcome> {
        let rf = self.resolve(plugin, name)?;
        rf.func.call(ctx, args).await
    }
}

impl std::fmt::Debug for FunctionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.map
                    .iter()
                    .map(|(p, fns)| (p, fns.keys().collect::<Vec<_>>())),
            )
            .finish()
    }
}

/// The engine's handle to its [`FunctionRegistry`], given to every plugin it
/// builds. Unsealed while plugins register; [`seal`](Self::seal)ed once, after
/// which [`get`](Self::get) resolves the registry.
///
/// Reading it before the seal is an error, never an empty namespace: a plugin
/// that cached an empty `heph.*` would serve it for its whole life. The slot
/// holds the registry **weakly** — the engine holds the only strong reference —
/// so a plugin function holding the slot keeps no registry (and no plugin)
/// alive, and an engine that is gone reads as an error rather than as a
/// registry from another engine.
#[derive(Default)]
pub struct FunctionSlot {
    sealed: OnceLock<Weak<FunctionRegistry>>,
}

impl FunctionSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A slot already sealed with `registry` — for callers (the LSP, tests)
    /// that hold a finished registry rather than an engine. `registry` must be
    /// kept alive by the caller.
    pub fn sealed(registry: &Arc<FunctionRegistry>) -> Arc<Self> {
        let slot = Self::new();
        slot.seal(registry);
        slot
    }

    /// Seal the slot with `registry`. Idempotent for the registry it was
    /// sealed with; a second, different registry is ignored (the first seal
    /// wins) and reported as `false`.
    pub fn seal(&self, registry: &Arc<FunctionRegistry>) -> bool {
        let weak = Arc::downgrade(registry);
        match self.sealed.set(weak) {
            Ok(()) => true,
            Err(_) => self
                .sealed
                .get()
                .is_some_and(|w| Weak::ptr_eq(w, &Arc::downgrade(registry))),
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.get().is_some()
    }

    /// The sealed registry.
    pub fn get(&self) -> anyhow::Result<Arc<FunctionRegistry>> {
        let weak = self.sealed.get().ok_or_else(|| {
            anyhow::anyhow!(
                "the plugin function registry was read before every plugin was registered; \
                 a plugin may call `heph.<plugin>.<fn>` functions only once the engine has \
                 finished loading plugins"
            )
        })?;
        weak.upgrade().ok_or_else(|| {
            anyhow::anyhow!("the engine that owned this plugin function registry is gone")
        })
    }
}

impl std::fmt::Debug for FunctionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionSlot")
            .field("sealed", &self.is_sealed())
            .finish()
    }
}

#[async_trait]
impl FunctionCaller for FunctionSlot {
    async fn call(
        &self,
        plugin: &str,
        name: &str,
        ctx: &FnCallContext<'_>,
        args: FnArgs,
    ) -> anyhow::Result<FnOutcome> {
        let registry = self.get()?;
        let rf = registry.resolve(plugin, name)?.func.clone();
        rf.call(ctx, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaring(name: &str) -> FnOutcome {
        FnOutcome {
            value: Value::String(format!("//p:{name}")),
            targets: vec![DeclaredTarget {
                name: name.to_string(),
                ..Default::default()
            }],
            states: vec![DeclaredState {
                provider: "p".to_string(),
                args: HashMap::new(),
            }],
        }
    }

    struct Const(&'static str);

    #[async_trait]
    impl PluginFn for Const {
        async fn call(&self, _: &FnCallContext<'_>, _: FnArgs) -> anyhow::Result<FnOutcome> {
            Ok(Value::String(self.0.to_string()).into())
        }
    }

    fn def(name: &str, value: &'static str) -> PluginFnDef {
        PluginFnDef {
            name: name.to_string(),
            signature: FnSignature {
                positional: vec![],
                named: vec![],
                variadic: None,
                returns: hcore::htvalue::signature::ParamType::String,
            },
            doc: String::new(),
            func: Arc::new(Const(value)),
        }
    }

    /// What a plugin function must do with another plugin function's
    /// outcome: keep its declarations, use its value. Reading `.value` instead
    /// is the one way declarations are still lost, and no guard can see it.
    #[test]
    fn absorb_keeps_the_inner_declarations_and_yields_its_value() {
        let mut outer = FnOutcome::from(Value::Null());
        let value = outer.absorb(declaring("inner"));

        assert_eq!(value, Value::String("//p:inner".to_string()));
        assert_eq!(
            outer.value,
            Value::Null(),
            "the inner value is returned, not adopted"
        );
        assert_eq!(
            outer
                .targets
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["inner"]
        );
        assert_eq!(outer.states.len(), 1);

        // Absorbing twice accumulates, so a function may relay several calls.
        outer.absorb(declaring("second"));
        assert_eq!(
            outer
                .targets
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["inner", "second"]
        );
    }

    /// A caller that cannot carry declarations gets an error, never the value
    /// with the declarations quietly missing. The message says what was about to
    /// be lost; which peer is old, and the remedy, is the wire handler's to add.
    #[test]
    fn into_value_only_refuses_to_drop_declarations() {
        let err = declaring("t").into_value_only().expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("declared 1 target(s)"), "{msg}");
        assert!(msg.contains("1 provider_state(s)"), "{msg}");

        let v = FnOutcome::from(Value::Bool(true))
            .into_value_only()
            .expect("a value-only outcome passes");
        assert_eq!(v, Value::Bool(true));
    }

    /// D7: a duplicate function name within one plugin is refused, and the
    /// refused insert leaves nothing behind. Today's silent overwrite is gone.
    #[test]
    fn duplicate_function_name_within_plugin_is_refused() {
        let mut reg = FunctionRegistry::default();
        let err = reg
            .insert("gen", vec![def("rule", "a"), def("rule", "b")])
            .expect_err("a duplicate name must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("\"gen\""), "{msg}");
        assert!(msg.contains("two functions named \"rule\""), "{msg}");
        assert!(reg.get("gen", "rule").is_none(), "nothing was inserted");

        reg.insert("gen", vec![def("rule", "a")]).expect("insert");
        let err = reg
            .insert("gen", vec![def("other", "b")])
            .expect_err("a plugin registers its functions once");
        assert!(format!("{err:#}").contains("twice"), "{err:#}");
    }

    /// The registry resolves by `(plugin, fn)`, and a miss names what exists.
    #[test]
    fn resolve_names_what_exists() {
        let mut reg = FunctionRegistry::default();
        reg.insert("go", vec![def("build_addr", "a"), def("gocache_addr", "b")])
            .expect("insert");
        assert!(reg.resolve("go", "build_addr").is_ok());
        let msg = format!("{:#}", reg.resolve("go", "nope").err().expect("miss"));
        assert_eq!(
            msg,
            "plugin \"go\" has no function \"nope\"; available: build_addr, gocache_addr"
        );
        let msg = format!("{:#}", reg.resolve("foo", "x").err().expect("miss"));
        assert!(msg.contains("plugins with functions: go"), "{msg}");
    }

    /// The slot errs before the seal, resolves after, and holds the registry
    /// weakly: once its owner drops the registry, the slot says so instead of
    /// keeping it alive.
    #[test]
    fn slot_errs_before_seal_and_holds_the_registry_weakly() {
        let slot = FunctionSlot::new();
        let err = slot.get().expect_err("unsealed");
        assert!(format!("{err:#}").contains("before every plugin was registered"));

        let mut reg = FunctionRegistry::default();
        reg.insert("p", vec![def("f", "v")]).expect("insert");
        let reg = Arc::new(reg);
        assert!(slot.seal(&reg));
        assert!(slot.seal(&reg), "resealing with the same registry is fine");
        assert!(!slot.seal(&Arc::new(FunctionRegistry::default())));
        assert!(slot.get().expect("sealed").get("p", "f").is_some());

        drop(reg);
        let err = slot.get().expect_err("owner gone");
        assert!(format!("{err:#}").contains("is gone"), "{err:#}");
    }
}
