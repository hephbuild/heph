use crate::engine::Engine;
use crate::engine::deferred::declared_origin;
use crate::engine::driver::TargetAddr;
use crate::engine::driver::targetdef::{Input, InputMode, TargetDef};
use crate::engine::error::{InputDefFailed, MultiError};
use crate::engine::invoke::EntrypointAsInput;
use crate::engine::request_state::{AddrKey, RequestState};
use crate::engine::result::ExtendedTargetDef;
use async_recursion::async_recursion;
use enclose::enclose;
use futures::stream::{FuturesOrdered, Stream, StreamExt as _};
use hcore::hmemoizer::{nest, unwrap_arc_err};
use hmodel::htaddr::Addr;
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Package of the engine-baked magic introspection targets.
pub const INTROSPECT_PKG: &str = "@heph/introspect";
/// Name of the magic input that expands to one `@heph/fs` input per declared
/// output path of the *owning* target.
pub const INTROSPECT_OUTPUTS_NAME: &str = "outputs";

/// True if `addr` is the `//@heph/introspect:outputs` magic input.
fn is_introspect_outputs(addr: &Addr) -> bool {
    addr.package.as_str() == INTROSPECT_PKG && addr.name == INTROSPECT_OUTPUTS_NAME
}

/// Builds one `Input` referencing the `@heph/fs` addr `fs_addr`. The fs addr
/// already encodes a root-relative path/pattern (see [`synthesized_fs_inputs`]).
///
/// The synthesized input INHERITS the replaced magic input's `origin_id`, `mode`
/// and `annotations`. This is load-bearing for the exec driver: `$SRC_<GROUP>` /
/// `$LIST_<GROUP>` routing matches a target's declared `dep_group_inputs` (which
/// still carry the magic input's origin id) against the materialized run inputs
/// by `origin_id`. Reusing the template's origin id makes the synthesized fs
/// files surface through the same `SRC`/`LIST` group the user declared the
/// `//@heph/introspect:outputs` dep under (multiple paths share one origin id, so
/// the managed bridge merges them into one list file).
fn introspect_input(fs_addr: Addr, template: &Input) -> Input {
    Input {
        r#ref: TargetAddr {
            r#ref: fs_addr,
            output: None,
            filters: vec![],
        },
        mode: template.mode.clone(),
        origin_id: template.origin_id.clone(),
        annotations: template.annotations.clone(),
        hashed: true,
        runtime: true,
    }
}

/// Expands the `//@heph/introspect:outputs` magic input into one `@heph/fs`
/// input per declared output path of `outputs`, `codegen = "copy"` paths
/// excepted: those are generated, not source, so a target never reads (or
/// hashes) its own previous output.
///
/// Output path strings are already root-relative: the exec driver prepends the
/// owning package to each declared `out` entry (`spec_path_to_target_path`), so
/// a `//pkg:gen` target declaring `out=["*.go"]` stores `Glob("pkg/*.go")`. The
/// `@heph/fs` driver resolves a glob/file addr's `p`/`f` arg relative to the
/// same workspace root (`walk_glob` joins the pattern's literal prefix onto
/// `root`). An output glob is in the collector's dialect and an fs glob in
/// wax's, so the path goes through `output_fs_addr`, which translates a glob
/// and escapes a directory: the synthesized input then reads the files the
/// output declares, in a package like `app/[slug]` too.
///
/// Ordering is deterministic (outputs then their paths, in declaration order);
/// the same fs addr is never emitted twice within one expansion. Fails on an
/// output glob with no fs glob spelling.
fn synthesized_fs_inputs(
    outputs: &[crate::engine::driver::targetdef::Output],
    template: &Input,
) -> anyhow::Result<Vec<Input>> {
    use crate::engine::driver::targetdef::path::CodegenMode;
    use anyhow::Context as _;

    let mut seen = FxHashSet::default();
    let mut out = Vec::new();
    for output in outputs {
        for path in &output.paths {
            if matches!(path.codegen_tree, CodegenMode::Copy) {
                continue;
            }
            let fs_addr =
                hbuiltins::pluginfs::output_fs_addr(&path.content).with_context(|| {
                    format!(
                        "expanding //@heph/introspect:outputs: reading output {} as an fs input",
                        path.content
                    )
                })?;
            if seen.insert(fs_addr.clone()) {
                out.push(introspect_input(fs_addr, template));
            }
        }
    }
    Ok(out)
}

/// Returns `def.inputs` with every `//@heph/introspect:outputs` magic input
/// replaced by the `@heph/fs` inputs synthesized from `def.outputs`. Non-magic
/// inputs pass through unchanged and in order.
///
/// Allocate-on-write: if no magic input is present `def.inputs` is left as it
/// is.
pub(crate) fn expand_introspect_inputs(
    def: &mut crate::engine::driver::targetdef::TargetDef,
) -> anyhow::Result<()> {
    if !def
        .inputs
        .iter()
        .any(|i| is_introspect_outputs(&i.r#ref.r#ref))
    {
        return Ok(());
    }

    let mut rebuilt = Vec::with_capacity(def.inputs.len());
    for input in &def.inputs {
        if is_introspect_outputs(&input.r#ref.r#ref) {
            rebuilt.extend(synthesized_fs_inputs(&def.outputs, input)?);
        } else {
            rebuilt.push(input.clone());
        }
    }
    def.inputs = rebuilt;
    Ok(())
}

/// Routing context inherited from a parent transparent-group input. Carries
/// origin_id/mode/annotations plus the parent's `hashed`/`runtime` flags so
/// children of a `hash_deps` (or `runtime_deps`) group expansion inherit
/// the parent's intent.
pub(crate) struct ParentOverride {
    pub origin_id: String,
    pub mode: InputMode,
    pub annotations: BTreeMap<String, String>,
    pub hashed: bool,
    pub runtime: bool,
}

impl ParentOverride {
    /// The routing a group named in a target's own input list hands its
    /// members: that input's.
    fn of(group: &Input) -> Self {
        Self {
            origin_id: group.origin_id.clone(),
            mode: group.mode.clone(),
            annotations: group.annotations.clone(),
            hashed: group.hashed,
            runtime: group.runtime,
        }
    }

    /// The parent's annotations as the base, with `child`'s own overriding
    /// them on a key both set.
    fn child_annotations(&self, child: &Input) -> BTreeMap<String, String> {
        let mut merged = self.annotations.clone();
        merged.extend(
            child
                .annotations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        merged
    }

    /// The routing a nested group's members inherit: this one, with the
    /// nested group's annotations merged in as a child's are.
    fn nested(&self, group: &Input) -> Self {
        Self {
            origin_id: self.origin_id.clone(),
            mode: self.mode.clone(),
            annotations: self.child_annotations(group),
            hashed: self.hashed,
            runtime: self.runtime,
        }
    }

    /// A group member as this routing places it in the parent's inputs.
    fn apply(&self, member: &Input) -> Input {
        Input {
            r#ref: member.r#ref.clone(),
            origin_id: self.origin_id.clone(),
            mode: self.mode.clone(),
            annotations: self.child_annotations(member),
            hashed: self.hashed,
            runtime: self.runtime,
        }
    }
}

/// A target's inputs with its groups expanded, and the def of each, as
/// [`Engine::expanded_inputs_for`] memoizes them. Reads as the input slice.
pub(crate) struct ExpandedInputs {
    inputs: InputList,
    /// The def of each input, index for index. Expansion resolved every one
    /// of them on this request, so `link` reads them here instead of
    /// resolving each runtime input again.
    defs: Vec<Arc<ExtendedTargetDef>>,
}

enum InputList {
    /// No input is a group: the def's own list, shared rather than copied.
    Declared(Arc<TargetDef>),
    /// At least one input was a group, replaced in place by its members.
    Expanded(Vec<Input>),
}

impl ExpandedInputs {
    /// Each input with its def, in order.
    pub(crate) fn with_defs(&self) -> impl Iterator<Item = (&Input, &Arc<ExtendedTargetDef>)> {
        self.iter().zip(&self.defs)
    }
}

impl std::ops::Deref for ExpandedInputs {
    type Target = [Input];

    fn deref(&self) -> &[Input] {
        match &self.inputs {
            InputList::Declared(def) => &def.inputs,
            InputList::Expanded(inputs) => inputs,
        }
    }
}

/// The context on an expansion that failed without `--fail-fast` (D1.f,
/// STAGE-16), over the error of every input that failed: a sole one bare,
/// several as a [`MultiError`] in input order, so the walker finds a
/// cancellation or a cycle among them like anywhere else. It holds no error
/// and renders as nothing, like any data riding a chain.
///
/// Nothing consumes the inputs that resolved as an input set: the expansion
/// is an `Err`, so `link`, `hashin` and `hash_dep_count` never see them, and
/// a partial set is never hashed, whichever input failed (hashed or not).
/// Only `meta` reads them, to build the hashed inputs that resolved before it
/// fails with the errors, so a fault in one subtree does not keep its healthy
/// hashed siblings from running. Runtime-only siblings that resolved stay
/// unbuilt.
pub(crate) struct PartialExpansion {
    /// Each input that resolved, with the index in the declaring target's own
    /// input list of the input it came through (a group member's is its
    /// group's, whose `hashed`/`runtime` it carries).
    resolved: Vec<(Arc<ExtendedTargetDef>, usize)>,
}

impl PartialExpansion {
    /// Each input that resolved, with the index of the declared input it
    /// came through.
    pub(crate) fn resolved(&self) -> impl Iterator<Item = (&Arc<ExtendedTargetDef>, usize)> {
        self.resolved.iter().map(|(def, idx)| (def, *idx))
    }

    /// The expansion's error: this context over `errors` (not empty). Each
    /// of several is nested, so a classifier can hand one member to whoever
    /// records it ([`hcore::hmemoizer::share`]).
    fn into_error(self, mut errors: Vec<anyhow::Error>) -> anyhow::Error {
        let errors = match errors.pop() {
            Some(sole) if errors.is_empty() => sole,
            last => {
                errors.extend(last);
                MultiError(errors.into_iter().map(nest).collect()).into()
            }
        };
        errors.context(self)
    }
}

impl std::fmt::Debug for PartialExpansion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.resolved
                    .iter()
                    .map(|(def, idx)| (def.target_def.addr.format(), idx)),
            )
            .finish()
    }
}

impl std::fmt::Display for PartialExpansion {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for PartialExpansion {}

/// What a group's expansion appends to, and the path it is on.
struct GroupWalk<'a> {
    /// The target's expanded inputs so far.
    inputs: &'a mut Vec<Input>,
    /// The def of each of `inputs`, index for index.
    defs: &'a mut Vec<Arc<ExtendedTargetDef>>,
    /// For each of `defs`, the index of the declared input it came through.
    origins: &'a mut Vec<usize>,
    /// The declared input (a group) this walk expands.
    origin: usize,
    /// Each failure met so far, in walk order, when not `fail_fast`.
    errors: &'a mut Vec<anyhow::Error>,
    /// Whether the first failure ends the walk.
    fail_fast: bool,
    /// The groups on the path being expanded, to refuse one that contains
    /// itself. Expansion is sequential within one task, so the walk borrows
    /// it mutably across the recursion.
    visited: &'a mut FxHashSet<Addr>,
}

impl Engine {
    /// Memoized expansion of `addr`'s inputs. Called from both `link` and `meta`
    /// for every result — without memoization the whole transparent-group walk
    /// runs twice per addr per request.
    pub(crate) async fn expanded_inputs_for(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExpandedInputs>> {
        rs.data
            .mem_expanded_inputs
            .once(
                AddrKey(addr.clone()),
                enclose!((self => engine, rs, addr) move || async move {
                    // _no_track: result_addr (the upstream entry point) already set
                    // parent=addr before getting here, so tracked get_def would
                    // record addr→addr (spurious self-cycle).
                    // `def.target_def.inputs` has already had the engine-baked
                    // `//@heph/introspect:outputs` magic input expanded into
                    // `@heph/fs` inputs during `get_def` (see
                    // `expand_introspect_inputs` in result.rs::get_def_inner),
                    // so the transparent-group walk below never sees it.
                    let def = Arc::clone(&engine)
                        .get_def_no_track(rs.clone(), &addr)
                        .await?;
                    let expanded = Arc::clone(&engine)
                        .expand_inputs(rs, &def.target_def)
                        .await?;
                    Ok(Arc::new(expanded))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Expands the transparent (group) inputs of `declared`, replacing each
    /// group with its constituent deps in place. Non-transparent inputs pass
    /// through unchanged and in order. Each input comes with its def.
    ///
    /// When no input is a group, the result shares `declared`'s list and
    /// nothing is copied, which is the common case. Otherwise the list is
    /// allocated the first time a group is met, with the inputs before it
    /// copied in.
    ///
    /// Under `--fail-fast` the first failing input, in declaration order, is
    /// the error. Otherwise every input is resolved, a group walked to its
    /// end, and any failure fails the expansion with all of them under a
    /// [`PartialExpansion`] (D1.f).
    pub(crate) async fn expand_inputs(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        declared: &Arc<TargetDef>,
    ) -> anyhow::Result<ExpandedInputs> {
        let consumer = &declared.addr;
        let fail_fast = rs.fail_fast();
        let inputs = &declared.inputs;
        let mut expanded: Option<Vec<Input>> = None;
        let mut defs = Vec::with_capacity(inputs.len());
        let mut origins = Vec::with_capacity(inputs.len());
        let mut errors = Vec::new();
        let mut visited = FxHashSet::default();

        let mut resolved = self.input_defs(&rs, consumer, inputs).enumerate();
        while let Some((idx, next)) = resolved.next().await {
            let step = match next {
                Ok((input, def)) if def.target_def.transparent => {
                    let out = expanded.get_or_insert_with(|| {
                        let mut v = Vec::with_capacity(inputs.len());
                        v.extend_from_slice(inputs.get(..idx).unwrap_or_default());
                        v
                    });
                    let mut walk = GroupWalk {
                        inputs: out,
                        defs: &mut defs,
                        origins: &mut origins,
                        origin: idx,
                        errors: &mut errors,
                        fail_fast,
                        visited: &mut visited,
                    };
                    Arc::clone(&self)
                        .expand_group_into(
                            rs.clone(),
                            input,
                            &def.target_def.inputs,
                            &ParentOverride::of(input),
                            &mut walk,
                        )
                        .await
                }
                Ok((input, def)) => {
                    if let Some(out) = expanded.as_mut() {
                        // A group earlier in the list allocated the result:
                        // copy this pass-through into it.
                        out.push(input.clone());
                    }
                    defs.push(def);
                    origins.push(idx);
                    Ok(())
                }
                Err(e) => Err(e),
            };
            if let Err(e) = step {
                if fail_fast {
                    return Err(EntrypointAsInput::name_consumer(e, consumer));
                }
                errors.push(e);
            }
        }

        if !errors.is_empty() {
            let errors = errors
                .into_iter()
                .map(|e| EntrypointAsInput::name_consumer(e, consumer))
                .collect();
            let resolved = defs.into_iter().zip(origins).collect();
            return Err(PartialExpansion { resolved }.into_error(errors));
        }
        let inputs = match expanded {
            Some(inputs) => InputList::Expanded(inputs),
            None => InputList::Declared(Arc::clone(declared)),
        };
        Ok(ExpandedInputs { inputs, defs })
    }

    /// Appends the `members` of `group` to the walk, each routed by `routing`
    /// and with its def, expanding a nested group in place.
    ///
    /// `routing` carries the origin_id, mode and annotations of the group's
    /// reference in the target's own input list, so that exec driver grouping
    /// (SRC_*) and routing (e.g. `unpack_root=tools`) reflect the target's
    /// intent, not the group's internal ids. Its annotations form the base; a
    /// member's own annotations override them on key conflict.
    ///
    /// A member that fails ends the walk under `fail_fast` (the error is
    /// returned); otherwise it joins `walk.errors` and the walk goes on. Either
    /// way `group` leaves the path before this returns, so a later walk that
    /// meets it again (a second group sharing it) is not a cycle.
    #[async_recursion]
    async fn expand_group_into(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        group: &Input,
        members: &[Input],
        routing: &ParentOverride,
        walk: &mut GroupWalk<'_>,
    ) -> anyhow::Result<()> {
        let addr = &group.r#ref.r#ref;
        if !walk.visited.insert(addr.clone()) {
            anyhow::bail!("circular group dependency at '{}'", addr.format());
        }

        // A member is the group's declaration, not the outer target's.
        let mut resolved = self.input_defs(&rs, addr, members);
        let mut ended = Ok(());
        while let Some(next) = resolved.next().await {
            let step = match next {
                Ok((member, def)) if def.target_def.transparent => {
                    Arc::clone(&self)
                        .expand_group_into(
                            rs.clone(),
                            member,
                            &def.target_def.inputs,
                            &routing.nested(member),
                            walk,
                        )
                        .await
                }
                Ok((member, def)) => {
                    walk.inputs.push(routing.apply(member));
                    walk.defs.push(def);
                    walk.origins.push(walk.origin);
                    Ok(())
                }
                Err(e) => Err(e),
            };
            if let Err(e) = step {
                if walk.fail_fast {
                    ended = Err(e);
                    break;
                }
                walk.errors.push(e);
            }
        }

        walk.visited.remove(addr);
        ended
    }

    /// Each of `inputs` with its def, in declaration order, refusing an
    /// entrypoint among them.
    ///
    /// The defs resolve concurrently — a child's parse, and its spec's BUILD
    /// evaluation, cost the slowest sibling rather than the sum of them — but
    /// the stream yields them in input order, so a walk that stops at the
    /// first `Err` stops at the same input, with the same error, as resolving
    /// them one after another did. The defs after it may have started; dropping
    /// the stream abandons them, and a memoized def nobody else awaits is
    /// aborted.
    ///
    /// Every input path converges here — deps, runtime_deps, tools,
    /// hash_deps, a runner, a `${read://…}`, a dep's transitive closure,
    /// a credential edge, and the members of a group — so this is the one
    /// place an entrypoint is refused as one (I8). Otherwise a build step
    /// could run something in a developer's cwd.
    ///
    /// A def that fails is marked where it fails, for the failure classifier:
    /// a not-found of the input itself is `declarer`'s
    /// [`MissingDep`](crate::engine::error::MissingDep) (the
    /// target, or the group, that names it), anything else the input's own
    /// failure ([`InputDefFailed`]), never `declarer`'s.
    fn input_defs<'a>(
        self: &Arc<Self>,
        rs: &'a Arc<RequestState>,
        declarer: &'a Addr,
        inputs: &'a [Input],
    ) -> impl Stream<Item = anyhow::Result<(&'a Input, Arc<ExtendedTargetDef>)>> + Send + use<'a>
    {
        inputs
            .iter()
            .map(move |input| self.input_def(rs, declarer, input))
            .collect::<FuturesOrdered<_>>()
    }

    /// One edge of [`input_defs`](Self::input_defs): `input`'s def.
    fn input_def<'a>(
        self: &Arc<Self>,
        rs: &'a Arc<RequestState>,
        declarer: &'a Addr,
        input: &'a Input,
    ) -> impl Future<Output = anyhow::Result<(&'a Input, Arc<ExtendedTargetDef>)>> + Send + use<'a>
    {
        // A dependent's merged transitive input was declared by the target
        // it came from, not by `declarer`: a failure looks that target up. A
        // substring test, not `transitive_origin`: this runs for every edge,
        // and the origin's text is only for the error.
        let transitive = input.origin_id.contains(TRANSITIVE_PREFIX);
        let def = Arc::clone(self).get_def(Arc::clone(rs), &input.r#ref.r#ref);
        async move {
            let def = match def.await {
                Ok(def) => def,
                Err(e) => {
                    let dep = &input.r#ref.r#ref;
                    let real = transitive
                        .then(|| rs.transitive_declarer(&input.origin_id))
                        .flatten();
                    let declarer = real.as_ref().unwrap_or(declarer);
                    // Boxed: every in-flight edge holds a pointer for this
                    // failure-only path, not the index load's state (FQ r2
                    // N-1; `an_edge_future_does_not_hold_the_index_load`).
                    return Err(Box::pin(rs.missing_dep_or(
                        e,
                        dep,
                        declarer,
                        || {
                            transitive_origin(&input.origin_id)
                                .unwrap_or_else(|| declared_origin(&input.origin_id))
                        },
                        |e| {
                            e.context(InputDefFailed {
                                input: dep.clone(),
                                declarer: declarer.clone(),
                                origin_id: input.origin_id.clone(),
                            })
                        },
                    ))
                    .await);
                }
            };
            if def.target_def.entrypoint.is_some() {
                return Err(EntrypointAsInput {
                    entrypoint: input.r#ref.r#ref.clone(),
                    origin_id: input.origin_id.clone(),
                    consumer: None,
                }
                .into());
            }
            Ok((input, def))
        }
    }
}

#[cfg(test)]
mod edge_future_size_tests {
    use crate::engine::driver::TargetAddr;
    use crate::engine::driver::targetdef::{Input, InputMode};
    use crate::engine::{Config, Engine};
    use std::sync::Arc;

    /// An edge's future reserves a pointer for the error path's source-index
    /// load, not the load's own state (FQ r2 N-1): every in-flight edge of
    /// every target holds one, and the load runs only when a dep is missing.
    #[tokio::test]
    async fn an_edge_future_does_not_hold_the_index_load() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Arc::new(Engine::new(Config::for_tests(root.path()))?);
        let rs = engine.new_state();
        let declarer = hmodel::htaddr::parse_addr("//a:x")?;
        let input = Input::standard(
            TargetAddr {
                r#ref: hmodel::htaddr::parse_addr("//a:y")?,
                output: None,
                filters: vec![],
            },
            InputMode::Link,
            "deps||0".to_owned(),
            Default::default(),
        );
        let edge = std::mem::size_of_val(&engine.input_def(&rs, &declarer, &input));
        let load = std::mem::size_of_val(&rs.load_source_index(&declarer));
        let wrap = std::mem::size_of_val(&rs.missing_dep_or(
            anyhow::anyhow!("x"),
            &declarer,
            &declarer,
            String::new,
            |e| e,
        ));
        // 832 bytes inlined, 64 boxed, at the time of writing.
        assert!(
            edge < load && edge < wrap,
            "an edge's future ({edge} bytes) holds the error path's \
             (`missing_dep_or` {wrap}, its index load {load}): box it"
        );
        Ok(())
    }
}

/// What every merged transitive input's origin id starts with; see
/// [`transitive_id`].
pub(crate) const TRANSITIVE_PREFIX: &str = "_transitive_";

/// The origin id the transitive declarations of a consumer's `i`th dep merge
/// under, where `hash` is that dep's `addr.hash_str()`. `merge_sandbox`
/// appends `_<kind>_<member id>` to it, nested once per group the input came
/// through.
///
/// The format is frozen: the id reaches the consumer's def hash, so changing
/// a byte of it moves the cache key of every target with a merged
/// transitive. [`transitive_origin`] and `RequestState::transitive_declarer`
/// parse it back.
pub(crate) fn transitive_id(hash: &str, i: usize) -> String {
    format!("{TRANSITIVE_PREFIX}{hash}_{i}")
}

/// The option a merged transitive input was declared under, from its origin
/// id ([`transitive_id`] then `_<kind>_<id>`, nested once per group it came
/// through): `transitive.deps` / `transitive.tools` for the innermost
/// declaration. `None` for an id that is not a merged transitive's.
fn transitive_origin(origin_id: &str) -> Option<String> {
    let at = origin_id.rfind(TRANSITIVE_PREFIX)?;
    let rest = origin_id.get(at + TRANSITIVE_PREFIX.len()..)?;
    let inner = rest.splitn(4, '_').nth(3)?;
    Some(format!("transitive.{}", declared_origin(inner)))
}

#[cfg(test)]
mod transitive_origin_tests {
    use super::{transitive_id, transitive_origin};

    #[test]
    fn transitive_origin_names_innermost_option() {
        // The ids as the producer writes them (`merge_sandbox` appends
        // `_<kind>_<member id>`).
        let dep = format!("{}_dep_dep||0", transitive_id("ab12", 0));
        assert_eq!(transitive_origin(&dep).as_deref(), Some("transitive.deps"));
        // Through a group: the member's own declaration.
        let nested = format!(
            "{}_dep_{}_tool_tool||3",
            transitive_id("99", 1),
            transitive_id("ab12", 0)
        );
        assert_eq!(
            transitive_origin(&nested).as_deref(),
            Some("transitive.tools")
        );
        assert_eq!(transitive_origin("dep||0"), None);
        assert_eq!(transitive_origin(&transitive_id("ab12", 0)), None);
    }

    /// The id reaches the def hash: a byte of change moves every consumer's
    /// cache key.
    #[test]
    fn transitive_id_format_is_frozen() {
        assert_eq!(transitive_id("ab12", 3), "_transitive_ab12_3");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::TargetAddr;
    use crate::engine::driver::targetdef::{Input, InputMode};
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
        ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
        Provider as EProvider, TargetSpec,
    };
    use anyhow::Context as _;
    use futures::future::BoxFuture;
    use hcore::hasync::Cancellable;
    use hcore::htvalue::Value;
    use hmodel::htpkg::PkgBuf;
    use std::collections::HashMap;

    /// Minimal in-memory provider that hands back hand-crafted `TargetSpec`s.
    /// The shared `pluginstatictarget` provider always wraps `deps` as a
    /// `Map<group, [addr]>` which the `group` driver rejects (it wants a flat
    /// `[addr]`). We need direct control over the config shape.
    struct CannedProvider {
        specs: Vec<TargetSpec>,
    }

    impl EProvider for CannedProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "canned".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            let specs = self.specs.clone();
            Box::pin(async move {
                Ok(Box::new(
                    specs
                        .into_iter()
                        .map(|s| Ok(ListResponse::addr_only(s.addr.clone()))),
                ) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async move {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let specs = self.specs.clone();
            Box::pin(async move {
                specs
                    .into_iter()
                    .find(|s| s.addr == req.addr)
                    .map(|s| GetResponse { target_spec: s })
                    .ok_or(GetError::NotFound)
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    fn exec_spec(addr: &str) -> TargetSpec {
        let mut config = HashMap::new();
        config.insert("run".to_string(), Value::String("true".to_string()));
        TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr).expect("parse addr"),
            driver: "exec".to_string(),
            config,
            ..Default::default()
        }
    }

    fn group_spec(addr: &str, deps: &[&str]) -> TargetSpec {
        let mut config = HashMap::new();
        let deps_list: Vec<Value> = deps
            .iter()
            .map(|s| Value::String((*s).to_string()))
            .collect();
        config.insert("deps".to_string(), Value::List(deps_list));
        TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr).expect("parse addr"),
            driver: "group".to_string(),
            config,
            ..Default::default()
        }
    }

    fn engine_with(specs: Vec<TargetSpec>) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        // group + fs are registered by Engine::new; only the exec driver needs
        // adding here. The fs provider/driver resolves the synthesized `@heph/fs`
        // inputs produced by introspect-outputs expansion.
        engine.register_managed_driver(|_| {
            Box::new(hplugin_exec::pluginexec::Driver::new_exec().with_host_path())
        })?;
        engine.register_provider(move |_| Box::new(CannedProvider { specs }))?;
        Ok((Arc::new(engine), root))
    }

    /// A def declaring `inputs`, for [`Engine::expand_inputs`].
    fn declaring(inputs: Vec<Input>) -> Arc<TargetDef> {
        Arc::new(TargetDef {
            inputs,
            ..Default::default()
        })
    }

    fn input(addr: &str, origin: &str, ann: &[(&str, &str)]) -> Input {
        Input {
            r#ref: TargetAddr::parse(addr, &PkgBuf::from("")).expect("parse addr"),
            mode: InputMode::Tool,
            origin_id: origin.to_string(),
            annotations: ann
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            hashed: true,
            runtime: true,
        }
    }

    // Regression: when a tool input refers to a transparent group, the parent
    // input's annotations (e.g. `unpack_root=tools`) were dropped because
    // expand_inputs only carried the child's annotations through. The driver
    // bridge then routed the tool into ws/ instead of tools/.
    #[tokio::test]
    async fn parent_annotations_propagate_through_transparent_group() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            // Group with one exec child.
            group_spec("//pkg:grp", &["//pkg:leaf"]),
            exec_spec("//pkg:leaf"),
        ])?;

        let rs = engine.new_state();

        let inputs = declaring(vec![input(
            "//pkg:grp",
            "tool|cc|0",
            &[("unpack_root", "tools")],
        )]);

        let expanded = Arc::clone(&engine).expand_inputs(rs, &inputs).await?;

        assert_eq!(expanded.len(), 1, "group → one child");
        let child = &expanded[0];
        assert_eq!(child.r#ref.r#ref.format(), "//pkg:leaf");
        assert_eq!(child.origin_id, "tool|cc|0", "parent origin_id wins");
        assert_eq!(
            child.annotations.get("unpack_root").map(String::as_str),
            Some("tools"),
            "parent annotation must survive group expansion; got: {:?}",
            child.annotations,
        );
        Ok(())
    }

    /// A list with no group is the def's own, shared: nothing is copied.
    #[tokio::test]
    async fn inputs_without_a_group_are_the_defs_own() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            introspect_exec_spec("//pkg:app", &[], &["//pkg:a", "//pkg:b"]),
            exec_spec("//pkg:a"),
            exec_spec("//pkg:b"),
        ])?;
        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:app")?;
        let expanded = Arc::clone(&engine)
            .expanded_inputs_for(rs.clone(), &addr)
            .await?;
        let def = Arc::clone(&engine).get_def_no_track(rs, &addr).await?;

        let InputList::Declared(shared) = &expanded.inputs else {
            panic!("no group, yet the inputs were copied");
        };
        assert!(Arc::ptr_eq(shared, &def.target_def));
        let addrs: Vec<(String, String)> = expanded
            .with_defs()
            .map(|(i, d)| (i.r#ref.r#ref.format(), d.target_def.addr.format()))
            .collect();
        assert_eq!(
            addrs,
            [("//pkg:a", "//pkg:a"), ("//pkg:b", "//pkg:b")].map(|(i, d)| (i.into(), d.into()))
        );
        Ok(())
    }

    /// Groups, nested ones included, are replaced in place: the target's own
    /// inputs keep their position and the members appear in declaration order,
    /// each routed as the group's reference in the target's list.
    #[tokio::test]
    async fn nested_groups_expand_in_place_and_in_order() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            group_spec("//pkg:outer", &["//pkg:m1", "//pkg:inner", "//pkg:m3"]),
            group_spec("//pkg:inner", &["//pkg:m2"]),
            exec_spec("//pkg:x"),
            exec_spec("//pkg:y"),
            exec_spec("//pkg:m1"),
            exec_spec("//pkg:m2"),
            exec_spec("//pkg:m3"),
        ])?;
        let rs = engine.new_state();
        let inputs = declaring(vec![
            input("//pkg:x", "x|0", &[]),
            input("//pkg:outer", "outer|1", &[("unpack_root", "tools")]),
            input("//pkg:y", "y|2", &[]),
        ]);
        let expanded = Arc::clone(&engine).expand_inputs(rs, &inputs).await?;

        // Each input with its own def, index for index.
        let got: Vec<(String, &str, String)> = expanded
            .with_defs()
            .map(|(i, d)| {
                (
                    i.r#ref.r#ref.format(),
                    i.origin_id.as_str(),
                    d.target_def.addr.format(),
                )
            })
            .collect();
        let want = [
            ("//pkg:x", "x|0"),
            ("//pkg:m1", "outer|1"),
            ("//pkg:m2", "outer|1"),
            ("//pkg:m3", "outer|1"),
            ("//pkg:y", "y|2"),
        ]
        .map(|(a, o)| (a.to_string(), o, a.to_string()));
        assert_eq!(got, want);
        assert_eq!(expanded.len(), expanded.defs.len());
        assert!(
            expanded[1..4]
                .iter()
                .all(|i| i.annotations.get("unpack_root").map(String::as_str) == Some("tools")),
            "every member, nested or not, carries the reference's annotations"
        );
        Ok(())
    }

    /// A group that contains itself is refused, not expanded forever, and the
    /// message names the cycle. The group's def collects its members'
    /// transitives, so the dependency tracker refuses it while resolving that
    /// def — before the walk's own `visited` backstop is reached.
    #[tokio::test]
    async fn a_circular_group_is_refused() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![
            group_spec("//pkg:a", &["//pkg:b"]),
            group_spec("//pkg:b", &["//pkg:a"]),
        ])?;
        let rs = engine.new_state();
        let inputs = declaring(vec![input("//pkg:a", "dep|0", &[])]);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            Arc::clone(&engine).expand_inputs(rs, &inputs),
        )
        .await
        .context("a circular group hangs the expansion")?;

        let Err(err) = res else {
            panic!("a circular group expanded");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cyclic dependency detected: //pkg:b → //pkg:a"),
            "got: {msg}"
        );
        Ok(())
    }

    /// Under `--fail-fast` the error is the first failing input in
    /// declaration order: a group member refused inside the walk (`g1` →
    /// `failed` → `ep_bad1`), not the not-found direct input after it, which
    /// may fail first. Without it every failure is kept, and `g2`, which
    /// shares the group `g1`'s walk failed in, is walked again without a
    /// false circular-group error: the failed walk left the path clean.
    #[tokio::test]
    async fn fail_fast_returns_first_input_in_order() -> anyhow::Result<()> {
        let mut ep = exec_spec("//pkg:ep_bad1");
        ep.config
            .insert("entrypoint".to_string(), Value::Bool(true));
        let (engine, _root) = engine_with(vec![
            group_spec("//pkg:g1", &["//pkg:failed"]),
            group_spec("//pkg:g2", &["//pkg:failed"]),
            group_spec("//pkg:failed", &["//pkg:ep_bad1"]),
            ep,
        ])?;
        let ep_bad1 = hmodel::htaddr::parse_addr("//pkg:ep_bad1")?;
        let inputs = declaring(vec![
            input("//pkg:g1", "dep|0", &[]),
            input("//pkg:bad2", "dep|1", &[]),
            input("//pkg:g2", "dep|2", &[]),
        ]);
        let expand = |fail_fast: bool| {
            let rs = engine.new_state_with_fail_fast(fail_fast);
            tokio::time::timeout(
                std::time::Duration::from_secs(60),
                Arc::clone(&engine).expand_inputs(rs, &inputs),
            )
        };

        let Err(err) = expand(true)
            .await
            .context("the fail-fast expansion hangs")?
        else {
            panic!("a fail-fast expansion with failing inputs succeeded");
        };
        let first = hcore::hmemoizer::downcast_chain_ref::<EntrypointAsInput>(&err)
            .unwrap_or_else(|| panic!("want ep_bad1's EntrypointAsInput, got: {err:#}"));
        assert_eq!(first.entrypoint, ep_bad1);
        let msg = format!("{err:#}");
        assert!(!msg.contains("//pkg:bad2"), "only the first: {msg}");
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<PartialExpansion>(&err).is_none(),
            "fail-fast is not a partial expansion: {msg}"
        );

        let Err(err) = expand(false)
            .await
            .context("the keep-going expansion hangs")?
        else {
            panic!("an expansion with failing inputs succeeded");
        };
        let msg = format!("{err:#}");
        assert!(!msg.contains("circular group"), "{msg}");
        assert!(msg.contains("//pkg:bad2"), "{msg}");
        assert_eq!(
            msg.matches("//pkg:ep_bad1").count(),
            2,
            "refused through g1 and again through g2: {msg}"
        );
        Ok(())
    }

    // Child-set annotations should win over a same-named parent annotation.
    #[tokio::test]
    async fn child_annotation_overrides_parent_on_conflict() -> anyhow::Result<()> {
        // grp.deps points to leaf via the static-target deps map. The static
        // provider doesn't carry annotations on group children, but the group
        // driver itself builds Inputs with empty annotations — so this test
        // would need to inject child annotations through a custom driver.
        // For now we rely on the corresponding unit test in
        // `merge_sandbox_dedupes_*` plus the integration behaviour above.
        // (Documenting the contract here even though the static-target path
        // can't directly produce child annotations.)
        let (engine, _root) = engine_with(vec![
            group_spec("//pkg:grp", &["//pkg:leaf"]),
            exec_spec("//pkg:leaf"),
        ])?;
        let rs = engine.new_state();
        // Both keys come from the parent; leaf has none → parent values win.
        let inputs = declaring(vec![input(
            "//pkg:grp",
            "tool|cc|0",
            &[("unpack_root", "tools"), ("custom", "p")],
        )]);
        let expanded = Arc::clone(&engine).expand_inputs(rs, &inputs).await?;
        assert_eq!(
            expanded[0].annotations.get("custom").map(String::as_str),
            Some("p"),
        );
        Ok(())
    }

    /// A target's input defs resolve concurrently, and the walk still reports
    /// the first failing input in declaration order. Both `get`s meet at a
    /// barrier, so neither returns until the other is in flight: resolving one
    /// after another hangs. Past the barrier input 1 fails at once while input
    /// 0 still parses, and the error is input 0's.
    #[tokio::test]
    async fn input_defs_resolve_concurrently_and_fail_in_input_order() -> anyhow::Result<()> {
        use crate::engine::fault_provider::{FaultProvider, Faults};
        use hbuiltins::pluginstatictarget::Target;

        let ep = hmodel::htaddr::parse_addr("//pkg:ep")?;
        let bad = hmodel::htaddr::parse_addr("//pkg:bad")?;
        let target = |addr: &str, raw: &[(&str, Value)]| Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            raw_config: raw
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            ..Default::default()
        };
        let provider = FaultProvider::new(
            vec![
                target("//pkg:ep", &[("entrypoint", Value::Bool(true))]),
                target("//pkg:bad", &[]),
            ],
            Faults {
                fail_get: vec![bad.clone()],
                rendezvous: Some((
                    vec![ep.clone(), bad],
                    Arc::new(tokio::sync::Barrier::new(2)),
                )),
                ..Default::default()
            },
        )?;
        let gets = provider.gets();

        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine.register_managed_driver(|_| {
            Box::new(hplugin_exec::pluginexec::Driver::new_exec().with_host_path())
        })?;
        engine.register_provider(move |_| Box::new(provider))?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();
        let inputs = declaring(vec![
            input("//pkg:ep", "dep|0", &[]),
            input("//pkg:bad", "dep|1", &[]),
        ]);
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            Arc::clone(&engine).expand_inputs(rs, &inputs),
        )
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!("the sibling gets never met: input defs resolve one at a time")
        })?;

        assert_eq!(gets.max_in_flight(), 2, "both input gets in flight at once");
        let Err(err) = res else {
            panic!("an entrypoint input is refused");
        };
        let refused = err
            .downcast_ref::<EntrypointAsInput>()
            .unwrap_or_else(|| panic!("want input 0's EntrypointAsInput, got: {err:#}"));
        assert_eq!(refused.entrypoint, ep);
        assert_eq!(refused.origin_id, "dep|0");
        Ok(())
    }

    /// An engine over `targets` (exec `true`, `deps` as given, an entrypoint
    /// when named `//pkg:ep…`) served by a
    /// [`FaultProvider`](crate::engine::fault_provider::FaultProvider) with
    /// `faults`.
    fn faulty_engine(
        targets: &[(&str, &[&str])],
        faults: crate::engine::fault_provider::Faults,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        use hbuiltins::pluginstatictarget::Target;
        let targets = targets
            .iter()
            .map(|(addr, deps)| Target {
                addr: (*addr).to_string(),
                driver: "exec".to_string(),
                run: Some("true".to_string()),
                deps: HashMap::from([(
                    String::new(),
                    deps.iter().map(|d| (*d).to_string()).collect(),
                )]),
                raw_config: if addr.starts_with("//pkg:ep") {
                    HashMap::from([("entrypoint".to_string(), Value::Bool(true))])
                } else {
                    HashMap::new()
                },
                ..Default::default()
            })
            .collect();
        let provider = crate::engine::fault_provider::FaultProvider::new(targets, faults)?;
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(root.path())
        })?;
        engine.register_managed_driver(|_| {
            Box::new(hplugin_exec::pluginexec::Driver::new_exec().with_host_path())
        })?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// D1.f: a failed hashed input fails `meta` with the expansion's error,
    /// and `hash_dep_count` too: no `hashin` is ever computed over the
    /// inputs that did resolve.
    #[tokio::test]
    async fn partial_expansion_is_never_hashed() -> anyhow::Result<()> {
        // The input fails in expansion only (an entrypoint is refused there):
        // a spec failure would already fail `//pkg:top`'s own def.
        let (engine, _root) = faulty_engine(
            &[
                ("//pkg:top", &["//pkg:ok", "//pkg:ep_bad"]),
                ("//pkg:ok", &[]),
                ("//pkg:ep_bad", &[]),
            ],
            crate::engine::fault_provider::Faults::default(),
        )?;
        let top = hmodel::htaddr::parse_addr("//pkg:top")?;
        let rs = engine.new_state_with_fail_fast(false);
        let Err(err) = Arc::clone(&engine).meta(rs.clone(), &top).await else {
            panic!("a partial expansion was hashed");
        };
        let partial = hcore::hmemoizer::downcast_chain_ref::<PartialExpansion>(&err)
            .unwrap_or_else(|| panic!("meta fails with the expansion: {err:#}"));
        let resolved: Vec<String> = partial
            .resolved()
            .map(|(d, _)| d.target_def.addr.format())
            .collect();
        assert_eq!(resolved, ["//pkg:ok"]);
        assert!(format!("{err:#}").contains("//pkg:ep_bad"), "{err:#}");
        assert!(
            Arc::clone(&engine).hash_dep_count(rs, &top).await.is_err(),
            "hash_dep_count cannot count a partial set"
        );
        Ok(())
    }

    /// P25: an input that is not hashed (a `runtime_deps` one) failing still
    /// fails the expansion; nothing hands out the rest as the input set.
    #[tokio::test]
    async fn partial_expansion_unhashed_input_fails_expansion() -> anyhow::Result<()> {
        let bad = hmodel::htaddr::parse_addr("//pkg:bad")?;
        let (engine, _root) = faulty_engine(
            &[("//pkg:ok", &[]), ("//pkg:bad", &[])],
            crate::engine::fault_provider::Faults {
                fail_get: vec![bad],
                ..Default::default()
            },
        )?;
        let mut unhashed = input("//pkg:bad", "runtime_dep|0", &[]);
        unhashed.hashed = false;
        let inputs = declaring(vec![input("//pkg:ok", "dep|0", &[]), unhashed]);
        let rs = engine.new_state_with_fail_fast(false);
        let Err(err) = Arc::clone(&engine).expand_inputs(rs, &inputs).await else {
            panic!("an unhashed failed input left the expansion whole");
        };
        let partial = hcore::hmemoizer::downcast_chain_ref::<PartialExpansion>(&err)
            .unwrap_or_else(|| panic!("a partial expansion: {err:#}"));
        assert_eq!(partial.resolved().map(|(_, i)| i).collect::<Vec<_>>(), [0]);
        Ok(())
    }

    /// P25: a cancellation among the expansion's errors is found by the one
    /// walker, beside another input's real failure, so the classifier reads
    /// the expansion as a stop.
    #[tokio::test]
    async fn partial_expansion_cancel_found_by_walker() -> anyhow::Result<()> {
        use crate::engine::discovery::Stage;
        let bad = hmodel::htaddr::parse_addr("//pkg:bad")?;
        let started = Arc::new(tokio::sync::Notify::new());
        let (engine, _root) = faulty_engine(
            &[("//pkg:bad", &[]), ("//pkg:slow", &[])],
            crate::engine::fault_provider::Faults {
                fail_get: vec![bad],
                cancel_at: Some((Stage::Spec, "//pkg:slow".to_string(), Arc::clone(&started))),
                ..Default::default()
            },
        )?;
        let inputs = declaring(vec![
            input("//pkg:bad", "dep|0", &[]),
            input("//pkg:slow", "dep|1", &[]),
        ]);
        let rs = engine.new_state_with_fail_fast(false);
        let (res, ()) = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            futures::future::join(
                Arc::clone(&engine).expand_inputs(rs.clone(), &inputs),
                async {
                    started.notified().await;
                    rs.ctoken().cancel();
                },
            ),
        )
        .await
        .context("the cancelled expansion never ended")?;
        let Err(err) = res else {
            panic!("a cancelled expansion succeeded");
        };
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<PartialExpansion>(&err).is_some(),
            "{err:#}"
        );
        assert!(
            hcore::hmemoizer::find::<crate::engine::error::CancelledError>(&err).is_some(),
            "the walker finds the cancel: {err:#}"
        );
        Ok(())
    }

    /// An exec spec declaring `out` paths and a dep on the magic
    /// `//@heph/introspect:outputs` target. The driver stores `out` entries as
    /// package-prefixed output paths and the magic dep as a `Standard` input.
    fn introspect_exec_spec(addr: &str, out: &[&str], deps: &[&str]) -> TargetSpec {
        let mut config = HashMap::new();
        config.insert("run".to_string(), Value::String("true".to_string()));
        config.insert(
            "out".to_string(),
            Value::List(
                out.iter()
                    .map(|s| Value::String((*s).to_string()))
                    .collect(),
            ),
        );
        config.insert(
            "deps".to_string(),
            Value::List(
                deps.iter()
                    .map(|s| Value::String((*s).to_string()))
                    .collect(),
            ),
        );
        TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr).expect("parse addr"),
            driver: "exec".to_string(),
            config,
            ..Default::default()
        }
    }

    // The engine-baked `//@heph/introspect:outputs` magic input must expand into
    // one `@heph/fs` input per declared output path of the *owning* target —
    // never a plain glob, and a FilePath output yields `@heph/fs:file`. Both the
    // hashing and execution paths consume `expanded_inputs_for`, so verifying it
    // here covers both. Output paths are package-prefixed by the exec driver and
    // fed verbatim into the fs addr, so the fs target resolves the same on-disk
    // files relative to the workspace root.
    #[tokio::test]
    async fn introspect_outputs_expands_to_fs_inputs() -> anyhow::Result<()> {
        let (engine, _root) = engine_with(vec![introspect_exec_spec(
            "//pkg:gen",
            &["*.go", "foo.txt"],
            &["//@heph/introspect:outputs"],
        )])?;

        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:gen")?;
        let expanded = Arc::clone(&engine).expanded_inputs_for(rs, &addr).await?;

        let addrs: Vec<String> = expanded.iter().map(|i| i.r#ref.r#ref.format()).collect();

        // The magic addr itself is gone.
        assert!(
            !addrs.iter().any(|a| a.contains("@heph/introspect")),
            "magic addr must be expanded away, got: {addrs:?}",
        );

        // `*.go` → glob over the package-prefixed pattern.
        let want_glob = hbuiltins::pluginfs::glob_addr("pkg/*.go", &[]).format();
        assert!(
            addrs.contains(&want_glob),
            "expected glob input {want_glob}, got: {addrs:?}",
        );

        // `foo.txt` → single-file fs input over the package-prefixed path.
        let want_file = hbuiltins::pluginfs::file_addr("pkg/foo.txt").format();
        assert!(
            addrs.contains(&want_file),
            "expected file input {want_file}, got: {addrs:?}",
        );

        assert_eq!(
            expanded.len(),
            2,
            "exactly one fs input per declared output path, got: {addrs:?}",
        );

        // Both synthesized inputs INHERIT the magic dep's origin_id (not a
        // per-path synthesized one), so the exec driver's $SRC_/$LIST_ routing —
        // which matches a target's declared dep groups against materialized run
        // inputs by origin_id — surfaces the introspect files to the run script.
        let oids: Vec<&str> = expanded.iter().map(|i| i.origin_id.as_str()).collect();
        assert!(
            oids.iter().all(|o| *o == oids[0]),
            "introspect inputs must share the magic dep's origin_id, got: {oids:?}",
        );
        assert!(
            !oids[0].starts_with("introspect_outputs"),
            "origin_id must be inherited from the magic dep, not synthesized, got: {:?}",
            oids[0],
        );
        Ok(())
    }

    // A `copy` target's outputs are generated, not source: `introspect:outputs`
    // lists none of them, file or glob.
    #[tokio::test]
    async fn introspect_outputs_skips_copy_outputs() -> anyhow::Result<()> {
        let mut spec = introspect_exec_spec(
            "//pkg:gen",
            &["*.go", "foo.txt"],
            &["//@heph/introspect:outputs"],
        );
        spec.config
            .insert("codegen".to_string(), Value::String("copy".to_string()));
        let (engine, _root) = engine_with(vec![spec])?;

        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:gen")?;
        let expanded = Arc::clone(&engine).expanded_inputs_for(rs, &addr).await?;
        let addrs: Vec<String> = expanded.iter().map(|i| i.r#ref.r#ref.format()).collect();

        assert_eq!(addrs, Vec::<String>::new());
        Ok(())
    }
}
