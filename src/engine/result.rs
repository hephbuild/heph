use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::{
    CancelledError, CycleError, MultiError, ProcessFailed, TargetFailure, TargetNotFoundError,
    UpstreamFailed,
};
use crate::engine::provider::{
    GetError, GetRequest, GetResponse, ListRequest, ProbeRequest, ProviderExecutor, State,
    TargetSpec,
};
use crate::engine::request_state::RequestState;
use crate::engine::spec::EngineTargetSpec;
use crate::hmemoizer::{downcast_chain_ref, unwrap_arc_err};
use crate::htaddr::Addr;
use crate::htmatcher::MatchResult;
use crate::htpkg::PkgBuf;
use async_recursion::async_recursion;
use enclose::enclose;

use crate::defer;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::link::LinkedTargetDef;
use crate::engine::local_cache::{CacheArtifact, ManifestArtifactType};
use crate::engine::result_lock::ResultReadGuard;
use crate::hartifactcontent::{Content, ReadSeek, WalkEntry};
use crate::htmatcher::Matcher;
use anyhow::Context;
use futures::TryStreamExt;
use crate::engine::grow_stack::{GrowStack, grow_stack};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use tokio::task::JoinSet;

/// How long to block on the per-addr result lock before surfacing a "waiting on
/// lock" notice (with the holder's pid) to the progress stream. The notice is
/// purely informational; the wait itself continues until acquired or cancelled.
const RESULT_LOCK_NOTICE: std::time::Duration = std::time::Duration::from_secs(5);

/// The boxed future produced by `#[async_recursion]` for `result_addr_impl`,
/// wrapped per-poll by [`GrowStack`] in [`Engine::result_addr`].
type BoxedResultFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Arc<EResult>>> + Send + 'a>>;

/// rs carries the parent addr (set by result_addr via with_parent) so the executor
/// does not need to store it separately.
struct EngineProviderExecutor {
    engine: Weak<Engine>,
    rs: Arc<RequestState>,
}

impl ProviderExecutor for EngineProviderExecutor {
    fn result<'a>(
        &'a self,
        addr: &'a Addr,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            engine
                .result_addr(
                    self.rs.clone(),
                    addr,
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            let rs = self.rs.clone();

            // Collect packages eagerly (non-Send iterator dropped before first await)
            let pkg_iter = engine.packages(m, &rs).await?;
            let pkgs: Vec<String> = pkg_iter.collect::<anyhow::Result<_>>()?;

            let mut result = Vec::new();

            for pkg_str in pkgs {
                let pkg = PkgBuf::from(pkg_str.as_str());

                let states = Arc::clone(&engine).probe_segments(&rs, &pkg).await?;

                for provider in &engine.providers {
                    if rs.skip_providers.contains(&provider.name)
                        || extra_skip.iter().any(|n| n == &provider.name)
                    {
                        continue;
                    }
                    // Collect list results eagerly (non-Send iterator dropped before next await)
                    let list_iter = provider
                        .provider
                        .list(
                            ListRequest {
                                request_id: rs.request_id().to_string(),
                                package: pkg.clone(),
                                states: states
                                    .iter()
                                    .filter(|s| s.provider == provider.name)
                                    .cloned()
                                    .collect(),
                            },
                            rs.ctoken(),
                        )
                        .await?;
                    let raw: Vec<_> = list_iter.collect::<anyhow::Result<_>>()?;

                    for item in raw {
                        let addr = item.addr;
                        if addr.package != pkg {
                            continue;
                        }

                        match m.matches_addr(&addr) {
                            MatchResult::MatchYes => result.push(addr),
                            MatchResult::MatchNo => {}
                            MatchResult::MatchShrug => {
                                let spec = match Arc::clone(&engine)
                                    .get_spec(rs.clone(), &addr)
                                    .await
                                {
                                    Ok(spec) => Ok(spec),
                                    Err(e)
                                        if downcast_chain_ref::<TargetNotFoundError>(&e)
                                            .is_some() =>
                                    {
                                        continue;
                                    }
                                    // Cycle means this target depends (transitively) on the
                                    // current query caller. It cannot be a dep of the caller
                                    // — skip it from the query results rather than error.
                                    Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => {
                                        continue;
                                    }
                                    res => res,
                                }?;

                                match m.matches_spec(&spec) {
                                    MatchResult::MatchYes => result.push(addr),
                                    MatchResult::MatchNo => {}
                                    MatchResult::MatchShrug => {
                                        let def_res =
                                            Arc::clone(&engine).get_def(rs.clone(), &addr).await;
                                        let def = match def_res {
                                            Ok(def) => def,
                                            // Same as the get_spec branch: cycle means this
                                            // target transitively depends on the query caller —
                                            // it can't be a dep of the caller. Skip it.
                                            Err(e)
                                                if downcast_chain_ref::<CycleError>(&e)
                                                    .is_some() =>
                                            {
                                                continue;
                                            }
                                            Err(e) => return Err(e),
                                        };
                                        if m.matches(&def.target_def) == MatchResult::MatchYes {
                                            result.push(addr);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            Ok(result)
        })
    }
}

impl fmt::Debug for dyn Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Artifact")
    }
}

pub struct ExtendedTargetDef {
    pub target_def: Arc<TargetDef>,
    pub applied_transitive: Option<Sandbox>,
    /// Registry name of the driver that produced `target_def`. Folded into
    /// `hashin` so swapping drivers under the same addr invalidates cache —
    /// even if the produced `TargetDef` bytes happen to match.
    pub driver: String,
}

#[derive(Clone)]
pub struct ArtifactMeta {
    pub hashout: String,
}

/// Aggregate of a multi-target fanout. `errors` is non-empty only when the
/// request was started with `Engine::new_state_with_fail_fast(false)`; with
/// fail-fast (default), the first error short-circuits `Engine::result` and
/// `errors` stays empty.
#[derive(Default)]
pub struct BatchResult {
    pub ok: Vec<Arc<EResult>>,
    pub errors: Vec<(Addr, anyhow::Error)>,
}

#[derive(Clone, Default)]
pub struct EResult {
    pub artifacts: Vec<Arc<dyn Content>>,
    /// Auxiliary artifacts a dependent target materializes into its sandbox
    /// without surfacing in SRC/list env routing. Sourced from the producing
    /// target's `support_files` declaration; never filtered by output group.
    pub support_artifacts: Vec<Arc<dyn Content>>,
    pub artifacts_meta: Vec<ArtifactMeta>,
}

/// A cache-backed artifact paired with the read lock guarding its cache entry.
/// Delegates every [`Content`] method to the inner artifact; the guard is held
/// purely for RAII, so the cache entry cannot be overwritten/deleted while any
/// handle to it (here, or cloned into a dependent's sandbox input) is alive. The
/// lock releases when the last `Arc<dyn Content>` for the entry drops.
struct GuardedArtifact {
    inner: Arc<dyn Content>,
    _lock: Arc<ResultReadGuard>,
}

impl Content for GuardedArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        self.inner.reader()
    }
    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        self.inner.walk()
    }
    fn hashout(&self) -> anyhow::Result<String> {
        self.inner.hashout()
    }
    fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
        self.inner.seekable_reader()
    }
    fn byte_size(&self) -> Option<u64> {
        self.inner.byte_size()
    }
}

/// Build an [`EResult`] from cached artifacts, filtering by output group and
/// type, and attaching `guard` (the read lock for this target's cache entry) to
/// each kept artifact. `guard` is `None` only for the non-cacheable (force/shell)
/// path, whose artifacts are ephemeral and need no long-lived lock.
fn build_eresult(
    cached: Vec<CacheArtifact>,
    artifacts_meta: Vec<ArtifactMeta>,
    outputs: &[String],
    guard: Option<Arc<ResultReadGuard>>,
) -> EResult {
    let wrap = |a: CacheArtifact| -> Arc<dyn Content> {
        let inner: Arc<dyn Content> = Arc::new(a);
        match &guard {
            Some(lock) => Arc::new(GuardedArtifact {
                inner,
                _lock: Arc::clone(lock),
            }),
            None => inner,
        }
    };

    let mut artifacts: Vec<Arc<dyn Content>> = Vec::new();
    let mut support_artifacts: Vec<Arc<dyn Content>> = Vec::new();
    for a in cached {
        match a.r#type {
            ManifestArtifactType::Output if outputs.contains(&a.group) => artifacts.push(wrap(a)),
            ManifestArtifactType::SupportFile => support_artifacts.push(wrap(a)),
            _ => {}
        }
    }
    EResult {
        artifacts,
        support_artifacts,
        artifacts_meta,
    }
}

pub type InteractiveInner = Box<
    dyn for<'io> FnOnce(
            Option<&'io mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin)>,
            Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
            Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
        ) -> futures::future::BoxFuture<'io, anyhow::Result<()>>
        + Send,
>;

pub type InteractiveWrapper = Arc<
    dyn Fn(InteractiveInner) -> futures::future::BoxFuture<'static, anyhow::Result<()>>
        + Send
        + Sync,
>;

#[derive(Default, Clone)]
pub struct ResultOptions {
    pub force: bool,
    pub shell: bool,
    pub interactive: Option<InteractiveWrapper>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutputMatcher {
    None,
    All,
    Exact(Vec<String>),
}

struct ExecuteOptions<'a> {
    hashin: &'a String,
    spec: &'a TargetSpec,
    def: &'a LinkedTargetDef,
    force: bool,
    interactive: Option<InteractiveWrapper>,
    shell: bool,
}

/// Single classifier chokepoint for any target error.
///
/// Decides whether an error is this target's **own** failure (record it once in
/// the per-request registry, return a fresh `UpstreamFailed{root: addr}`) or
/// merely collateral from a failing dependency (propagate a fresh
/// `UpstreamFailed{root}` without recording). Cancellation is propagated as-is.
///
/// Every collateral hop replaces (never wraps) its incoming error with a fresh
/// `UpstreamFailed`, so chain depth stays O(1) on any graph.
fn classify_failure(
    rs: &RequestState,
    addr: &Addr,
    interactive: bool,
    e: anyhow::Error,
) -> anyhow::Error {
    // Cancellation: propagate unchanged, never record.
    if downcast_chain_ref::<CancelledError>(&e).is_some() {
        return e;
    }

    // Cyclic dependency: a structural error detected at potentially many nodes
    // of the cycle, not a single target's own work failing. Propagate unchanged
    // so the cycle surfaces directly to the caller (and never gets masked behind
    // an `UpstreamFailed` marker or duplicated into the failure registry).
    if downcast_chain_ref::<CycleError>(&e).is_some() {
        return e;
    }

    // Already a collateral marker: reuse the existing root, do not record.
    if let Some(uf) = downcast_chain_ref::<UpstreamFailed>(&e) {
        return UpstreamFailed {
            root: uf.root.clone(),
        }
        .into();
    }

    // Aggregation of child failures. If *every* child is already a recorded
    // collateral marker (or a cancellation), the real root causes live in the
    // registry downstream — this target has no own work to blame, so collapse to
    // a cheap marker without recording. But if any child is a genuine,
    // unrecorded cause (e.g. a `TargetNotFound` raised while resolving an input's
    // def in `link`/`meta`, which never passes through `result_addr`), fall
    // through and record the whole aggregation against this target so the detail
    // (every broken input) isn't lost.
    if let Some(multi) = downcast_chain_ref::<MultiError>(&e) {
        let all_collateral = multi.0.iter().all(|inner| {
            downcast_chain_ref::<UpstreamFailed>(inner).is_some()
                || downcast_chain_ref::<CancelledError>(inner).is_some()
        });
        if all_collateral {
            let root = multi
                .0
                .iter()
                .find_map(|inner| {
                    downcast_chain_ref::<UpstreamFailed>(inner).map(|u| u.root.clone())
                })
                .unwrap_or_else(|| addr.clone());
            return UpstreamFailed { root }.into();
        }
    }

    // This target's own failure (or an aggregation of unrecorded causes): record
    // the rich diagnostic once (first-writer-wins) and propagate a cheap marker.
    // Interactive targets stream their output straight to the user's terminal as
    // they run, so the captured log tail is redundant — drop it from the box.
    let log_tail = if interactive {
        None
    } else {
        extract_log_tail(&e)
    };
    rs.record_failure(
        addr.clone(),
        Arc::new(TargetFailure::new(addr.clone(), log_tail, e)),
    );
    UpstreamFailed { root: addr.clone() }.into()
}

/// Pull the captured log tail out of a `ProcessFailed` anywhere in the chain so
/// the recorded `TargetFailure` can surface it in its diagnostic.
fn extract_log_tail(e: &anyhow::Error) -> Option<String> {
    downcast_chain_ref::<ProcessFailed>(e).map(|pf| pf.log_tail.clone())
}

/// At the outermost `result_addr` frame (the directly-requested target, with no
/// parent), replace the lightweight `UpstreamFailed` marker with a clone of the
/// rich recorded `TargetFailure` so direct API/library consumers get the real
/// root cause rather than "dependency failed". The CLI renders from the registry
/// instead, so this is purely about the value returned to direct callers. No-op
/// for inner frames and for non-marker errors (cancellation, cycles, …).
fn surface_top(is_top: bool, rs: &RequestState, e: anyhow::Error) -> anyhow::Error {
    if !is_top {
        return e;
    }
    let is_marker = downcast_chain_ref::<UpstreamFailed>(&e).is_some()
        || downcast_chain_ref::<MultiError>(&e).is_some();
    if !is_marker {
        return e;
    }
    // Prefer the rich failure for the marker's named root.
    if let Some(tf) =
        downcast_chain_ref::<UpstreamFailed>(&e).and_then(|uf| rs.get_failure(&uf.root))
    {
        return anyhow::Error::new((*tf).clone());
    }
    // Named root wasn't recorded (e.g. a link-time resolution aggregation whose
    // causes were recorded against the individual deps). Surface the first
    // recorded root cause — there is always at least one for a real failure.
    if let Some(tf) = rs.first_failure() {
        return anyhow::Error::new((*tf).clone());
    }
    e
}

impl Engine {
    /// Resolve a target's result. Thin wrapper over [`Self::result_addr_impl`]
    /// that grows the physical stack on demand.
    ///
    /// Every dependency-graph edge recurses through here, and a cache-warm
    /// descent polls the whole subtree synchronously in one go (~100 KiB of
    /// stack per level). Wrapping each level's poll in [`grow_stack`] keeps that
    /// cascade from overflowing the 2 MiB tokio worker stack on deep graphs.
    /// See `engine::grow_stack` for the full rationale.
    pub fn result_addr<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &'a Addr,
        outputs: OutputMatcher,
        opts: &'a ResultOptions,
    ) -> GrowStack<BoxedResultFuture<'a>> {
        grow_stack(self.result_addr_impl(rs, addr, outputs, opts))
    }

    #[async_recursion]
    async fn result_addr_impl(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<Arc<EResult>> {
        if opts.shell && opts.interactive.is_none() {
            anyhow::bail!("cannot use --shell in non-interactive mode");
        }

        // Stop the moment the request is cancelled (Ctrl-C). Every queued
        // target in a batch enters here; bailing before spec/def resolution
        // and execution means a cancelled build doesn't start new work — it
        // just unwinds. In-flight targets that are already past this point are
        // aborted by their driver's cancellation handling.
        if rs.ctoken().is_cancelled() {
            return Err(CancelledError.into());
        }

        // Announce worker capacity once per request. Covers the single-target
        // entry (`run` of one addr) that bypasses `Engine::result`; the once-guard
        // makes the dep recursion below a no-op.
        rs.announce_max_workers(self.max_workers);

        // Single-target entry (`run` of one addr, which bypasses `Engine::result`):
        // claim the matched stream and emit the set-of-one as already-complete
        // (no `~`). The once-guard keeps this silent for dep recursion and for
        // result_addr calls under a batch `result` (which already claimed) — only
        // a genuine top-level single addr wins the claim here.
        if rs.claim_matched_stream() {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: vec![addr.format()],
                complete: true,
            });
        }

        // Directly-requested target (no parent) — the outermost frame. Used below
        // to surface the rich recorded failure to the caller instead of the
        // internal `UpstreamFailed` marker.
        let is_top = rs.parent.is_none();

        // Cycle check: fires for every caller (including those awaiting an in-flight future)
        // before the memoizer blocks, preventing memoizer deadlocks on dependency cycles.
        if let Some(ref parent) = rs.parent {
            let mut dag = rs.data.dep_dag.lock();
            dag.add_dep(parent, addr).map_err(anyhow::Error::new)?;
        }

        // Set addr as parent so all sub-calls carry the right context for cycle detection.
        // Done outside the memoizer so context setup isn't buried in the deduplication boundary.
        let rs = rs.with_parent(addr.clone());

        // Transparent targets (groups) never execute — inline their deps' results.
        // Handled before the memoizer: nothing to deduplicate for groups, and calling
        // result_addr recursively here is safe because #[async_recursion] boxes the future,
        // breaking the Send inference cycle that would occur inside the memoizer closure.
        //
        // Use _no_track: result_addr just updated `parent → addr` above and set parent=addr;
        // calling tracked get_def would try to record addr→addr (spurious self-cycle).
        let def = match Arc::clone(&self).get_def_no_track(rs.clone(), addr).await {
            Ok(def) => def,
            Err(e) => {
                return Err(surface_top(
                    is_top,
                    &rs,
                    classify_failure(&rs, addr, opts.interactive.is_some(), e),
                ));
            }
        };
        if def.target_def.transparent {
            let mut opts = opts.clone();
            if opts.shell {
                opts.interactive = None;
            }

            let futures: Vec<_> = def
                .target_def
                .inputs
                .iter()
                .map(|input| {
                    let dep_addr = input.r#ref.r#ref.clone();
                    enclose!((self => engine, rs, opts) async move {
                        engine
                            .result_addr(rs, &dep_addr, OutputMatcher::All, &opts)
                            .await
                    })
                })
                .collect();
            let results =
                match crate::engine::fanout::join_all_failable(futures, rs.fail_fast()).await {
                    Ok(results) => results,
                    Err(e) => {
                        return Err(surface_top(
                            is_top,
                            &rs,
                            classify_failure(&rs, addr, opts.interactive.is_some(), e),
                        ));
                    }
                };
            let mut merged = EResult::default();
            for r in results {
                merged.artifacts.extend(r.artifacts.iter().cloned());
                merged
                    .support_artifacts
                    .extend(r.support_artifacts.iter().cloned());
                merged
                    .artifacts_meta
                    .extend(r.artifacts_meta.iter().cloned());
            }
            return Ok(Arc::new(merged));
        }

        // Sort Exact output names so distinct caller-side orderings of the same
        // logical output set share one memoizer entry.
        let mut key_outputs = outputs.clone();
        if let OutputMatcher::Exact(names) = &mut key_outputs {
            names.sort();
        }
        let key = (addr.clone(), key_outputs);
        let opts = opts.clone();
        let interactive = opts.interactive.is_some();
        let res = rs
            .data
            .mem_result
            .once(
                key,
                enclose!((self => engine, rs, addr, outputs) move || async move {
                    match engine.inner_result_addr(rs.clone(), &addr, outputs, &opts).await {
                        Ok(v) => Ok(Arc::new(v)),
                        Err(e) => Err(classify_failure(&rs, &addr, interactive, e)),
                    }
                }),
            )
            .await
            .map_err(unwrap_arc_err);

        match res {
            Ok(v) => Ok(v),
            Err(e) => Err(surface_top(is_top, &rs, e)),
        }
    }

    pub async fn result(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<BatchResult> {
        let mut opts = opts.clone();
        if !matches!(matcher, Matcher::Addr(_)) {
            opts.interactive = None;
        }

        // Announce worker capacity up front so the client can paint a fixed
        // worker-slot indicator before any execute lands.
        rs.announce_max_workers(self.max_workers);

        let fail_fast = rs.fail_fast();
        let mut set: JoinSet<(Addr, anyhow::Result<Arc<EResult>>)> = JoinSet::new();

        // Only the first/top-level `result` streams the matched set. Inner
        // invocations sharing this request's data stay silent — re-emitting
        // would inflate the client's matched count and trip `complete` early.
        let owns_matched = rs.claim_matched_stream();

        // Advertise the matched line up front (provisional, empty set) so the
        // client paints "~0" the instant the query starts, instead of waiting
        // for the first match to stream — the matcher walk can take a while.
        if owns_matched {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: Vec::new(),
                complete: false,
            });
        }

        let stream = Arc::clone(&self).query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            // Stop enqueuing new targets once cancelled — don't keep draining
            // the matcher and spawning work that would immediately bail.
            if rs.ctoken().is_cancelled() {
                break;
            }
            // Announce each match as it resolves so the client can render a
            // provisional "done X / ~N" that grows while the matcher streams.
            if owns_matched {
                rs.emit(crate::engine::event::BuildEventKind::Matched {
                    addrs: vec![addr.format()],
                    complete: false,
                });
            }
            crate::hmemoizer::join_set_spawn(
                &mut set,
                enclose!((self => engine, rs, opts, addr) async move {
                    let r = engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await;
                    (addr, r)
                }),
            );
        }
        // Matcher fully resolved: mark the matched set final (drops the `~`).
        if owns_matched {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: Vec::new(),
                complete: true,
            });
        }

        let mut ok: Vec<Arc<EResult>> = vec![];
        let mut errors: Vec<(Addr, anyhow::Error)> = vec![];
        // First genuine (non-cancellation) failure under fail_fast. We never
        // break out of the JoinSet — doing so would drop it and tear down
        // in-flight tasks mid-execution. Instead, the first failure *signals*
        // every other target to stop (cancelling the request token broadcasts
        // SIGINT to running children) and we keep draining until they have all
        // stopped by themselves, then return this error.
        let mut fatal: Option<anyhow::Error> = None;
        while let Some(joined) = set.join_next().await {
            let (addr, res) = match joined {
                Ok(pair) => pair,
                // A task panicked (we never abort tasks). Capture it, signal
                // stop, and keep draining the rest — don't propagate via `?`,
                // which would drop the JoinSet.
                Err(join_err) => {
                    if fatal.is_none() {
                        fatal = Some(anyhow::Error::new(join_err).context("result task panicked"));
                        rs.ctoken().cancel();
                    }
                    continue;
                }
            };
            match res {
                Ok(v) => ok.push(v),
                Err(e) if downcast_chain_ref::<CancelledError>(&e).is_some() => {
                    // Cancellation is stop-fallout, not a genuine failure: the
                    // token is cancelled, so we surface a single `CancelledError`
                    // after draining rather than recording it per-addr.
                }
                Err(e) => {
                    if !fail_fast {
                        errors.push((addr, e));
                    } else if fatal.is_none() {
                        // Fail-fast: tell everything to stop, then wait for it.
                        // Failures that land after we signalled don't override it.
                        fatal = Some(e);
                        rs.ctoken().cancel();
                    }
                }
            }
        }

        if let Some(e) = fatal {
            return Err(e);
        }
        // A cancelled token (Ctrl-C, or a signalled stop) aborts the whole build
        // regardless of `fail_fast`: surface it so the caller reports an abort, not
        // success. Genuine failures collected before the stop remain in the
        // request's failure registry for rich rendering.
        if rs.ctoken().is_cancelled() {
            return Err(CancelledError.into());
        }

        Ok(BatchResult { ok, errors })
    }

    async fn inner_result_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<EResult> {
        let addr_str = addr.format();
        crate::engine::event::emit_scope(
            &rs,
            crate::engine::event::BuildEventKind::ResultStart {
                addr: addr_str.clone(),
            },
            move |error| crate::engine::event::BuildEventKind::ResultEnd {
                addr: addr_str,
                error,
            },
            async {
                // Use _no_track: result_addr set parent=addr before entering the memoizer,
                // so tracked variants would record addr→addr.
                let spec = Arc::clone(&self)
                    .get_spec_no_track(rs.clone(), addr)
                    .await?;
                let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;

                // `link` and `meta` operate on disjoint data once `def` is known: link
                // resolves output names + filter checks across the input list; meta
                // recursively walks inputs to compute hashin. Run them concurrently
                // via `tokio::join!` so the shorter one isn't gated on the longer.
                // Uses `join!` (stack-pinned futures, no per-branch boxing) rather
                // than `try_join_all` — overhead is negligible on the hot path.
                let link_fut = Arc::clone(&self).link(rs.clone(), def.target_def.clone());
                let meta_fut = Arc::clone(&self).meta(rs.clone(), addr);
                let (link_res, meta_res) = tokio::join!(link_fut, meta_fut);
                let def = link_res.with_context(|| "link")?;
                let meta = meta_res.with_context(|| "meta")?;

                let output_names = match outputs {
                    OutputMatcher::None => anyhow::Ok(Vec::<String>::new()),
                    OutputMatcher::All => Ok(def.target.output_names()),
                    OutputMatcher::Exact(names) => {
                        let all_output_names = def.target.output_names();
                        for name in &names {
                            if !all_output_names.contains(name) {
                                anyhow::bail!("output not found: {}", name);
                            }
                        }

                        Ok(names)
                    }
                }?;

                self.execute_and_cache(
                    rs.clone(),
                    &def,
                    output_names,
                    &ExecuteOptions {
                        hashin: &meta.hashin,
                        spec: &spec,
                        def: &def,
                        force: opts.force,
                        interactive: opts.interactive.clone(),
                        shell: opts.shell,
                    },
                )
                .await
            },
        )
        .await
    }

    /// Acquire a lock guard, surfacing a "waiting on lock" notice (with the
    /// holder's pid) if the wait outlasts [`RESULT_LOCK_NOTICE`]. The notice is
    /// purely informational; the wait continues until acquired or cancelled.
    pub(crate) async fn acquire_with_notice<G>(
        &self,
        rs: &Arc<RequestState>,
        addr: &Addr,
        lock_fut: impl Future<Output = anyhow::Result<G>>,
    ) -> anyhow::Result<G> {
        tokio::pin!(lock_fut);
        match tokio::time::timeout(RESULT_LOCK_NOTICE, &mut lock_fut).await {
            Ok(res) => res.with_context(|| format!("acquiring result lock for {addr}")),
            Err(_elapsed) => {
                let addr_str = addr.format();
                let holder_pid = self.result_lock().holder_pid(addr);
                crate::engine::event::emit_scope(
                    rs,
                    crate::engine::event::BuildEventKind::ResultLockWaitStart {
                        addr: addr_str.clone(),
                        holder_pid,
                    },
                    move |_| crate::engine::event::BuildEventKind::ResultLockWaitEnd {
                        addr: addr_str,
                    },
                    async { (&mut lock_fut).await },
                )
                .await
                .with_context(|| format!("acquiring result lock for {addr}"))
            }
        }
    }

    async fn execute_and_cache(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        outputs: Vec<String>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<EResult> {
        let can_cache = !opts.force && opts.def.target.cache.enabled && !opts.shell;
        let addr = &def.target.addr;
        let ctoken = rs.ctoken();

        // Non-cacheable (force/shell): execute under an exclusive write lock —
        // serializing per addr across requests/processes — and return ephemeral
        // artifacts with no long-lived read lock.
        if !can_cache {
            // pluginfs targets are pure, ephemeral filesystem reads (cache off):
            // no cross-process state to serialize and GC never touches them, so
            // the per-addr write lock is pure overhead. Skip it.
            // TODO(targetdef): expose this as an explicit flag on TargetDef
            // (e.g. `needs_lock`) instead of hardcoding the fs driver name here.
            let skip_lock = opts.spec.driver == crate::pluginfs::DRIVER_NAME;
            let _w = if skip_lock {
                None
            } else {
                Some(
                    self.acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
                        .await?,
                )
            };
            let (cached, meta) = self
                .clone()
                .execute_and_cache_inner(rs.clone(), opts)
                .await?;
            return Ok(build_eresult(cached, meta, &outputs, None));
        }

        // 1. Optimistically take a plain shared read lock and look in the cache.
        //    The read lock rides with the returned artifacts (attached per
        //    artifact in build_eresult), protecting the cache entry while in use.
        let read = self
            .acquire_with_notice(&rs, addr, self.result_lock().read(addr, ctoken))
            .await?;
        if let Some((cached, meta)) = self
            .result_from_cache(rs.clone(), def, opts, outputs.clone())
            .await?
        {
            // A. Cache hit — attach this read lock and return.
            return Ok(build_eresult(cached, meta, &outputs, Some(Arc::new(read))));
        }

        // B. Miss: a plain read cannot upgrade. Drop it and take the exclusive
        //    write lock directly — after a miss we'll almost certainly execute,
        //    so this skips the upgradable→upgrade two-step. It also serializes
        //    the execute phase per addr, replacing the old exclusive result lock.
        drop(read);
        let write = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
            .await?;

        // Re-check under the write lock: covers the drop window above and any
        // writer that produced the artifacts while we waited. The write lock
        // excludes all others, so one re-check suffices. On the rare hit (another
        // worker raced us) we skip execution; otherwise we execute + cache.
        let (cached, meta) = match self
            .result_from_cache(rs.clone(), def, opts, outputs.clone())
            .await?
        {
            Some(res) => res,
            None => {
                self.clone()
                    .execute_and_cache_inner(rs.clone(), opts)
                    .await?
            }
        };

        // Downgrade to an upgradable read, then take a plain read while still
        // holding the gateway (gap-free — no writer can delete what we just
        // confirmed/wrote), and release the gateway. The plain read rides with
        // the artifacts.
        let up = write
            .downgrade(ctoken)
            .await
            .with_context(|| format!("downgrading result lock for {addr}"))?;
        let read = self.result_lock().read(addr, ctoken).await?;
        drop(up);
        Ok(build_eresult(cached, meta, &outputs, Some(Arc::new(read))))
    }

    // Memoized by addr:hashin — at most one execute+cache cycle runs per target per request,
    // preventing double-execute when the same target is requested with different output matchers.
    async fn execute_and_cache_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<(Vec<CacheArtifact>, Vec<ArtifactMeta>)> {
        let addr = opts.def.target.addr.clone();
        let hashin = opts.hashin.clone();
        let spec = opts.spec.clone();
        let def = opts.def.clone();
        let use_tmp_cache = !opts.def.target.cache.enabled || opts.shell;
        let interactive = opts.interactive.clone();
        let shell = opts.shell;
        let key = (addr.clone(), hashin.clone());

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    crate::hmemoizer::set_phase("execute_cache:engine_execute");
                    let (artifacts, sandbox_cleanup, fuse_slot_guards) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin, interactive, shell)
                        .await
                        .with_context(|| format!("execute {addr}"))?;

                    let artifacts_meta = artifacts
                        .iter()
                        .filter(|a| matches!(
                            a.r#type,
                            outputartifact::Type::Output | outputartifact::Type::SupportFile
                        ))
                        .map(|a| Ok(ArtifactMeta { hashout: a.hashout()? }))
                        .collect::<anyhow::Result<Vec<_>>>()
                        .with_context(|| format!("read artifact metas for {addr}"))?;

                    // SlotGuards drop here too — moved into the defer so
                    // they live across cache_locally (which reads from
                    // the FUSE-side sandbox) and only deregister after
                    // cleanup is enqueued. The cleanup closure is owned
                    // by the bridge that built the sandbox; it knows
                    // whether to rm the plain dir or the FUSE upper.
                    let cleanup_label = format!("{addr}");
                    let bg_pending = rs.bg_pending();
                    defer! {
                        drop(fuse_slot_guards);
                        if let Some(job) = sandbox_cleanup {
                            crate::engine::sandbox_cleaner::enqueue(cleanup_label, job, bg_pending);
                        }
                    }

                    crate::hmemoizer::set_phase("execute_cache:cache_locally");
                    let write_addr = addr.format();
                    let out = crate::engine::event::emit_scope(
                        &rs,
                        crate::engine::event::BuildEventKind::LocalCacheWriteStart {
                            addr: write_addr.clone(),
                        },
                        move |error| crate::engine::event::BuildEventKind::LocalCacheWriteEnd {
                            addr: write_addr,
                            error,
                        },
                        engine.cache_locally(rs.ctoken(), &addr, &hashin, artifacts, use_tmp_cache),
                    )
                    .await
                    .map(|cached| (cached, artifacts_meta))
                    .with_context(|| format!("cache_locally {addr}"));
                    // TODO(remote-cache): bracket the remote push with its own
                    // RemoteCacheWrite{Start,End} events here once it lands.

                    // Post-write GC: trim this target's stale revisions in the
                    // background (same lane as sandbox cleanup), skipping
                    // uncacheable/tmp entries which are ephemeral and would be
                    // dropped anyway. Fire-and-forget; the trim runs only if the
                    // addr's lock is free, so it never blocks the hot path.
                    if out.is_ok() && !use_tmp_cache {
                        let keep = def.target.cache.history;
                        crate::engine::sandbox_cleaner::enqueue(
                            format!("gc {addr}"),
                            Box::new(enclose!((engine, addr, hashin) move || {
                                engine.try_trim_after_write(&addr, keep, &hashin);
                                Ok(())
                            })),
                            rs.bg_pending(),
                        );
                    }

                    crate::hmemoizer::clear_phase();
                    out
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Look up `def`'s artifacts in the local cache, emitting the hit/miss event.
    /// Returns the raw cached artifacts (already filtered to `outputs` by
    /// [`artifacts_from_local_cache`](Self::artifacts_from_local_cache)); the
    /// caller wraps them via [`build_eresult`], attaching the read lock it holds.
    async fn result_from_cache(
        &self,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
        outputs: Vec<String>,
    ) -> anyhow::Result<Option<(Vec<CacheArtifact>, Vec<ArtifactMeta>)>> {
        match self
            .artifacts_from_local_cache(rs.ctoken(), def, opts.hashin.as_str(), outputs)
            .await?
        {
            // TODO(remote-cache): emit RemoteCache* here; bracket the remote
            // lookup with its own RemoteCacheRead{Start,End} events so it shows in
            // the per-target op breakdown.
            Some(res) => {
                rs.emit(crate::engine::event::BuildEventKind::LocalCacheHit {
                    addr: def.target.addr.format(),
                });
                Ok(Some(res))
            }
            None => {
                rs.emit(crate::engine::event::BuildEventKind::LocalCacheMiss {
                    addr: def.target.addr.format(),
                });
                Ok(None)
            }
        }
    }

    /// Public, tracked. Records `parent → addr` in `dep_dag` and updates `parent`
    /// before delegating to the memoizer. External callers (provider executor,
    /// query stream, `collect_transitive_deps`) use this.
    ///
    /// Internal callers that have already done their own cycle tracking + parent
    /// update (e.g. `result_addr`, `inner_result_addr`, `get_def_inner` resolving
    /// its own spec) must call `get_def_no_track` instead to avoid a spurious
    /// self-edge.
    #[async_recursion]
    pub async fn get_def(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        if let Some(ref parent) = rs.parent {
            let mut dag = rs.data.dep_dag.lock();
            dag.add_dep(parent, addr).map_err(anyhow::Error::new)?;
        }
        let rs = rs.with_parent(addr.clone());
        self.get_def_no_track(rs, addr).await
    }

    pub async fn get_def_no_track(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        rs.data
            .mem_def
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.get_def_inner(rs, &addr, true).await.with_context(|| format!("get_def: {}", addr))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    pub async fn get_direct_def(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        if let Some(ref parent) = rs.parent {
            let mut dag = rs.data.dep_dag.lock();
            dag.add_dep(parent, addr).map_err(anyhow::Error::new)?;
        }
        let rs = rs.with_parent(addr.clone());
        self.get_def_inner(rs, addr, false)
            .await
            .with_context(|| format!("get_def: {}", addr))
    }

    async fn get_def_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        apply_transitive: bool,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        // Use _no_track: get_def (or get_direct_def) already updated parent=addr
        // before invoking us. Tracked get_spec here would record addr→addr.
        let spec = Arc::clone(&self)
            .get_spec_no_track(rs.clone(), addr)
            .await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver
            .driver
            .parse(
                ParseRequest {
                    request_id: rs.request_id().to_string(),
                    target_spec: Arc::clone(&spec.spec),
                },
                rs.ctoken(),
            )
            .await
            .with_context(|| format!("{} parse", driver.name))?;
        let def = rewrite_query_inputs(res.target_def, addr, &spec.provider);

        let all_transitive = if apply_transitive {
            let sb = Arc::clone(&self)
                .collect_transitive_deps(rs.clone(), &def.inputs)
                .await?;

            if sb.empty() { None } else { Some(sb) }
        } else {
            None
        };

        let def = match &all_transitive {
            Some(sb) if !sb.empty() => {
                let res = driver
                    .driver
                    .apply_transitive(
                        ApplyTransitiveRequest {
                            request_id: rs.request_id().to_string(),
                            target_def: def,
                            sandbox: sb.clone(),
                        },
                        rs.ctoken(),
                    )
                    .await
                    .with_context(|| "apply transitive")?;

                res.target_def
            }
            _ => def,
        };

        if def.hash.is_empty() {
            anyhow::bail!("missing hash");
        }

        Ok(Arc::new(ExtendedTargetDef {
            target_def: Arc::new(def),
            applied_transitive: all_transitive,
            driver: spec.driver.clone(),
        }))
    }

    async fn collect_transitive_deps(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: &[Input],
    ) -> anyhow::Result<Sandbox> {
        // Hash-only inputs (`hash_deps`) don't participate in the runtime
        // sandbox, so their transitive sandbox state must not leak in either.
        let futures = inputs
            .iter()
            .filter(|i| i.runtime)
            .enumerate()
            .map(|(i, input)| {
                let input_ref = input.r#ref.clone();
                enclose!((self => engine, rs) async move {
                    let spec = Arc::clone(&engine)
                        .get_spec(rs.clone(), &input_ref.r#ref)
                        .await
                        .with_context(|| format!("get spec: {}", input_ref))?;

                    // For transparent targets (groups), use the pre-computed applied_transitive
                    // which already recursively aggregates all nested deps' transitives.
                    // For all other targets, use spec.transitive directly.
                    // Important: avoid calling get_def on non-transparent targets here — get_def
                    // calls collect_transitive_deps which would re-enter the mem_def memoizer
                    // and deadlock on cyclic dep graphs.
                    let transitive = if spec.driver == crate::plugingroup::DRIVER_NAME {
                        let dep_def = Arc::clone(&engine)
                            .get_def(rs.clone(), &input_ref.r#ref)
                            .await
                            .with_context(|| format!("get def for group: {:?}", input_ref))?;
                        dep_def.applied_transitive.clone()
                    } else {
                        Some(spec.transitive.clone())
                    };

                    if let Some(transitive) = transitive {
                        let id = format!("_transitive_{}_{}", spec.addr.hash_str(), i);
                        anyhow::Ok(Some((id, transitive)))
                    } else {
                        anyhow::Ok(None)
                    }
                })
            });

        let results = crate::engine::fanout::join_all_failable(futures, rs.fail_fast()).await?;

        let mut sb = Sandbox::default();
        for (id, transitive) in results.into_iter().flatten() {
            sb.merge_sandbox(transitive, id);
        }

        Ok(sb)
    }

    /// Public, tracked. Records `parent → addr` in `dep_dag` and updates `parent`
    /// before delegating to the memoizer. External callers use this.
    ///
    /// Internal callers that have already done their own cycle tracking + parent
    /// update must call `get_spec_no_track` instead.
    pub async fn get_spec(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        if let Some(ref parent) = rs.parent {
            let mut dag = rs.data.dep_dag.lock();
            dag.add_dep(parent, addr).map_err(anyhow::Error::new)?;
        }
        let rs = rs.with_parent(addr.clone());
        self.get_spec_no_track(rs, addr).await
    }

    pub async fn get_spec_no_track(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        rs.data
            .mem_spec
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.get_spec_inner(&rs, &addr).await
                }),
            )
            .await
            .map_err(|arc| {
                // Preserve typed errors so callers can downcast_ref to them even when
                // the Arc is shared across concurrent memoizer waiters.
                // Reconstruct TargetNotFoundError here (rather than via the chain wrapper)
                // because callers in deeper code use `e.downcast_ref::<TargetNotFoundError>()`
                // at the top level.
                if arc.downcast_ref::<TargetNotFoundError>().is_some() {
                    TargetNotFoundError { addr: addr.clone() }.into()
                } else {
                    unwrap_arc_err(arc)
                }
            })
    }

    /// Probe every registered provider for every parent package of `pkg`, accumulating
    /// the returned `State`s. Mirrors the Go `ProbeSegments` flow.
    ///
    /// Outer memoize per `pkg` (so repeat callers within a request share the result),
    /// inner memoize per `(provider_name, probe_pkg)` so a given provider is probed at
    /// most once per package per request.
    pub async fn probe_segments(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        pkg: &PkgBuf,
    ) -> anyhow::Result<Arc<Vec<State>>> {
        // Single chokepoint for every provider-dispatch path (get/probe/list all
        // route through here), so provider functions are wired before any BUILD eval.
        self.ensure_provider_functions_wired();
        rs.data
            .mem_probe
            .once(
                pkg.clone(),
                enclose!((self => engine, rs, pkg) move || async move {
                    let mut acc: Vec<State> = Vec::new();
                    for probe_pkg in pkg.parent_packages() {
                        for provider in engine.providers.iter() {
                            let inner = rs
                                .data
                                .mem_probe_inner
                                .once(
                                    (provider.name.clone(), probe_pkg.clone()),
                                    enclose!((provider, rs, probe_pkg) move || async move {
                                        let res = provider
                                            .provider
                                            .probe(
                                                ProbeRequest {
                                                    request_id: rs.request_id().to_string(),
                                                    package: probe_pkg,
                                                },
                                                rs.ctoken(),
                                            )
                                            .await?;
                                        Ok(Arc::new(res.states))
                                    }),
                                )
                                .await
                                .map_err(unwrap_arc_err)?;
                            acc.extend(inner.iter().cloned());
                        }
                    }
                    Ok(Arc::new(acc))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn get_spec_inner(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
        let states = Arc::clone(&self).probe_segments(rs, &addr.package).await?;
        for provider in self.providers.iter() {
            let provider_rs = rs.with_skip_provider(&provider.name);
            let executor: Arc<dyn ProviderExecutor> = Arc::new(EngineProviderExecutor {
                engine: Arc::downgrade(&self),
                rs: provider_rs,
            });

            let spec = match provider
                .provider
                .get(
                    GetRequest {
                        request_id: rs.request_id().to_string(),
                        addr: addr.clone(),
                        states: states
                            .iter()
                            .filter(|s| s.provider == provider.name)
                            .cloned()
                            .collect(),
                        executor: Arc::clone(&executor),
                    },
                    rs.ctoken(),
                )
                .await
            {
                Ok(GetResponse { target_spec }) => target_spec,
                Err(GetError::NotFound) => continue,
                // Return e directly (not bail!(e)) to preserve typed-error downcast
                // through the anyhow::Error chain — required for CycleError handling.
                Err(GetError::Other(e)) => return Err(e),
            };

            return anyhow::Ok(Arc::new(EngineTargetSpec {
                spec: Arc::new(spec),
                provider: provider.name.clone(),
            }));
        }

        Err(TargetNotFoundError { addr: addr.clone() }.into())
    }
}

/// Stamp `_origin = dest.hash_str()` (and, when known, `exclude_provider =
/// dest_provider`) onto any input whose ref points at a query target.
///
/// `_origin` makes each requesting target get its own per-dest variant of the
/// query addr so distinct `mem_spec` cells are computed per dest — the
/// engine-level cycle detector then trips per dest instead of poisoning a
/// shared cell.
///
/// `exclude_provider` ensures the query resolution skips the dest's own
/// provider when iterating candidates. Without it, a provider-emitted target
/// carrying a query input would force the engine to re-iterate the same
/// provider's `list(pkg)` during query resolution, dragging unrelated targets'
/// spec computations into the call stack and opening the door to same-task
/// memoizer re-entrance deadlocks (see `pluginquery::PACKAGE`). User-supplied
/// `exclude_provider` values are not overwritten.
///
/// Hash stability: `def.hash` already covers `def.addr` and these stamps are
/// pure functions of `def.addr` + `dest_provider`. Same dest ⇒ same stamp;
/// different dests live in distinct `mem_def` cells already keyed by addr.
/// No re-hash.
fn rewrite_query_inputs(
    mut def: crate::engine::driver::targetdef::TargetDef,
    dest: &Addr,
    dest_provider: &str,
) -> crate::engine::driver::targetdef::TargetDef {
    let origin = dest.hash_str();
    for input in &mut def.inputs {
        let r = &input.r#ref.r#ref;
        if r.package.as_str() == crate::pluginquery::PACKAGE {
            let mut args = r.args.clone();
            args.insert("_origin".to_string(), origin.clone());
            if !dest_provider.is_empty() && !args.contains_key("exclude_provider") {
                args.insert("exclude_provider".to_string(), dest_provider.to_string());
            }
            input.r#ref.r#ref = Addr::new(r.package.clone(), r.name.clone(), args);
        }
    }
    def
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, ListPackageResponse, ListPackagesRequest, ListResponse,
        ProbeRequest, ProbeResponse,
    };
    use crate::engine::result_lock::{LockBackend, ResultLock};
    use crate::hasync::{Cancellable, StdCancellationToken};
    use crate::htmatcher::Matcher;
    use crate::htpkg::PkgBuf;
    use futures::future::BoxFuture;
    use std::collections::BTreeMap;
    use std::sync::Arc as SArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    /// Minimal [`Content`] for guard-lifetime tests; carries no real bytes.
    struct DummyContent;
    impl Content for DummyContent {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::empty()))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(std::iter::empty()))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("dummy".to_string())
        }
    }

    /// The read lock travels with the artifact, not the `EResult`: it stays held
    /// as long as *any* handle to the artifact is alive — including a handle
    /// cloned into a dependent's sandbox input (or a group target's merged
    /// result) after the producing `EResult` has dropped — and releases only when
    /// the last handle drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guarded_artifact_holds_read_lock_until_all_handles_drop() {
        let dir = tempdir().expect("tempdir");
        let lock = SArc::new(ResultLock::new(LockBackend::Mem, dir.path().to_path_buf()));
        let addr = Addr::new(PkgBuf::from("pkg"), "x".to_string(), BTreeMap::new());

        let read = lock
            .read(&addr, &StdCancellationToken::new())
            .await
            .expect("read");
        let guarded: Arc<dyn Content> = Arc::new(GuardedArtifact {
            inner: Arc::new(DummyContent),
            _lock: Arc::new(read),
        });
        // A dependent clones the artifact handle into its own structures.
        let cloned = Arc::clone(&guarded);

        // A writer for the same addr blocks while any handle is alive.
        let lock2 = SArc::clone(&lock);
        let addr2 = addr.clone();
        let writer = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.write(&addr2, &tok).await.map(|_| ())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!writer.is_finished(), "writer blocked while artifact alive");

        // Producer's EResult drops, but the dependent still holds `cloned`.
        drop(guarded);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !writer.is_finished(),
            "still blocked: the cloned handle keeps the read lock alive"
        );

        drop(cloned);
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("writer acquires once the last artifact handle drops");
    }

    struct CountingProvider {
        name: String,
        list_calls: SArc<AtomicUsize>,
    }

    impl crate::engine::provider::Provider for CountingProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.clone(),
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
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
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
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// Provider that emits a `State` from every package it's probed for, and
    /// records every `GetRequest.states` it observes. Used to verify
    /// `probe_segments` walks parent packages and feeds the result into `get`.
    struct ProbeRecorder {
        name: String,
        get_states: SArc<std::sync::Mutex<Vec<Vec<State>>>>,
    }

    impl crate::engine::provider::Provider for ProbeRecorder {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.name.clone(),
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
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
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
            Box::pin(async {
                Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let states = req.states.clone();
            let recorder = SArc::clone(&self.get_states);
            Box::pin(async move {
                recorder.lock().expect("get_states lock").push(states);
                Err(GetError::NotFound)
            })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            let name = self.name.clone();
            let pkg = req.package.clone();
            Box::pin(async move {
                Ok(ProbeResponse {
                    states: vec![State {
                        package: pkg,
                        provider: name,
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    #[tokio::test]
    async fn probe_segments_walks_parent_packages() -> anyhow::Result<()> {
        let root = tempdir()?;
        let get_states = SArc::new(std::sync::Mutex::new(Vec::<Vec<State>>::new()));
        let get_states_clone = SArc::clone(&get_states);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(ProbeRecorder {
                name: "rec".to_string(),
                get_states: SArc::clone(&get_states_clone),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let states = SArc::clone(&engine)
            .probe_segments(&rs, &PkgBuf::from("a/b/c"))
            .await?;

        let pkgs: Vec<String> = states
            .iter()
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(pkgs, vec!["a/b/c", "a/b", "a", ""]);
        for s in states.iter() {
            assert_eq!(s.provider, "rec");
        }

        // get_spec should also forward the accumulated states.
        let addr = Addr::new(PkgBuf::from("a/b/c"), "t".to_string(), Default::default());
        let _ = SArc::clone(&engine).get_spec(rs, &addr).await;
        let recorded = get_states.lock().unwrap();
        assert_eq!(recorded.len(), 1, "get called once");
        let recorded_pkgs: Vec<String> = recorded[0]
            .iter()
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(recorded_pkgs, vec!["a/b/c", "a/b", "a", ""]);
        Ok(())
    }

    #[test]
    fn provider_functions_lists_exposed_functions() {
        let root = tempdir().unwrap();
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .unwrap();
        engine
            .register_provider(|_| Box::new(crate::pluginfs::Provider))
            .unwrap();
        let fns = engine.provider_functions();
        assert!(
            fns.contains(&("fs".to_string(), "glob".to_string())),
            "{fns:?}"
        );
    }

    #[tokio::test]
    async fn engine_wires_provider_functions_into_buildfile() -> anyhow::Result<()> {
        // End-to-end: the engine must aggregate `fs`'s exposed `glob` function and
        // inject it into the buildfile provider, so a BUILD calling `heph.fs.glob`
        // resolves at spec time.
        let root = tempdir()?;
        std::fs::write(root.path().join("a.txt"), "")?;
        std::fs::write(root.path().join("b.txt"), "")?;
        std::fs::write(root.path().join("c.md"), "")?;
        std::fs::write(
            root.path().join("BUILD"),
            r#"target(name = "t", driver = "d", srcs = heph.fs.glob("*.txt"))"#,
        )?;

        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|_| Box::new(crate::pluginfs::Provider))?;
        engine.register_provider(|root| {
            Box::new(crate::pluginbuildfile::Provider::new(root.to_path_buf()))
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let addr = Addr::new(PkgBuf::from(""), "t".to_string(), Default::default());
        let spec = SArc::clone(&engine).get_spec(rs, &addr).await?;

        let mut srcs = match spec.spec.config.get("srcs") {
            Some(crate::htvalue::Value::List(l)) => l
                .iter()
                .map(|e| match e {
                    crate::htvalue::Value::String(s) => s.clone(),
                    other => panic!("expected string, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("expected list, got {other:?}"),
        };
        srcs.sort();
        assert_eq!(srcs, vec!["a.txt".to_string(), "b.txt".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn skip_providers_excludes_provider_from_query() -> anyhow::Result<()> {
        let root = tempdir()?;
        let list_calls = SArc::new(AtomicUsize::new(0));
        let list_calls_clone = SArc::clone(&list_calls);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(CountingProvider {
                name: "test_provider".to_string(),
                list_calls: SArc::clone(&list_calls_clone),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();
        let skipped_rs = rs.with_skip_provider("test_provider");

        let executor = EngineProviderExecutor {
            engine: SArc::downgrade(&engine),
            rs: skipped_rs,
        };

        let _addrs = executor
            .query(&Matcher::Package(PkgBuf::from("any")), &[])
            .await?;

        assert_eq!(
            list_calls.load(Ordering::SeqCst),
            0,
            "skipped provider must not be called during query"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_engine_result_not_found() -> anyhow::Result<()> {
        let root = tempdir()?;
        let cfg = Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        };

        let engine = Arc::new(Engine::new(cfg)?);
        let rs = engine.new_state();
        let addr = Addr::new(
            PkgBuf::from("non"),
            "existent".to_string(),
            Default::default(),
        );

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::None, &ResultOptions::default())
            .await;
        assert!(result.is_err());
        let err = result.err().unwrap();

        // The full error chain must mention the address and the not-found cause.
        let full_chain = format!("{:#}", err);
        assert!(
            full_chain.contains("non:existent"),
            "expected addr in error chain: {full_chain}"
        );
        assert!(
            full_chain.contains("target not found"),
            "expected 'target not found' in error chain: {full_chain}"
        );

        Ok(())
    }

    use crate::pluginstatictarget;
    use std::collections::HashMap;

    fn static_target(addr: &str, labels: &[&str], deps: &[&str]) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: deps_map,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Exec target with a custom `run` command (e.g. `"exit 1"` to fail, or a
    /// script that emits log lines). Used by the error-handling tests.
    fn run_target(addr: &str, deps: &[&str], run: &str) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some(run.to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: deps_map,
            labels: vec![],
        }
    }

    /// Target with a named codegen-tree output. Used to exercise matchers
    /// (like `TreeOutputTo`) that only resolve at def level — those force the
    /// executor's query to call `get_def(candidate)`, which is the path the
    /// dep_dag cycle detector guards.
    fn codegen_target(
        addr: &str,
        labels: &[&str],
        out_group: &str,
        deps: &[&str],
    ) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        let mut out = HashMap::new();
        out.insert(out_group.to_string(), vec![format!("{out_group}/")]);
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            out,
            codegen: Some("copy".to_string()),
            deps: deps_map,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn engine_with(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_managed_driver(Box::new(crate::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn get_spec_cross_target_cycle_returns_typed_error() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let addr_b = crate::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        // a→b: succeeds, records edge.
        engine
            .clone()
            .get_spec(rs.with_parent(addr_a.clone()), &addr_b)
            .await?;

        // b→a: would close the cycle. Cycle check fires before memoizer.
        let err = engine
            .clone()
            .get_spec(rs.with_parent(addr_b.clone()), &addr_a)
            .await
            .err()
            .expect("expected cycle error");
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_def_cross_target_cycle_returns_typed_error() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let addr_b = crate::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        engine
            .clone()
            .get_def(rs.with_parent(addr_a.clone()), &addr_b)
            .await?;

        let err = engine
            .clone()
            .get_def(rs.with_parent(addr_b.clone()), &addr_a)
            .await
            .err()
            .expect("expected cycle error");
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_request_bails_before_executing() -> anyhow::Result<()> {
        // A request whose token is already cancelled must not resolve or
        // execute the target — it returns CancelledError immediately so a
        // ctrl-c'd build stops starting new work.
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();
        rs.ctoken().cancel();

        let err = engine
            .clone()
            .result_addr(rs, &addr_a, OutputMatcher::All, &ResultOptions::default())
            .await
            .err()
            .expect("cancelled request must error");
        assert!(
            err.downcast_ref::<CancelledError>().is_some(),
            "expected CancelledError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_input_annotated_with_origin_hash() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@label=foo"],
        )])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();

        let def = engine.clone().get_def(rs, &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("expected query input");
        let origin = input
            .r#ref
            .r#ref
            .args
            .get("_origin")
            .expect("query input must be annotated with _origin");
        assert_eq!(*origin, addr_a.hash_str());
        Ok(())
    }

    #[tokio::test]
    async fn query_input_annotated_with_exclude_provider() -> anyhow::Result<()> {
        // Auto-injection: the dest's producing provider must be stamped onto
        // query inputs so they can't re-iterate that provider's targets.
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@label=foo"],
        )])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("expected query input");
        let stamped = input
            .r#ref
            .r#ref
            .args
            .get("exclude_provider")
            .expect("query input must be annotated with exclude_provider");
        // engine_with registers pluginstatictarget under that name.
        assert_eq!(stamped, "pluginstatictarget");
        Ok(())
    }

    #[tokio::test]
    async fn query_input_user_exclude_provider_not_clobbered() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target(
            "//pkg:a",
            &[],
            &["//@heph/query:q@label=foo,exclude_provider=__user__"],
        )])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("expected query input");
        let stamped = input.r#ref.r#ref.args.get("exclude_provider").unwrap();
        assert_eq!(
            stamped, "__user__",
            "user-supplied exclude_provider must not be overwritten"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_spec_returns_engine_target_spec_with_provider_name() -> anyhow::Result<()> {
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let spec = engine.clone().get_spec(engine.new_state(), &addr_a).await?;
        assert_eq!(
            spec.provider, "pluginstatictarget",
            "EngineTargetSpec must carry the producing provider's name"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_with_cyclic_candidate_skips_and_completes() -> anyhow::Result<()> {
        // //pkg:a is a codegen target whose output tree matches the query's
        // tree_output_to. The matcher resolves only at def level, so the
        // executor's query must call get_def(a). a's def transitively re-asks
        // for the same (per-dest annotated) query → cycle → a must be skipped
        // from its own query result. b is a sibling codegen target with no
        // query dep, so it has no cycle and is included.
        //
        // `exclude_provider=__none__` opts out of the auto-injected
        // exclusion of the dest's own provider — we want intra-provider
        // candidate enumeration here.
        // Matcher pkg is `pkg/gen` because the codegen output of a target at
        // `//pkg:*` with DirPath `gen/` lands in package `pkg/gen`.
        let q = "//@heph/query:q@tree_output_to=pkg/gen,exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[]),
        ])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();

        // Must not hang. Pre-fix this either deadlocked or surfaced
        // MemoizerCycleError (when HEPH_DEBUG_MEMOIZER_CYCLE=1).
        let def_a = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            engine.clone().get_def(rs.clone(), &addr_a),
        )
        .await
        .expect("get_def hung — cycle detection failed")?;

        // Pull the annotated query input out of a's def, then call get_spec on it
        // and assert a is excluded from the query result.
        let q_addr = def_a
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("annotated query input")
            .r#ref
            .r#ref
            .clone();
        let q_spec = engine.clone().get_spec(engine.new_state(), &q_addr).await?;
        let deps = match q_spec.config.get("deps") {
            Some(crate::htvalue::Value::List(l)) => l,
            other => panic!("expected deps list, got {other:?}"),
        };
        let dep_strs: Vec<String> = deps
            .iter()
            .map(|v| match v {
                crate::htvalue::Value::String(s) => s.clone(),
                other => panic!("expected string dep, got {other:?}"),
            })
            .collect();
        assert!(
            !dep_strs.iter().any(|s| s.starts_with("//pkg:a")),
            "cyclic candidate //pkg:a must be excluded from query result, got {dep_strs:?}"
        );
        assert!(
            dep_strs.iter().any(|s| s.starts_with("//pkg:b")),
            "non-cyclic candidate //pkg:b must be present, got {dep_strs:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn query_with_two_dests_returns_per_target_results() -> anyhow::Result<()> {
        // Two codegen targets both depending on the tree_output_to query. Per-
        // dest _origin annotation gives each its own mem_spec cell — a's query
        // excludes a but includes b, b's excludes b but includes a. Pre-fix
        // (shared cell) the first to compute would cache a result missing
        // itself, and the second target would see wrong data.
        //
        // `exclude_provider=__none__` opts out of the auto-injected exclusion
        // — we want both same-provider candidates to be enumerable.
        let q = "//@heph/query:q@tree_output_to=pkg/gen,exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[q]),
        ])?;
        let addr_a = crate::htaddr::parse_addr("//pkg:a")?;
        let addr_b = crate::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        let def_a = engine.clone().get_def(rs.clone(), &addr_a).await?;
        let def_b = engine.clone().get_def(rs.clone(), &addr_b).await?;

        let q_addr_a = def_a
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("a's query input")
            .r#ref
            .r#ref
            .clone();
        let q_addr_b = def_b
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == crate::pluginquery::PACKAGE)
            .expect("b's query input")
            .r#ref
            .r#ref
            .clone();
        assert_ne!(
            q_addr_a, q_addr_b,
            "per-dest annotation must produce distinct query addrs"
        );

        let extract_deps = |spec: &TargetSpec| -> Vec<String> {
            match spec.config.get("deps") {
                Some(crate::htvalue::Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        crate::htvalue::Value::String(s) => s.clone(),
                        other => panic!("expected string, got {other:?}"),
                    })
                    .collect(),
                other => panic!("expected deps list, got {other:?}"),
            }
        };

        let spec_a = engine
            .clone()
            .get_spec(engine.new_state(), &q_addr_a)
            .await?;
        let spec_b = engine
            .clone()
            .get_spec(engine.new_state(), &q_addr_b)
            .await?;
        let deps_a = extract_deps(&spec_a);
        let deps_b = extract_deps(&spec_b);

        assert!(
            !deps_a.iter().any(|s| s.starts_with("//pkg:a")),
            "a's query must exclude a, got {deps_a:?}"
        );
        assert!(
            deps_a.iter().any(|s| s.starts_with("//pkg:b")),
            "a's query must include b, got {deps_a:?}"
        );
        assert!(
            !deps_b.iter().any(|s| s.starts_with("//pkg:b")),
            "b's query must exclude b, got {deps_b:?}"
        );
        assert!(
            deps_b.iter().any(|s| s.starts_with("//pkg:a")),
            "b's query must include a, got {deps_b:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn result_fail_fast_on_bails_on_first_failure() -> anyhow::Result<()> {
        // Three targets in `pkg` each depending on a missing target. With
        // fail_fast=true (default), Engine::result must surface Err — no
        // BatchResult is returned.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &["//missing:y"]),
            static_target("//pkg:c", &[], &["//missing:z"]),
        ])?;
        let rs = engine.new_state();
        let res = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await;
        assert!(res.is_err(), "fail_fast=true must surface Err");
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_batch_returns_cancelled_not_success() -> anyhow::Result<()> {
        // A cancelled fail-fast batch must abort with CancelledError, not
        // silently report success. The matcher loop stops enqueuing, the
        // JoinSet drains, and the post-drain token check surfaces the abort.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let rs = engine.new_state();
        rs.ctoken().cancel();
        let err = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("cancelled fail-fast build must return Err");
        assert!(
            downcast_chain_ref::<CancelledError>(&err).is_some(),
            "expected CancelledError, got: {err:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn fail_fast_failure_signals_cancellation_to_siblings() -> anyhow::Result<()> {
        // The user contract: a fail-fast failure does not short-circuit the
        // JoinSet. It signals every other in-flight target to stop (cancels
        // the request token → broadcasts SIGINT) and drains, then surfaces the
        // error. We assert the signal was sent (token cancelled) and the build
        // still returned the failure.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &[]),
            static_target("//pkg:c", &[], &[]),
        ])?;
        let rs = engine.new_state();
        let token = rs.ctoken().clone();
        let res = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await;
        assert!(res.is_err(), "fail_fast must surface the failure");
        assert!(
            token.is_cancelled(),
            "fail_fast failure must signal stop to in-flight siblings"
        );
        Ok(())
    }

    #[tokio::test]
    async fn result_fail_fast_off_collects_all_target_failures() -> anyhow::Result<()> {
        // Same setup, fail_fast=false. Every target must be attempted and
        // every per-target error must surface in BatchResult.errors keyed by
        // its own addr — no error is dropped, no early bail.
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &["//missing:x"]),
            static_target("//pkg:b", &[], &["//missing:y"]),
            static_target("//pkg:c", &[], &["//missing:z"]),
        ])?;
        let rs = engine.new_state_with_fail_fast(false);
        let batch = engine
            .clone()
            .result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        assert!(batch.ok.is_empty(), "no targets should have succeeded");
        assert_eq!(batch.errors.len(), 3, "expected 3 per-target errors");

        let mut addr_names: Vec<String> =
            batch.errors.iter().map(|(a, _)| a.name.clone()).collect();
        addr_names.sort();
        assert_eq!(addr_names, vec!["a", "b", "c"]);
        Ok(())
    }

    #[tokio::test]
    async fn nested_fail_fast_off_records_aggregated_input_failures() -> anyhow::Result<()> {
        // A parent target with multiple bad inputs (each referencing a missing
        // target). Input *def* resolution happens in link/meta via get_def — not
        // result_addr — so the missing targets never get per-dep registry
        // entries. With fail_fast=false the fanout drives every input to
        // completion and aggregates into a MultiError of unrecorded causes; that
        // aggregation is recorded once against the parent (whose input-resolution
        // work failed), preserving every broken input. The direct caller gets the
        // rich diagnostic via boundary surfacing, never the bare marker.
        use crate::engine::error::{TargetFailure, UpstreamFailed};

        let engine = engine_with(vec![static_target(
            "//pkg:parent",
            &[],
            &["//missing:a", "//missing:b"],
        )])?;
        let addr = crate::htaddr::parse_addr("//pkg:parent")?;
        let rs = engine.new_state_with_fail_fast(false);
        let res = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        let err = res.err().expect("parent must fail");
        assert!(
            downcast_chain_ref::<UpstreamFailed>(&err).is_none(),
            "top-level error must be surfaced as the rich cause, not the marker: {err:#}"
        );
        downcast_chain_ref::<TargetFailure>(&err)
            .expect("expected a surfaced TargetFailure at the boundary");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("missing:a") && rendered.contains("missing:b"),
            "the surfaced failure must list every broken input, got: {rendered}"
        );

        let failures = rs.take_failures();
        assert_eq!(
            failures.len(),
            1,
            "the aggregation is recorded once (against the parent), not duplicated per dep"
        );
        assert_eq!(failures[0].addr.format(), "//pkg:parent");
        Ok(())
    }

    #[tokio::test]
    async fn diamond_failure_recorded_once_at_root() -> anyhow::Result<()> {
        // top → leaf1, leaf2 → base; base fails (its own work errors). Both
        // leaves and top are collateral (they failed only because base did) and
        // must NOT be recorded — only the root cause `base`, exactly once.
        // (The lib harness can't spawn subprocesses, so base fails at exec spawn;
        // the dedup contract is what this exercises — see the e2e suite for real
        // process failures.)
        use crate::engine::error::{TargetFailure, UpstreamFailed};

        let engine = engine_with(vec![
            run_target("//d:base", &[], "exit 1"),
            run_target("//d:leaf1", &["//d:base"], "true"),
            run_target("//d:leaf2", &["//d:base"], "true"),
            run_target("//d:top", &["//d:leaf1", "//d:leaf2"], "true"),
        ])?;
        let addr = crate::htaddr::parse_addr("//d:top")?;
        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("top must fail");

        let failures = rs.take_failures();
        assert_eq!(
            failures.len(),
            1,
            "only the root cause is recorded, not the collateral leaves/top"
        );
        assert_eq!(failures[0].addr.format(), "//d:base");
        // base is recorded as its OWN failure (a genuine cause, not a marker).
        assert!(
            downcast_chain_ref::<UpstreamFailed>(&failures[0].source).is_none(),
            "root cause must be a genuine failure, not an UpstreamFailed marker"
        );
        // The boundary surfaces a rich diagnostic to the direct caller.
        assert!(downcast_chain_ref::<TargetFailure>(&err).is_some());
        Ok(())
    }

    #[test]
    fn deep_chain_failure_has_bounded_error_depth() {
        // A linear chain a0→a1→…→aN where only the tail fails. The error
        // propagated to the caller must NOT accumulate one frame per hop — each
        // collateral hop replaces (never wraps) its incoming error with a fresh
        // marker, so the chain stays O(1). Proven by comparing two very different
        // chain lengths: the surfaced error's depth must be identical, and only
        // the tail is recorded.
        //
        // Run on a large-stack thread with its own runtime: the engine's `meta`
        // walk recurses once per hop and overflows the 2MB default test stack
        // well before the depths exercised here.
        use crate::engine::error::TargetFailure;

        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    async fn run_chain(n: usize) -> (usize, usize, String) {
                        let addrs: Vec<String> =
                            (0..n).map(|i| format!("//chain:a{i}")).collect();
                        let mut targets = Vec::with_capacity(n);
                        for i in 0..n {
                            if i + 1 < n {
                                targets
                                    .push(run_target(&addrs[i], &[addrs[i + 1].as_str()], "true"));
                            } else {
                                targets.push(run_target(&addrs[i], &[], "exit 1"));
                            }
                        }
                        let engine = engine_with(targets).expect("engine");
                        let head = crate::htaddr::parse_addr(&addrs[0]).expect("addr");
                        let rs = engine.new_state();
                        let err = engine
                            .clone()
                            .result_addr(
                                rs.clone(),
                                &head,
                                OutputMatcher::All,
                                &ResultOptions::default(),
                            )
                            .await
                            .err()
                            .expect("head must fail");
                        assert!(downcast_chain_ref::<TargetFailure>(&err).is_some());
                        let failures = rs.take_failures();
                        (
                            failures.len(),
                            err.chain().count(),
                            failures.first().map(|f| f.addr.format()).unwrap_or_default(),
                        )
                    }

                    let (rec_short, depth_short, root_short) = run_chain(10).await;
                    let (rec_long, depth_long, root_long) = run_chain(200).await;

                    assert_eq!(rec_short, 1, "one recorded root cause regardless of length");
                    assert_eq!(rec_long, 1, "one recorded root cause regardless of length");
                    assert_eq!(root_short, "//chain:a9");
                    assert_eq!(root_long, "//chain:a199");
                    assert_eq!(
                        depth_short, depth_long,
                        "error chain depth must be O(1) — independent of graph depth ({depth_short} vs {depth_long})"
                    );
                    assert!(
                        depth_long < 10,
                        "surfaced error must be a shallow chain, got {depth_long}"
                    );
                });
            })
            .expect("spawn")
            .join()
            .expect("join");
    }

    #[tokio::test]
    async fn classify_attaches_process_log_tail_to_recorded_failure() -> anyhow::Result<()> {
        // When a target's own failure carries a `ProcessFailed` (anywhere in the
        // chain), the recorded `TargetFailure` must surface its log tail. Driven
        // directly through `classify_failure` so it's deterministic and doesn't
        // depend on spawning a real subprocess (the lib harness can't; the e2e
        // suite covers the live path). `last_n_lines` itself is unit-tested in
        // `engine::error`.
        use crate::engine::error::{ProcessFailed, UpstreamFailed};

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let tail = "line6\nline7\nline8\nline9\nline10";
        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log_tail: tail.to_string(),
        })
        .context("driver run")
        .context("execute //pkg:a");

        let out = classify_failure(&rs, &addr, false, e);
        // Own failure → cheap marker propagated upward.
        assert!(downcast_chain_ref::<UpstreamFailed>(&out).is_some());
        // …and the rich record carries the log tail pulled from ProcessFailed.
        let recorded = rs.get_failure(&addr).expect("failure must be recorded");
        assert_eq!(recorded.log_tail.as_deref(), Some(tail));
        Ok(())
    }

    #[tokio::test]
    async fn classify_drops_log_tail_for_interactive_targets() -> anyhow::Result<()> {
        // Interactive targets stream their output live to the user's terminal, so
        // the captured log tail must NOT be re-rendered in the failure box.
        use crate::engine::error::ProcessFailed;

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log_tail: "line9\nline10".to_string(),
        })
        .context("execute //pkg:a");

        let _ = classify_failure(&rs, &addr, true, e);
        let recorded = rs.get_failure(&addr).expect("failure must be recorded");
        assert_eq!(recorded.log_tail, None);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_is_not_recorded_as_failure() -> anyhow::Result<()> {
        // A pre-cancelled request bails with CancelledError before doing work;
        // cancellation is not a target failure and must not be recorded.
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();
        rs.ctoken().cancel();
        let err = engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .err()
            .expect("cancelled request must fail");
        assert!(downcast_chain_ref::<CancelledError>(&err).is_some());
        assert!(
            rs.take_failures().is_empty(),
            "cancellation must not be recorded as a failure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cycle_detection_returns_typed_cycle_error() -> anyhow::Result<()> {
        let root = tempdir()?;
        let engine = Arc::new(Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);
        let addr = Addr::new(PkgBuf::from("p"), "t".to_string(), Default::default());
        // Pre-populate dag with addr→addr already there is overkill; just call result_addr
        // twice with the same parent set, but result_addr sets parent via with_parent so the
        // second invocation inside the same parent chain triggers cycle. Simulate by manually
        // setting rs.parent = addr before calling result_addr(addr).
        let rs = engine.new_state().with_parent(addr.clone());
        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::None, &ResultOptions::default())
            .await;
        assert!(result.is_err(), "expected cycle error");
        let err = result.err().unwrap();
        assert!(
            err.downcast_ref::<CycleError>().is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    use crate::engine::event::{BuildEvent, BuildEventKind};

    fn static_target_run(addr: &str, run: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some(run.to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: vec![],
        }
    }

    /// Engine + the `TempDir` backing its `home`/cache. The caller must hold the
    /// returned `TempDir` alive for the duration of the test so the on-disk cache
    /// survives across resolves (warm-cache assertions read it back).
    fn engine_with_home(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_managed_driver(Box::new(crate::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// Resolve `addr` with a fresh event-collecting `RequestState`, then drop the
    /// state (closing the sender) and drain every emitted event.
    async fn resolve_collecting_events(
        engine: &Arc<Engine>,
        addr: &Addr,
    ) -> (anyhow::Result<Arc<EResult>>, Vec<BuildEvent>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        let res = engine
            .clone()
            .result_addr(
                rs.clone(),
                addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        drop(rs);
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        (res, events)
    }

    #[tokio::test]
    async fn emits_result_execute_and_cache_miss_for_fresh_target() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("fresh target must resolve");

        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ResultStart { addr } if addr == "//pkg:a")
            ),
            "expected ResultStart, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteStart { addr, driver, cache }
                    if addr == "//pkg:a" && driver == "exec" && *cache
            )),
            "expected ExecuteStart{{driver:exec, cache:true}}, got {events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:a")
            ),
            "expected LocalCacheMiss, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteEnd { addr, error: None } if addr == "//pkg:a"
            )),
            "expected ExecuteEnd{{error:None}}, got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ResultEnd { addr, error: None } if addr == "//pkg:a"
            )),
            "expected ResultEnd{{error:None}}, got {events:?}"
        );

        // Server-stamped: every event carries a non-zero wall-clock timestamp.
        for e in &events {
            assert!(e.at_unix_ms > 0, "event missing at_unix_ms stamp: {e:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn single_target_result_addr_announces_max_workers_once() -> anyhow::Result<()> {
        // Regression: `run` of a single addr calls `result_addr` directly,
        // bypassing `Engine::result`. The MaxWorkers announcement must still fire
        // (so the TUI paints the worker indicator), and exactly once.
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("fresh target must resolve");

        let max_workers: Vec<usize> = events
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::MaxWorkers { count } => Some(*count),
                _ => None,
            })
            .collect();
        assert_eq!(
            max_workers.len(),
            1,
            "expected exactly one MaxWorkers event, got {events:?}"
        );
        assert!(max_workers[0] >= 1, "worker count must be positive");
        Ok(())
    }

    #[tokio::test]
    async fn warm_cache_emits_local_cache_hit_and_no_execute() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        // First resolve populates the cache (same engine ⇒ same home/cache).
        let (first, _) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");

        // Second resolve on the same engine must hit the local cache.
        let (second, events) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");

        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::LocalCacheHit { addr } if addr == "//pkg:a")
            ),
            "warm resolve must emit LocalCacheHit, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, BuildEventKind::ExecuteStart { .. })),
            "warm resolve must not re-execute (no ExecuteStart), got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, BuildEventKind::ExecuteEnd { .. })),
            "warm resolve must not re-execute (no ExecuteEnd), got {events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failing_target_carries_error_in_execute_and_result_end() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "false")])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        assert!(res.is_err(), "run:false target must fail");

        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ExecuteEnd { addr, error: Some(_) } if addr == "//pkg:a"
            )),
            "ExecuteEnd must carry the error (drop-guard on ? path), got {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::ResultEnd { addr, error: Some(_) } if addr == "//pkg:a"
            )),
            "ResultEnd must carry the error, got {events:?}"
        );
        for e in &events {
            assert!(e.at_unix_ms > 0, "event missing at_unix_ms stamp: {e:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn result_emits_matched_with_resolved_set() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        let batch = engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        assert_eq!(batch.ok.len(), 2);
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // The matched set streams incrementally (one event per match) and is
        // followed by a final `complete` marker.
        let mut matched: Vec<String> = Vec::new();
        let mut saw_complete = false;
        for e in &events {
            if let BuildEventKind::Matched { addrs, complete } = &e.kind {
                matched.extend(addrs.iter().cloned());
                saw_complete |= *complete;
            }
        }
        assert!(
            saw_complete,
            "result must emit a final complete Matched event"
        );
        assert_eq!(matched.len(), 2, "matched set: {matched:?}");
        assert!(matched.contains(&"//pkg:a".to_string()), "{matched:?}");
        assert!(matched.contains(&"//pkg:b".to_string()), "{matched:?}");
        Ok(())
    }

    #[tokio::test]
    async fn result_emits_provisional_zero_matched_up_front() -> anyhow::Result<()> {
        // The matched line is advertised the instant the query starts: the first
        // Matched event carries an empty set with complete=false (provisional
        // "~0"), before any match has streamed.
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let first = events
            .iter()
            .find_map(|e| match &e.kind {
                BuildEventKind::Matched { addrs, complete } => Some((addrs.clone(), *complete)),
                _ => None,
            })
            .expect("a Matched event must be emitted");
        assert_eq!(
            first,
            (Vec::new(), false),
            "first Matched event must advertise an empty, provisional set"
        );
        Ok(())
    }

    #[tokio::test]
    async fn single_addr_result_addr_emits_matched_complete() -> anyhow::Result<()> {
        // The single-addr entry (run of one addr) goes straight to
        // `result_addr`, which must announce the set-of-one as complete.
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = crate::htaddr::parse_addr("//pkg:a")?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut matched: Vec<String> = Vec::new();
        let mut saw_complete = false;
        while let Ok(ev) = rx.try_recv() {
            if let BuildEventKind::Matched { addrs, complete } = &ev.kind {
                matched.extend(addrs.iter().cloned());
                saw_complete |= *complete;
            }
        }
        assert!(saw_complete, "single-addr must emit complete Matched");
        assert_eq!(matched, vec!["//pkg:a".to_string()], "matched: {matched:?}");
        Ok(())
    }

    #[tokio::test]
    async fn inner_result_does_not_re_emit_matched() -> anyhow::Result<()> {
        // Only the first/top-level `result` owns the matched stream. A second
        // `result` sharing the same request data (the "inner" case) must stay
        // silent so it can't inflate the matched count or trip `complete`.
        let (engine, _home) = engine_with_home(vec![
            static_target_run("//pkg:a", "true"),
            static_target_run("//pkg:b", "true"),
        ])?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = engine.new_state_with_events(true, Some(tx));

        // First call claims the stream and emits Matched.
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;

        // Drain everything the first call emitted.
        while rx.try_recv().is_ok() {}

        // Second call on the same request data must not emit any Matched event.
        engine
            .clone()
            .result(
                rs.clone(),
                &Matcher::Package(PkgBuf::from("pkg")),
                &ResultOptions::default(),
            )
            .await?;
        drop(rs);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, BuildEventKind::Matched { .. })),
            "inner result must not emit Matched, got {events:?}"
        );
        Ok(())
    }
}
