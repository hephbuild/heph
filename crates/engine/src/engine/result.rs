use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::{
    CancelledError, CycleError, HashUnknownError, MultiError, ProcessFailed,
    ShellNeedsSingleTarget, TargetFailure, TargetNotFoundError, UpstreamFailed,
};
use crate::engine::provider::{
    GetError, GetRequest, GetResponse, ListRequest, ProbeRequest, ProviderExecutor, State,
    TargetSpec,
};
use crate::engine::request_state::{AddrKey, RequestState};
use crate::engine::spec::EngineTargetSpec;
use async_recursion::async_recursion;
use enclose::enclose;
use hcore::hasync::Cancellable;
use hcore::hmemoizer::{downcast_chain_ref, unwrap_arc_err};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::MatchResult;
use hmodel::htpkg::PkgBuf;

use crate::engine::driver::sandbox::Sandbox;
use crate::engine::link::LinkedTargetDef;
use crate::engine::local_cache::{BlobResidency, CacheArtifact, Manifest, ManifestArtifactType};
use crate::engine::remote_cache::RemoteRevision;
use crate::engine::result_lock::ResultReadGuard;
use anyhow::Context;
use futures::{StreamExt, TryStreamExt};
use hcore::hartifactcontent::{Content, ReadSeek, WalkEntry, WalkEntryKind};
use hmodel::htmatcher::Matcher;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use tokio::task::JoinSet;

/// How long to block on the per-addr result lock before surfacing a "waiting on
/// lock" notice (with the holder's pid) to the progress stream. The notice is
/// purely informational; the wait itself continues until acquired or cancelled.
const RESULT_LOCK_NOTICE: std::time::Duration = std::time::Duration::from_secs(5);

/// The boxed future produced by `#[async_recursion]` for `result_addr_impl`.
type BoxedResultFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Arc<EResult>>> + Send + 'a>>;

/// rs carries the parent addr (set by result_addr via with_parent) so the executor
/// does not need to store it separately.
pub(crate) struct EngineProviderExecutor {
    engine: Weak<Engine>,
    rs: Arc<RequestState>,
    /// True for an executor constructed via [`Self::for_list`] — i.e. one handed
    /// to `Provider::list` by a discovery walk's per-package fan-out. Only
    /// `query()` checks it; `result`/`note_dep`/`states_under` work regardless.
    ///
    /// Carried on the instance rather than in a tokio task-local deliberately:
    /// a task-local is scoped to one poll chain, so a provider that moves the
    /// executor into a `tokio::spawn`ed task (or calls back across the plugin
    /// ABI seam, where the guest cannot see host task-locals at all) would
    /// silently escape the guard. The instance flag survives any spawn.
    for_provider_list: bool,
}

impl EngineProviderExecutor {
    pub(crate) fn new(engine: Weak<Engine>, rs: Arc<RequestState>) -> Self {
        Self {
            engine,
            rs,
            for_provider_list: false,
        }
    }

    /// The executor to hand to a `Provider::list` call dispatched by a discovery
    /// walk's per-package fan-out — both `Engine::query`'s (in `query.rs`) and
    /// `EngineProviderExecutor::query`'s own nested one (below) use this for
    /// their `.list(...)` call.
    ///
    /// `ListRequest::executor` hands `list()` implementations the same `query()`
    /// capability `get()` gets (see the ABI note at the `query()` call site). A
    /// provider that calls `executor.query()` back from inside its own `list()`
    /// would nest another K-wide walk under an already-running one — degrading to
    /// pinned memory and scheduler pressure rather than a hard deadlock (nesting
    /// doesn't manufacture extra `PKG_EVAL_SLOTS` permits, see the long comment at
    /// the fan-out), but with no in-tree caller and no diagnostic if a third-party
    /// or out-of-process plugin ever does it by accident. `query()` checks
    /// `for_provider_list` first and fails loudly instead of silently nesting.
    pub(crate) fn for_list(engine: Weak<Engine>, rs: Arc<RequestState>) -> Self {
        Self {
            engine,
            rs,
            for_provider_list: true,
        }
    }
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

    fn note_dep<'a>(
        &'a self,
        addr: &'a Addr,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        // Edge-only: register parent → addr (the synchronous cycle check) without
        // executing. parent is already set on `rs` by the enclosing result_addr.
        Box::pin(async move { self.rs.track_dep(addr).map_err(anyhow::Error::new) })
    }

    fn states_under<'a>(
        &'a self,
        prefix: &'a PkgBuf,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<State>>> {
        Box::pin(async move {
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            let rs = self.rs.clone();

            // Memoize the whole subtree gather per prefix: a `list` that calls
            // `states_under` for many packages (e.g. the go go_src query) then pays
            // the package walk + probes once, not once per package — which would
            // otherwise flood the blocking pool with concurrent `list_packages`
            // walks. Registers no DepDag edge — states are config, not a dep.
            let states = rs
                .data
                .mem_states_under
                .once(
                    prefix.clone(),
                    enclose!((engine, rs, prefix.clone() => prefix) move || async move {
                        // Enumerate every package at or under `prefix`, then union
                        // each package's own `provider_state`s (all providers).
                        let matcher = Matcher::PackagePrefix(prefix);
                        let pkg_iter = engine.packages(&matcher, &rs).await?;
                        let pkgs: Vec<String> = pkg_iter.collect::<anyhow::Result<_>>()?;

                        // One probe per `(package, provider)` pair, overlapped.
                        //
                        // `buffered`, never `buffer_unordered`: `acc` order is
                        // the order plugin-go's `variant::build_universe` walks
                        // the module universe in, which is the order its `list`
                        // emits library addrs in, which reaches a def hash
                        // through `pluginquery` → `plugingroup`. `buffered`
                        // yields in submission order, so `acc` is byte-identical
                        // to what the serial nested loop produced no matter
                        // which probe lands first; `buffer_unordered` would put
                        // completion order — i.e. probe latency — into a build
                        // definition.
                        //
                        // The pair sequence is produced lazily and mapped to
                        // futures one at a time, so the live set stays O(K) —
                        // materializing `packages × providers` up front would be
                        // ~80k `(PkgBuf, Arc)` pairs on a 20k-package workspace,
                        // for no gain. Everything the iterator touches is owned
                        // (the memoized closure's future must be `'static`), so
                        // the registry is shared by `Arc` rather than borrowed.
                        let providers: Arc<Vec<Arc<crate::engine::engine::Provider>>> =
                            Arc::new(engine.providers.clone());
                        let n_providers = providers.len();
                        // Set when the walk is abandoning. A probe is *not* cheap
                        // — `pluginbuildfile::probe` is `run_pkg`, the heaviest
                        // synchronous unit in a build, and it takes a
                        // `PKG_EVAL_SLOTS` permit — so without this the drain
                        // below keeps topping the buffer from the underlying
                        // iterator and Starlark-evaluates the whole workspace
                        // *after* the walk has already failed.
                        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let probes = futures::stream::iter(pkgs.into_iter().flat_map(move |pkg_str| {
                            let pkg = PkgBuf::from(pkg_str.as_str());
                            let providers = Arc::clone(&providers);
                            // `i < n_providers` holds by construction; `get`
                            // rather than `[]` keeps this panic-free without an
                            // unwrap, and without allocating a `Vec` per package.
                            (0..n_providers).filter_map(move |i| {
                                providers
                                    .get(i)
                                    .map(|provider| (pkg.clone(), Arc::clone(provider)))
                            })
                        }))
                        .map(|(pkg, provider)| {
                            enclose!((rs, stop) async move {
                                if rs.ctoken().is_cancelled()
                                    || stop.load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    return Err(anyhow::Error::new(CancelledError));
                                }
                                rs.data
                                    .mem_probe_inner
                                    .once(
                                        (provider.name.clone(), pkg.clone()),
                                        enclose!((provider, rs, pkg) move || async move {
                                            let res = provider
                                                .provider
                                                .probe(
                                                    ProbeRequest {
                                                        request_id: rs.request_id().to_string(),
                                                        package: pkg,
                                                    },
                                                    rs.ctoken(),
                                                )
                                                .await?;
                                            Ok(Arc::new(res.states))
                                        }),
                                    )
                                    .await
                                    .map_err(unwrap_arc_err)
                            })
                        })
                        .buffered(crate::engine::fanout::discovery_concurrency());
                        tokio::pin!(probes);

                        let mut acc: Vec<State> = Vec::new();
                        loop {
                            match probes.next().await {
                                None => break,
                                Some(Ok(inner)) => acc.extend(inner.iter().cloned()),
                                // Same reason as the two `query` walks: returning
                                // here would drop up to K in-flight probes, each
                                // holding an `Arc<RequestState>`, while this
                                // fan-out is itself running *inside* the
                                // `mem_states_under` cell. Since #241 that is no
                                // longer a leak — dropping a probe releases its
                                // memoizer interest, and the last one out evicts
                                // the `mem_probe_inner` cell and drops its future
                                // — so what the drain buys now is ordering, not
                                // liveness: the request's in-flight probes are
                                // finished before the error escapes, rather than
                                // torn down underneath a caller that may still
                                // want them. `Buffered` refills from the
                                // underlying iterator on every poll, so the drain
                                // visits all N pairs, not just the K in flight;
                                // `stop` is what keeps the tail from costing a
                                // package evaluation each.
                                Some(Err(e)) => {
                                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                                    while probes.next().await.is_some() {}
                                    return Err(e);
                                }
                            }
                        }
                        Ok(Arc::new(acc))
                    }),
                )
                .await
                .map_err(unwrap_arc_err)?;
            Ok(states.as_ref().clone())
        })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move {
            // See `EngineProviderExecutor::for_list`: a provider calling back into
            // `query()` from its own `list()` would nest a K-wide walk under an
            // already-running one. Fail loudly here rather than let it silently
            // nest.
            if self.for_provider_list {
                anyhow::bail!(
                    "ListRequest::executor.query() was called from inside Provider::list() \
                     — this would nest a K-wide package walk under an already-running one; \
                     use ProviderExecutor::states_under instead"
                );
            }
            let engine = self
                .engine
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
            let rs = self.rs.clone();

            // Collect packages eagerly (non-Send iterator dropped before first await)
            let pkg_iter = engine.packages(m, &rs).await?;
            let pkgs: Vec<String> = pkg_iter.collect::<anyhow::Result<_>>()?;

            // One `list` callback surface for the whole walk, not one per
            // (package, provider) pair — it is stateless apart from the engine
            // handle and the request. `for_list`, so a reentrant `query()` on it
            // is caught — see its doc comment.
            let executor: Arc<dyn ProviderExecutor> = Arc::new(EngineProviderExecutor::for_list(
                Arc::downgrade(&engine),
                rs.clone(),
            ));

            // Set when the walk is abandoning; see `Engine::query`.
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

            // Owned copy: each package runs as its own `'static` task (see the
            // spawn note below), so the borrowed `extra_skip` cannot cross into
            // it. One `Arc` for the whole walk, cloned per package, not per
            // (package, provider) pair.
            let extra_skip: Arc<[String]> = extra_skip.into();

            // Enumeration overlapped, matcher evaluation serial — the same split,
            // for the same reasons, as `Engine::query`; see the long note there.
            // The short version: the `MatchShrug` arm runs on a *speculative*
            // `RequestState` whose cycle detection is a per-chain breadcrumb walk
            // that bypasses the shared `DepDag`, so two concurrent chains are
            // mutually invisible — which decides by race both which provider wins
            // a shared `mem_spec` cell (and so `hashin`, which folds
            // `def.driver`) and whether a two-chain cycle is reported or hangs.
            // Serial *within this walk* is structural: the arm is in the consumer
            // below, so the fan-out is not polled while it awaits. Across walks it
            // is not guaranteed — see the note in `Engine::query`.
            //
            // ABI note: `ListRequest::executor` hands this `query()` to K
            // concurrent `list` calls, so a provider that calls back into it from
            // `list` gets K nested walks. No in-tree provider does (plugin-go's
            // `list` calls only `states_under`), but it is a constraint on the
            // plugin surface, not an accident of the current callers — and it is
            // enforced (the executor above is `for_list`, so its `query()`
            // refuses), not just documented here.
            let per_pkg = futures::stream::iter(pkgs.into_iter()
                // Ends the source when abandoning, so the drain joins only the
                // <=K tasks already spawned instead of spawning one per remaining
                // package — see `Engine::query`.
                .take_while(enclose!((stop) move |_| !stop.load(std::sync::atomic::Ordering::Relaxed)))
                .map(|pkg_str| {
                let pkg = PkgBuf::from(pkg_str.as_str());

                // No package-scope prune here, same as `Engine::query`:
                // `packages()` above already answers within `m`'s scope
                // regardless of what the provider did with the prefix hint, so
                // no package reaching this point can be rejected on its path
                // alone.
                //
                // Spawned here, not through a later `.map()` — see `Engine::query`.
                hcore::hmemoizer::spawn_with_cycle_ctx(
                    enclose!((engine, rs, executor, stop, extra_skip) async move {
                        // See `Engine::query`: an abandoning walk stops evaluating
                        // packages it has not started, so the drain below costs a
                        // cheap poll per remaining package rather than a package
                        // evaluation each. `Err`, never `Ok(vec![])` — this sequence
                        // is `pluginquery`'s `deps` and is folded in order into a def
                        // hash, so a short answer would hash a truncated graph.
                        if rs.ctoken().is_cancelled()
                            || stop.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            return Err(anyhow::Error::new(CancelledError));
                        }
                        let mut candidates: Vec<Addr> = Vec::new();
                        let states = Arc::clone(&engine).probe_segments(&rs, &pkg).await?;

                        for provider in &engine.providers {
                            if rs.skip_providers.contains(&provider.name)
                                || extra_skip.iter().any(|n| n == &provider.name)
                            {
                                continue;
                            }
                            // Collect list results eagerly (non-Send iterator dropped before next await).
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
                                        executor: Arc::clone(&executor),
                                    },
                                    rs.ctoken(),
                                )
                                .await?;
                            let raw: Vec<_> = list_iter.collect::<anyhow::Result<Vec<_>>>()?;

                            for item in raw {
                                if item.addr.package == pkg {
                                    candidates.push(item.addr);
                                }
                            }
                        }

                        anyhow::Ok(candidates)
                    }),
                )
            }))
            // One task per package — see the long note in `Engine::query`: as
            // plain futures the `PKG_EVAL_SLOTS` permit holders would be
            // unpollable while this walk's consumer awaits the `MatchShrug` arm,
            // and the consumer would then queue behind them on the same
            // semaphore.
            .buffered(crate::engine::fanout::discovery_concurrency())
            .map(|joined| match joined {
                Ok(res) => res,
                Err(e) => Err(anyhow::Error::new(e).context("package discovery task panicked")),
            });
            tokio::pin!(per_pkg);

            let mut result = Vec::new();
            loop {
                let candidates = match per_pkg.next().await {
                    None => break,
                    Some(Ok(candidates)) => candidates,
                    // Never `?` straight out — returning drops `per_pkg` with up
                    // to K-1 package *tasks* still running, each holding an
                    // `Arc<RequestState>` it releases only when it finishes. See
                    // `Engine::query` for why late deregistration races
                    // `drain_bg`.
                    Some(Err(e)) => {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        while per_pkg.next().await.is_some() {}
                        return Err(e);
                    }
                };
                for addr in candidates {
                    match m.matches_addr(&addr) {
                        MatchResult::MatchYes => result.push(addr),
                        MatchResult::MatchNo => {}
                        MatchResult::MatchShrug => {
                            // Resolve the candidate's spec/def only to evaluate the
                            // matcher — a speculative inspection, not a dependency. Use a
                            // speculative rs so a rejected candidate leaves no edge in the
                            // shared dep DAG (an edge would close a false cycle later).
                            // One chain at a time *within this walk* — see the note
                            // above the fan-out.
                            let spec_rs = rs.speculative();
                            let spec =
                                match Arc::clone(&engine).get_spec(spec_rs.clone(), &addr).await {
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

                            match crate::engine::matcher_spec::match_spec(m, &spec) {
                                MatchResult::MatchYes => result.push(addr),
                                MatchResult::MatchNo => {}
                                MatchResult::MatchShrug => {
                                    let def_res =
                                        Arc::clone(&engine).get_def(spec_rs.clone(), &addr).await;
                                    let def = match def_res {
                                        Ok(def) => def,
                                        // Same as the get_spec branch: cycle means this
                                        // target transitively depends on the query caller —
                                        // it can't be a dep of the caller. Skip it.
                                        Err(e)
                                            if downcast_chain_ref::<CycleError>(&e).is_some() =>
                                        {
                                            continue;
                                        }
                                        Err(e) => return Err(e),
                                    };
                                    if crate::engine::matcher_target::match_target(
                                        m,
                                        &def.target_def,
                                    ) == MatchResult::MatchYes
                                    {
                                        result.push(addr);
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

pub struct ExtendedTargetDef {
    pub target_def: Arc<TargetDef>,
    pub applied_transitive: Option<Sandbox>,
    /// Registry name of the driver that produced `target_def`. Folded into
    /// `hashin` so swapping drivers under the same addr invalidates cache —
    /// even if the produced `TargetDef` bytes happen to match.
    pub driver: String,
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

// `EResult` + `ArtifactMeta` now live in the `heph-plugin` contract crate (the
// Driver/Provider trait surface returns them); re-exported here so
// `crate::engine::result::EResult` keeps resolving across the engine + plugins.
pub use hplugin::eresult::{ArtifactMeta, EResult};

/// A cache-backed artifact paired with the read lock guarding its cache entry.
/// Delegates every [`Content`] method to the inner artifact; the guard is held
/// purely for RAII, so the cache entry cannot be overwritten/deleted while any
/// handle to it (here, or cloned into a dependent's sandbox input) is alive. The
/// lock releases when the last `Arc<dyn Content>` for the entry drops.
///
/// **Every method must be forwarded**, including the ones with a working
/// default. This wrapper sits on *every* cacheable result artifact, so a method
/// left to its default is that method's fast path switched off product-wide,
/// with the inner artifact's implementation dead and nothing to see: `file_path`
/// silently answered `None` for on-disk cache blobs (the stable-ABI seam then
/// streamed them chunk-by-chunk instead of opening the file) and `entry_paths`
/// silently fell back to a byte-reading `walk` instead of the tar header scan.
/// `missing_trait_methods` on the impl below makes that a compile error rather
/// than a comment nobody reads — a doc cannot fail a build, and this exact
/// omission survived two `Content` methods being added.
///
/// [`file_path`](Content::file_path) is safe to forward for the same reason it
/// needs the guard: cache GC deletes a revision only under the per-addr *write*
/// lock (`Engine::gc_apply`, `Engine::try_trim_after_write`), which cannot be
/// taken while this read guard is alive — so the path stays valid for exactly as
/// long as this artifact does, which is the contract `Content::file_path`
/// states. Handing the bare `PathBuf` further out than the artifact would break
/// that; the one consumer (`HostArtifactContent::path`) opens it while still
/// holding the `Arc<dyn Content>`, and the open fd pins the inode thereafter.
struct GuardedArtifact {
    inner: Arc<dyn Content>,
    _lock: Arc<ResultReadGuard>,
}

#[deny(
    clippy::missing_trait_methods,
    reason = "this wrapper must forward every Content method; a default here \
              silently disables that method's fast path for every cached artifact"
)]
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
    fn entry_paths(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
        self.inner.entry_paths()
    }
    fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
        self.inner.seekable_reader()
    }
    fn byte_size(&self) -> Option<u64> {
        self.inner.byte_size()
    }
    fn file_path(&self) -> Option<std::path::PathBuf> {
        self.inner.file_path()
    }
}

/// One produced output of a target as it travels the result pipeline: an opaque
/// [`Content`] handle plus the group/type metadata that [`build_eresult`] and
/// codegen write-back need. The content is either a cache-backed [`CacheArtifact`]
/// (the normal stored/packed path) or a zero-copy passthrough — e.g. an
/// `@heph/fs:file` source file that never entered the local cache, carried as
/// its raw [`OutputArtifact`](outputartifact::OutputArtifact). Passthrough
/// artifacts skip [`Engine::cache_locally`] entirely, so they are never of type
/// `CacheArtifact`.
#[derive(Clone)]
pub struct ResultArtifact {
    pub content: Arc<dyn Content>,
    pub group: String,
    pub r#type: ManifestArtifactType,
}

impl ResultArtifact {
    /// Wrap a cache-backed artifact (the normal stored/packed output).
    fn from_cache(a: CacheArtifact) -> Self {
        Self {
            group: a.group.clone(),
            r#type: a.r#type.clone(),
            content: Arc::new(a),
        }
    }

    /// Wrap a zero-copy passthrough output that skipped the local cache. The
    /// content reads the durable source file directly (no tar, no cache blob),
    /// and — because the file was never snapshotted into the cache — re-hashes
    /// it on consume and fails if it diverges from the hash recorded at
    /// input-hashing time. See [`PassthroughContent`].
    ///
    /// `is_passthrough` only ever flags a `Content::File` or a
    /// `Content::View`; any other variant reaching here is a producer bug, so
    /// it falls back to carrying the raw artifact rather than panicking.
    fn passthrough(a: outputartifact::OutputArtifact) -> Self {
        let group = a.group.clone();
        let r#type = manifest_artifact_type(&a.r#type);
        let content: Arc<dyn Content> = match a.content {
            outputartifact::Content::File(f) => Arc::new(PassthroughContent {
                source_path: f.source_path,
                out_path: f.out_path,
                x: f.x,
                expected: a.hashout,
            }),
            // Hand the view out directly rather than re-wrapping it in its
            // `OutputArtifact`: the `ViewContent` is already a full `Content`,
            // so consumers get its header-only `entry_paths` and its
            // data-forwarding `walk` instead of the enum's generic fallbacks.
            outputartifact::Content::View(v) => v.view,
            other => Arc::new(outputartifact::OutputArtifact {
                content: other,
                ..a
            }),
        };
        Self {
            group,
            r#type,
            content,
        }
    }
}

/// [`Content`] for a passthrough source-file artifact (e.g. `@heph/fs:file`):
/// referenced by path and read live on consume, never copied into the cache.
///
/// Because nothing snapshots the bytes, the workspace file could be modified
/// between when it was hashed (the value folded into the target's `hashin` cache
/// key) and when a consumer reads it here. The live bytes would then silently
/// diverge from the cache key, poisoning every downstream entry. To turn that
/// silent corruption into a hard, explicit failure, the bytes are re-hashed as
/// they stream through — no extra I/O, the consumer is reading them anyway — and
/// the digest is checked against the recorded `hashout` at EOF.
///
/// `seekable_reader`/`file_path` stay `None` (Content defaults): the FUSE
/// tar-index path is bypassed and every consumer materializes via `walk()`, so
/// the verifying reader is always on the materialization path.
struct PassthroughContent {
    source_path: String,
    out_path: String,
    x: bool,
    /// Content hash recorded when the target was hashed; the just-read bytes
    /// must still hash to this.
    expected: String,
}

impl PassthroughContent {
    fn verifying_reader(&self) -> anyhow::Result<VerifyingReader> {
        Ok(self.open()?.0)
    }

    /// Open the source once and read its size off the resulting handle.
    ///
    /// `fstat` on the open descriptor, not a second `stat` by path: this runs
    /// once per source file per materialization — the hottest walk in a build —
    /// so a path-based size lookup would pay a full second path resolution for
    /// a file already open in this function.
    fn open(&self) -> anyhow::Result<(VerifyingReader, u64)> {
        let file = std::fs::File::open(&self.source_path)
            .with_context(|| format!("open passthrough source '{}'", self.source_path))?;
        let size = file
            .metadata()
            .with_context(|| format!("stat passthrough source '{}'", self.source_path))?
            .len();
        Ok((
            VerifyingReader {
                inner: Box::new(file),
                hasher: xxhash_rust::xxh3::Xxh3::new(),
                x: self.x,
                expected: self.expected.clone(),
                source_path: self.source_path.clone(),
                verified: false,
            },
            size,
        ))
    }
}

impl Content for PassthroughContent {
    fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
        Ok(Box::new(self.verifying_reader()?))
    }

    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
        let (reader, size) = self.open()?;
        let data: Box<dyn std::io::Read> = Box::new(reader);
        Ok(Box::new(std::iter::once(Ok(WalkEntry {
            path: std::path::PathBuf::from(&self.out_path),
            kind: WalkEntryKind::File {
                data,
                x: self.x,
                size,
            },
        }))))
    }

    fn hashout(&self) -> anyhow::Result<String> {
        Ok(self.expected.clone())
    }
}

/// A `Read` adapter that hashes the bytes it passes through and, at EOF, verifies
/// the digest matches the expected content hash — failing the read (and so the
/// consuming target) otherwise. The algorithm is identical to
/// [`hwalk::file_hashout`]: xxh3 over the content followed by a single exec-bit
/// marker byte. `passthrough_reader_matches_file_hashout` pins the two together
/// so they cannot silently drift.
struct VerifyingReader {
    inner: Box<dyn std::io::Read>,
    hasher: xxhash_rust::xxh3::Xxh3,
    x: bool,
    expected: String,
    source_path: String,
    verified: bool,
}

impl std::io::Read for VerifyingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            // EOF: finalize and verify exactly once. A reader read to completion
            // (the materialization copy always is) triggers this; a partial read
            // that is dropped early simply does not verify.
            if !self.verified {
                self.verified = true;
                self.hasher.update(&[self.x as u8]);
                let got = format!("{:x}", self.hasher.digest());
                if got != self.expected {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "passthrough source file '{}' was modified after it was hashed: \
                             content hash {got} no longer matches the {} recorded at \
                             input-hashing time — a source file changed mid-build",
                            self.source_path, self.expected
                        ),
                    ));
                }
            }
            return Ok(0);
        }
        if let Some(chunk) = buf.get(..n) {
            self.hasher.update(chunk);
        }
        Ok(n)
    }
}

fn manifest_artifact_type(t: &outputartifact::Type) -> ManifestArtifactType {
    match t {
        outputartifact::Type::Output => ManifestArtifactType::Output,
        outputartifact::Type::Log => ManifestArtifactType::Log,
        outputartifact::Type::SupportFile => ManifestArtifactType::SupportFile,
    }
}

/// Whether a produced output is a zero-copy passthrough that must skip the
/// local cache. Two shapes qualify, both on the uncached (`tmp`) path only:
///
/// * a `Content::File` its producer flagged as a durable source reference
///   (e.g. `@heph/fs:file`);
/// * a `Content::View` — a path-rewritten window onto another target's
///   artifact (the `group` driver's relocate/filter mode). Storing it would
///   duplicate every byte of the source for nothing; the source revision is
///   already cached, and the view is a few string operations to re-derive.
///
/// Both borrow bytes they do not own, which is exactly why a *cacheable*
/// revision is never a passthrough: it must own a durable copy, since the
/// borrowed source may change or vanish across runs.
fn is_passthrough(use_tmp_cache: bool, content: &outputartifact::Content) -> bool {
    use_tmp_cache
        && match content {
            outputartifact::Content::File(f) => f.passthrough,
            outputartifact::Content::View(_) => true,
            _ => false,
        }
}

/// The `@heph/fs` addrs covering every `codegen = in_place` output path.
///
/// Uses the same path→addr mapping as [`crate::engine::expand`]'s synthesized fs
/// inputs, so a declared in_place output and the input that reads it resolve to
/// the same addr — which is what lets
/// [`Engine::check_in_place_inputs_unchanged`] match one against the other.
/// Verified on a real target: an output `FilePath("go/large/alpha/alpha.go")`
/// and the input `//@heph/fs:file@f=go/large/alpha/alpha.go` are the same addr.
fn in_place_fs_addrs(
    outputs: &[crate::engine::driver::targetdef::Output],
) -> rustc_hash::FxHashSet<Addr> {
    use crate::engine::driver::targetdef::path::{CodegenMode, Content};

    let mut out = rustc_hash::FxHashSet::default();
    for path in outputs
        .iter()
        .flat_map(|o| o.paths.iter())
        .filter(|p| matches!(p.codegen_tree, CodegenMode::InPlace))
    {
        out.insert(match &path.content {
            Content::FilePath(p) => hbuiltins::pluginfs::file_addr(p),
            Content::Glob(p) => hbuiltins::pluginfs::glob_addr(p, &[]),
            Content::DirPath(p) => {
                hbuiltins::pluginfs::glob_addr(&format!("{}/**/*", p.trim_end_matches('/')), &[])
            }
        });
    }
    out
}

/// Build an [`EResult`] from produced artifacts, filtering by output group and
/// type, and attaching `guard` (the read lock for this target's cache entry) to
/// each kept artifact. `guard` is `None` only for the non-cacheable (force/shell)
/// path, whose artifacts are ephemeral and need no long-lived lock.
fn build_eresult(
    produced: Vec<ResultArtifact>,
    artifacts_meta: Vec<ArtifactMeta>,
    outputs: &[String],
    guard: Option<Arc<ResultReadGuard>>,
) -> EResult {
    let wrap = |content: Arc<dyn Content>| -> Arc<dyn Content> {
        match &guard {
            Some(lock) => Arc::new(GuardedArtifact {
                inner: content,
                _lock: Arc::clone(lock),
            }),
            None => content,
        }
    };

    // Support files are gated by the same rule the cached read path uses, so a
    // caller is handed the same set whether this revision was just built or read
    // back from the cache. Left ungated, a freshly-executed target would stage
    // support files into a caller that a cache hit would not.
    let support_needed = crate::engine::local_cache::support_files_needed_for(
        produced
            .iter()
            .any(|a| a.r#type == ManifestArtifactType::Output),
        outputs,
    );

    let mut artifacts: Vec<Arc<dyn Content>> = Vec::new();
    let mut support_artifacts: Vec<Arc<dyn Content>> = Vec::new();
    for a in produced {
        match a.r#type {
            ManifestArtifactType::Output if outputs.contains(&a.group) => {
                artifacts.push(wrap(a.content))
            }
            ManifestArtifactType::SupportFile if support_needed => {
                support_artifacts.push(wrap(a.content))
            }
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
    /// `--frozen`: verify codegen targets' generated output matches the tree
    /// without writing. A mismatch surfaces a [`FrozenCheckError`].
    pub frozen: bool,
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
    frozen: bool,
    /// True only for the directly-requested (top-level) target. Gates codegen
    /// tree write-back so a codegen target pulled in as a *dependency* doesn't
    /// materialize its output into the workspace.
    is_top: bool,
}

/// Output-independent result of the per-addr lock dance, single-flighted by
/// `mem_locked_result` (keyed by `Addr` alone). Every `(outputs, is_top)`
/// memoizer cell of one addr awaits the same cell and shares its single riding
/// read, so two sibling computations can never both contend the non-reentrant
/// per-addr result lock — the self-deadlock this prevents.
pub(crate) struct LockedResolution {
    /// The single riding read shared by all callers, pinning the cache entry for
    /// the request lifetime. `None` only on the non-cacheable force/shell path
    /// (ephemeral artifacts, no long-lived read).
    guard: Option<Arc<ResultReadGuard>>,
    /// `Some(full set)` when THIS cell produced the artifacts (cacheable execute,
    /// or the force/shell branch); [`build_eresult`] filters it to each caller's
    /// outputs. `None` on a pre-existing cache hit — callers then read only THEIR
    /// own outputs from the local cache under the shared `guard`, so a partial /
    /// remote cache pulls just the blobs each caller asked for rather than every
    /// output group (the lazy-pull invariant this preserves).
    executed: Option<Arc<ExecutedArtifacts>>,
    /// The parsed manifest of a pre-existing cache hit, captured once by the
    /// presence-probe and reused by every `(outputs, is_top)` caller in
    /// [`execute_and_cache`] — so each caller filters its own outputs from this
    /// shared manifest instead of re-reading + re-deserializing it from the cache
    /// backend. `Some` exactly when `executed` is `None` (a pre-existing hit);
    /// `None` on the execute / force / shell paths (no pre-existing manifest).
    manifest: Option<Arc<Manifest>>,
    /// The remote revision this hit's blobs can be pulled from — resolved
    /// **lazily**, by the first caller that finds a blob it needs is not local.
    ///
    /// A locally-mirrored manifest names blobs that were never downloaded, so
    /// "is this revision still on a remote?" used to be answered eagerly by the
    /// presence-probe: one manifest GET plus one prefix LIST per addr per run,
    /// under the per-addr lock, to prove bytes were obtainable that — for an
    /// interior node of a cached graph — nobody was ever going to read. Deferring
    /// it to the first caller that actually wants bytes makes the hashout-only
    /// path free, and it makes a hashout-only build survive a remote outage
    /// outright (a tripped breaker answers "absent", which the eager probe turned
    /// into a miss and a rebuild).
    ///
    /// Initialized eagerly only where the answer is already in hand: `Some(rev)`
    /// when this cell mirrored the manifest off a remote itself, `None` when there
    /// is no manifest to serve (executed / force / shell).
    ///
    /// The cell is keyed by `Addr` alone, like the `LockedResolution` that holds
    /// it. Nothing per-caller may enter it: the value is a property of the
    /// revision ("where can its bytes be fetched from"), never of what one caller
    /// asked for — see [`Engine::locate_remote_revision`].
    remote: RemoteCell,
}

/// Lazily-resolved home of a hit's blobs: `Some(rev)` if a remote can still serve
/// the revision, `None` if none can (no readable cache, the target opted out of
/// remote, or the revision is gone). Once it *answers* the answer is shared by
/// every caller of the addr; an init that errors (cancelled, transport failure)
/// leaves the cell unset so the next caller retries rather than inheriting one
/// caller's bad luck. See [`LockedResolution::remote`].
type RemoteCell = tokio::sync::OnceCell<Option<Arc<RemoteRevision>>>;

/// A [`RemoteCell`] whose answer is already known — the executed / force / shell
/// paths (no manifest, so nothing to pull) and the freshly-mirrored remote hit.
fn known_remote(rev: Option<Arc<RemoteRevision>>) -> RemoteCell {
    RemoteCell::new_with(Some(rev))
}

/// Check that rebuilding a revision whose bytes turned out to be unavailable
/// reproduced the `hashout`s its manifest promised.
///
/// Presence is decided at the manifest level, so by the time a read discovers the
/// bytes are gone the revision has already been published as a hit: a dependent
/// may have folded these `hashout`s into its own `hashin` and cached artifacts
/// under it. If the rebuild yields different ones, that dependent's cache key
/// describes a version of this target that no longer exists anywhere — a silent
/// wrong build that outlives the run, in the shared cache. Failing the run is the
/// only honest outcome; the cause is a non-reproducible target, not a cache bug,
/// and the message says so.
///
/// Compared as sorted multisets: manifest order is not part of the contract, and
/// a dependent's key folds the hashouts sorted anyway (`Engine::inner_meta` sorts
/// before hashing), so a pure re-ordering changes nothing downstream.
///
/// The compared element is `(type, group, hashout)`, not the bare hashout. The
/// bare-hashout multiset would pass a rebuild that merely *permuted* which group
/// holds which bytes — and a dependent asking `Exact(["a"])` would then get
/// different bytes under an unchanged `hashin`, because the group→hashout mapping
/// is itself absent from the cache key. That gap is pre-existing and wider than
/// this branch, but the guard is here, so it checks the stronger property.
///
/// **In-process only.** It reconciles this process's rebuild against the manifest
/// this process read. Another process holding its own riding read of the same
/// revision is not covered — see the exemption written up at the call site.
fn reconcile_rebuilt_hashouts(
    addr: &Addr,
    hashin: &str,
    manifest: &Manifest,
    produced: &[ResultArtifact],
) -> anyhow::Result<()> {
    /// `None` for a type no read path surfaces (a log), otherwise a sortable tag
    /// standing in for the type — `ManifestArtifactType` is not `Ord`.
    fn tag(t: &ManifestArtifactType) -> Option<&'static str> {
        match t {
            ManifestArtifactType::Output => Some("output"),
            ManifestArtifactType::SupportFile => Some("support"),
            ManifestArtifactType::Log => None,
        }
    }

    let mut promised: Vec<(&str, &str, &str)> = manifest
        .artifacts
        .iter()
        .filter_map(|a| Some((tag(&a.r#type)?, a.group.as_str(), a.hashout.as_str())))
        .collect();
    // Owned first: `hashout()` yields a `String` per artifact, which the borrowed
    // comparison tuples below then point into.
    let rebuilt_owned: Vec<(&str, &str, String)> = produced
        .iter()
        .filter_map(|a| Some((tag(&a.r#type)?, a.group.as_str(), &a.content)))
        .map(|(tag, group, content)| {
            let hashout = content
                .hashout()
                .with_context(|| format!("reading rebuilt hashout of {addr} group {group:?}"))?;
            anyhow::Ok((tag, group, hashout))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut rebuilt: Vec<(&str, &str, &str)> = rebuilt_owned
        .iter()
        .map(|(t, g, h)| (*t, *g, h.as_str()))
        .collect();
    promised.sort_unstable();
    rebuilt.sort_unstable();
    if promised == rebuilt {
        return Ok(());
    }
    anyhow::bail!(
        "{addr}: cached revision {hashin} was accepted as a hit, then its artifacts turned out to \
         be unavailable — and rebuilding it produced different outputs.\n  \
         cached:   {promised:?}\n  rebuilt:  {rebuilt:?}\n\
         Anything that already folded the cached hashouts into its own cache key now describes \
         outputs that do not exist. This target is not reproducible: same inputs must always \
         produce the same outputs.\n\
         This surfaced now because a cached blob had been reclaimed (a local GC, or an \
         object-store lifecycle rule on a shared cache) and the revision had to be rebuilt; on a \
         run where those bytes are still resident, nothing rebuilds and nothing checks."
    )
}

/// Pull one blob of `rev` into the local cache. A free function taking every
/// capture by value so the memoizer cell it feeds
/// ([`RequestStateData::mem_remote_blob`](crate::engine::request_state::RequestStateData))
/// gets a plain `'static` future.
async fn pull_one_remote_blob(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    rev: Arc<RemoteRevision>,
    addr: Addr,
    hashin: String,
    name: String,
) -> anyhow::Result<bool> {
    let ctoken = rs.ctoken().clone_arc();
    engine
        .pull_remote_blobs(ctoken.as_ref(), &addr, &hashin, &rev, &[name])
        .await
}

/// The full freshly-produced artifact set of an executing [`LockedResolution`]
/// cell (every output group), shared across all `(outputs, is_top)` callers and
/// filtered per caller by [`build_eresult`].
struct ExecutedArtifacts {
    cached: Vec<ResultArtifact>,
    meta: Vec<ArtifactMeta>,
}

/// Whether a transparent target's inputs name exactly one distinct member addr.
///
/// The unit is the *executing target*, not the input entry: a group may list one
/// member twice under different output filters, and both entries resolve to the
/// same addr-keyed `mem_locked_result` / `mem_execute_cache` cell, so only one
/// execute — and therefore only one terminal wrapper — ever runs. Allocation-free
/// on the hot path; the deduplicated list is materialized only for diagnostics.
fn has_single_member(inputs: &[Input]) -> bool {
    let mut members = inputs.iter().map(|input| &input.r#ref.r#ref);
    match members.next() {
        Some(first) => members.all(|member| member == first),
        None => false,
    }
}

/// The distinct member addrs of a transparent target, in declaration order.
/// Diagnostics only — quadratic in the member count, which is why the hot-path
/// check is [`has_single_member`].
fn distinct_members(inputs: &[Input]) -> Vec<Addr> {
    let mut out: Vec<Addr> = Vec::with_capacity(inputs.len());
    for input in inputs {
        if !out.contains(&input.r#ref.r#ref) {
            out.push(input.r#ref.r#ref.clone());
        }
    }
    out
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

    // "This hash-only request may not build" is a property of the request, not a
    // failure of the target. Nothing is wrong with the dep; we simply declined to
    // build it. Propagate unchanged so the caller can recognise it — the fixpoint
    // recompute treats it as "skip", and the in_place write-back guard needs to
    // say "could not confirm", not "dependency failed".
    if downcast_chain_ref::<HashUnknownError>(&e).is_some() {
        return e;
    }

    // "--shell needs one target" is a property of the request, not a failure of
    // the target that raised it — nothing ran. Propagate unchanged so a group
    // nested inside a single-member group doesn't record the user's own input
    // error as a failed target and render a failure box for something that never
    // executed.
    if downcast_chain_ref::<ShellNeedsSingleTarget>(&e).is_some() {
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
        // A request-shape error aggregated with `fail_fast = false` is still a
        // request error, not this target's failure. Surface it alone: every
        // sibling raised the identical one.
        for inner in &multi.0 {
            if let Some(shell) = downcast_chain_ref::<ShellNeedsSingleTarget>(inner) {
                return anyhow::Error::new(shell.clone());
            }
        }
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
        extract_log_tail(&e, rs.log_tail_lines())
    };
    rs.record_failure(
        addr.clone(),
        Arc::new(TargetFailure::new(addr.clone(), log_tail, e)),
    );
    UpstreamFailed { root: addr.clone() }.into()
}

/// Read the last `n` lines of a `ProcessFailed`'s log (anywhere in the chain) so
/// the recorded `TargetFailure` can surface them in its diagnostic, tagged with
/// the real starting line number. Best-effort: a log file that can't be read
/// (e.g. already reclaimed) yields no tail rather than masking the failure.
fn extract_log_tail(e: &anyhow::Error, n: usize) -> Option<hplugin::error::LogTail> {
    use std::io::Read as _;
    let pf = downcast_chain_ref::<ProcessFailed>(e)?;
    let mut buf = String::new();
    pf.log.reader().ok()?.read_to_string(&mut buf).ok()?;
    let (text, start_line) = hplugin::error::last_n_lines_with_start(&buf, n);
    Some(hplugin::error::LogTail { text, start_line })
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
    /// Resolve a target's result.
    ///
    /// Every dependency-graph edge recurses through here. With task-backed
    /// request memoizers each level is its own spawned task, so per-poll
    /// stack depth is O(1) in graph depth (the two deep-chain tests pin this
    /// on an explicit 2 MiB stack); the only remaining inline recursion is
    /// the transparent-group re-inline, whose boxed frames are small (its
    /// own pinned test covers 300 levels).
    pub fn result_addr<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &'a Addr,
        outputs: OutputMatcher,
        opts: &'a ResultOptions,
    ) -> BoxedResultFuture<'a> {
        self.result_addr_impl(rs, addr, outputs, opts)
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
            return Err(ShellNeedsSingleTarget::NotInteractive { addr: addr.clone() }.into());
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
        rs.announce_request_config(self.max_workers);

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
        rs.track_dep(addr).map_err(anyhow::Error::new)?;

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
            // Dependencies are never interactive. The terminal goes to the
            // single target the user named — never to something the engine
            // pulled in on its behalf.
            //
            // A transparent group with two or more members is exactly that
            // case: inlining is an implementation detail, and what the members
            // *are* is the run's dependencies. So this gate and the
            // `ResultOptions::default()` that dependency resolution already uses
            // are one principle enforced at two points, not two rules that
            // happen to agree — which is why `deps_never_inherit_the_terminal`
            // guards the general form of what this line is an instance of.
            // (Dependency resolution is doubly safe: deps are built by `meta`
            // while computing the parent's `hashin`, and `meta` is memoized per
            // addr with no `opts` in scope at all, so it *cannot* propagate a
            // wrapper; `inputs_result_exec` then re-fetches them with
            // `ResultOptions::default()`.)
            //
            // A group of one is a *name*, not a fan-out — there is nothing to
            // call a dependency — so inlining hands the terminal straight
            // through and `heph run //:dev` behaves like running its one member.
            //
            // The third enforcement point is `Engine::result`, which clears
            // `interactive` for any non-`Addr` matcher: a selection names no
            // single target to give the terminal to. Together the three
            // guarantee at most one live terminal wrapper per request, at any
            // nesting depth.
            //
            // Sharing the terminal between siblings is not merely untidy: each
            // member's wrapper builds its own `TtyReader`, so several readers
            // race the same terminal input queue and a keystroke lands in
            // whichever one the kernel picks; and the first member to finish
            // resumes the TUI (re-enabling raw mode and clearing Ctrl-C
            // suppression) underneath its live siblings — including the
            // cursor-position query the resume re-anchor issues, whose reply a
            // sibling's reader can swallow.
            if !has_single_member(&def.target_def.inputs) {
                if opts.shell {
                    return Err(ShellNeedsSingleTarget::Group {
                        addr: addr.clone(),
                        members: distinct_members(&def.target_def.inputs),
                    }
                    .into());
                }
                // `debug!`, not `info!` or a warning: nothing surprising
                // happened. Dependencies have never been interactive, and these
                // members are dependencies — there is no expectation to correct,
                // only a "why didn't I get a prompt?" to answer under `-v`.
                let dropped_terminal = opts.interactive.take().is_some();
                if dropped_terminal {
                    tracing::debug!(
                        addr = %addr.format(),
                        members = def.target_def.inputs.len(),
                        reason = "group_multi_member",
                        "group members are dependencies of the run; the terminal is not \
                         forwarded to them",
                    );
                }
            }

            let futures: Vec<_> = def
                .target_def
                .inputs
                .iter()
                .map(|input| {
                    let dep_addr = input.r#ref.r#ref.clone();
                    enclose!((self => engine, rs, opts) async move {
                        // The one surviving GrowStack site: group inlining
                        // recurses without a task hop (deliberately — nothing
                        // to memoize), so a deep group chain nests one boxed
                        // poll frame per level. See the pinned 2 MiB
                        // deep_transparent_group test.
                        crate::engine::grow_stack::grow_stack(
                            engine.result_addr(rs, &dep_addr, OutputMatcher::All, &opts),
                        )
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
        // `is_top` is part of the key: the same target can be reached both
        // top-level (is_top=true) and as a transparent-group member (is_top=false)
        // in one request. Both produce identical artifacts, but only the top-level
        // frame writes the codegen tree back / stores the fixpoint, so each
        // is_top variant needs its own memoizer cell or a race could bake the
        // wrong is_top into the shared computation. The second variant hits the
        // on-disk cache (keyed by hashin, not is_top), so there is no re-execute.
        let key = (AddrKey(addr.clone()), key_outputs, is_top);
        let opts = opts.clone();
        let interactive = opts.interactive.is_some();
        let res = rs
            .data
            .mem_result
            .once(
                key,
                enclose!((self => engine, rs, addr, outputs) move || async move {
                    match engine.inner_result_addr(rs.clone(), &addr, outputs, &opts, is_top).await {
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

    /// How many matched addrs may have a live top-level task at once.
    ///
    /// `Engine::result` used to spawn one task per matched addr as fast as the
    /// matcher yielded them, with only a cancellation check as a brake, and
    /// `result_semaphore` is acquired *inside* `execute` — downstream of spec,
    /// def, link, hashin and probe, and only on a miss. So nothing bounded the
    /// resolve pipeline itself: a hundred thousand targets meant a hundred
    /// thousand live boxed `#[async_recursion]` state machines, a hundred
    /// thousand jobs queued against the blocking pool's `2 * cores` threads, and
    /// a hundred thousand wakers in one global mutex — all before a single target
    /// was known to need building.
    ///
    /// **Sized against what is downstream, not against the target count.** The
    /// work a resolution actually does is bounded elsewhere and none of those
    /// bounds scale past a few dozen: the sqlite read pool and pipe budget are
    /// `max(2 * parallelism, 64)`, the blocking pool is `2 * cores` threads, and
    /// the remote cache has its own per-cache request split. Admitting far past
    /// them does not buy concurrency, it buys queue — which is the thing being
    /// fixed. Eight times the execute width leaves ample margin for slow
    /// resolutions not to idle the execute permits (`Engine::labels`, the
    /// adjacent stream-consuming walk, bounds at `2 * cores`), while staying in
    /// the neighbourhood of the machinery underneath.
    ///
    /// Clamped at both ends: the floor keeps a small `--jobs` from serialising
    /// resolution, and the ceiling keeps a large one from handing
    /// `Semaphore::new` an absurd number.
    ///
    /// This does **not** make request memory `O(limit)`. `query`'s `seen` set,
    /// the `ok` batch, the `mem_result` cells and the recursive dep fan-out are
    /// all structurally `O(matched)` — and so is the peak flock-fd count, since
    /// a read guard rides on the artifact and lives as long as the request holds
    /// it, so this bounds the rate of acquisition and not the count. What it
    /// bounds is live *task* state, blocking-queue depth, and waker churn.
    fn top_level_spawn_limit(max_workers: usize) -> usize {
        max_workers.saturating_mul(8).clamp(16, 2048)
    }

    /// Fold one finished top-level task into the batch's outcome.
    ///
    /// Shared by the reap inside the admission loop and the final drain, because
    /// the two must not diverge: fail-fast semantics, the treatment of
    /// cancellation as stop-fallout rather than a failure, and "the first
    /// failure wins" are all decided here, and a second copy would be a second
    /// place for them to drift.
    fn classify_joined(
        joined: Result<(Addr, anyhow::Result<Arc<EResult>>), tokio::task::JoinError>,
        fail_fast: bool,
        rs: &Arc<RequestState>,
        ok: &mut Vec<Arc<EResult>>,
        errors: &mut Vec<(Addr, anyhow::Error)>,
        fatal: &mut Option<anyhow::Error>,
    ) {
        let (addr, res) = match joined {
            Ok(pair) => pair,
            // A task panicked (we never abort them). Capture it, signal stop, and
            // let the caller keep draining the rest — never propagate via `?`,
            // which would drop the JoinSet.
            Err(join_err) => {
                if fatal.is_none() {
                    *fatal = Some(anyhow::Error::new(join_err).context("result task panicked"));
                    rs.ctoken().cancel();
                }
                return;
            }
        };
        match res {
            Ok(v) => ok.push(v),
            Err(e) if downcast_chain_ref::<CancelledError>(&e).is_some() => {
                // Cancellation is stop-fallout, not a genuine failure: the token
                // is cancelled, so we surface a single `CancelledError` after
                // draining rather than recording it per-addr.
            }
            Err(e) => {
                if !fail_fast {
                    errors.push((addr, e));
                } else if fatal.is_none() {
                    // Fail-fast: tell everything to stop, then wait for it.
                    // Failures landing after we signalled don't override it.
                    *fatal = Some(e);
                    rs.ctoken().cancel();
                }
            }
        }
    }

    /// Resolve every addr a matcher selects, admitting a bounded number at a
    /// time (see `top_level_spawn_limit`).
    ///
    /// # Two invariants keep the admission bound deadlock-free
    ///
    /// Neither is enforced by the type system, and breaking either one wedges a
    /// build rather than slowing it, so they are written down here:
    ///
    /// 1. **This function is only ever called from the top of a request** —
    ///    today `src/commands/run.rs` and the testkit harness, and nothing
    ///    else. It must never be reached from inside a running resolution (a
    ///    plugin entry point, a batch-within-a-batch): a permit holder that
    ///    re-entered here would be waiting for a permit it can only release by
    ///    finishing, which is a deadlock with no diagnostic. Every re-entrant
    ///    path goes to [`Engine::result_addr`] instead — provider callbacks via
    ///    `EngineProviderExecutor::result`, dep fan-out via `execute.rs`, and
    ///    the transparent-group re-inline — and none of them take a permit.
    ///
    /// 2. **The permit is taken around the *spawn*, never lower.** Gating
    ///    `result_addr` (or `execute`) on the same semaphore would put a permit
    ///    holder behind a permit request. `result_semaphore` is deliberately
    ///    acquired inside `execute` instead, for the diamond-deadlock reason
    ///    documented there.
    ///
    /// Together these give the property the bound relies on: **no holder of a
    /// permit is ever waiting for one.** A matched addr that is also some other
    /// target's dep does *not* wait for its own admission — `hmemoizer::Cell`
    /// elects a driver per poll, so the parent computes it inline in its own
    /// task, and the admission loop later hits the same memoized cell.
    pub async fn result(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<BatchResult> {
        let mut opts = opts.clone();
        // First leg of the one-target rule: a selection is many targets, so no
        // target gets the terminal (see the transparent-group gate in
        // `result_addr_impl` for the full rationale). `--shell` has nothing to
        // attach to once it is gone, so refuse it here — once, naming the
        // selection — rather than letting every matched target hit the
        // "non-interactive mode" guard below and tell a user sitting at a
        // terminal that they are not on one.
        if !matches!(matcher, Matcher::Addr(_)) {
            if opts.shell && opts.interactive.is_some() {
                return Err(ShellNeedsSingleTarget::Selection {
                    query: hmodel::htquery::format(matcher),
                }
                .into());
            }
            opts.interactive = None;
        }

        // Announce worker capacity up front so the client can paint a fixed
        // worker-slot indicator before any execute lands.
        rs.announce_request_config(self.max_workers);

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

        // First genuine (non-cancellation) failure — from the matcher walk below
        // or from a target. We never leave either loop by `?`: returning drops
        // the `JoinSet`, and the futures the spawned tasks were driving live in
        // this request's memoizers, which the tasks hold an `Arc` back into. The
        // un-polled future keeps that cycle alive, so `RequestStateData::drop`
        // never runs: children are never signalled, and their sandboxes are never
        // enqueued for cleanup. Instead the first failure *signals* every other
        // target to stop (cancelling the request token broadcasts SIGINT to
        // running children) and we keep draining until they have all stopped by
        // themselves, then return this error.
        let mut fatal: Option<anyhow::Error> = None;
        let mut ok: Vec<Arc<EResult>> = vec![];
        let mut errors: Vec<(Addr, anyhow::Error)> = vec![];

        // See `top_level_spawn_limit`. A finished task's result stays in the
        // `JoinSet` until the drain below, but its future — the expensive part —
        // is dropped on completion, so the permit and the state machine are
        // released together.
        let spawn_limit = Arc::new(tokio::sync::Semaphore::new(Self::top_level_spawn_limit(
            self.max_workers,
        )));

        // The walk runs in its own task feeding a bounded channel, rather than
        // being driven inline by the admission loop below.
        //
        // `query` is a lazy stream with no producer of its own: polled from the
        // same loop that parks on `spawn_limit`, the whole walk — package
        // enumeration, `provider.list` (whole-package Starlark evaluation),
        // `probe_segments` — stops dead every time admission is full. That
        // silently re-couples two things that used to overlap, and it can stop
        // the walk *indefinitely*: a permit holder sitting in `gate_approval` is
        // waiting on a human. The `Matched` stream is emitted from the walk, so
        // the client's "done X / ~N" denominator would freeze with it, showing a
        // stalled count and no reason for it.
        //
        // The channel is what keeps admission bounded regardless: the walk may
        // run ahead, but only by its capacity.
        let (matched_tx, mut matched_rx) = tokio::sync::mpsc::channel::<anyhow::Result<Addr>>(
            Self::top_level_spawn_limit(self.max_workers),
        );
        // `spawn_with_cycle_ctx`, not bare `tokio::spawn`: the walk calls
        // `Memoizer::once` (packages, `probe_segments`, the `MatchShrug`
        // spec/def resolutions), and without the parent frame those calls get no
        // wait-for edge — a cycle running through them would hang instead of
        // reporting `MemoizerCycleError`.
        let walk =
            hcore::hmemoizer::spawn_with_cycle_ctx(enclose!((self => engine, rs, owns_matched) {
                let matcher = matcher.clone();
                async move {
                    let stream = engine.query(rs.clone(), &matcher);
                    tokio::pin!(stream);
                    loop {
                        match stream.try_next().await {
                            Ok(Some(addr)) => {
                                // Announce each match as it resolves so the client can
                                // render a provisional "done X / ~N" that grows while
                                // the matcher streams.
                                if owns_matched {
                                    rs.emit(crate::engine::event::BuildEventKind::Matched {
                                        addrs: vec![addr.format()],
                                        complete: false,
                                    });
                                }
                                // A closed receiver means the consumer stopped
                                // enqueuing (cancelled, or a fatal landed); stop
                                // walking rather than keep evaluating packages.
                                if matched_tx.send(Ok(addr)).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => return,
                            Err(e) => {
                                drop(matched_tx.send(Err(e)).await);
                                return;
                            }
                        }
                    }
                }
            }));

        // Built once, not per matched addr: `cancelled()` boxes a future and
        // registers a waker in a shared map, and it resolves at most once.
        let mut cancelled = rs.ctoken().cancelled();
        // Whether the loop ended because the walk did, rather than by breaking.
        let mut walk_finished = false;
        loop {
            let Some(next) = matched_rx.recv().await else {
                walk_finished = true;
                break;
            };
            let addr = match next {
                Ok(addr) => addr,
                // The matcher walk itself failed. Stop enqueuing, signal the
                // already-spawned targets, and fall through to the drain.
                Err(e) => {
                    fatal = Some(e);
                    rs.ctoken().cancel();
                    break;
                }
            };
            // Stop enqueuing new targets once cancelled — don't keep draining
            // the matcher and spawning work that would immediately bail.
            if rs.ctoken().is_cancelled() {
                break;
            }
            // Admission control. Deadlock-free because a permit is only ever
            // taken by a *top-level* spawn: dep fan-out is resolved inline
            // inside the parent's task, and a memoizer cell is driven by
            // whichever awaiter polls it — so a matched addr that is also some
            // other target's dep is computed inside that parent and never waits
            // here. No holder of a permit is waiting for one.
            //
            // Raced against cancellation rather than only checked before it: a
            // permit holder can be parked on something slow (a human at
            // `gate_approval`, a wedged network pull), and Ctrl-C must not have
            // to wait for it to finish before this loop stops.
            let permit = tokio::select! {
                biased;
                () = &mut cancelled => break,
                permit = Arc::clone(&spawn_limit).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    // Only possible if the semaphore were closed, and nothing
                    // closes it — but treat it like the matcher failing rather
                    // than unwrapping: stop enqueuing, drain what is running.
                    Err(e) => {
                        fatal = Some(anyhow::Error::new(e).context("acquiring a resolution slot"));
                        rs.ctoken().cancel();
                        break;
                    }
                },
            };
            // Reap whatever finished while we waited for that permit — a permit
            // only frees when a task ends, so by here at least one is joinable.
            //
            // This is what keeps fail-fast working. Admission is now gated on
            // completions, so the drain below cannot start until the last addr
            // has been admitted; leaving classification entirely to it would
            // mean a target that failed at t=0 no longer stops the batch, and
            // every remaining target gets built before anyone notices. It also
            // holds the `JoinSet` at roughly the in-flight count instead of one
            // entry per matched addr.
            while let Some(joined) = set.try_join_next() {
                Self::classify_joined(joined, fail_fast, &rs, &mut ok, &mut errors, &mut fatal);
            }
            if fatal.is_some() {
                break;
            }
            hcore::hmemoizer::join_set_spawn(
                &mut set,
                enclose!((self => engine, rs, opts, addr, outputs) async move {
                    // Released when this task ends, however it ends.
                    let _permit = permit;
                    let r = engine.result_addr(rs, &addr, outputs, &opts).await;
                    (addr, r)
                }),
            );
        }
        drop(matched_rx);
        if walk_finished {
            // The walk is already over — this only collects a panic, so that one
            // is not reported as a cleanly-complete empty set.
            if let Err(join_err) = walk.await
                && fatal.is_none()
            {
                fatal = Some(anyhow::Error::new(join_err).context("matcher walk task panicked"));
                rs.ctoken().cancel();
            }
        } else {
            // Broke early (cancelled, or a fatal landed): its answer is no longer
            // wanted. Abort rather than wait it out, so the *enqueue* side of
            // Ctrl-C stays immediate.
            //
            // Read this precisely: aborting stops the walk **task**, which is the
            // consumer of the discovery fan-out. It does not stop the fan-out.
            // `Engine::query` runs each package as its own task and a dropped
            // `JoinHandle` detaches rather than cancels, so up to K
            // whole-package Starlark evaluations that were already in flight run
            // to completion after this returns. Ctrl-C therefore stops
            // *scheduling* new package work, not the work already started.
            //
            // That is deliberate, and it is the cheaper side of the trade: a
            // detached package task drives its `mem_probe` cell to completion, so
            // the cell releases its future and the `Arc<RequestState>` inside it,
            // and the ≤K whole-package evaluations already paid for land in the
            // memoizer instead of being thrown away. The residue is bounded at K
            // because `query`'s per-package futures short-circuit on a cancelled
            // token before starting anything new.
            //
            // Detaching is no longer what *rescues* those cells, only what
            // happens to them. Before #241 a cell nobody polled kept its future
            // forever — an Arc cycle through the `RequestStateData` that owns the
            // memoizer, so the request never deregistered — and running the tasks
            // out was the only way to avoid it. `Memoizer::process` now registers
            // an interest per frame, and the last one to go on an incomplete cell
            // evicts it and drops its future (`hmemoizer::cancel_abandoned`). So
            // aborting the package tasks would be *safe* today; detaching is kept
            // because it is the better trade, not because the alternative leaks.
            // The `JoinSet` drain below stands on its own reasoning, not on this.
            walk.abort();
        }

        // Matcher fully resolved: mark the matched set final (drops the `~`).
        // Not on a failed walk — the set never became final, so claiming it did
        // would paint a wrong denominator over an aborting run.
        if owns_matched && walk_finished && fatal.is_none() {
            rs.emit(crate::engine::event::BuildEventKind::Matched {
                addrs: Vec::new(),
                complete: true,
            });
        }

        while let Some(joined) = set.join_next().await {
            Self::classify_joined(joined, fail_fast, &rs, &mut ok, &mut errors, &mut fatal);
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
        is_top: bool,
    ) -> anyhow::Result<EResult> {
        let addr_str = addr.format();
        crate::engine::event::emit_scope(
            &rs,
            crate::engine::event::BuildEventKind::ResultStart {
                addr: addr_str.clone(),
            },
            // The one scope that carries structured detail: `ResultEnd` is where
            // a consumer learns a target's outcome, so it is where `upstream_of`
            // (root vs collateral), the exit status and the log tail belong.
            move |error| {
                let (error, upstream_of, exit_status, log_tail) = match error {
                    Some(d) => (Some(d.message), d.upstream_of, d.exit_status, d.log_tail),
                    None => (None, None, None, None),
                };
                crate::engine::event::BuildEventKind::ResultEnd {
                    addr: addr_str,
                    error,
                    upstream_of,
                    exit_status,
                    log_tail,
                }
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

                let result = self
                    .execute_and_cache(
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
                            frozen: opts.frozen,
                            is_top,
                        },
                    )
                    .await?;

                // Telemetry: artifact count + per-artifact sizes aren't on the
                // event stream, so record them here once the result is in hand.
                // Counts every resolved target across the process; the opt-out
                // only gates whether the snapshot is sent.
                let sizes: Vec<u64> = result
                    .artifacts
                    .iter()
                    .filter_map(|a| a.byte_size())
                    .collect();
                htelemetry::telemetry::record_artifacts(result.artifacts.len() as u64, &sizes);

                Ok(result)
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

    /// Thin per-`(outputs, is_top)` wrapper over [`resolve_locked`]. The lock
    /// dance runs at most once per addr per request (single-flighted in
    /// `resolve_locked`), producing one shared riding read.
    ///
    /// If that cell executed, it hands back the full freshly-produced set and we
    /// filter it here. On a pre-existing cache hit it hands back only the shared
    /// read, and we fetch *just this caller's* outputs from the local cache under
    /// it — so a partial/remote cache pulls only the blobs each caller asked for,
    /// never the whole output set.
    ///
    /// The codegen write-back + fixpoint registration live here (not in the
    /// shared cell) because they are per-`(outputs, is_top)`: `materialize_codegen`
    /// is is_top-gated, and both run under this caller's shared riding read. This
    /// matches the pre-single-flight placement — `materialize_codegen` on every
    /// path, `maybe_store_fixpoint` only on the cacheable path (never force/shell).
    async fn execute_and_cache(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        outputs: Vec<String>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<EResult> {
        let locked = self.clone().resolve_locked(rs.clone(), def, opts).await?;
        let (cached, meta): (Vec<ResultArtifact>, Vec<ArtifactMeta>) = match &locked.executed {
            // This cell produced the artifacts; filter the full set to `outputs`.
            // Already `ResultArtifact`s (cache-backed or passthrough).
            Some(ex) => (ex.cached.clone(), ex.meta.clone()),
            // Pre-existing hit: read only this caller's outputs under the shared
            // riding read. Silent — the shared cell already emitted the addr's
            // hit/miss event; re-emitting per caller would double-count. Reuse the
            // manifest the probe already parsed (shared across all callers of this
            // single-flight cell) instead of re-reading + re-deserializing it;
            // fall back to a fresh read only if it is somehow absent. A cache hit
            // is always cache-backed (a passthrough never wrote a manifest), so
            // every artifact maps through `from_cache`.
            None => {
                // Destructured once for the whole arm: the same manifest that
                // decides the read below is the one the rebuild is reconciled
                // against, so the two can never disagree about which revision was
                // announced.
                let manifest = locked.manifest.as_deref();
                let res = match manifest {
                    Some(manifest) => {
                        // Lazy materialization: probe just the blobs THIS caller
                        // reads, and pull only those it doesn't already have.
                        // A hashout-only caller needs none, so it neither probes
                        // nor touches the network.
                        //
                        // An unservable blob answers `false` instead of failing —
                        // the entry was confirmed, then the bytes went away. Both
                        // that and a local blob that vanished fall through to
                        // `unavailable` below.
                        // The residency token is the only way to get
                        // `Established`, and it means: every blob this caller
                        // reads was just confirmed present or pulled, over exactly
                        // the set the read below walks (`needed_artifacts` defines
                        // both). So the read skips its own probe — which would have
                        // nothing to learn, and on freshly pulled keys would park a
                        // worker on the sqlite writer's next batch commit.
                        match self
                            .materialize_blobs(
                                &rs,
                                manifest,
                                &locked.remote,
                                def,
                                opts.hashin.as_str(),
                                &outputs,
                            )
                            .await?
                        {
                            Some(residency) => {
                                self.artifacts_from_manifest(
                                    rs.ctoken(),
                                    &def.target.addr,
                                    opts.hashin.as_str(),
                                    manifest,
                                    &outputs,
                                    residency,
                                )
                                .await?
                            }
                            None => None,
                        }
                    }
                    None => {
                        self.clone()
                            .artifacts_from_local_cache(
                                rs.ctoken(),
                                def,
                                opts.hashin.as_str(),
                                outputs.clone(),
                            )
                            .await?
                    }
                };
                match res {
                    Some((cache_arts, meta)) => (
                        cache_arts
                            .into_iter()
                            .map(ResultArtifact::from_cache)
                            .collect(),
                        meta,
                    ),
                    // The entry was confirmed but its bytes are unavailable: a
                    // remote object expired before the read, a transfer failed, or
                    // a local blob went missing. Build the target instead of
                    // failing the run — the same outcome a plain cache miss would
                    // have had, just decided later. Debug, not warn: a shared cache
                    // reclaiming an old revision mid-build is ordinary, and the
                    // rebuild is the correct response.
                    //
                    // This executes under the riding *read* lock rather than the
                    // write lock the miss path holds, and that is a real, accepted
                    // exemption rather than a proof of safety:
                    //
                    // - In-process, the execute memoizer collapses concurrent
                    //   callers to one run, and `reconcile_rebuilt_hashouts` below
                    //   catches a rebuild that disagrees with the manifest.
                    // - Across *processes* it is not closed. Reads do not exclude
                    //   reads, so process B can be holding its own riding read of
                    //   this revision while A rebuilds. Blobs are keyed by
                    //   `(addr, hashin, name)` — by inputs, NOT by content — so A's
                    //   write lands on the same key B is reading, and
                    //   `CacheArtifact::hashout()` reports the *manifest's* recorded
                    //   hashout, which nothing re-verifies against the bytes. If the
                    //   target is not reproducible, B can read A's new bytes under
                    //   the old hashout.
                    //
                    // Closing that would mean taking the write lock here — and this
                    // branch runs while THIS request holds a riding read on the
                    // same addr, which the write acquire would block on forever
                    // (the lock is not reentrant and a read cannot upgrade). It
                    // would take restructuring, not a line, and that restructuring
                    // is the per-addr write-lock cost this path exists to avoid.
                    //
                    // Note the frequency changed with the lazy remote lookup: the
                    // trigger used to be "an eviction raced a concurrent build",
                    // and is now "a needed blob is not local and cannot be
                    // fetched" — which is every interior node of a
                    // remote-mirrored cache during a remote outage. So two
                    // processes concurrently rebuilding the same target is no
                    // longer exotic. For a hermetic target that is wasted work,
                    // not a wrong answer; the exposure is a target that is not
                    // reproducible, which is a model violation the reconcile
                    // below catches in-process. Accepted deliberately.
                    None => {
                        // A hash-only request may hash, probe and read, but must
                        // never build: the caller it is nested inside (the in_place
                        // write-back guard, the fixpoint recompute) is holding
                        // guards it cannot release until this returns. Building
                        // here would also write a revision, spawn a remote upload
                        // and enqueue a GC on its behalf. The miss path guards this
                        // at the lock acquire; this branch reaches
                        // `execute_and_cache_inner` without one, so it needs its
                        // own. Answer "unknown" and let the caller decide.
                        if rs.hash_only() {
                            return Err(HashUnknownError {
                                addr: def.target.addr.clone(),
                            }
                            .into());
                        }
                        tracing::debug!(
                            addr = %def.target.addr,
                            hashin = opts.hashin.as_str(),
                            "cache entry confirmed but its artifacts are unavailable; rebuilding",
                        );
                        // This addr was already announced as a cache hit. The
                        // event-stream consumers (TUI, GHA summary) retract that
                        // themselves when the `ExecuteStart` below lands, but the
                        // telemetry collector keeps no per-addr state and cannot,
                        // so tell it explicitly.
                        htelemetry::telemetry::record_cache_hit_rebuilt();
                        let (cached, meta) = self
                            .clone()
                            .execute_and_cache_inner(rs.clone(), opts)
                            .await?;
                        // The revision was already accepted as a hit before this
                        // rebuild, so a dependent may already have folded the
                        // manifest's `hashout`s into its own `hashin`. If the
                        // rebuild produced different ones, that dependent's key
                        // now describes bytes that never existed — a silent wrong
                        // build. Reconcile, and fail loudly if they diverge.
                        // `Some` on every path that announced a hit. The
                        // `artifacts_from_local_cache` fallback above is the only
                        // way to arrive without one, and it cannot be reached for
                        // a pre-existing hit (which always carries its manifest).
                        if let Some(manifest) = manifest {
                            reconcile_rebuilt_hashouts(
                                &def.target.addr,
                                opts.hashin.as_str(),
                                manifest,
                                &cached,
                            )?;
                        }
                        (cached, meta)
                    }
                }
            }
        };

        // Guard the write-back against a tree that moved under us — an in_place
        // target is about to overwrite the very files it hashed as inputs.
        self.clone()
            .check_in_place_inputs_unchanged(&rs, opts)
            .await?;
        // Codegen tree write-back: is_top-gated, idempotent, runs on every path
        // (a cache hit on an in_place fmt must still materialize). Uses this
        // caller's `cached`, so the is_top requester must have asked for the
        // codegen output groups — exactly as before the single-flight split.
        let wrote = self
            .materialize_codegen(opts.is_top, opts.def, &cached, opts.frozen)
            .await?;
        // Fixpoint registration only on the cacheable path (force/shell never
        // cache a fixpoint). Idempotent across hit/miss; a no-op unless this is a
        // top-level in_place codegen target whose tree just moved — and when the
        // write-back moved nothing, the guard above already established that the
        // tree hashes to `opts.hashin`, so the fixpoint would recompute that same
        // key and early-return. Skip it and save a full input re-hash on the
        // steady-state path (an already-formatted tree), where it is the common
        // case.
        let can_cache = !opts.force && opts.def.target.cache.enabled && !opts.shell;
        if can_cache && wrote {
            self.clone().maybe_store_fixpoint(&rs, opts).await?;
        }

        Ok(build_eresult(cached, meta, &outputs, locked.guard.clone()))
    }

    /// Make every blob this caller reads local, and touch nothing else.
    ///
    /// The materialization half of a cache hit: presence was decided from the
    /// manifest alone, so residency is settled here, scoped to `outputs` (plus
    /// support files) — which is why resolving a target purely for its `hashout`
    /// probes nothing, transfers nothing, and makes no network call, even on a
    /// revision whose manifest was mirrored from a remote. Runs under the riding
    /// read lock.
    ///
    /// `None` when a needed blob is neither local nor obtainable — the caller
    /// rebuilds the target rather than failing (see the `unavailable` branch of
    /// [`execute_and_cache`](Self::execute_and_cache), which reconciles the
    /// rebuilt `hashout`s against the manifest's).
    ///
    /// Returns the [`BlobResidency`] token rather than a bool so that
    /// `Established` cannot be claimed without coming through here: the read that
    /// follows skips its own per-blob probe on the strength of this call having
    /// walked exactly [`needed_artifacts`](Self::needed_artifacts) over the same
    /// `outputs`, and a token is a harder thing to pass by accident than a `true`.
    async fn materialize_blobs(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        manifest: &Manifest,
        remote: &RemoteCell,
        def: &LinkedTargetDef,
        hashin: &str,
        outputs: &[String],
    ) -> anyhow::Result<Option<BlobResidency>> {
        let addr = &def.target.addr;
        let missing = self
            .missing_local_blobs(rs.ctoken(), addr, hashin, manifest, outputs)
            .await?;
        if missing.is_empty() {
            return Ok(Some(BlobResidency::Established));
        }

        // Something this caller reads is not local. Only now is it worth asking
        // where the revision's bytes live — once per addr, shared with every other
        // caller of this cell.
        let Some(rev) = self.remote_revision(rs, remote, def, hashin).await? else {
            return Ok(None);
        };
        Ok(self
            .pull_missing_blobs(rs, addr, hashin, rev, missing)
            .await?
            .then_some(BlobResidency::Established))
    }

    /// The remote revision backing this hit, forcing the addr's [`RemoteCell`] if
    /// no caller has yet. Runs the lookup once per answer, not once per caller —
    /// but an errored lookup leaves the cell unset, so a later caller retries.
    ///
    /// Only the revision's own coordinates go in — never `outputs`, never
    /// `is_top`. The cell is addr-keyed and shared, so a per-caller signal
    /// entering it would let whichever caller happened to arrive first decide for
    /// the rest.
    async fn remote_revision<'a>(
        &self,
        rs: &Arc<RequestState>,
        remote: &'a RemoteCell,
        def: &LinkedTargetDef,
        hashin: &str,
    ) -> anyhow::Result<Option<&'a Arc<RemoteRevision>>> {
        let rev = remote
            .get_or_try_init(|| self.locate_remote_revision(rs, def, hashin))
            .await?;
        Ok(rev.as_ref())
    }

    /// Locate `(addr, hashin)` on the readable remotes: the body forced into
    /// [`LockedResolution::remote`], run at most once per addr per request.
    ///
    /// `None` when no remote can serve it — no readable cache, the target opted
    /// out of remote (`remote_enabled`; see the miss path in
    /// [`resolve_locked_inner`](Self::resolve_locked_inner) for why the nix driver
    /// sets it), or the revision is simply not there. A `None` here degrades the
    /// hit to a rebuild, never to an error.
    async fn locate_remote_revision(
        &self,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        hashin: &str,
    ) -> anyhow::Result<Option<Arc<RemoteRevision>>> {
        if !def.target.cache.remote_enabled || !self.remote_caches.has_readable() {
            return Ok(None);
        }
        Ok(self
            .remote_caches
            .fetch_manifest(rs.ctoken(), &def.target.addr, hashin)
            .await
            .with_context(|| format!("locating remote revision for {} {hashin}", def.target.addr))?
            .map(Arc::new))
    }

    /// Download `missing` and nothing else.
    ///
    /// See [`pull_one_remote_blob`] for the per-blob body. Each blob is
    /// single-flighted per `(addr, hashin, name)` through the request, so two
    /// output groups needing the same support file download it once. The
    /// `RemoteCacheRead` span is emitted only when something actually transfers, so
    /// a `↓` op in the timeline always means real bytes.
    ///
    /// `false` when the remote could not serve a blob it had advertised.
    async fn pull_missing_blobs(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        hashin: &str,
        rev: &Arc<RemoteRevision>,
        missing: Vec<String>,
    ) -> anyhow::Result<bool> {
        let addr_s = addr.format();
        let hashin = hashin.to_string();
        crate::engine::event::emit_scope(
            rs,
            crate::engine::event::BuildEventKind::RemoteCacheReadStart {
                addr: addr_s.clone(),
            },
            move |error| crate::engine::event::BuildEventKind::RemoteCacheReadEnd {
                addr: addr_s,
                error: error.map(crate::engine::event::ErrorDetail::into_message),
            },
            async {
                let mut pulls = Vec::with_capacity(missing.len());
                for name in &missing {
                    let key = (AddrKey(addr.clone()), hashin.clone(), name.clone());
                    let engine = Arc::clone(self);
                    let rs_owned = Arc::clone(rs);
                    let rev = Arc::clone(rev);
                    let addr = addr.clone();
                    let hashin = hashin.clone();
                    let name = name.clone();
                    pulls.push(rs.data.mem_remote_blob.once(key, move || {
                        pull_one_remote_blob(engine, rs_owned, rev, addr, hashin, name)
                    }));
                }
                // Bounded: one in-flight pull holds a temp file plus a live
                // response stream, and a caller may have asked for many groups.
                let served: Vec<bool> = futures::stream::iter(pulls)
                    .buffered(crate::engine::remote_cache::REVISION_BLOB_CONCURRENCY)
                    .try_collect()
                    .await
                    .map_err(unwrap_arc_err)?;
                anyhow::Ok(served.into_iter().all(|ok| ok))
            },
        )
        .await
    }

    /// Single-flight the per-addr result-lock + cache-fetch/execute, keyed by
    /// `Addr` ALONE (not `outputs`/`is_top`). All `(outputs, is_top)` cells of
    /// one addr await this one cell and share its single riding read, so two
    /// sibling computations of the same addr can never both hold the
    /// non-reentrant per-addr lock (the self-deadlock this prevents).
    ///
    /// Sound because the on-disk cache is keyed by `hashin` (independent of
    /// `outputs`/`is_top`) and execution always produces the full output set:
    /// the shared cell only decides build-vs-hit and hands back one riding read.
    /// Each caller then materializes just its own outputs (see
    /// [`execute_and_cache`](Self::execute_and_cache)), so the pull stays lazy.
    ///
    /// Invariants preserved: cross-process serialization (still exactly one real
    /// flock acquire here; other processes have their own `mem_locked_result`),
    /// cross-request contention (the cell is per-`RequestStateData`), and
    /// read-pinning (the riding read is a real flock read — `try_write` still
    /// fails while it's alive). Only the *duplicate* same-addr acquire is removed.
    async fn resolve_locked(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Arc<LockedResolution>> {
        let addr = def.target.addr.clone();
        // Owned copies so the memoizer closure is `'static`.
        let def_owned = opts.def.clone();
        let hashin = opts.hashin.clone();
        let spec = opts.spec.clone();
        let force = opts.force;
        let shell = opts.shell;
        let interactive = opts.interactive.clone();

        rs.data
            .mem_locked_result
            .once(
                AddrKey(addr),
                enclose!((self => engine, rs) move || async move {
                    // is_top/frozen are deliberately fixed here: the shared cell
                    // is addr-keyed and output/is_top-agnostic. It never runs
                    // codegen write-back or fixpoint storage (those are per-caller,
                    // in `execute_and_cache`), and neither `execute_and_cache_inner`
                    // nor `execute` reads these fields — so the values are inert.
                    let opts = ExecuteOptions {
                        hashin: &hashin,
                        spec: &spec,
                        def: &def_owned,
                        force,
                        interactive,
                        shell,
                        frozen: false,
                        is_top: false,
                    };
                    engine.resolve_locked_inner(rs, &opts).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Body of the per-addr lock dance: the optimistic read → miss → drop →
    /// write → re-check → execute → downgrade → riding-read sequence (and the
    /// non-cacheable force/shell branch). Runs once per addr per request via
    /// [`resolve_locked`].
    ///
    /// Cache presence is decided at the **manifest** level, never by downloading
    /// outputs, and the split must stay intact:
    ///
    /// - *Presence (here):* a manifest — local, or fetched from a remote. Nothing
    ///   more: presence deliberately does **not** establish that the blobs the
    ///   manifest names are still obtainable, because proving that costs a remote
    ///   round trip per addr per run for bytes most callers never read. A manifest
    ///   carries every artifact's `hashout`, which is all a dependent needs to
    ///   compute its own `hashin`, so this answers "has this addr been built?"
    ///   while moving no output bytes. See
    ///   [`probe_cache_manifest`](Self::probe_cache_manifest).
    /// - *Materialization (per caller, in [`execute_and_cache`]):* where residency
    ///   is settled and bytes actually move. Each caller probes and pulls only the
    ///   output groups it asked for (plus support files), so a target resolved
    ///   just to feed a dependent's hash — the common case in a fully-cached
    ///   build — touches neither the disk nor the network beyond the manifest it
    ///   already has. A caller that needs bytes that are gone degrades to a
    ///   rebuild there, which is where the missing half of the old presence
    ///   guarantee is now handled.
    /// - *Hazard:* that per-caller pull writes blobs into the local cache while
    ///   holding only the riding **read** lock. The read excludes GC (whose
    ///   `try_write` fails while it is alive), and a pulled blob is a copy of the
    ///   revision a manifest already fixed, so a concurrent puller writes
    ///   identical bytes; the FS backend renames into place and the SQLite backend
    ///   writes in one transaction, so no reader ever observes a half-written blob.
    ///   (A *rebuild* under that same read lock is a stronger claim and is
    ///   written up at its own site in [`execute_and_cache`].)
    async fn resolve_locked_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Arc<LockedResolution>> {
        let def = opts.def;
        let can_cache = !opts.force && def.target.cache.enabled && !opts.shell;
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
            let skip_lock = opts.spec.driver == hbuiltins::pluginfs::DRIVER_NAME;
            let _w = if skip_lock {
                None
            } else {
                // Hash-only requests may take THIS write lock, unlike the
                // cacheable paths below. The self-deadlock they guard against
                // needs the outer request to be riding a read guard on the
                // addr, and uncacheable resolutions hand out no guard at all
                // (`guard: None` below) — by the time a nested recompute runs,
                // the outer request holds nothing here. Blanket-refusing made
                // the in_place write-back guard structurally unable to verify
                // any target whose meta chain crosses a cache-off dep (e.g. a
                // toolchain reached through `//@heph/bin:*` — hostbin is
                // cache-off), failing `r lint //...` on already-linted trees.
                Some(
                    self.acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
                        .await?,
                )
            };
            let (cached, meta) = self
                .clone()
                .execute_and_cache_inner(rs.clone(), opts)
                .await?;
            return Ok(Arc::new(LockedResolution {
                guard: None,
                executed: Some(Arc::new(ExecutedArtifacts { cached, meta })),
                manifest: None,
                remote: known_remote(None),
            }));
        }

        // 1. Optimistically take a plain shared read lock and probe the cache at
        //    the manifest level (see the doc comment). The read rides with every
        //    caller's artifacts, protecting the entry while in use.
        let read = self
            .acquire_with_notice(&rs, addr, self.result_lock().read(addr, ctoken))
            .await?;
        if let Some(manifest) = self.probe_cache_manifest(&rs, def, opts).await? {
            // A. Hit — share this read; each caller reads its own outputs under
            // it and runs its own codegen write-back / fixpoint in
            // `execute_and_cache` (is_top-gated, under this riding read). The
            // parsed manifest rides along so callers skip a second read+parse.
            // Where its blobs live is left unresolved: a caller that needs bytes
            // and hasn't got them forces the cell, and one that only needs
            // hashouts never does.
            return Ok(Arc::new(LockedResolution {
                guard: Some(Arc::new(read)),
                executed: None,
                manifest: Some(manifest),
                remote: RemoteCell::new(),
            }));
        }

        // B. Miss: a plain read cannot upgrade. Drop it and take the exclusive
        //    write lock directly — after a miss we'll almost certainly execute,
        //    so this skips the upgradable→upgrade two-step. It also serializes
        //    the execute phase per addr, replacing the old exclusive result lock.
        //
        //    Unless this is a hash-only request. Then the write acquire below
        //    would contend a riding read held by the very request we are nested
        //    inside, which cannot release it until we return. Answer "unknown"
        //    and let the caller decide — that is a deadlock, not contention.
        if rs.hash_only() {
            return Err(HashUnknownError { addr: addr.clone() }.into());
        }
        drop(read);
        let write = self
            .acquire_with_notice(&rs, addr, self.result_lock().write(addr, ctoken))
            .await?;

        // Re-check (manifest level) under the write lock: covers the drop window
        // above and any writer that produced the artifacts while we waited. The
        // write lock excludes all others, so one re-check suffices. On the rare
        // race-win we share without executing; otherwise we execute + cache the
        // full set, which `build_eresult` filters per caller.
        let (executed, manifest, remote) = match self.probe_cache_manifest(&rs, def, opts).await? {
            // Same as branch A: where the blobs live stays unresolved until a
            // caller needs them.
            Some(manifest) => (None, Some(manifest), RemoteCell::new()),
            None => {
                // The settled local miss: both probes came back empty under the
                // write lock, so this fires exactly once per cold target — the
                // probe itself stays silent on a miss (see
                // `probe_cache_manifest`). Uncacheable targets never reach this
                // path (early return above), so the hit/miss stats cover only
                // targets that could have hit.
                rs.emit(crate::engine::event::BuildEventKind::LocalCacheMiss {
                    addr: addr.format(),
                });
                // Local miss under the write lock: ask the remote caches whether
                // this revision exists. Manifest only — no output blob is
                // downloaded here, and none may be: a dependent that just needs
                // this target's `hashout` must not pay for its outputs. The
                // manifest is mirrored into the local cache (safe under the write
                // lock, which excludes GC and other writers) and each caller pulls
                // the groups it reads in `execute_and_cache`.
                //
                // Bracket the pull with one `RemoteCacheRead` span per target
                // (only when a readable cache exists) so a slow download shows as
                // a single `↓` op in the per-target timeline — never one per
                // blob/cache.
                // Honor the target's `remote_enabled`: drivers whose output
                // embeds host-local paths (e.g. the nix driver bakes absolute
                // `/nix/store/...` wrapper paths) set it false so a wrapper built
                // on one machine is never pulled onto another that lacks that
                // store path — which would `exec` a missing path (status 127).
                let remote_attempted =
                    def.target.cache.remote_enabled && self.remote_caches.has_readable();
                let located = if remote_attempted {
                    let addr_s = addr.format();
                    // Every blob a caller of this addr could read must still be on
                    // the remote for the hit to stand — checked by presence, not by
                    // transfer (see `RemoteCacheSet::blobs_exist`).
                    let needed: Vec<String> = def.target.output_names();
                    crate::engine::event::emit_scope(
                        &rs,
                        crate::engine::event::BuildEventKind::RemoteCacheReadStart {
                            addr: addr_s.clone(),
                        },
                        move |error| crate::engine::event::BuildEventKind::RemoteCacheReadEnd {
                            addr: addr_s,
                            error: error.map(crate::engine::event::ErrorDetail::into_message),
                        },
                        self.probe_remote_revision(ctoken, addr, opts.hashin.as_str(), &needed),
                    )
                    .await?
                } else {
                    None
                };
                match located {
                    Some((manifest, rev)) => {
                        // A revision located on the remote is a cache hit —
                        // "already built elsewhere", execution skipped. Emit it so
                        // the cached count (TUI, GHA, telemetry) reflects remote
                        // hits, not just local ones.
                        rs.emit(crate::engine::event::BuildEventKind::RemoteCacheHit {
                            addr: addr.format(),
                        });
                        // The one place the answer is already in hand: this cell
                        // just located the revision, so the lazy cell starts
                        // resolved and no caller re-fetches the manifest.
                        (
                            None,
                            Some(Arc::new(manifest)),
                            known_remote(Some(Arc::new(rev))),
                        )
                    }
                    None => {
                        // Only a real remote lookup that came back empty is a
                        // remote miss; skip the event when no readable remote was
                        // consulted at all.
                        if remote_attempted {
                            rs.emit(crate::engine::event::BuildEventKind::RemoteCacheMiss {
                                addr: addr.format(),
                            });
                        }
                        let (cached, meta) = self
                            .clone()
                            .execute_and_cache_inner(rs.clone(), opts)
                            .await?;
                        (
                            Some(Arc::new(ExecutedArtifacts { cached, meta })),
                            None,
                            known_remote(None),
                        )
                    }
                }
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
        Ok(Arc::new(LockedResolution {
            guard: Some(Arc::new(read)),
            executed,
            manifest,
            remote,
        }))
    }

    /// Materialize the codegen output tree for a freshly-resolved target.
    ///
    /// Gated to top-level requested targets (`rs.parent.is_none()`) with at least
    /// one codegen output path. For each codegen output group:
    /// - `frozen`: build a unified diff between the generated bytes and the tree
    ///   file, accumulate per-file diffs, and on any divergence return a
    ///   [`FrozenCheckError`] without writing anything.
    /// - otherwise: unpack the cached artifact into the workspace root (copy
    ///   semantics). `InPlace` groups overwrite tracked source files and never
    ///   write onto a path some `Copy` target already owns.
    ///
    /// Either way, a target with `Copy` output paths first **registers** them as
    /// codegen claims, so a later `glob()`/`file()` excludes them. Registration
    /// happens here, next to the write, for the reason the extended attribute
    /// this replaced got right: the claim must land in the same operation that
    /// puts the file on disk, or there is a window where the generated file
    /// exists and looks like source.
    ///
    /// The gates are cheap and stay here; everything past them runs on
    /// `hcore::blocking` (see [`Self::materialize_codegen_tree`]).
    async fn materialize_codegen(
        &self,
        is_top: bool,
        def: &LinkedTargetDef,
        cached: &[ResultArtifact],
        frozen: bool,
    ) -> anyhow::Result<bool> {
        use crate::engine::driver::targetdef::path::CodegenMode;

        // Gate: only the top-level requested target writes its tree back, and
        // only when it actually declares a codegen output path.
        if !is_top {
            return Ok(false);
        }
        let has_codegen = def.target.outputs.iter().any(|o| {
            o.paths
                .iter()
                .any(|p| !matches!(p.codegen_tree, CodegenMode::None))
        });
        if !has_codegen {
            return Ok(false);
        }

        // Past the gates this walks every generated file, reads its bytes out of
        // the cache, reads the tree file back and (when frozen) diffs the two —
        // the heaviest synchronous read on the result path, and one that parks
        // on any queued sqlite write to the artifact it is walking. It does not
        // belong on a runtime worker. Cloned rather than borrowed because a pool
        // job outlives a dropped caller future: an `Arc<TargetDef>` bump and a
        // `Vec` of `ResultArtifact` (an `Arc` plus two short strings each).
        //
        // Outliving the caller is the accepted cost here. A run cancelled mid
        // write-back reports cancelled while the job finishes rewriting the
        // tree, where before it could only be interrupted by a signal. Stopping
        // half way would be worse: the write-back is per-file and the tree is
        // the user's source, so an abandoned job leaves a *partial* codegen tree
        // either way, and letting it finish at least leaves a consistent one.
        let (target, cached, root, claims) = (
            Arc::clone(&def.target),
            cached.to_vec(),
            self.cfg.root.clone(),
            Arc::clone(&self.codegen_claims),
        );
        hcore::blocking::run(move || {
            Self::materialize_codegen_tree(&target, &cached, &root, &claims, frozen)
        })
        .await
    }

    /// The body of [`Self::materialize_codegen`], past its gates.
    ///
    /// Synchronous and byte-moving: called only through `hcore::blocking::run`.
    fn materialize_codegen_tree(
        target: &crate::engine::driver::targetdef::TargetDef,
        cached: &[ResultArtifact],
        root: &std::path::Path,
        claims: &hwalk::CodegenClaims,
        frozen: bool,
    ) -> anyhow::Result<bool> {
        use crate::engine::driver::targetdef::path::CodegenMode;

        // Whether anything about the tree actually moved: content writes, exec-bit
        // reconciles and symlink recreates. `false` therefore means the tree still
        // hashes exactly as it did before this call — which is what lets the
        // caller skip the fixpoint recompute.
        let mut wrote = false;
        let mut frozen_diff = String::new();

        // Register this target's `Copy` outputs BEFORE touching the tree, and on
        // the frozen path too. Ordering is deliberate: a claim with no file yet is
        // harmless, while a file with no claim is exactly the hole this mechanism
        // closes, so if the write below fails we want to have erred on the safe
        // side. Derived from the *declared* paths rather than from what the walk
        // happens to write, so the claim also covers a file the generator will
        // emit later, and re-recording releases a path it no longer emits. A
        // no-op once the ledger already says this, which is the steady state
        // after the first run.
        let copy_claims: Vec<hwalk::Claim> = target
            .outputs
            .iter()
            .flat_map(|o| o.paths.iter())
            .filter(|p| matches!(p.codegen_tree, CodegenMode::Copy))
            .map(|p| crate::engine::gitignore::content_to_claim(&p.content))
            .collect();
        if !copy_claims.is_empty() {
            claims
                .record(&target.addr.format(), &copy_claims)
                .with_context(|| format!("register codegen claims for {}", target.addr.format()))?;
        }
        let claim_set = claims.snapshot();

        // Map each codegen output group to its declared mode (first non-None
        // path wins). One group can back MULTIPLE cached Output artifacts (e.g.
        // several `out` entries sharing a group), so the loop below is driven off
        // the cached artifacts and looks the mode up per artifact — covering every
        // artifact, and exactly once.
        let mut group_mode: std::collections::HashMap<&str, &CodegenMode> =
            std::collections::HashMap::new();
        for output in &target.outputs {
            if let Some(mode) = output
                .paths
                .iter()
                .map(|p| &p.codegen_tree)
                .find(|m| !matches!(m, CodegenMode::None))
            {
                group_mode.entry(output.group.as_str()).or_insert(mode);
            }
        }

        for artifact in cached {
            if artifact.r#type != ManifestArtifactType::Output {
                continue;
            }
            let Some(mode) = group_mode.get(artifact.group.as_str()).copied() else {
                continue;
            };
            let group = artifact.group.as_str();

            if frozen {
                // Compare each generated file against the tree; never write.
                let walker = artifact
                    .content
                    .walk()
                    .with_context(|| format!("walk codegen output for frozen check: {group}"))?;
                for entry in walker {
                    let entry = entry
                        .with_context(|| format!("read codegen entry for frozen check: {group}"))?;
                    let (new_bytes, x) = match entry.kind {
                        WalkEntryKind::File { mut data, x, .. } => {
                            let mut buf = Vec::new();
                            std::io::Read::read_to_end(&mut data, &mut buf)
                                .with_context(|| format!("read generated file {:?}", entry.path))?;
                            (buf, x)
                        }
                        WalkEntryKind::Symlink { .. } => continue,
                    };
                    let tree_path = root.join(&entry.path);
                    // Symmetric with the write-back guard below: an `in_place`
                    // target never touches a copy-owned tree file, so a
                    // divergence there is not drift this target would reconcile —
                    // don't flag it in the frozen check.
                    if matches!(mode, CodegenMode::InPlace) && claim_set.claims(&entry.path) {
                        continue;
                    }
                    let old_bytes = match std::fs::read(&tree_path) {
                        Ok(b) => b,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                        Err(e) => {
                            return Err(e).with_context(|| {
                                format!("read tree file {:?} for frozen check", tree_path)
                            });
                        }
                    };
                    let content_same = old_bytes == new_bytes;
                    // The exec bit is part of the (content + exec-bit) fs hash, so
                    // a mode-only divergence is real drift the non-frozen run
                    // would write back — flag it here too, symmetric with the
                    // write-back reconcile.
                    #[cfg(unix)]
                    let exec_same = {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::metadata(&tree_path)
                            .map(|m| (m.permissions().mode() & 0o111 != 0) == x)
                            .unwrap_or(!x)
                    };
                    #[cfg(not(unix))]
                    let exec_same = {
                        // No exec-bit concept on this platform.
                        let _ = x;
                        true
                    };
                    if content_same && exec_same {
                        continue;
                    }
                    let path_label = entry.path.display().to_string();
                    if !content_same {
                        let old = String::from_utf8_lossy(&old_bytes);
                        let new = String::from_utf8_lossy(&new_bytes);
                        let diff = similar::TextDiff::from_lines(old.as_ref(), new.as_ref());
                        // Git-style headers so the per-file path rides on the
                        // `---`/`+++` lines (no redundant label above them).
                        let rendered = diff
                            .unified_diff()
                            .header(&format!("a/{path_label}"), &format!("b/{path_label}"))
                            .to_string();
                        frozen_diff.push_str(&rendered);
                        if !frozen_diff.ends_with('\n') {
                            frozen_diff.push('\n');
                        }
                    }
                    #[cfg(unix)]
                    if !exec_same {
                        let want = if x { "executable" } else { "non-executable" };
                        frozen_diff
                            .push_str(&format!("mode change: {path_label} should be {want}\n"));
                    }
                }
            } else {
                // Materialize the generated tree into the workspace root, but
                // write a file's bytes ONLY when they differ from what's on disk
                // (and reconcile its exec bit separately, below). `@heph/fs`
                // hashes inputs by (content, exec-bit), so re-reading an
                // unchanged file yields the same hash and an idempotent in_place
                // target hits the fixpoint cache instead of re-executing.
                // Skipping identical writes also avoids needless source-control
                // churn and pointless mtime bumps.
                let walker = artifact
                    .content
                    .walk()
                    .with_context(|| format!("walk codegen output for write-back: {group}"))?;
                for entry in walker {
                    let entry = entry
                        .with_context(|| format!("read codegen entry for write-back: {group}"))?;
                    let dest = root.join(&entry.path);
                    // An `in_place` target must not write back into a tree file
                    // that another `codegen = "copy"` target claims — doing so
                    // would clobber the copy target's output and leave the
                    // provenance pointing at the wrong producer. Leave such files
                    // to their owner.
                    if matches!(mode, CodegenMode::InPlace) && claim_set.claims(&entry.path) {
                        continue;
                    }
                    match entry.kind {
                        WalkEntryKind::File { mut data, x, .. } => {
                            let mut new_bytes = Vec::new();
                            std::io::Read::read_to_end(&mut data, &mut new_bytes)
                                .with_context(|| format!("read generated file {:?}", entry.path))?;
                            let unchanged =
                                matches!(std::fs::read(&dest), Ok(old) if old == new_bytes);
                            if !unchanged {
                                if let Some(parent) = dest.parent() {
                                    std::fs::create_dir_all(parent).with_context(|| {
                                        format!("create parent dir for {:?}", dest)
                                    })?;
                                }
                                std::fs::write(&dest, &new_bytes)
                                    .with_context(|| format!("write codegen file {:?}", dest))?;
                                wrote = true;
                            }
                            // The exec bit is part of the `@heph/fs` (content,
                            // exec-bit) input hash, so reconcile it to the
                            // generated artifact's `x` even when the bytes are
                            // unchanged — otherwise a target that only flips +x
                            // would never land on disk and the recomputed
                            // fixpoint key would disagree with what ran. Touch
                            // only the exec bits, and only when the boolean
                            // actually differs, so other mode bits stay put and
                            // an already-correct file sees no spurious churn.
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if let Ok(meta) = std::fs::metadata(&dest) {
                                    let cur = meta.permissions().mode();
                                    if (cur & 0o111 != 0) != x {
                                        let want = if x { cur | 0o111 } else { cur & !0o111 };
                                        std::fs::set_permissions(
                                            &dest,
                                            std::fs::Permissions::from_mode(want),
                                        )
                                        .with_context(
                                            || format!("reconcile exec bit on {:?}", dest),
                                        )?;
                                        wrote = true;
                                    }
                                }
                            }
                        }
                        WalkEntryKind::Symlink { target } => {
                            // Codegen outputs are regular files in practice;
                            // recreate symlinks only when missing or divergent.
                            #[cfg(unix)]
                            {
                                let recreate = match std::fs::read_link(&dest) {
                                    Ok(cur) => cur != target,
                                    Err(_) => true,
                                };
                                if recreate {
                                    if let Some(parent) = dest.parent() {
                                        std::fs::create_dir_all(parent).with_context(|| {
                                            format!("create parent dir for {:?}", dest)
                                        })?;
                                    }
                                    match std::fs::remove_file(&dest) {
                                        Ok(()) => {}
                                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                        Err(e) => {
                                            return Err(e).with_context(|| {
                                                format!("remove {:?} before symlink", dest)
                                            });
                                        }
                                    }
                                    std::os::unix::fs::symlink(&target, &dest).with_context(
                                        || format!("symlink {:?} -> {:?}", dest, target),
                                    )?;
                                    wrote = true;
                                }
                            }
                        }
                    }
                }
            }
        }

        if frozen && !frozen_diff.is_empty() {
            return Err(anyhow::Error::new(crate::engine::error::FrozenCheckError {
                addr: target.addr.clone(),
                diff: frozen_diff,
            }));
        }

        Ok(wrote)
    }

    // Memoized by addr:hashin — at most one execute+cache cycle runs per target per request,
    // preventing double-execute when the same target is requested with different output matchers.
    async fn execute_and_cache_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<(Vec<ResultArtifact>, Vec<ArtifactMeta>)> {
        let addr = opts.def.target.addr.clone();
        let hashin = opts.hashin.clone();
        let spec = opts.spec.clone();
        let def = opts.def.clone();
        let use_tmp_cache = !opts.def.target.cache.enabled || opts.shell;
        let interactive = opts.interactive.clone();
        let shell = opts.shell;
        let key = (AddrKey(addr.clone()), hashin.clone());

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    // Approval gate: an `approval`-required target pauses here for
                    // an explicit user decision (interactive Y/N, stdin prompt, or
                    // `--auto-approve`). Single-flighted by this memoizer cell, so
                    // a target prompts at most once per request. Runs before the
                    // execute semaphore is acquired, so a waiting prompt holds no
                    // worker permit.
                    engine
                        .gate_approval(&rs, &spec, &def)
                        .await
                        .with_context(|| format!("approval {addr}"))?;
                    hcore::hmemoizer::set_phase("execute_cache:engine_execute");
                    let (artifacts, sandbox_teardown, sandbox_guards) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin, interactive, shell)
                        .await
                        .with_context(|| format!("execute {addr}"))?;

                    let artifacts_meta = match artifacts
                        .iter()
                        .filter(|a| matches!(
                            a.r#type,
                            outputartifact::Type::Output | outputartifact::Type::SupportFile
                        ))
                        .map(|a| Ok(ArtifactMeta { hashout: a.hashout()? }))
                        .collect::<anyhow::Result<Vec<_>>>()
                        .with_context(|| format!("read artifact metas for {addr}"))
                    {
                        Ok(metas) => metas,
                        Err(err) => {
                            // A target failure, like any `Err` leaving the
                            // execute: the sandbox stays on disk for the
                            // failure diagnostic (which reads it lazily) and
                            // for a post-mortem of the artifacts it failed to
                            // hash. A bare `?` here would fall into the drop
                            // path, which reclaims — that path must mean
                            // cancellation only.
                            sandbox_teardown.leave_for_diagnostics();
                            return Err(err);
                        }
                    };

                    hcore::hmemoizer::set_phase("execute_cache:cache_locally");

                    // Partition produced outputs. A zero-copy passthrough (an
                    // uncached source file flagged by its producer — e.g.
                    // `@heph/fs:file`) skips the local cache entirely and is
                    // carried as its raw `OutputArtifact`, so the cache-write
                    // hot path does no file read, tar, or copy. Everything else
                    // is packed/stored by `cache_locally`. Order is irrelevant —
                    // `build_eresult` filters by (group, type), not position —
                    // so the two sets are simply concatenated below.
                    let mut passthrough: Vec<ResultArtifact> = Vec::new();
                    let mut to_cache: Vec<outputartifact::OutputArtifact> = Vec::new();
                    for artifact in artifacts {
                        if is_passthrough(use_tmp_cache, &artifact.content) {
                            passthrough.push(ResultArtifact::passthrough(artifact));
                        } else {
                            to_cache.push(artifact);
                        }
                    }

                    // Run `cache_locally` (and emit a LocalCacheWrite span)
                    // whenever the target is genuinely cacheable — even with
                    // zero outputs. Writing an empty manifest is what lets a
                    // no-output gate/check target (e.g. `go_lint_gate`,
                    // `go_format_check`) register a cache HIT on re-run; the
                    // read side already treats an empty-artifact manifest as a
                    // hit. Skip only the tmp/all-passthrough path
                    // (`use_tmp_cache`), which persists nothing.
                    let cached = if to_cache.is_empty() && use_tmp_cache {
                        Ok(Vec::new())
                    } else {
                        let write_addr = addr.format();
                        crate::engine::event::emit_scope(
                            &rs,
                            crate::engine::event::BuildEventKind::LocalCacheWriteStart {
                                addr: write_addr.clone(),
                            },
                            move |error| {
                                crate::engine::event::BuildEventKind::LocalCacheWriteEnd {
                                    addr: write_addr,
                                    error: error.map(crate::engine::event::ErrorDetail::into_message),
                                }
                            },
                            engine.cache_locally(
                                rs.ctoken(),
                                &addr,
                                &hashin,
                                to_cache,
                                use_tmp_cache,
                            ),
                        )
                        .await
                    };

                    let out = cached
                        .map(move |cached| {
                            let mut produced = passthrough;
                            produced
                                .extend(cached.into_iter().map(ResultArtifact::from_cache));
                            (produced, artifacts_meta)
                        })
                        .with_context(|| format!("cache_locally {addr}"));

                    // Remote push: fire-and-forget on a background task (tracked
                    // by `bg_pending`, so the CLI/TUI stays open until it drains).
                    // Cacheable revisions only — tmp/uncacheable are never shared.
                    // `remote_enabled` gates it too: a target whose output embeds
                    // host-local paths (nix wrappers) must never be uploaded, or
                    // another machine pulls a wrapper pointing at a store path it
                    // doesn't have.
                    if out.is_ok() && !use_tmp_cache && def.target.cache.remote_enabled {
                        engine.spawn_remote_upload(&rs, addr.clone(), hashin.clone());
                    }

                    // Post-write GC: record that this target's stale revisions
                    // are due a trim, skipping uncacheable/tmp entries which are
                    // ephemeral and would be dropped anyway.
                    //
                    // Recorded, not run: the trim needs the addr's write lock,
                    // and this request is holding a read on it — the riding read
                    // in `mem_locked_result`, plus a clone in every artifact
                    // handed out — until the request state drops. Running it
                    // here means its `try_write` can never succeed, which is
                    // exactly how `cache.history` came to be unenforced during a
                    // run. `RequestState::defer_trim` submits it once the guards
                    // are gone, onto the bookkeeping lane and still
                    // fire-and-forget.
                    if out.is_ok() && !use_tmp_cache {
                        rs.defer_trim(&addr, def.target.cache.history, hashin);
                    }

                    // Completion path of the sandbox teardown. The teardown is
                    // an RAII guard armed in `Engine::execute` when the path
                    // was claimed, and every exit resolves it exactly once,
                    // three ways:
                    //
                    // * Reaching this line enqueues the bridge-owned cleanup
                    //   closure, generation-guarded. That includes a *failed
                    //   cache write* (`out` is `Err` here after a successful
                    //   run) — deliberately, matching the old `defer!`: the
                    //   run itself succeeded, so there is no process log tail
                    //   riding on this sandbox's survival.
                    // * A *failing target* never gets here: the run error in
                    //   `Engine::execute` and the `artifacts_meta` error above
                    //   both resolve as `leave_for_diagnostics`, keeping the
                    //   sandbox (and its log) on disk for the lazily-rendered
                    //   failure diagnostic, exactly as before teardown
                    //   ownership existed.
                    // * A bare drop — cancellation or an unwind, and only
                    //   those — enqueues a generation-checked reclaim.
                    //
                    // The old `defer!` here also enqueued at drop time in this
                    // window, but *unguarded*: with cancellation now routine
                    // (cancel-on-abandonment), that handed the cleaner a
                    // `remove_dir_all` at an arbitrarily later time, racing the
                    // next execute of the same addr as it recreates this very
                    // directory. The generation check is what makes drop-time
                    // teardown safe in every ordering.
                    //
                    // Runs after `cache_locally`, which reads from the sandbox
                    // (`project_sandbox_cleanup_ordering`). SlotGuards drop
                    // first, having lived across that read; the bridge closure
                    // knows whether to rm the plain dir or the FUSE upper.
                    drop(sandbox_guards);
                    sandbox_teardown.complete(format!("{addr}"));

                    hcore::hmemoizer::clear_phase();
                    out
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Refuse to write an in_place target's tree back when the sources moved
    /// under it.
    ///
    /// An in_place target (a formatter, a lint fixer) rewrites the very files it
    /// took as inputs, and the rewrite is derived from the bytes those inputs
    /// held when the run hashed them. Everything between that hash and this
    /// write-back — resolving deps, staging, executing, reading the cache — is a
    /// window in which the tree can move: an editor save, a `git checkout`, a
    /// concurrent run, a *sibling* in_place target in this very run.
    /// `materialize_codegen` writes unconditionally, so without this check the
    /// newer bytes are silently replaced by (stale bytes + transform), with no
    /// error and no diff.
    ///
    /// # What is checked, and why it is this and not the whole `hashin`
    ///
    /// The contract is "the files this target is about to overwrite still hold
    /// the bytes it hashed". That is a question about *this target's in_place
    /// output paths*, and it is answered by re-reading exactly the `@heph/fs`
    /// inputs covering them — `O(files written)`, not `O(transitive closure)`.
    ///
    /// It used to be answered by recomputing the whole `hashin` under a fresh
    /// request. That worked, and it cost the entire graph: `hashin` folds every
    /// dep's hashout, so reproducing it re-resolved every spec, def, package walk
    /// and provider probe beneath the target, with all twelve request memoizers
    /// empty. On a fully cached `run lint //go/large/...` over 2000 Go packages
    /// that was 1831 such requests re-resolving 26,768 nodes each, and it took
    /// the run from 8.8s to 45.5s — for a question about one file per target.
    ///
    /// The narrowed check is **more** sensitive where it applies, not less: a
    /// change in a watched file now trips it directly, where before it had to
    /// survive being folded into a digest alongside every other input, and a
    /// def-level change could in principle compensate it back to the same value.
    ///
    /// It is **less** sensitive in one deliberate way: a *dependency's* hashout
    /// moving mid-run no longer fails the write-back. That is the intended trade
    /// — the target is not about to overwrite its dependency, so a moved dep is
    /// a stale *output*, which the next run recomputes, and not the data loss
    /// this guard exists to prevent. Only the files being overwritten are
    /// irreplaceable.
    ///
    /// Gated exactly like the write-back it guards: top-level only (nothing else
    /// writes), in_place only (nothing else overwrites its own inputs), and never
    /// on a frozen run (which writes nothing at all).
    ///
    /// A failure to re-read is *not* waved through: not being able to confirm the
    /// tree is precisely the case where overwriting it is unsafe.
    async fn check_in_place_inputs_unchanged(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<()> {
        if opts.frozen || !opts.is_top {
            return Ok(());
        }
        // The `@heph/fs` addrs covering the paths about to be overwritten. Same
        // path→addr mapping `expand::synthesized_fs_inputs` uses, so a declared
        // in_place output and the input that reads it land on the same addr.
        let guarded = in_place_fs_addrs(&opts.def.target.outputs);
        if guarded.is_empty() {
            return Ok(());
        }

        let addr = &opts.def.target.addr;
        // Memoized on this request — the run already expanded these.
        let inputs = Arc::clone(&self)
            .expanded_inputs_for(rs.clone(), addr)
            .await
            .with_context(|| {
                format!("listing the inputs of {addr} to guard its in-place write-back")
            })?;
        // Only inputs that are *hashed* and that read a path being overwritten.
        // An in_place output the target never read is not something it
        // transformed, so there is no stale-transform hazard to guard — writing
        // it is the declared behaviour.
        let watched: Vec<Addr> = inputs
            .iter()
            .filter(|i| i.hashed && guarded.contains(&i.r#ref.r#ref))
            .map(|i| i.r#ref.r#ref.clone())
            .collect();
        if watched.is_empty() {
            return Ok(());
        }

        // One fresh request for the whole check. It exists only so the `@heph/fs`
        // reads below re-stat the tree instead of answering from this run's
        // snapshot: `cached_glob_walk` is keyed by `request_id`, and
        // `CachedWalker::file_hash` revalidates. Nothing else is resolved under
        // it, which is the entire difference from what this used to cost.
        let fresh = self.new_hash_only_state(addr.clone());
        for input_addr in watched {
            let before = Arc::clone(&self)
                .fs_input_hashout(rs.clone(), &input_addr)
                .await
                .with_context(|| format!("reading the hash {input_addr} had when {addr} ran"))?;
            let after = Arc::clone(&self)
                .fs_input_hashout(fresh.clone(), &input_addr)
                .await
                // Sealed for the same reason the whole-`hashin` recompute was:
                // the chain may carry request-property markers that
                // `classify_failure` refuses to record, which would leave a
                // non-zero exit with nothing printed.
                .map_err(|e| anyhow::anyhow!("{e:#}"))
                .with_context(|| {
                    format!(
                        "re-reading {input_addr} to confirm it still matches what {addr} \
                         transformed, before writing its in-place output back over it"
                    )
                })?;
            if before != after {
                // "changed while it ran" is asserted verbatim by
                // `e2e::codegen_in_place::in_place_refuses_to_write_back_over_a_changed_source`
                // — this is the sentence a user sees when their edit is what
                // stopped the write-back, and it is pinned on purpose.
                anyhow::bail!(
                    "{addr} rewrites {input_addr} in place, and it changed while it ran \
                     (hash {before} → {after}). Its output was computed from the older bytes, \
                     so writing it back would discard the newer ones — nothing was written. \
                     Re-run once the tree is settled."
                );
            }
        }
        Ok(())
    }

    /// The combined hashout of an `@heph/fs` input, as this request sees it.
    ///
    /// Sorted and joined rather than compared as a set: a file addr yields one
    /// artifact and a glob addr yields one per matched file, and for a glob the
    /// *set* of files is as much a change as any file's bytes.
    async fn fs_input_hashout(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<String> {
        let res = self
            .result_addr(rs, addr, OutputMatcher::None, &ResultOptions::default())
            .await?;
        let mut hashouts: Vec<&str> = res
            .artifacts_meta
            .iter()
            .map(|m| m.hashout.as_str())
            .collect();
        hashouts.sort_unstable();
        Ok(hashouts.join(","))
    }

    /// Register the just-executed in_place target's cache entry under the key a
    /// subsequent run will compute, so re-running an idempotent transform on the
    /// already-transformed tree is a no-op cache hit.
    ///
    /// Called only on the execute path, *after* `materialize_codegen` has written
    /// the transformed files back to the tree. We recompute the target's `hashin`
    /// against that post-write-back state using a *fresh* request (the `@heph/fs`
    /// inputs are cache-off and memoized per request, so a new request re-reads
    /// the just-written files and yields exactly the `hashin` the next run will
    /// see), then duplicate this run's primary cache revision under it.
    ///
    /// This is faithful to how `@heph/fs` actually hashes inputs (content +
    /// exec-bit): the key is derived from the real tree, not from output content
    /// hashes that the next run would never reproduce. Because the hash tracks
    /// content (and the write-back leaves an already-correct file byte-for-byte
    /// identical), re-reading the post-write-back tree reproduces the same key,
    /// so the hit is stable across repeated runs. Best-effort: any failure leaves
    /// the primary entry intact and the next run simply re-executes.
    async fn maybe_store_fixpoint(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<()> {
        use crate::engine::driver::targetdef::path::CodegenMode;

        // Only top-level targets write their tree back, and only in_place targets
        // mutate their own inputs (so only they can reach a fixpoint). Frozen runs
        // never write, hence never cache a fixpoint.
        if opts.frozen || !opts.is_top {
            return Ok(());
        }
        // The fresh request below runs on its own cancellation token (unlinked
        // from this build), so refuse to start once the original build is
        // cancelled — the fixpoint is a pure optimization and must never delay a
        // Ctrl-C teardown.
        if rs.ctoken().is_cancelled() {
            return Ok(());
        }
        let is_in_place = opts.def.target.outputs.iter().any(|o| {
            o.paths
                .iter()
                .any(|p| matches!(p.codegen_tree, CodegenMode::InPlace))
        });
        if !is_in_place {
            return Ok(());
        }

        let addr = opts.def.target.addr.clone();
        // Fresh request → fs inputs re-stat the post-write-back tree, yielding the
        // exact hashin the NEXT run will compute.
        let fresh = self.new_hash_only_state(addr.clone());
        let fixpoint = match Arc::clone(&self).meta(fresh, &addr).await {
            Ok(m) => m.hashin,
            Err(e) => {
                // Optimization only: the primary entry is already cached.
                //
                // `HashUnknownError` lands here whenever a cacheable dep is not
                // cached at its post-write-back inputs — which is exactly the
                // shape whose fixpoint would be unsound anyway: the recomputed
                // key folds that dep's NEW hashout, while the artifacts being
                // filed under it were produced from the old one. Skipping is
                // both the safe answer and the correct one.
                tracing::debug!(error = %format!("{e:#}"), %addr, "fixpoint: meta recompute failed");
                return Ok(());
            }
        };
        if fixpoint == *opts.hashin {
            // Tree already at the fixpoint (idempotent run changed nothing).
            return Ok(());
        }
        // Blob copy is synchronous IO — run it off the async poll like every
        // other cache write (see `cache_artifact_locally`).
        let primary = opts.hashin.clone();
        let dup = hcore::blocking::run(enclose!(
            (self => engine, addr, fixpoint, primary) move || {
                engine.duplicate_cache_revision(&addr, &primary, &fixpoint)
            }
        ))
        .await;
        if let Err(e) = dup {
            // Best-effort: the primary cache entry is already written, so a
            // failure here just means the next run re-executes instead of
            // hitting the fixpoint. Never fail a successful build over it.
            tracing::debug!(error = %format!("{e:#}"), %addr, "fixpoint: duplicate cache revision failed");
        }
        Ok(())
    }

    /// Probe the **local** manifest for `(addr, hashin)` and decide whether it is
    /// a usable hit, emitting `LocalCacheHit` on a hit. Returns the parsed
    /// manifest so the per-caller output read in
    /// [`execute_and_cache`](Self::execute_and_cache) reuses it instead of
    /// re-reading + re-deserializing it from the cache backend.
    ///
    /// A miss emits **nothing** here: this runs twice per cold target (the
    /// optimistic probe under the read lock, then the re-check under the write
    /// lock), so emitting the miss inside the probe double-counted every cold
    /// target in the hit/miss stats. The caller emits `LocalCacheMiss` exactly
    /// once, at the settled miss under the write lock — see
    /// [`resolve_locked_inner`](Self::resolve_locked_inner). Only one probe can
    /// hit (a hit returns before the second probe runs), so the hit emission
    /// stays here.
    ///
    /// Neither event is emitted for a hash-only request. Those are nested
    /// recomputes — the in_place write-back guard re-deriving a dep's `hashout`
    /// (see [`Self::meta`]) — not resolutions a user asked for, and the same
    /// addr is probed again by the real request that wraps them. They carry no
    /// event sender, so the TUI and CI summaries never saw them either way, but
    /// hooks (telemetry, the GHA summary) are dispatched regardless of the
    /// sender and were counting each nested probe as another hit.
    ///
    /// A local manifest is not proof of residency: a revision whose manifest was
    /// mirrored from a remote (see
    /// [`probe_remote_revision`](Self::probe_remote_revision)) names blobs that
    /// were never downloaded, because nothing has needed them yet. The probe does
    /// **not** try to settle that here — it costs a manifest GET plus a prefix
    /// LIST, per addr, per run, under the per-addr lock, and on an interior node
    /// of a remote-served graph it proves the obtainability of bytes no caller
    /// will ever open. Residency is settled per caller, over the blobs that caller
    /// actually reads, in [`execute_and_cache`](Self::execute_and_cache):
    ///
    /// - **everything it needs is local** → served from the local cache, no
    ///   network at all. A hashout-only caller is always in this case: it needs no
    ///   blob (see [`needed_artifacts`](Self::needed_artifacts)).
    /// - **something is missing** → force [`LockedResolution::remote`] and pull
    ///   just those blobs.
    /// - **missing, and the remote answers "absent"** (no readable remote, remote
    ///   opted out for this target, the revision is gone, or a transport error
    ///   that `fetch_manifest`/`fetch_blob` reports as absence) → the read returns
    ///   "unavailable" and the target is rebuilt, fail-soft, exactly as a plain
    ///   miss would have been — with the freshly-produced `hashout`s reconciled
    ///   against this manifest's, so a dependent that already folded the cached
    ///   ones can never end up carrying a key for bytes that no longer exist.
    ///
    /// Only *absence* is fail-soft. A transfer that starts and then errors, or a
    /// temp-dir failure, propagates out of `pull_missing_blobs` and fails the run;
    /// it is not turned into a rebuild.
    async fn probe_cache_manifest(
        &self,
        rs: &Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<Option<Arc<Manifest>>> {
        let addr = &def.target.addr;
        let hashin = opts.hashin.as_str();
        let hit = self
            .read_manifest_blocking(rs.ctoken(), addr, hashin)
            .await?
            .map(Arc::new);
        if hit.is_some() && !rs.hash_only() {
            rs.emit(crate::engine::event::BuildEventKind::LocalCacheHit {
                addr: addr.format(),
            });
        }
        Ok(hit)
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
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
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
                AddrKey(addr.clone()),
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
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
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
        let mut def = rewrite_query_inputs(res.target_def, addr, &spec.provider);
        // Expand the engine-baked `//@heph/introspect:outputs` magic input into
        // one `@heph/fs` input per declared output path of this target. Done
        // here (the single seam where `def.inputs` is finalized) so transitive
        // collection below and every downstream consumer — hashing and
        // execution alike — see the synthesized fs inputs and never the magic
        // addr, which no provider serves.
        crate::engine::expand::expand_introspect_inputs(&mut def);

        let all_transitive = if apply_transitive {
            let sb = Arc::clone(&self)
                .collect_transitive_deps(rs.clone(), &def.inputs)
                .await?;

            if sb.empty() { None } else { Some(sb) }
        } else {
            None
        };

        let mut def = match &all_transitive {
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

        // In-place codegen persists TWO cache revisions per logical input state:
        // the primary (keyed over the pre-transform tree) and the fixpoint (keyed
        // over the post-write-back tree). Double the GC history so `cache.history`
        // still retains that many input *states*, not half as many. Applied here,
        // on the final def, so BOTH GC paths that read `def.cache.history` — the
        // post-write trim and the `heph gc` sweep — inherit it. The exec hash does
        // not include `cache`, so this never invalidates existing cache entries.
        if def.cache.history > 0
            && def.outputs.iter().any(|o| {
                o.paths.iter().any(|p| {
                    matches!(
                        p.codegen_tree,
                        crate::engine::driver::targetdef::path::CodegenMode::InPlace
                    )
                })
            })
        {
            def.cache.history = def.cache.history.saturating_mul(2);
        }

        if def.hash.is_empty() {
            anyhow::bail!("missing hash");
        }

        // Validate approval notices against the finalized input set at definition
        // time — before any result resolution or execution — so a notice naming a
        // non-existent input group fails fast and identically on every path.
        Self::validate_approval(&spec, addr, def.inputs.iter().map(|i| i.origin_id.as_str()))
            .with_context(|| "approval")?;

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
                    let transitive = if spec.driver == hbuiltins::plugingroup::DRIVER_NAME {
                        let dep_def = Arc::clone(&engine)
                            .get_def(rs.clone(), &input_ref.r#ref)
                            .await
                            .with_context(|| format!("get def for group: {:?}", input_ref))?;
                        dep_def.applied_transitive.clone()
                    } else if spec.transitive.empty() {
                        // Nothing to contribute. Checked before the clone because
                        // `transitive` is an opt-in BUILD-file feature
                        // (`target(..., transitive = {...})`) that most targets never
                        // use — every Go target builds its spec with an empty one — so
                        // this is the common case on every dependency edge in the
                        // graph, which is the largest per-run count in the engine.
                        // Merging an empty sandbox is a no-op, so skipping it here
                        // reaches the same `sb` while dropping the clone and, below,
                        // the `id` that only a merge would ever have read.
                        None
                    } else {
                        Some(spec.transitive.clone())
                    };

                    if let Some(transitive) = transitive {
                        // `hash_str` digests the whole addr and formats it, so build
                        // the id only once something will actually merge under it.
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
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
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
                AddrKey(addr.clone()),
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
        // A provider whose `get()` cycles doesn't preclude a later provider
        // resolving the same addr acyclically (e.g. the go provider over-claims a
        // buildfile codegen target in a Go package dir and drags `go list` into a
        // cycle; the buildfile provider resolves it cleanly). Skip the cyclic
        // provider and keep going; surface the cycle only if NO provider succeeds
        // — never deadlock, never silently drop a resolvable target.
        let mut pending_cycle: Option<anyhow::Error> = None;
        for provider in self.providers.iter() {
            let provider_rs = rs.with_skip_provider(&provider.name);
            let executor: Arc<dyn ProviderExecutor> = Arc::new(EngineProviderExecutor::new(
                Arc::downgrade(&self),
                provider_rs,
            ));

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
                Err(GetError::Other(e)) => {
                    if downcast_chain_ref::<CycleError>(&e).is_some() {
                        pending_cycle = Some(e);
                        continue;
                    }
                    // Attach the target so the failure is traceable even in
                    // non-tui output, where the addr isn't otherwise shown. The
                    // context wraps but preserves the chain, so downstream typed
                    // downcasts still work.
                    return Err(e.context(format!("resolving target `{addr}`")));
                }
            };

            return anyhow::Ok(Arc::new(EngineTargetSpec {
                spec: Arc::new(spec),
                provider: provider.name.clone(),
            }));
        }

        // No provider produced a spec. If one cycled, that's the meaningful
        // failure (hard fail, loud); otherwise the addr is genuinely unknown.
        if let Some(e) = pending_cycle {
            return Err(e);
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
        if r.package.as_str() == hplugin_query::pluginquery::PACKAGE {
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
    use futures::future::BoxFuture;
    use hcore::hasync::{Cancellable, StdCancellationToken};
    use hmodel::htmatcher::Matcher;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::sync::Arc as SArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    fn file_output(
        source_path: &str,
        out_path: &str,
        passthrough: bool,
        hashout: &str,
    ) -> outputartifact::OutputArtifact {
        outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: "f".to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::File(outputartifact::ContentFile {
                source_path: source_path.to_string(),
                out_path: out_path.to_string(),
                x: false,
                passthrough,
            }),
            hashout: hashout.to_string(),
        }
    }

    /// Passthrough is gated two ways: only on the uncached (`tmp`) path, and only
    /// for a producer-flagged `Content::File`. A cacheable revision, an unflagged
    /// file, or any non-file content is never a passthrough — it must be packed.
    #[test]
    fn is_passthrough_gates_on_tmp_and_producer_flag() {
        let flagged = file_output("/ws/go.mod", "go.mod", true, "h").content;
        let unflagged = file_output("/ws/go.mod", "go.mod", false, "h").content;
        let raw = outputartifact::Content::Raw(outputartifact::ContentRaw {
            data: vec![1, 2, 3],
            path: "x".to_string(),
            x: false,
        });

        assert!(is_passthrough(true, &flagged), "tmp + flagged file");
        assert!(!is_passthrough(false, &flagged), "cacheable must pack");
        assert!(
            !is_passthrough(true, &unflagged),
            "unflagged file must pack"
        );
        assert!(!is_passthrough(true, &raw), "non-file must pack");
    }

    /// A passthrough `ResultArtifact` is the raw `OutputArtifact` as `Content`: it
    /// never becomes a `CacheArtifact`, carries no cache blob, and `walk()` yields
    /// the single source file at its `out_path`, read directly from the durable
    /// `source_path`. `seekable_reader`/`file_path` stay `None` so the FUSE
    /// tar-index path is bypassed and consumers materialize via generic unpack.
    #[tokio::test]
    async fn passthrough_result_artifact_reads_source_without_cache() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("go.mod");
        std::fs::write(&source_path, b"module example\n").expect("write");

        let hashout = hwalk::file_hashout(&source_path, false).expect("hash");
        let oa = file_output(
            source_path.to_str().expect("utf8"),
            "mgmt/go/go.mod",
            true,
            &hashout,
        );
        let ra = ResultArtifact::passthrough(oa);

        assert_eq!(ra.group, "out");
        assert_eq!(ra.r#type, ManifestArtifactType::Output);
        // No cache backing: a passthrough exposes neither a seekable tar nor a
        // cache file path.
        assert!(ra.content.seekable_reader().expect("seekable").is_none());
        assert!(ra.content.file_path().is_none());

        let mut walk = ra.content.walk().expect("walk");
        let entry = walk.next().expect("one entry").expect("ok");
        assert!(walk.next().is_none(), "single file");
        assert_eq!(entry.path, std::path::PathBuf::from("mgmt/go/go.mod"));
        let WalkEntryKind::File { mut data, .. } = entry.kind else {
            panic!("expected file entry");
        };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut data, &mut buf).expect("read");
        assert_eq!(buf, b"module example\n");
    }

    /// A passthrough file is referenced by path and read live, never snapshotted
    /// into the cache. If the workspace file is modified between hashing and
    /// consume, its content no longer matches the recorded `hashout` — which is
    /// folded into the cache key — so reading it to EOF must fail explicitly
    /// rather than silently feed divergent bytes into a downstream cache entry.
    #[tokio::test]
    async fn passthrough_read_fails_when_source_modified_after_hashing() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("go.mod");
        std::fs::write(&source_path, b"module example\n").expect("write");

        // Hash recorded at input-hashing time.
        let hashout = hwalk::file_hashout(&source_path, false).expect("hash");

        // File mutated after hashing, before the consumer reads it.
        std::fs::write(&source_path, b"module tampered\n").expect("rewrite");

        let oa = file_output(
            source_path.to_str().expect("utf8"),
            "mgmt/go/go.mod",
            true,
            &hashout,
        );
        let ra = ResultArtifact::passthrough(oa);

        let mut walk = ra.content.walk().expect("walk");
        let entry = walk.next().expect("one entry").expect("ok");
        let WalkEntryKind::File { mut data, .. } = entry.kind else {
            panic!("expected file entry");
        };
        let mut buf = Vec::new();
        let err = std::io::Read::read_to_end(&mut data, &mut buf)
            .expect_err("modified source must fail the read");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let msg = err.to_string();
        assert!(msg.contains("modified after it was hashed"), "msg: {msg}");
        assert!(msg.contains("go.mod"), "msg names the file: {msg}");
    }

    /// The verifying reader must compute byte-for-byte the same digest as
    /// [`hwalk::file_hashout`] — they are two implementations of one hash and
    /// would silently break passthrough verification if they drifted. Reading an
    /// unmodified file through the reader (with the canonical hash as `expected`)
    /// succeeds; with any other `expected` it fails.
    #[tokio::test]
    async fn passthrough_reader_matches_file_hashout() {
        let dir = tempdir().expect("tempdir");
        let source_path = dir.path().join("blob.bin");
        // Larger than the reader's chunking to exercise multiple `update`s.
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source_path, &bytes).expect("write");

        let canonical = hwalk::file_hashout(&source_path, false).expect("hash");

        let pc = |expected: &str| PassthroughContent {
            source_path: source_path.to_str().expect("utf8").to_string(),
            out_path: "blob.bin".to_string(),
            x: false,
            expected: expected.to_string(),
        };

        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut pc(&canonical).reader().expect("reader"), &mut buf)
            .expect("read");
        assert_eq!(buf, bytes, "bytes pass through unchanged");

        // Same content, wrong expected → the reader rejects at EOF.
        let mut sink = Vec::new();
        std::io::Read::read_to_end(
            &mut pc(&format!("{canonical}0")).reader().expect("reader"),
            &mut sink,
        )
        .expect_err("wrong expected hash must fail");
    }

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

    /// A [`Content`] whose fast paths are answerable and *differ* from what the
    /// trait defaults would produce: `entry_paths` reports a name `walk` never
    /// yields, and `file_path` names a real file whose bytes `reader` refuses to
    /// serve. Standing in for a `CacheArtifact`, whose overrides are exactly
    /// these two.
    struct FastPathContent {
        path: std::path::PathBuf,
    }

    impl Content for FastPathContent {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            anyhow::bail!("must be read through file_path, not the stream")
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(std::iter::once(Ok(WalkEntry {
                path: std::path::PathBuf::from("from-walk"),
                kind: WalkEntryKind::Symlink {
                    target: std::path::PathBuf::from("t"),
                },
            }))))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("fast".to_string())
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
            Ok(vec![std::path::PathBuf::from("from-index")])
        }
        fn file_path(&self) -> Option<std::path::PathBuf> {
            Some(self.path.clone())
        }
    }

    /// `GuardedArtifact` wraps *every* cacheable result artifact, so a `Content`
    /// method it leaves to the trait default is that method's fast path switched
    /// off product-wide — the inner artifact's implementation unreachable, and
    /// nothing failing to show it. Both defaults are silent: `file_path` reports
    /// "not a file" for an on-disk cache blob (the stable-ABI seam then streams
    /// it 64 KiB at a time instead of opening it), and `entry_paths` falls back
    /// to a `walk` that reads every byte instead of the tar header scan.
    ///
    /// Asserted through `&dyn Content`, which is how every consumer sees it.
    #[tokio::test]
    async fn guarded_artifact_forwards_the_direct_open_fast_paths() {
        let dir = tempdir().expect("tempdir");
        let lock = SArc::new(ResultLock::new(LockBackend::Mem, dir.path().to_path_buf()));
        let addr = Addr::new(PkgBuf::from("pkg"), "x".to_string(), BTreeMap::new());
        let read = lock
            .read(&addr, &StdCancellationToken::new())
            .await
            .expect("read");

        let blob = dir.path().join("blob.tar");
        std::fs::write(&blob, b"artifact bytes").expect("write blob");

        let guarded: Arc<dyn Content> = Arc::new(GuardedArtifact {
            inner: Arc::new(FastPathContent { path: blob.clone() }),
            _lock: SArc::new(read),
        });

        // The direct-open path reaches the consumer: it gets the cache file, not
        // `None`, and the bytes are readable without going near `reader()`.
        let path = guarded
            .file_path()
            .expect("the wrapper must not hide the artifact's on-disk path");
        assert_eq!(path, blob);
        assert_eq!(std::fs::read(&path).expect("read"), b"artifact bytes");

        // The header-scan enumeration reaches the consumer too — the walk-based
        // default would answer `from-walk`.
        assert_eq!(
            guarded.entry_paths().expect("entry_paths"),
            vec![std::path::PathBuf::from("from-index")],
            "must use the inner artifact's index, not the byte-reading walk"
        );
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

    /// A provider over a fixed package set whose `probe` is slow, with a
    /// per-package delay the test picks. Tracks peak in-flight probes.
    struct SlowProbe {
        pkgs: Vec<String>,
        delay: SArc<dyn Fn(usize) -> Duration + Send + Sync>,
        inflight: SArc<AtomicUsize>,
        peak: SArc<AtomicUsize>,
    }

    impl crate::engine::provider::Provider for SlowProbe {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "slowprobe".to_string(),
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
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            let items: Vec<anyhow::Result<ListPackageResponse>> = self
                .pkgs
                .iter()
                .map(|p| {
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from(p.as_str()),
                    })
                })
                .collect();
            Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
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
            req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            let pkg = req.package.clone();
            // Packages this provider does not own (the always-on built-in
            // `@heph/fs`) cost nothing and declare nothing.
            let Some(idx) = self.pkgs.iter().position(|p| p == pkg.as_str()) else {
                return Box::pin(async { Ok(ProbeResponse { states: vec![] }) });
            };
            let d = (self.delay)(idx);
            let (inflight, peak) = (SArc::clone(&self.inflight), SArc::clone(&self.peak));
            Box::pin(async move {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(d).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                Ok(ProbeResponse {
                    states: vec![State {
                        package: pkg,
                        provider: "slowprobe".to_string(),
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    fn slow_probe_engine(
        pkgs: Vec<String>,
        delay: impl Fn(usize) -> Duration + Send + Sync + 'static,
        inflight: SArc<AtomicUsize>,
        peak: SArc<AtomicUsize>,
    ) -> anyhow::Result<(SArc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let delay: SArc<dyn Fn(usize) -> Duration + Send + Sync> = SArc::new(delay);
        engine.register_provider(move |_| {
            Box::new(SlowProbe {
                pkgs: pkgs.clone(),
                delay: SArc::clone(&delay),
                inflight: SArc::clone(&inflight),
                peak: SArc::clone(&peak),
            })
        })?;
        Ok((SArc::new(engine), root))
    }

    fn states_under_of(engine: &SArc<Engine>, rs: &SArc<RequestState>) -> EngineProviderExecutor {
        EngineProviderExecutor::new(SArc::downgrade(engine), SArc::clone(rs))
    }

    /// `states_under`'s output order is a build input, not a display detail:
    /// plugin-go feeds it straight into `variant::build_universe`, which walks it
    /// in order to build the module variant universe, which fixes the order its
    /// `list` emits library addrs in — and that reaches a def hash.
    ///
    /// So the fan-out must be `buffered`, never `buffer_unordered`. The delays
    /// here make the probes complete in the exact reverse of listing order, so a
    /// `buffer_unordered` implementation returns the reverse sequence.
    #[tokio::test]
    async fn states_under_returns_states_in_package_order_not_completion_order()
    -> anyhow::Result<()> {
        const N: usize = 8;
        let pkgs: Vec<String> = (0..N).map(|i| format!("p{i:02}")).collect();
        let (engine, _root) = slow_probe_engine(
            pkgs.clone(),
            // First package probed sleeps longest, last sleeps least.
            |i| Duration::from_millis(((N - i) * 15) as u64),
            SArc::new(AtomicUsize::new(0)),
            SArc::new(AtomicUsize::new(0)),
        )?;
        let rs = engine.new_state();

        let states = states_under_of(&engine, &rs)
            .states_under(&PkgBuf::from(""))
            .await?;

        let got: Vec<String> = states
            .iter()
            .filter(|s| s.provider == "slowprobe")
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(
            got, pkgs,
            "states must accumulate in package-listing order even though the \
             probes complete in the reverse order"
        );
        Ok(())
    }

    /// The same walk, overlapped: `for pkg { for provider { probe.await } }` paid
    /// the sum of every probe. plugin-go calls this for a whole module root
    /// before it can emit a single addr, so it sits on the critical path of a
    /// Go workspace's discovery.
    /// `states_under(prefix)` means "the packages at or under `prefix`" — and a
    /// probe is not cheap: `pluginbuildfile::probe` is `run_pkg`, a whole-package
    /// Starlark evaluation holding a `PKG_EVAL_SLOTS` permit.
    ///
    /// It used to probe the *whole workspace* for any prefix. `list_packages`'s
    /// prefix is only a hint — the buildfile provider ignores it, as the fake
    /// here does — and this walk, unlike `Engine::query`, never re-pruned what
    /// came back. plugin-go calls it once per first-party library package with
    /// the module root, so a `heph run <label> //some/dir/...` in a workspace
    /// with a root-level module paid a full-workspace evaluation per library.
    /// `Engine::packages` now applies the matcher itself, which is what bounds
    /// this to the subtree.
    #[tokio::test]
    async fn states_under_probes_only_packages_under_the_prefix() -> anyhow::Result<()> {
        /// Records every package it is asked to probe; ignores the list prefix.
        struct RecordingProbe {
            pkgs: Vec<String>,
            probed: SArc<std::sync::Mutex<Vec<String>>>,
        }

        impl crate::engine::provider::Provider for RecordingProbe {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "recording".to_string(),
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
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                let items: Vec<anyhow::Result<ListPackageResponse>> = self
                    .pkgs
                    .iter()
                    .map(|p| {
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from(p.as_str()),
                        })
                    })
                    .collect();
                Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
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
                req: ProbeRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
                let pkg = req.package.clone();
                self.probed
                    .lock()
                    .expect("probed")
                    .push(pkg.as_str().to_string());
                Box::pin(async move {
                    Ok(ProbeResponse {
                        states: vec![State {
                            package: pkg,
                            provider: "recording".to_string(),
                            state: Default::default(),
                        }],
                    })
                })
            }
        }

        let probed = SArc::new(std::sync::Mutex::new(Vec::new()));
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let pkgs: Vec<String> = ["bar", "foo", "foo/deep", "foobar", "unrelated"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        engine.register_provider(enclose!((probed) move |_| Box::new(RecordingProbe {
            pkgs: pkgs.clone(),
            probed: SArc::clone(&probed),
        })))?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let states = states_under_of(&engine, &rs)
            .states_under(&PkgBuf::from("foo"))
            .await?;

        let mut got = probed.lock().expect("probed").clone();
        got.sort();
        // `foobar` is not under `foo` — the scope is a package prefix, not a
        // string one.
        assert_eq!(got, vec!["foo".to_string(), "foo/deep".to_string()]);
        let declared: Vec<&str> = states.iter().map(|s| s.package.as_str()).collect();
        assert_eq!(declared, vec!["foo", "foo/deep"]);
        Ok(())
    }

    /// A failed `states_under` walk must stop probing.
    ///
    /// `Buffered` refills its queue from the underlying iterator on *every*
    /// poll, so `while probes.next().await.is_some() {}` walks the entire
    /// remaining `(package, provider)` sequence rather than the K in flight.
    /// Without the `stop` flag each of those pulls runs a real probe — and
    /// `pluginbuildfile::probe` is `run_pkg`, a whole-package Starlark
    /// evaluation holding a `PKG_EVAL_SLOTS` permit. The observable difference
    /// is stark: the whole workspace gets evaluated *after* the walk has
    /// already failed.
    ///
    /// The drain itself stays — it finishes this request's in-flight probes
    /// before the error escapes, rather than tearing them down from inside the
    /// `mem_states_under` cell they run under. (Since #241 dropping them would
    /// not *leak*: an abandoned cell now evicts itself and drops its future. The
    /// drain is about ordering, not liveness.) What the flag buys is that the
    /// drain costs a cheap poll per remaining pair instead of a package
    /// evaluation per remaining pair.
    #[tokio::test]
    async fn failed_states_under_walk_stops_probing() -> anyhow::Result<()> {
        /// Counts real probes; the first package it is asked about fails.
        struct CountingFailProbe {
            pkgs: Vec<String>,
            probes: SArc<AtomicUsize>,
        }

        impl crate::engine::provider::Provider for CountingFailProbe {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "countingfail".to_string(),
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
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                let items: Vec<anyhow::Result<ListPackageResponse>> = self
                    .pkgs
                    .iter()
                    .map(|p| {
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from(p.as_str()),
                        })
                    })
                    .collect();
                Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
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
                req: ProbeRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
                let pkg = req.package.clone();
                let Some(idx) = self.pkgs.iter().position(|p| p == pkg.as_str()) else {
                    return Box::pin(async { Ok(ProbeResponse { states: vec![] }) });
                };
                let probes = SArc::clone(&self.probes);
                Box::pin(async move {
                    probes.fetch_add(1, Ordering::SeqCst);
                    // Yield so the fan-out actually fills before the first
                    // result is consumed — otherwise a single-threaded runtime
                    // could complete pair 0 before any sibling starts and the
                    // test would prove nothing about the drain.
                    tokio::task::yield_now().await;
                    if idx == 0 {
                        anyhow::bail!("probe blew up on package 0");
                    }
                    Ok(ProbeResponse {
                        states: vec![State {
                            package: pkg,
                            provider: "countingfail".to_string(),
                            state: Default::default(),
                        }],
                    })
                })
            }
        }

        // Large enough that "bounded" and "the whole workspace" cannot be
        // confused on any core count.
        const N: usize = 400;
        let pkgs: Vec<String> = (0..N).map(|i| format!("p{i:04}")).collect();
        let probes = SArc::new(AtomicUsize::new(0));
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(enclose!((probes) move |_| Box::new(CountingFailProbe {
            pkgs: pkgs.clone(),
            probes: SArc::clone(&probes),
        })))?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let err = states_under_of(&engine, &rs)
            .states_under(&PkgBuf::from(""))
            .await
            .expect_err("package 0's probe fails, so the walk must fail");
        assert!(
            format!("{err:#}").contains("blew up"),
            "expected the probe's error, got: {err:#}"
        );

        // Package 0 is the first pair submitted, so `buffered` surfaces its
        // error before anything later. Everything already in flight when that
        // lands still counts — the bound is K-ish, not 1 — but the ~N-K pairs
        // behind it must cost nothing.
        let ran = probes.load(Ordering::SeqCst);
        let k = crate::engine::fanout::discovery_concurrency();
        assert!(
            ran <= k * 4,
            "a failed walk must stop probing: ran {ran} probes over {N} packages \
             (K={k}). Unbounded would be ~{N} — the whole workspace evaluated \
             after the walk had already failed."
        );
        Ok(())
    }

    #[tokio::test]
    async fn states_under_overlaps_probes() -> anyhow::Result<()> {
        const N: usize = 8;
        let delay = Duration::from_millis(40);
        let pkgs: Vec<String> = (0..N).map(|i| format!("p{i:02}")).collect();
        let inflight = SArc::new(AtomicUsize::new(0));
        let peak = SArc::new(AtomicUsize::new(0));
        let (engine, _root) = slow_probe_engine(
            pkgs,
            move |_| delay,
            SArc::clone(&inflight),
            SArc::clone(&peak),
        )?;
        let rs = engine.new_state();

        let start = std::time::Instant::now();
        let states = states_under_of(&engine, &rs)
            .states_under(&PkgBuf::from(""))
            .await?;
        let elapsed = start.elapsed();

        assert_eq!(
            states.iter().filter(|s| s.provider == "slowprobe").count(),
            N
        );
        // A serial `for pkg { for provider { probe.await } }` peaks at exactly
        // one in-flight probe, on every machine.
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "probes must overlap; serial discovery peaks at 1 in-flight probe"
        );
        // The buffer interleaves this provider's slow probes with the built-in
        // `fs` provider's instant ones, so only about K/2 slow probes are live
        // at once. At K=2 — a 1-CPU cgroup, where `available_parallelism`
        // reports 1 — that is ~1, and no wall-clock bound can distinguish
        // overlapped from serial. Rather than assert something vacuous there,
        // the timing claim is made only where it means something; `peak > 1`
        // above is the machine-independent half and carries the test either way.
        let k = crate::engine::fanout::discovery_concurrency();
        if k >= 4 {
            assert!(
                elapsed < delay * (N as u32) * 3 / 4,
                "overlapped probing of {N} packages at K={k} must beat serial ({:?}), \
                 took {elapsed:?}",
                delay * (N as u32)
            );
        }
        Ok(())
    }

    #[test]
    fn provider_functions_lists_exposed_functions() {
        let root = tempdir().unwrap();
        let _rt = crate::engine::test_rt_enter();
        // `fs` is auto-registered by `Engine::new`.
        let engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .unwrap();
        let fns = engine.provider_functions();
        assert!(
            fns.iter().any(|(p, n, sig)| p == "fs"
                && n == "glob"
                && sig == "glob(pattern: string) -> list[string]"),
            "{fns:?}"
        );
    }

    // Repro: `//...` (PackagePrefix("")) must surface targets in the ROOT
    // package's BUILD file, not just nested packages.
    #[tokio::test]
    async fn query_recursive_includes_root_build_file() -> anyhow::Result<()> {
        let root = tempdir()?;
        // Root-package BUILD.
        std::fs::write(
            root.path().join("BUILD"),
            r#"target(name = "root_t", driver = "d")"#,
        )?;
        // Nested-package BUILD.
        std::fs::create_dir_all(root.path().join("sub"))?;
        std::fs::write(
            root.path().join("sub").join("BUILD"),
            r#"target(name = "sub_t", driver = "d")"#,
        )?;

        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|init| {
            Box::new(hplugin_buildfile::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let addrs: Vec<Addr> = SArc::clone(&engine)
            .query(rs, &Matcher::PackagePrefix(PkgBuf::from("")))
            .try_collect()
            .await?;

        let names: Vec<&str> = addrs.iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"sub_t"),
            "nested target must be found: {names:?}"
        );
        assert!(
            names.contains(&"root_t"),
            "root-package target must be found: {names:?}"
        );
        Ok(())
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
        // `fs` is auto-registered by `Engine::new`.
        engine.register_provider(|init| {
            Box::new(hplugin_buildfile::pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let addr = Addr::new(PkgBuf::from(""), "t".to_string(), Default::default());
        let spec = SArc::clone(&engine).get_spec(rs, &addr).await?;

        let mut srcs = match spec.spec.config.get("srcs") {
            Some(hcore::htvalue::Value::List(l)) => l
                .iter()
                .map(|e| match e {
                    hcore::htvalue::Value::String(s) => s.clone(),
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

        let executor = EngineProviderExecutor::new(SArc::downgrade(&engine), skipped_rs);

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

    use hbuiltins::pluginstatictarget;
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    /// Like [`engine_with_home`] but wired to a read+write remote cache at
    /// `remote_uri` (a `file://` dir). Used to assert the per-target
    /// `remote_enabled` gate on remote upload/download. The returned `TempDir`
    /// backs the engine's home/lock dirs and must be held alive for the test.
    fn engine_with_remote(
        targets: Vec<pluginstatictarget::Target>,
        remote_uri: &str,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            remote_caches: vec![crate::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: remote_uri.to_string(),
                read: true,
                write: true,
                concurrency: 10,
            }],
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// Exec target that keeps local caching on but disables the remote cache
    /// (`cache = {enabled: true, remote: false}`) — the shape the nix driver
    /// emits because its output embeds host-local `/nix/store` paths.
    fn no_remote_target(addr: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "exec".to_string(),
            run: Some("true".to_string()),
            cache: Some(hcore::htvalue::Value::Map(HashMap::from([
                ("enabled".to_string(), hcore::htvalue::Value::Bool(true)),
                ("remote".to_string(), hcore::htvalue::Value::Bool(false)),
            ]))),
            ..Default::default()
        }
    }

    /// Count regular files under `dir`, recursively. An empty count means the
    /// remote cache received nothing.
    fn count_files(dir: &std::path::Path) -> usize {
        let mut n = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                n += count_files(&path);
            } else {
                n += 1;
            }
        }
        n
    }

    /// Drain background uploads tracked by the request's `bg_pending` counter so
    /// the remote-cache assertions observe a settled state.
    async fn drain_bg(rs: Arc<crate::engine::request_state::RequestState>) {
        let bg = rs.bg_pending();
        // Takes the request by value and releases it before waiting. Some
        // background work is only *submitted* when the request state drops (the
        // post-write cache trim) and holds a slot until then, so a waiter that
        // keeps `rs` alive waits on a counter that cannot reach zero. `heph run`
        // unwinds in this order too — the drain loop runs after the app future,
        // which owns the request, has returned.
        drop(rs);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bg.load(Ordering::Acquire) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "bg_pending never drained"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A target that disables its remote cache (`remote_enabled = false`) must
    /// never be uploaded to the remote — otherwise another machine pulls an
    /// artifact whose host-local paths it lacks (the nix-wrapper `exit 127`
    /// bug). The control target (remote on) proves the path is otherwise live.
    #[tokio::test]
    async fn remote_upload_honors_per_target_remote_enabled() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());

        // Remote-disabled target: must leave the remote empty.
        let (engine, _home) = engine_with_remote(vec![no_remote_target("//pkg:off")], &remote_uri)?;
        let addr_off = hmodel::htaddr::parse_addr("//pkg:off")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr_off,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(rs).await;
        assert_eq!(
            count_files(remote.path()),
            0,
            "remote_enabled=false target must not be uploaded to the remote cache"
        );

        // Control: a default (remote-on) target with the same shape DOES upload,
        // proving the assertion above is the gate doing its job, not a dead path.
        let remote_on = tempdir()?;
        let remote_on_uri = format!("file://{}", remote_on.path().display());
        let (engine, _home) =
            engine_with_remote(vec![static_target("//pkg:on", &[], &[])], &remote_on_uri)?;
        let addr_on = hmodel::htaddr::parse_addr("//pkg:on")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr_on,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(rs).await;
        assert!(
            count_files(remote_on.path()) > 0,
            "remote-enabled target must upload to the remote cache"
        );
        Ok(())
    }

    /// The symmetric download gate: even when the remote *has* a matching
    /// revision, a `remote_enabled = false` target must execute locally rather
    /// than pull it — pulling would land an artifact whose host-local paths this
    /// machine lacks. Exec's def hash excludes the cache config, so the on/off
    /// targets share a `hashin` and the seeded entry is a genuine candidate.
    #[tokio::test]
    async fn remote_download_honors_per_target_remote_enabled() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        // Seed the remote: a default target executes and uploads.
        let (seeder, _seeder_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let rs = seeder.new_state();
        seeder
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(rs).await;
        assert!(
            count_files(remote.path()) > 0,
            "seed must populate the remote"
        );

        // Cold engine, same addr but remote disabled: must execute, never pull.
        let (off, _off_home) = engine_with_remote(vec![no_remote_target("//pkg:t")], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&off, &addr).await;
        res.expect("remote-off target must resolve");
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "remote-off target must execute locally, not pull: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::RemoteCacheReadStart { addr } if addr == "//pkg:t"
            )),
            "remote-off target must not attempt a remote pull: {events:?}"
        );

        // Control: cold engine, default target, same addr — pulls the seeded
        // entry instead of executing, proving the entry was a live candidate.
        let (on, _on_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&on, &addr).await;
        res.expect("remote-on target must resolve");
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::RemoteCacheReadStart { addr } if addr == "//pkg:t"
            )),
            "remote-on target must attempt a remote pull: {events:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "remote-on target must use the remote hit, not execute: {events:?}"
        );
        Ok(())
    }

    /// Bash target that actually writes an output, so a cache hit has a blob to
    /// materialize — a no-output target has nothing to pull and would never reach
    /// the lazy path.
    fn out_target(addr: &str) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "bash".to_string(),
            run: Some("echo hi > $OUT".to_string()),
            out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
            codegen: None,
            deps: HashMap::new(),
            labels: vec![],
            ..Default::default()
        }
    }

    /// [`engine_with_remote`] wired to the bash driver, for targets that need a
    /// shell to produce an output.
    fn engine_with_remote_bash(
        targets: Vec<pluginstatictarget::Target>,
        remote_uri: &str,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            remote_caches: vec![crate::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: remote_uri.to_string(),
                read: true,
                write: true,
                concurrency: 10,
            }],
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_bash()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// Object store that still advertises every object but can only serve
    /// manifests — a revision whose blobs were reclaimed after their manifest was
    /// written, or a backend erroring on blob reads only.
    struct EvictedBlobBackend {
        /// Directory of a `file://` remote seeded by a real engine, so the
        /// manifest it serves is the genuine one for the target's `hashin`.
        root: std::path::PathBuf,
    }

    #[async_trait::async_trait]
    impl crate::engine::remote_cache::RemoteCacheBackend for EvictedBlobBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn tokio::io::AsyncRead + Send>>>> {
            if !key.ends_with(crate::engine::local_cache::MANIFEST_V1) {
                // The blob is gone, even though `exists` still claims otherwise.
                return Ok(None);
            }
            match std::fs::read(self.root.join(key)) {
                Ok(bytes) => Ok(Some(Box::pin(std::io::Cursor::new(bytes)))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        async fn open_write(
            &self,
            _key: &str,
        ) -> anyhow::Result<Pin<Box<dyn tokio::io::AsyncWrite + Send>>> {
            anyhow::bail!("read-only stub backend")
        }
        /// Still advertised: this is what makes the pull, not the probe, discover
        /// that the bytes are gone.
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        /// Also still advertised — the presence check lists the revision, and the
        /// seeded `file://` tree really does hold every object. Only `open_read`
        /// lies, which is the point: the loss is discovered mid-read.
        async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            let dir = self.root.join(prefix);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return Ok(Vec::new());
            };
            Ok(entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect())
        }
    }

    /// A revision the remote advertised but cannot actually serve must rebuild the
    /// target, not fail the run.
    ///
    /// The hit is decided from the manifest plus a presence check, so the bytes can
    /// still disappear before the read: an object expired in that window, or the
    /// blob GET failed. That leaves the engine holding a confirmed "already built"
    /// entry with nothing behind it — which must degrade to executing the target,
    /// exactly as a plain cache miss would have.
    #[tokio::test]
    async fn unservable_remote_blob_rebuilds_instead_of_failing() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        // Seed the remote for real, so the stub serves a genuine manifest.
        let (seeder, _seeder_home) =
            engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let seed_rs = seeder.new_state();
        seeder
            .clone()
            .result_addr(
                seed_rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(seed_rs).await;

        // Cold engine whose remote serves the manifest, claims the blobs exist,
        // and then cannot produce them.
        let (engine, _home) = engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let mut engine = engine;
        let home = engine.home.clone();
        Arc::get_mut(&mut engine)
            .expect("engine must not be shared yet")
            .remote_caches = crate::engine::RemoteCacheSet::with_backend(
            Arc::new(EvictedBlobBackend {
                root: remote.path().to_path_buf(),
            }),
            home,
        );

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("an unservable remote revision must rebuild, not fail the run");
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "the target must be rebuilt when its advertised blobs cannot be served: {events:?}"
        );
        Ok(())
    }

    /// Serves a seeded `file://` tree faithfully while counting every request,
    /// split the way the object stores bill them: metadata (GET of a small
    /// object, HEAD, LIST) versus blob transfers.
    ///
    /// Writes are accepted and discarded — a rebuild mid-test re-uploads, and the
    /// fixture must not mutate underneath the assertions.
    #[derive(Default)]
    struct CountingRemoteBackend {
        root: std::path::PathBuf,
        /// GETs of the manifest object, HEADs, and revision LISTs — the requests
        /// a presence check costs.
        metadata: AtomicUsize,
        /// GETs of a blob object: real bytes moving.
        blobs: AtomicUsize,
    }

    impl CountingRemoteBackend {
        fn new(root: &std::path::Path) -> Self {
            Self {
                root: root.to_path_buf(),
                ..Default::default()
            }
        }
        fn metadata(&self) -> usize {
            self.metadata.load(Ordering::SeqCst)
        }
        fn blobs(&self) -> usize {
            self.blobs.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::engine::remote_cache::RemoteCacheBackend for CountingRemoteBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn tokio::io::AsyncRead + Send>>>> {
            if key.ends_with(crate::engine::local_cache::MANIFEST_V1) {
                self.metadata.fetch_add(1, Ordering::SeqCst);
            } else {
                self.blobs.fetch_add(1, Ordering::SeqCst);
            }
            match std::fs::read(self.root.join(key)) {
                Ok(bytes) => Ok(Some(Box::pin(std::io::Cursor::new(bytes)))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        async fn open_write(
            &self,
            _key: &str,
        ) -> anyhow::Result<Pin<Box<dyn tokio::io::AsyncWrite + Send>>> {
            Ok(Box::pin(tokio::io::sink()))
        }
        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            self.metadata.fetch_add(1, Ordering::SeqCst);
            Ok(self.root.join(key).exists())
        }
        async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.metadata.fetch_add(1, Ordering::SeqCst);
            let Ok(entries) = std::fs::read_dir(self.root.join(prefix)) else {
                return Ok(Vec::new());
            };
            Ok(entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect())
        }
    }

    /// Bash target with an output and (optionally) deps, so a graph can be built
    /// whose interior node is only ever resolved for its `hashout`.
    fn out_target_with_deps(addr: &str, deps: &[&str]) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        if !deps.is_empty() {
            deps_map.insert("".to_string(), deps.iter().map(|s| s.to_string()).collect());
        }
        pluginstatictarget::Target {
            deps: deps_map,
            ..out_target(addr)
        }
    }

    /// Swap `engine`'s remote set for one backed by `backend`.
    fn with_counting_remote(
        engine: Arc<Engine>,
        backend: Arc<CountingRemoteBackend>,
    ) -> Arc<Engine> {
        let mut engine = engine;
        let home = engine.home.clone();
        Arc::get_mut(&mut engine)
            .expect("engine must not be shared yet")
            .remote_caches = crate::engine::RemoteCacheSet::with_backend(backend, home);
        engine
    }

    async fn resolve(
        engine: &Arc<Engine>,
        rs: &Arc<crate::engine::request_state::RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
    ) -> anyhow::Result<Arc<EResult>> {
        engine
            .clone()
            .result_addr(rs.clone(), addr, outputs, &ResultOptions::default())
            .await
    }

    fn hashouts(res: &EResult) -> Vec<String> {
        let mut h: Vec<String> = res
            .artifacts_meta
            .iter()
            .map(|m| m.hashout.clone())
            .collect();
        h.sort();
        h
    }

    /// A remote-served graph must cost nothing on the second run.
    ///
    /// Run 1 mirrors each revision's manifest into the local cache and pulls only
    /// the blobs the top-level target reads; the interior node is resolved for its
    /// `hashout` alone, so none of its bytes ever land. A mirrored manifest names
    /// blobs that are not resident, which used to make every later run re-prove
    /// they were still obtainable — one manifest GET plus one revision LIST per
    /// interior node, per run, under the per-addr lock. Nothing reads those bytes,
    /// so nothing may pay for them.
    #[tokio::test]
    async fn a_second_run_over_a_remote_served_graph_issues_no_metadata_requests()
    -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let app = hmodel::htaddr::parse_addr("//pkg:app")?;
        let targets = || {
            vec![
                out_target("//pkg:lib"),
                out_target_with_deps("//pkg:app", &["//pkg:lib"]),
            ]
        };

        // Seed the remote for real: both revisions, uploaded by a live engine.
        let (seeder, _seeder_home) = engine_with_remote_bash(targets(), &remote_uri)?;
        let seed_rs = seeder.new_state();
        resolve(&seeder, &seed_rs, &app, OutputMatcher::All).await?;
        drain_bg(seed_rs).await;

        let backend = Arc::new(CountingRemoteBackend::new(remote.path()));
        let (engine, _home) = engine_with_remote_bash(targets(), &remote_uri)?;
        let engine = with_counting_remote(engine, backend.clone());

        // Run 1: cold local cache, so both manifests are fetched and mirrored.
        let rs1 = engine.new_state();
        resolve(&engine, &rs1, &app, OutputMatcher::All).await?;
        // A run ends by dropping its state — which releases the riding read locks
        // its results hold. Run 2 must start from the same footing a fresh process
        // would. `drain_bg` takes the state by value and drops it before waiting,
        // so handing it over *is* that drop.
        drain_bg(rs1).await;
        let after_run1 = backend.metadata();
        assert!(
            after_run1 > 0,
            "run 1 must actually go to the remote, or the test proves nothing"
        );
        assert!(
            backend.blobs() > 0,
            "run 1 must pull the requested target's bytes"
        );

        // Run 2: every manifest is local and every blob anyone reads is local.
        let rs2 = engine.new_state();
        resolve(&engine, &rs2, &app, OutputMatcher::All).await?;
        drain_bg(rs2).await;
        assert_eq!(
            backend.metadata(),
            after_run1,
            "a fully-cached second run must issue no metadata request at all"
        );
        Ok(())
    }

    /// The trace the lazy remote lookup has to survive: a manifest mirrored with
    /// none of its blobs, the remote's copies gone, a dependent that folds the
    /// `hashout` and only *then* needs the bytes.
    ///
    /// The hashout-only leg must not touch the network — that is the whole point,
    /// and it means a build that only needs hashouts rides out a remote outage.
    /// The leg that wants bytes must then look the revision up for itself, find it
    /// unservable, rebuild — and reproduce exactly the `hashout`s the first leg
    /// already folded into its cache key.
    #[tokio::test]
    async fn a_folded_hashout_survives_the_bytes_going_away() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        let (seeder, _seeder_home) =
            engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let seed_rs = seeder.new_state();
        resolve(&seeder, &seed_rs, &addr, OutputMatcher::All).await?;
        drain_bg(seed_rs).await;

        let backend = Arc::new(CountingRemoteBackend::new(remote.path()));
        let (engine, _home) = engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let engine = with_counting_remote(engine, backend.clone());

        // Run 1 — hashout only. The manifest is mirrored; no blob is pulled.
        let rs1 = engine.new_state();
        let folded = resolve(&engine, &rs1, &addr, OutputMatcher::None).await?;
        // End of run 1: `drain_bg` takes the state by value and drops it before
        // waiting, which is what releases its riding read locks — so run 2 starts
        // from the same footing a fresh process would.
        drain_bg(rs1).await;
        let promised = hashouts(&folded);
        assert!(
            !promised.is_empty(),
            "a hashout-only resolve must still report the revision's metas"
        );
        assert!(
            folded.artifacts.is_empty(),
            "a hashout-only resolve must carry no artifact"
        );
        assert_eq!(backend.blobs(), 0, "no byte may move for a hashout");
        // The result carries the riding read guards too, so it has to go as well.
        drop(folded);

        // The remote loses the bytes but keeps the manifest — an object-store
        // lifecycle rule expiring blobs is exactly this.
        let removed = evict_remote_blobs(remote.path());
        assert!(removed > 0, "the test must actually evict a blob");

        // Run 2, leg 1 — the dependent folds the hashout again. The manifest is
        // local now, so this must be answered without a single request, expired
        // blobs or not.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs2 = engine.new_state_with_events(true, Some(tx));
        let before = backend.metadata();
        let again = resolve(&engine, &rs2, &addr, OutputMatcher::None).await?;
        assert_eq!(hashouts(&again), promised);
        assert_eq!(
            backend.metadata(),
            before,
            "a hashout-only hit on a mirrored manifest must not consult the remote"
        );

        // Run 2, leg 2 — now someone wants the bytes. Same addr, same shared
        // resolution cell, which so far has never looked for a remote. It must
        // look now, find the revision unservable, and rebuild.
        let full = resolve(&engine, &rs2, &addr, OutputMatcher::All).await?;
        assert_eq!(full.artifacts.len(), 1, "the outputs must be served");
        assert!(
            backend.metadata() > before,
            "the leg that needs bytes must be the one that looks the revision up"
        );
        assert_eq!(
            hashouts(&full),
            promised,
            "the rebuild must reproduce the hashouts the first leg already folded"
        );
        drop(rs2);
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(
            events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "the target must be rebuilt once its bytes turn out to be unobtainable: {events:?}"
        );
        Ok(())
    }

    /// Same trace, but the revision's recorded `hashout` no longer matches what
    /// the target produces — a non-reproducible target, forced here by rewriting
    /// the remote manifest before it is mirrored.
    ///
    /// The dependent has already folded the recorded `hashout` into its own key by
    /// the time the rebuild happens, so silently accepting the new one publishes a
    /// cache entry describing outputs that never existed. It has to fail, and the
    /// message has to name the target and both hashouts.
    #[tokio::test]
    async fn a_rebuild_that_changes_the_hashout_fails_loudly() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        let (seeder, _seeder_home) =
            engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let seed_rs = seeder.new_state();
        resolve(&seeder, &seed_rs, &addr, OutputMatcher::All).await?;
        drain_bg(seed_rs).await;

        let real = retag_remote_hashouts(remote.path(), "deadbeefdeadbeef");
        assert!(!real.is_empty(), "the seeded manifest must be rewritten");

        let backend = Arc::new(CountingRemoteBackend::new(remote.path()));
        let (engine, _home) = engine_with_remote_bash(vec![out_target("//pkg:t")], &remote_uri)?;
        let engine = with_counting_remote(engine, backend.clone());

        // A dependent folds the (now wrong) hashout, mirroring the manifest.
        let rs1 = engine.new_state();
        let folded = resolve(&engine, &rs1, &addr, OutputMatcher::None).await?;
        // Takes the state by value and drops it before waiting — that drop is
        // what releases the run's riding read locks.
        drain_bg(rs1).await;
        assert_eq!(hashouts(&folded), vec!["deadbeefdeadbeef".to_string()]);
        // The result carries the riding read guards too, so it has to go as well.
        drop(folded);

        assert!(evict_remote_blobs(remote.path()) > 0);

        // Asking for the bytes rebuilds — and the rebuild disagrees.
        let rs2 = engine.new_state();
        let Err(err) = resolve(&engine, &rs2, &addr, OutputMatcher::All).await else {
            panic!("a rebuild that changes the hashout must not be accepted");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("//pkg:t"), "must name the target: {msg}");
        assert!(
            msg.contains("deadbeefdeadbeef"),
            "must name the hashout dependents already folded: {msg}"
        );
        for h in &real {
            assert!(msg.contains(h), "must name the rebuilt hashout: {msg}");
        }
        Ok(())
    }

    /// Delete every blob object under a seeded remote, keeping the manifests —
    /// what an object-store lifecycle rule does. Returns how many were removed.
    fn evict_remote_blobs(dir: &std::path::Path) -> usize {
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                removed += evict_remote_blobs(&path);
            } else if path.file_name().and_then(|n| n.to_str())
                != Some(crate::engine::local_cache::MANIFEST_V1)
            {
                std::fs::remove_file(&path).expect("evict blob");
                removed += 1;
            }
        }
        removed
    }

    /// Rewrite every `hashout` in every remote manifest under `dir` to `tag`,
    /// returning the ones replaced. Fakes a revision whose recorded outputs do not
    /// match what the target actually builds.
    fn retag_remote_hashouts(dir: &std::path::Path, tag: &str) -> Vec<String> {
        let mut replaced = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return replaced;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                replaced.extend(retag_remote_hashouts(&path, tag));
                continue;
            }
            if path.file_name().and_then(|n| n.to_str())
                != Some(crate::engine::local_cache::MANIFEST_V1)
            {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read remote manifest");
            let mut manifest: crate::engine::remote_cache::RemoteManifest =
                borsh::from_slice(&bytes).expect("parse remote manifest");
            for a in &mut manifest.artifacts {
                replaced.push(std::mem::replace(&mut a.hashout, tag.to_string()));
            }
            std::fs::write(&path, borsh::to_vec(&manifest).expect("serialize")).expect("rewrite");
        }
        replaced
    }

    /// A remote pull that lands a revision must emit `RemoteCacheHit` (not just
    /// the `RemoteCacheRead` span) so the cached count downstream — TUI header,
    /// GHA summary, telemetry — includes remote hits, not only local ones. A
    /// remote lookup that comes back empty must emit `RemoteCacheMiss`.
    #[tokio::test]
    async fn remote_hit_and_miss_emit_cache_events() -> anyhow::Result<()> {
        let remote = tempdir()?;
        let remote_uri = format!("file://{}", remote.path().display());
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        // Cold engine, empty remote: the pull misses, so a `RemoteCacheMiss`
        // fires and the target executes.
        let (miss_engine, _miss_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&miss_engine, &addr).await;
        res.expect("target must resolve on a remote miss");
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::RemoteCacheMiss { addr } if addr == "//pkg:t")
            ),
            1,
            "empty remote must emit exactly one RemoteCacheMiss: {events:?}"
        );
        // The remote lookup only runs after the local miss settles under the
        // write lock, and that miss is emitted once — not once per probe.
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:t")
            ),
            1,
            "expected exactly one LocalCacheMiss: {events:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::RemoteCacheHit { addr } if addr == "//pkg:t")
            ),
            "a miss must not emit RemoteCacheHit: {events:?}"
        );

        // Seed the remote deterministically: a fresh engine sharing the remote
        // executes and uploads, guaranteeing the revision is present to pull.
        let (seeder, _seeder_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let seed_rs = seeder.new_state();
        seeder
            .clone()
            .result_addr(
                seed_rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        drain_bg(seed_rs).await;
        assert!(
            count_files(remote.path()) > 0,
            "seed must populate the remote"
        );

        // Cold engine against the seeded remote: the pull hits, emitting
        // `RemoteCacheHit` and skipping execution.
        let (hit_engine, _hit_home) =
            engine_with_remote(vec![static_target("//pkg:t", &[], &[])], &remote_uri)?;
        let (res, events) = resolve_collecting_events(&hit_engine, &addr).await;
        res.expect("target must resolve on a remote hit");
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::RemoteCacheHit { addr } if addr == "//pkg:t")
            ),
            1,
            "a remote pull must emit exactly one RemoteCacheHit: {events:?}"
        );
        // A remote-served target is a local miss exactly once — the shape the
        // TUI/telemetry summaries render on a remote-backed cold run.
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:t")
            ),
            1,
            "expected exactly one LocalCacheMiss: {events:?}"
        );
        assert!(
            !events.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:t")
            ),
            "a remote hit must not execute: {events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_spec_cross_target_cycle_returns_typed_error() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
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
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
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

    /// Provider whose `get` fails for one addr (and `NotFound` otherwise).
    ///
    /// With `fail_kind = Cycle` it raises a typed `CycleError` — modeling a
    /// provider that over-claims a name and induces a cycle deep in resolution
    /// (like the go provider over-claiming a buildfile codegen target), used to
    /// verify the engine *contains* the cycle by falling through to the next
    /// provider. With `fail_kind = Typed` it raises a plain typed error, the
    /// ordinary "this provider blew up" path that must surface with the addr
    /// attached.
    struct CyclingProvider {
        name: String,
        cycles_for: String,
        fail_kind: FailKind,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FailKind {
        Cycle,
        Typed,
    }

    /// A typed error carried out of a provider's `get`, so a test can assert the
    /// chain still downcasts after the engine attaches addr context.
    #[derive(Debug)]
    struct ProviderBlewUp;
    impl std::fmt::Display for ProviderBlewUp {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "provider blew up")
        }
    }
    impl std::error::Error for ProviderBlewUp {}
    impl crate::engine::provider::Provider for CyclingProvider {
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
            let fails = req.addr.format() == self.cycles_for;
            let kind = self.fail_kind;
            let addr = req.addr.clone();
            Box::pin(async move {
                match (fails, kind) {
                    (false, _) => Err(GetError::NotFound),
                    (true, FailKind::Cycle) => Err(GetError::Other(
                        CycleError {
                            from: addr.clone(),
                            to: addr,
                        }
                        .into(),
                    )),
                    (true, FailKind::Typed) => Err(GetError::Other(ProviderBlewUp.into())),
                }
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    fn engine_with_cycling(
        cycles_for: &str,
        statics: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<Arc<Engine>> {
        engine_with_failing(cycles_for, FailKind::Cycle, statics)
    }

    fn engine_with_failing(
        cycles_for: &str,
        fail_kind: FailKind,
        statics: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        // Cycling provider FIRST, so `get_spec` hits it before the static one.
        let cycles_for = cycles_for.to_string();
        engine.register_provider(move |_| {
            Box::new(CyclingProvider {
                name: "cyc".to_string(),
                cycles_for: cycles_for.clone(),
                fail_kind,
            })
        })?;
        if !statics.is_empty() {
            let provider = pluginstatictarget::Provider::new(statics)?;
            engine.register_provider(move |_| Box::new(provider))?;
        }
        Ok(Arc::new(engine))
    }

    // A provider whose `get` cycles must not abort `get_spec`: the engine falls
    // through to the next provider that resolves the addr acyclically. This is
    // the engine-level containment that makes `q <label> .` find a buildfile
    // target even when the go provider (registered first) over-claims it.
    #[tokio::test]
    async fn get_spec_falls_through_a_cyclic_provider_to_the_next() -> anyhow::Result<()> {
        let engine = engine_with_cycling("//pkg:t", vec![static_target("//pkg:t", &["lbl"], &[])])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;
        let spec = engine.clone().get_spec(engine.new_state(), &addr).await?;
        assert_eq!(
            spec.spec.labels,
            vec!["lbl".to_string()],
            "must resolve via the non-cyclic provider"
        );
        Ok(())
    }

    // When the ONLY provider serving an addr cycles, `get_spec` hard-fails with
    // the typed `CycleError` (loud) — never deadlocks, never silently NotFound.
    #[tokio::test]
    async fn get_spec_hard_fails_when_every_provider_cycles() -> anyhow::Result<()> {
        let engine = engine_with_cycling("//pkg:t", vec![])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;
        let err = engine
            .clone()
            .get_spec(engine.new_state(), &addr)
            .await
            .err()
            .expect("expected a cycle error");
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<CycleError>(&err).is_some(),
            "expected CycleError, got: {err:#}"
        );
        Ok(())
    }

    // A provider `get` failure must name the target it was resolving — without
    // it, non-tui output shows only the provider's own message with no clue
    // which addr triggered it. The context must wrap, not replace: the typed
    // error still has to downcast out of the chain.
    #[tokio::test]
    async fn get_spec_error_names_the_target_and_keeps_the_chain() -> anyhow::Result<()> {
        let engine = engine_with_failing("//pkg:t", FailKind::Typed, vec![])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:t")?;

        let err = engine
            .clone()
            .get_spec(engine.new_state(), &addr)
            .await
            .err()
            .expect("provider failure must propagate");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("resolving target `//pkg:t`"),
            "must name the addr: {msg}"
        );
        assert!(
            msg.contains("provider blew up"),
            "must keep the provider's message: {msg}"
        );
        assert!(
            hcore::hmemoizer::downcast_chain_ref::<ProviderBlewUp>(&err).is_some(),
            "typed downcast must survive the context wrap: {msg}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_request_bails_before_executing() -> anyhow::Result<()> {
        // A request whose token is already cancelled must not resolve or
        // execute the target — it returns CancelledError immediately so a
        // ctrl-c'd build stops starting new work.
        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
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
            &["//@heph/query:q@expr=label(foo)"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let rs = engine.new_state();

        let def = engine.clone().get_def(rs, &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
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
            &["//@heph/query:q@expr=label(foo)"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
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
            &["//@heph/query:q@expr=label(foo),exclude_provider=__user__"],
        )])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let def = engine.clone().get_def(engine.new_state(), &addr_a).await?;
        let input = def
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
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
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
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
        let q = "//@heph/query:q@expr=tree_output(pkg/gen),exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
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
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("annotated query input")
            .r#ref
            .r#ref
            .clone();
        let q_spec = engine.clone().get_spec(engine.new_state(), &q_addr).await?;
        let deps = match q_spec.config.get("deps") {
            Some(hcore::htvalue::Value::List(l)) => l,
            other => panic!("expected deps list, got {other:?}"),
        };
        let dep_strs: Vec<String> = deps
            .iter()
            .map(|v| match v {
                hcore::htvalue::Value::String(s) => s.clone(),
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
        let q = "//@heph/query:q@expr=tree_output(pkg/gen),exclude_provider=__none__";
        let engine = engine_with(vec![
            codegen_target("//pkg:a", &[], "gen", &[q]),
            codegen_target("//pkg:b", &[], "gen", &[q]),
        ])?;
        let addr_a = hmodel::htaddr::parse_addr("//pkg:a")?;
        let addr_b = hmodel::htaddr::parse_addr("//pkg:b")?;
        let rs = engine.new_state();

        let def_a = engine.clone().get_def(rs.clone(), &addr_a).await?;
        let def_b = engine.clone().get_def(rs.clone(), &addr_b).await?;

        let q_addr_a = def_a
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
            .expect("a's query input")
            .r#ref
            .r#ref
            .clone();
        let q_addr_b = def_b
            .target_def
            .inputs
            .iter()
            .find(|i| i.r#ref.r#ref.package.as_str() == hplugin_query::pluginquery::PACKAGE)
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
                Some(hcore::htvalue::Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        hcore::htvalue::Value::String(s) => s.clone(),
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
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        assert!(res.is_err(), "fail_fast=true must surface Err");
        Ok(())
    }

    /// Counts `get` calls — cumulative and concurrent — and parks each one
    /// until released.
    struct AdmissionGate {
        inner: pluginstatictarget::Provider,
        entered: SArc<AtomicUsize>,
        inflight: SArc<AtomicUsize>,
        peak: SArc<AtomicUsize>,
        gate: SArc<tokio::sync::Semaphore>,
    }

    impl crate::engine::provider::Provider for AdmissionGate {
        fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            self.inner.config(req)
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            self.inner.list(req, ctoken)
        }
        fn list_packages<'a>(
            &'a self,
            req: ListPackagesRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            self.inner.list_packages(req, ctoken)
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async move {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                // Park here until the test lets go, so every admitted task
                // is simultaneously in flight and the count is the plateau.
                let permit = self.gate.acquire().await;
                self.inflight.fetch_sub(1, Ordering::SeqCst);
                drop(permit);
                self.inner.get(req, ctoken).await
            })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            self.inner.probe(req, ctoken)
        }
    }

    /// `Engine::result` used to spawn one task per matched addr as fast as the
    /// matcher yielded them. The execute semaphore is taken *inside* `execute` —
    /// downstream of spec, def, link, hashin and probe, and only on a miss — so
    /// nothing bounded the resolve pipeline itself, and a selection of a hundred
    /// thousand meant a hundred thousand live state machines, queued blocking
    /// jobs, wakers and flock fds before a single target was known to need
    /// building.
    ///
    /// Gates the provider's `get` so every spawned task parks in the same place,
    /// then asserts the in-flight count plateaus at the limit rather than at the
    /// selection size.
    #[tokio::test]
    async fn top_level_resolution_is_admission_controlled() -> anyhow::Result<()> {
        // parallelism 1 keeps the limit small enough to overshoot cheaply.
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: Some(1),
            ..Default::default()
        })?;
        let limit = Engine::top_level_spawn_limit(engine.max_workers);
        let targets: Vec<_> = (0..limit + 50)
            .map(|i| static_target(&format!("//pkg:t{i}"), &[], &[]))
            .collect();
        let selected = targets.len();

        let (entered, inflight, peak) = (
            SArc::new(AtomicUsize::new(0)),
            SArc::new(AtomicUsize::new(0)),
            SArc::new(AtomicUsize::new(0)),
        );
        let gate = SArc::new(tokio::sync::Semaphore::new(0));
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = AdmissionGate {
            inner: pluginstatictarget::Provider::new(targets)?,
            entered: SArc::clone(&entered),
            inflight: SArc::clone(&inflight),
            peak: SArc::clone(&peak),
            gate: SArc::clone(&gate),
        };
        engine.register_provider(move |_| Box::new(provider))?;
        let engine = SArc::new(engine);

        let rs = engine.new_state();
        let batch = tokio::spawn(enclose!((engine, rs) async move {
            engine
                .result(
                    rs,
                    &Matcher::Package(PkgBuf::from("pkg")),
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        }));

        // Wait for the plateau: every permit taken, every holder parked in `get`.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while inflight.load(Ordering::SeqCst) < limit {
            assert!(
                tokio::time::Instant::now() < deadline,
                "resolution never reached the limit; in flight {}",
                inflight.load(Ordering::SeqCst),
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Give an unbounded spawn loop room to overshoot if it is going to.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let observed = peak.load(Ordering::SeqCst);
        assert_eq!(
            observed, limit,
            "top-level resolution must plateau at the limit, not at the {selected} matched",
        );

        // Admission must be coupled to *completion*, not to elapsed time: an
        // implementation that merely spawned slowly would have passed the
        // plateau check above. Releasing exactly one holder must admit exactly
        // one more.
        gate.add_permits(1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while entered.load(Ordering::SeqCst) < limit + 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "releasing one holder must admit one more; entered {}",
                entered.load(Ordering::SeqCst),
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Cancelling while the enqueue loop is parked on a permit must return,
        // not wait out the slowest in-flight target.
        rs.ctoken().cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(30), batch)
            .await
            .expect("a cancelled batch must not hang on the admission permit")
            .expect("join");
        // Release the parked holders so the engine's teardown isn't left with
        // tasks blocked inside the provider.
        gate.add_permits(selected);
        drop(res);
        Ok(())
    }

    /// The bound must not drop the tail. A selection larger than the limit has to
    /// resolve *every* matched addr — a `break` on the wrong branch or a leaked
    /// permit would wedge the loop after exactly `limit` and every other batch
    /// test in this file uses two or three targets, so none of them would notice.
    #[tokio::test]
    async fn a_selection_larger_than_the_limit_resolves_every_target() -> anyhow::Result<()> {
        let (engine, _root) = engine_with_parallelism(1, |max_workers| {
            (0..Engine::top_level_spawn_limit(max_workers) + 50)
                .map(|i| static_target(&format!("//pkg:t{i}"), &[], &[]))
                .collect()
        })?;
        let expected = Engine::top_level_spawn_limit(engine.max_workers) + 50;

        let rs = engine.new_state();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            engine.clone().result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                OutputMatcher::All,
                &ResultOptions::default(),
            ),
        )
        .await
        .expect("the bounded enqueue loop must not wedge")?;

        assert_eq!(
            res.ok.len(),
            expected,
            "every matched target must resolve, not just the first window"
        );
        Ok(())
    }

    /// The deadlock-freedom argument, exercised rather than asserted in a
    /// comment.
    ///
    /// A permit is only ever taken by a *top-level* spawn: dep fan-out resolves
    /// inline inside the parent's task, and a memoizer cell is driven by
    /// whichever awaiter polls it, so a dep never needs a task of its own to be
    /// scheduled. The shape that would break it if that stopped holding is this
    /// one — the first `limit` matched addrs all depend on an addr the matcher
    /// yields *last*, so every permit is held by something waiting on a target
    /// that could never be admitted.
    #[tokio::test]
    async fn matched_targets_depending_on_a_late_matched_target_do_not_deadlock()
    -> anyhow::Result<()> {
        let (engine, _root) = engine_with_parallelism(1, |max_workers| {
            // `zzz` sorts last, so the walk yields it after every dependent.
            let mut targets = vec![static_target("//pkg:zzz", &[], &[])];
            targets.extend(
                (0..Engine::top_level_spawn_limit(max_workers) + 50)
                    .map(|i| static_target(&format!("//pkg:t{i}"), &[], &["//pkg:zzz"])),
            );
            targets
        })?;
        let expected = Engine::top_level_spawn_limit(engine.max_workers) + 51;

        let rs = engine.new_state();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            engine.clone().result(
                rs,
                &Matcher::Package(PkgBuf::from("pkg")),
                OutputMatcher::All,
                &ResultOptions::default(),
            ),
        )
        .await
        .expect("a selection whose admitted targets all await a late match must not deadlock")?;

        assert_eq!(res.ok.len(), expected);
        Ok(())
    }

    /// The limit is a sizing decision, so it gets a test rather than living only
    /// in a doc comment — otherwise the multiplier can be changed to anything and
    /// every test stays green.
    #[test]
    fn the_spawn_limit_is_clamped_at_both_ends() {
        // Floor: a small `--jobs` must not serialise resolution behind execution.
        assert_eq!(Engine::top_level_spawn_limit(0), 16);
        assert_eq!(Engine::top_level_spawn_limit(1), 16);
        assert_eq!(Engine::top_level_spawn_limit(2), 16);
        // Eight times the execute width in the ordinary range.
        assert_eq!(Engine::top_level_spawn_limit(20), 160);
        // Ceiling: a large `--jobs` must not hand `Semaphore::new` an absurd
        // number, and `saturating_mul` alone would not stop it.
        assert_eq!(Engine::top_level_spawn_limit(usize::MAX), 2048);
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
                OutputMatcher::All,
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

    const WALK_BLEW_UP: &str = "matcher walk blew up";
    /// Package listed after the real one, whose `list` is the failing step.
    const LATE_PKG: &str = "zzz";

    /// Reproduces "a provider dies mid-walk, after `Engine::result` already
    /// spawned tasks for the addrs it did yield" — deterministically.
    ///
    /// `get` parks every spawned task inside its `mem_spec` cell (waking only on
    /// cancellation) and counts it in; the `list` of the trailing [`LATE_PKG`]
    /// waits for that count before failing. So by the time the query stream
    /// errors, every spawned task is guaranteed to be parked in a memoizer cell —
    /// which is the state the `JoinSet` must not be dropped in.
    struct GateProvider {
        inner: pluginstatictarget::Provider,
        parked: SArc<AtomicUsize>,
        expect_parked: usize,
        /// Held with **zero** permits until the test hands them out, so a task
        /// that wakes on cancellation cannot finish on its own. That is what makes
        /// the drain observable as an *ordering* rather than a count: while this
        /// is shut, a `result` that is genuinely draining cannot return, and one
        /// that dropped its `JoinSet` returns anyway.
        release: SArc<tokio::sync::Semaphore>,
        /// Parked tasks that were let go — resumed after cancellation *and* after
        /// the release, so their `get` returned instead of being dropped mid-poll.
        resumed: SArc<AtomicUsize>,
    }

    impl crate::engine::provider::Provider for GateProvider {
        fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            self.inner.config(req)
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            if req.package.as_str() != LATE_PKG {
                return self.inner.list(req, ctoken);
            }
            Box::pin(async move {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while self.parked.load(Ordering::SeqCst) < self.expect_parked {
                    if std::time::Instant::now() >= deadline {
                        // Distinct message: the test asserts on WALK_BLEW_UP, so a
                        // gate that never opened fails loudly instead of passing
                        // for the wrong reason.
                        anyhow::bail!("gate timed out before every target parked");
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(anyhow::anyhow!(WALK_BLEW_UP))
            })
        }
        fn list_packages<'a>(
            &'a self,
            req: ListPackagesRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async move {
                let it = self.inner.list_packages(req, ctoken).await?;
                let late = std::iter::once(Ok(ListPackageResponse {
                    pkg: PkgBuf::from(LATE_PKG),
                }));
                Ok(Box::new(it.chain(late)) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async move {
                self.parked.fetch_add(1, Ordering::SeqCst);
                // Park until the request is cancelled, then wait for the test to
                // let go. Both waits matter, and for different regressions: an
                // early return that skips `ctoken().cancel()` never gets past the
                // first, and one that cancels *and then* returns is caught by the
                // second — its tasks are still held here, so `result` returning is
                // proof it abandoned them rather than waiting.
                ctoken.cancelled().await;
                let permit = self.release.acquire().await;
                self.resumed.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                Err(GetError::NotFound)
            })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            self.inner.probe(req, ctoken)
        }
    }

    /// A failing matcher walk must surface its error *through the drain*, not by
    /// returning early. Returning drops the `JoinSet`, so every spawned task is
    /// aborted where it stands: an in-flight target is never told to stop and
    /// never gets to unwind — its child process is not taken through
    /// `interrupt_child`'s SIGINT/grace/SIGKILL, and whatever cleanup its
    /// destructors enqueue lands *after* `result` returned, outside the
    /// `bg_pending` wait the caller uses to know the run is over.
    ///
    /// **The assertion moved, and the old one was vacuous.** This test used to
    /// prove the drain indirectly: the un-polled futures the dropped `JoinSet`
    /// left behind held an `Arc<RequestState>` back into the memoizer that owned
    /// them, that cycle pinned `RequestStateData` forever, and so finding the
    /// request deregistered proved the cycle was never formed. #241 removed the
    /// cycle at the source — an abandoned memoizer cell now evicts itself and
    /// drops its in-flight future — so the request is released either way.
    /// Verified: with `result` restored to the pre-fix early return, the
    /// registry assertion **passes**. It had stopped being able to catch the
    /// regression, while still being able to fail spuriously on a loaded runner
    /// (linux/arm64, CI run 30435613380), which is the worst of both.
    ///
    /// So the drain is now asserted where it happens, and as an **ordering**
    /// rather than a count. `GateProvider::get` parks, waits for cancellation,
    /// and then blocks on a semaphore this test holds shut. A `result` that is
    /// draining is inside `join_next`, waiting on tasks it cannot get — so it
    /// *cannot* return while the gate is shut. One that dropped its `JoinSet` has
    /// nothing to wait for and returns anyway. A resumed-count alone would not
    /// have been enough: the half-fixed shape that cancels the token and *then*
    /// returns early wakes all three tasks, and on a multi-core runner they can
    /// reach the counter before the abort lands.
    ///
    /// The registry check is kept as a bounded wait, downgraded to what it now
    /// covers — that a request carrying real in-flight work is released rather
    /// than leaked. It no longer discriminates this regression; the assertion
    /// above does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_matcher_walk_drains_instead_of_dropping_the_joinset() -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let targets = vec![
            static_target("//pkg:a", &[], &[]),
            static_target("//pkg:b", &[], &[]),
            static_target("//pkg:c", &[], &[]),
        ];
        let expect_parked = targets.len();
        assert!(
            expect_parked > 0,
            "the fixture must spawn something to drain"
        );
        let parked = SArc::new(AtomicUsize::new(0));
        let resumed = SArc::new(AtomicUsize::new(0));
        // Starts shut. Every parked task blocks here after cancellation, so
        // nothing can finish until this test says so.
        let release = SArc::new(tokio::sync::Semaphore::new(0));
        let inner = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(
            enclose!((parked, resumed, release) move |_| Box::new(GateProvider {
                inner,
                parked,
                expect_parked,
                release,
                resumed,
            })),
        )?;
        let engine = Arc::new(engine);

        let rs = engine.new_state();

        // Driven from a task so the drain can be observed *while it is happening*
        // rather than inferred afterwards.
        let mut run = tokio::spawn(enclose!((engine, rs) async move {
            engine
                .result(
                    rs,
                    &Matcher::PackagePrefix(PkgBuf::from("")),
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        }));

        // The gate in `list` only fails the walk once every target has parked, so
        // reaching this means the walk has blown up (or is about to).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while parked.load(Ordering::SeqCst) < expect_parked {
            assert!(
                std::time::Instant::now() < deadline,
                "targets never parked, so the walk never failed"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // **The drain, asserted as an ordering rather than a count.** Every
        // spawned task is parked behind `release`, so a `result` that is draining
        // *cannot* return: it is inside `join_next`, waiting for tasks this test
        // is holding. One that dropped its `JoinSet` has nothing to wait for and
        // returns immediately — including the half-fixed shape that cancels the
        // token and *then* returns early, where a bare resumed-count would race
        // the abort and could pass.
        //
        // No wall-clock dependence on the green path: correct code cannot finish
        // in any window, so the timeout always elapses. The window only bounds how
        // long a *broken* build gets, and a broken one returns in microseconds.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut run)
                .await
                .is_err(),
            "`result` returned while every target it spawned was still parked: it \
             dropped the JoinSet instead of draining it, so those targets were \
             aborted mid-poll rather than told to stop and allowed to unwind"
        );

        release.add_permits(expect_parked);
        let err = run
            .await
            .expect("the result task must not panic")
            .err()
            .expect("a failed matcher walk must return Err");
        assert!(
            format!("{err:#}").contains(WALK_BLEW_UP),
            "the walk failure must be what surfaces, got: {err:#}"
        );

        // Every parked task got to the far side of the gate under its own power.
        // Reads 0 under both regression shapes; it would also read 0 if the
        // memoizer wait ever gained a cancellation race that drops the provider
        // `get` future mid-poll, so a failure here means *either* the JoinSet was
        // dropped *or* that race was introduced.
        let resumed = resumed.load(Ordering::SeqCst);
        assert_eq!(
            resumed,
            expect_parked,
            "{} of {expect_parked} parked targets never resumed, so `result` did \
             not drain them",
            expect_parked - resumed
        );

        drain_bg(rs).await;

        // A weaker invariant than it looks, and deliberately kept anyway: **this
        // no longer discriminates the drain regression** (see the doc comment —
        // with `result` restored to the early return it passes). What it still
        // freezes is that a request carrying real in-flight work — three spawned
        // tasks, an aborted walk task, live memoizer cells — is released rather
        // than leaked. Nothing else covers that; `test_request_state_tracking`
        // drops a request that never ran anything.
        //
        // Bounded rather than instantaneous because deregistration is not
        // synchronous with `result` returning: the matcher-walk task is `abort`ed
        // and deliberately never awaited (its own comment says why — it may be
        // deep inside a whole-package Starlark evaluation, and Ctrl-C must not
        // wait it out), so the runtime releases the `Arc` that task captured at
        // some later point of its choosing. Asserting otherwise is what made this
        // test fail on a loaded linux/arm64 runner while the machinery was working
        // correctly. A genuine leak is permanent, not slow, so the bound needs no
        // tuning.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !engine.requests.lock().expect("requests lock").is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "a RequestStateData was still registered 10s after the last external \
                 Arc to it was released: something is still holding one — a captured \
                 Arc<RequestState> in a task nobody joins, a hook, or a background \
                 job that never finished"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    /// The deep `MatchShrug` arm — the one that shrugs *again* at spec level and
    /// so reaches `get_def` + `matcher_target::match_target`.
    ///
    /// This arm is the entire justification for the shape of this walk
    /// (enumeration overlapped, matcher evaluation serial in the consumer), and
    /// the change physically moved it: it used to run inside the per-provider
    /// loop, interleaved with that provider's listing, and now runs after every
    /// provider for a package has listed, while K other packages are mid-`list`.
    /// `label(...)` does not reach it — that resolves at `match_spec` —
    /// so without a `TreeOutputTo` case the deepest path has no coverage at all.
    #[tokio::test]
    async fn query_tree_output_matcher_resolves_through_get_def() -> anyhow::Result<()> {
        // Both targets live in `gen`, so both survive the addr-level cheap
        // reject (which needs the two packages to be prefix-related) and both
        // shrug all the way down. Only `//gen:a`'s codegen tree actually lands
        // in `gen/dst`, and deciding that needs the def, not the spec.
        let engine = engine_with(vec![
            codegen_target("//gen:a", &[], "dst", &[]),
            codegen_target("//gen:b", &[], "other", &[]),
        ])?;
        let rs = engine.new_state();

        let matcher = Matcher::TreeOutputTo(PkgBuf::from("gen/dst"));
        let addrs: Vec<Addr> = SArc::clone(&engine)
            .query(rs, &matcher)
            .try_collect()
            .await?;

        let names: Vec<&str> = addrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a"],
            "only the target whose codegen tree lands in //gen/dst may match"
        );
        Ok(())
    }

    /// The same arm through `EngineProviderExecutor::query` — a second, separately
    /// edited copy of the loop, and the one `pluginquery` actually drives.
    #[tokio::test]
    async fn executor_query_tree_output_matcher_resolves_through_get_def() -> anyhow::Result<()> {
        let engine = engine_with(vec![
            codegen_target("//gen:a", &[], "dst", &[]),
            codegen_target("//gen:b", &[], "other", &[]),
        ])?;
        let rs = engine.new_state();
        let executor = EngineProviderExecutor::new(SArc::downgrade(&engine), SArc::clone(&rs));

        let matcher = Matcher::TreeOutputTo(PkgBuf::from("gen/dst"));
        let addrs = executor.query(&matcher, &[]).await?;

        let names: Vec<&str> = addrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a"]);
        Ok(())
    }

    /// Models the `PKG_EVAL_SLOTS` shape: `probe` takes a permit and holds it
    /// across a yield, `get` (reached from the shrug arm) needs one too.
    struct PermitProvider {
        pkgs: Vec<String>,
        slots: SArc<tokio::sync::Semaphore>,
    }

    impl crate::engine::provider::Provider for PermitProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "permit".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            let pkg = req.package.clone();
            if !self.pkgs.iter().any(|p| p == pkg.as_str()) {
                return Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) });
            }
            Box::pin(async move {
                let items: Vec<anyhow::Result<ListResponse>> = vec![Ok(ListResponse {
                    addr: Addr::new(pkg, "t".to_string(), Default::default()),
                })];
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
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
            let items: Vec<anyhow::Result<ListPackageResponse>> = self
                .pkgs
                .iter()
                .map(|p| {
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from(p.as_str()),
                    })
                })
                .collect();
            Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let slots = SArc::clone(&self.slots);
            let addr = req.addr.clone();
            Box::pin(async move {
                // The consumer's shrug arm lands here and needs a permit —
                // the ones the fan-out is holding.
                let _permit = slots
                    .acquire()
                    .await
                    .map_err(|e| GetError::Other(anyhow::Error::new(e)))?;
                Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr,
                        driver: "exec".to_string(),
                        config: Default::default(),
                        ..Default::default()
                    },
                })
            })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            let pkg = req.package.clone();
            if !self.pkgs.iter().any(|p| p == pkg.as_str()) {
                return Box::pin(async { Ok(ProbeResponse { states: vec![] }) });
            }
            let slots = SArc::clone(&self.slots);
            Box::pin(async move {
                // Mirrors `run_pkg`: permit acquired in async-land, then held
                // across a yield point.
                let _permit = slots.acquire().await.context("permit")?;
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(ProbeResponse { states: vec![] })
            })
        }
    }

    /// The discovery fan-out must not be able to starve its own consumer.
    ///
    /// `pluginbuildfile`'s `probe`/`list` reach `run_pkg`, which holds a
    /// `PKG_EVAL_SLOTS` permit — a global semaphore sized `cores` — across an
    /// await. The consumer of the fan-out then awaits `get_spec`/`get_def` in the
    /// `MatchShrug` arm, which can need a permit for a *different* package. If
    /// the fan-out's futures are only advanced by the consumer polling them, the
    /// permit holders are unpollable exactly while the consumer needs a permit,
    /// and `Semaphore` is FIFO: deadlock.
    ///
    /// Modelled here with the provider's own semaphore rather than
    /// `PKG_EVAL_SLOTS` (which is buildfile-internal): `probe` takes a permit and
    /// holds it across a yield, `get` — reached from the shrug arm via a `Label`
    /// matcher — needs one too, and there are more in-flight packages than
    /// permits. Fails by timing out rather than by assertion, which is the only
    /// honest way to test a deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn discovery_fanout_does_not_starve_the_matcher_consumer() -> anyhow::Result<()> {
        let k = crate::engine::fanout::discovery_concurrency();
        // Fewer permits than the fan-out width, so the fan-out can hold them all.
        let slots = SArc::new(tokio::sync::Semaphore::new((k / 2).max(1)));
        let n = k * 2;
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let pkgs: Vec<String> = (0..n).map(|i| format!("p{i:04}")).collect();
        engine.register_provider(enclose!((slots) move |_| Box::new(PermitProvider {
            pkgs: pkgs.clone(),
            slots: SArc::clone(&slots),
        })))?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        // A `Label` matcher shrugs at `matches_addr` for every candidate, so the
        // consumer awaits `get_spec` — and therefore a permit — between batches.
        let matcher = Matcher::Label("nope".to_string());
        let walk = SArc::clone(&engine)
            .query(rs, &matcher)
            .try_collect::<Vec<Addr>>();

        let addrs = tokio::time::timeout(Duration::from_secs(20), walk)
            .await
            .context(
                "discovery deadlocked: the fan-out held every permit while the \
                 consumer's MatchShrug arm waited for one, and the holders were \
                 only pollable by the consumer",
            )??;
        // No target carries the label, so nothing matches — the point is that it
        // terminated.
        assert!(addrs.is_empty(), "no target has this label, got {addrs:?}");
        Ok(())
    }

    /// The same starvation through `EngineProviderExecutor::query` — a separate
    /// copy of the loop, and the one behind every BUILD-file `query(...)` and
    /// every `//@heph/query:q@expr=` target. It is also the *nested* case: this
    /// walk runs inside `pluginquery::get`, reachable from `Engine::query`'s own
    /// shrug arm, so before the fix an outer walk could deadlock through it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn executor_query_fanout_does_not_starve_its_consumer() -> anyhow::Result<()> {
        let k = crate::engine::fanout::discovery_concurrency();
        let slots = SArc::new(tokio::sync::Semaphore::new((k / 2).max(1)));
        let n = k * 2;
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let pkgs: Vec<String> = (0..n).map(|i| format!("p{i:04}")).collect();
        engine.register_provider(enclose!((slots) move |_| Box::new(PermitProvider {
            pkgs: pkgs.clone(),
            slots: SArc::clone(&slots),
        })))?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();
        let executor = EngineProviderExecutor::new(SArc::downgrade(&engine), rs);

        let matcher = Matcher::Label("nope".to_string());
        let addrs = tokio::time::timeout(Duration::from_secs(20), executor.query(&matcher, &[]))
            .await
            .map_err(|_e| {
                anyhow::anyhow!(
                    "executor discovery deadlocked: the fan-out held every permit \
                     while the consumer's MatchShrug arm waited for one"
                )
            })??;
        assert!(addrs.is_empty(), "no target has this label, got {addrs:?}");
        Ok(())
    }

    /// A `list()` implementation that calls back into `req.executor.query()` —
    /// exactly the reentrant call the `for_list` executor flag exists to catch
    /// (see `EngineProviderExecutor::for_list`). No in-tree provider does this,
    /// but nothing stopped a third-party or out-of-process one from nesting a
    /// second K-wide walk under the one already dispatching this `list()` call,
    /// silently, with no diagnostic. Must fail loudly instead.
    #[tokio::test]
    async fn list_calling_back_into_query_is_rejected() -> anyhow::Result<()> {
        struct ReentrantQueryProvider {
            pkg: String,
        }

        impl crate::engine::provider::Provider for ReentrantQueryProvider {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "reentrant".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                Box::pin(async move {
                    // Incorrect provider behaviour: calling back into `query()`
                    // from inside `list()`. This must error, not nest.
                    req.executor
                        .query(&Matcher::PackagePrefix(PkgBuf::from("")), &[])
                        .await?;
                    Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                let pkg = self.pkg.clone();
                Box::pin(async move {
                    let items: Vec<anyhow::Result<ListPackageResponse>> =
                        vec![Ok(ListPackageResponse {
                            pkg: PkgBuf::from(pkg.as_str()),
                        })];
                    Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
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

        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|_| {
            Box::new(ReentrantQueryProvider {
                pkg: "p".to_string(),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let matcher = Matcher::PackagePrefix(PkgBuf::from(""));
        let err = SArc::clone(&engine)
            .query(rs, &matcher)
            .try_collect::<Vec<Addr>>()
            .await
            .expect_err(
                "a list() that calls back into query() must be rejected, not silently nested",
            );
        assert!(
            format!("{err:#}").contains("Provider::list"),
            "expected the reentrancy error, got: {err:#}"
        );
        Ok(())
    }

    /// The same reentrant `query()`, but issued from a `tokio::spawn`ed task
    /// inside `list()` — the shape a plugin-side runtime gives every provider
    /// body once `list` futures are spawned rather than polled inline.
    ///
    /// This is the case the instance-carried flag exists for: the previous
    /// task-local guard was scoped to the poll chain awaiting `list()`, so a
    /// spawn severed it and the nested walk went undetected. The flag rides on
    /// the executor the provider was handed, so it survives any spawn —
    /// including across the plugin ABI seam, where the guest cannot see host
    /// task-locals even in principle.
    #[tokio::test]
    async fn list_calling_back_into_query_from_spawned_task_is_rejected() -> anyhow::Result<()> {
        struct SpawnedReentrantQueryProvider {
            pkg: String,
        }

        impl crate::engine::provider::Provider for SpawnedReentrantQueryProvider {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "spawned-reentrant".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                Box::pin(async move {
                    // Incorrect provider behaviour, from a task the host did not
                    // poll into: the guard must still trip.
                    let executor = SArc::clone(&req.executor);
                    tokio::spawn(async move {
                        executor
                            .query(&Matcher::PackagePrefix(PkgBuf::from("")), &[])
                            .await
                    })
                    .await??;
                    Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                let pkg = self.pkg.clone();
                Box::pin(async move {
                    let items: Vec<anyhow::Result<ListPackageResponse>> =
                        vec![Ok(ListPackageResponse {
                            pkg: PkgBuf::from(pkg.as_str()),
                        })];
                    Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
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

        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(|_| {
            Box::new(SpawnedReentrantQueryProvider {
                pkg: "p".to_string(),
            })
        })?;
        let engine = SArc::new(engine);
        let rs = engine.new_state();

        let matcher = Matcher::PackagePrefix(PkgBuf::from(""));
        let err = SArc::clone(&engine)
            .query(rs, &matcher)
            .try_collect::<Vec<Addr>>()
            .await
            .expect_err(
                "a spawned task calling back into query() from list() must be \
                 rejected, not silently nested",
            );
        assert!(
            format!("{err:#}").contains("Provider::list"),
            "expected the reentrancy error, got: {err:#}"
        );
        Ok(())
    }

    /// Cancelling a walk must stop it *starting* packages it has not reached.
    ///
    /// Discovery is overlapped now, so an abandoned walk is abandoned with up to
    /// K package futures in flight. Those cannot be un-started — but everything
    /// behind them can, and must be: without the short-circuit the walk keeps
    /// evaluating whole BUILD files, one per remaining package, for a request
    /// whose answer nobody wants. On a 20k-package workspace that is the entire
    /// workspace evaluated after Ctrl-C.
    ///
    /// The provider counts `list` calls. Slow, non-cancellation-aware packages
    /// hold the fan-out open long enough for the cancel to land mid-walk.
    #[tokio::test]
    async fn cancelling_a_walk_stops_starting_new_packages() -> anyhow::Result<()> {
        struct CountingSlowList {
            pkgs: Vec<String>,
            lists: SArc<AtomicUsize>,
        }

        impl crate::engine::provider::Provider for CountingSlowList {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "slowcount".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                if !self.pkgs.iter().any(|p| p == req.package.as_str()) {
                    return Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) });
                }
                let lists = SArc::clone(&self.lists);
                Box::pin(async move {
                    lists.fetch_add(1, Ordering::SeqCst);
                    // Deliberately not cancellation-aware: a whole-package
                    // Starlark evaluation does not stop early either.
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(Box::new(std::iter::empty()) as Box<_>)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                let items: Vec<anyhow::Result<ListPackageResponse>> = self
                    .pkgs
                    .iter()
                    .map(|p| {
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from(p.as_str()),
                        })
                    })
                    .collect();
                Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
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

        let k = crate::engine::fanout::discovery_concurrency();
        // Far more packages than the buffer, so an un-short-circuited walk has
        // plenty left to start after the cancel lands.
        let n = k * 8;
        let root = tempdir()?;
        let lists = SArc::new(AtomicUsize::new(0));
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let pkgs: Vec<String> = (0..n).map(|i| format!("p{i:04}")).collect();
        engine.register_provider(enclose!((lists) move |_| Box::new(CountingSlowList {
            pkgs: pkgs.clone(),
            lists: SArc::clone(&lists),
        })))?;
        let engine = SArc::new(engine);

        let rs = engine.new_state();
        let matcher = Matcher::PackagePrefix(PkgBuf::from(""));
        let stream = SArc::clone(&engine).query(rs.clone(), &matcher);
        tokio::pin!(stream);

        // Let one buffer-load get under way, then cancel and keep draining.
        let drain = async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            rs.ctoken().cancel();
        };
        let consume = async {
            let mut last: Option<anyhow::Error> = None;
            while let Some(item) = stream.next().await {
                if let Err(e) = item {
                    last = Some(e);
                }
            }
            last
        };
        let (err, ()) = tokio::join!(consume, drain);

        let started = lists.load(Ordering::SeqCst);
        assert!(
            started < n,
            "a cancelled walk must stop starting packages: it listed {started} of {n}"
        );
        // And it must *say so*. Returning `Ok(vec![])` for the packages it
        // skipped would let the walk complete normally with a short answer —
        // and that answer becomes `pluginquery`'s `deps`, folded in order into
        // `plugingroup`'s def hash. A Ctrl-C during discovery would then write a
        // cache entry keyed on a graph whose size depends on when the cancel
        // landed, and that entry outlives the run that was abandoned.
        let err = err.expect("a cancelled walk must surface an error, not a short answer");
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
                OutputMatcher::All,
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
                OutputMatcher::All,
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
        let addr = hmodel::htaddr::parse_addr("//pkg:parent")?;
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
        let addr = hmodel::htaddr::parse_addr("//d:top")?;
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

    /// Deeply nested transparent groups complete on a 2 MiB stack.
    ///
    /// Transparent targets are inlined BEFORE the memoizer (`result_addr`'s
    /// group path recurses via `#[async_recursion]` without a task hop), so
    /// their descent is the one place a poll still nests one frame per level
    /// after the task-cell flip. Group frames are boxed and small (~KBs, not
    /// the ~100KiB memoized frames), but "small enough" is measured here, on
    /// a pinned 2 MiB thread — this is the gate that lets `GrowStack` go.
    #[test]
    fn deep_transparent_group_chain_completes_on_a_2mib_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    const N: usize = 300;
                    // g0 → g1 → … → g299 → leaf, every gN a transparent group
                    // of one member (a *name*, inlined without a task hop).
                    let mut targets = Vec::with_capacity(N + 1);
                    for i in 0..N {
                        targets.push(pluginstatictarget::Target {
                            addr: format!("//chain:g{i}"),
                            driver: "group".to_string(),
                            raw_config: HashMap::from([(
                                "deps".to_string(),
                                hcore::htvalue::Value::List(vec![hcore::htvalue::Value::String(
                                    format!("//chain:g{}", i + 1),
                                )]),
                            )]),
                            ..Default::default()
                        });
                    }
                    targets.push(run_target(&format!("//chain:g{N}"), &[], "true"));
                    let (engine, _home) = engine_with_home(targets).expect("engine");
                    let head = hmodel::htaddr::parse_addr("//chain:g0").expect("addr");
                    let rs = engine.new_state();
                    engine
                        .clone()
                        .result_addr(rs, &head, OutputMatcher::All, &ResultOptions::default())
                        .await
                        .expect("group chain resolves");
                });
            })
            .expect("spawn group-chain thread")
            .join()
            .expect("group-chain thread panicked");
    }

    /// A deep, fully-warm descent completes on a 2 MiB stack.
    ///
    /// The poll-cell model descended a warm chain synchronously in one poll
    /// (~100 KiB per level — the overflow `GrowStack` exists for). With
    /// task-backed request memoizers every level is its own task, so per-poll
    /// stack depth is O(1) regardless of graph depth. Pinned to a 2 MiB
    /// thread (explicitly, because `RUST_MIN_STACK` makes the default
    /// unfalsifiable) so a regression back to deep synchronous polling fails
    /// here rather than on a user's monorepo. This is the regression gate for
    /// deleting `GrowStack`.
    #[test]
    fn deep_warm_chain_completes_on_a_2mib_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    const N: usize = 200;
                    let addrs: Vec<String> = (0..N).map(|i| format!("//chain:a{i}")).collect();
                    let mut targets = Vec::with_capacity(N);
                    for i in 0..N {
                        if i + 1 < N {
                            targets.push(run_target(&addrs[i], &[addrs[i + 1].as_str()], "true"));
                        } else {
                            targets.push(run_target(&addrs[i], &[], "true"));
                        }
                    }
                    // `engine_with_home`: the TempDir must outlive both runs —
                    // the warm run reads the lock dir and cache under it.
                    let (engine, _home) = engine_with_home(targets).expect("engine");
                    let head = hmodel::htaddr::parse_addr(&addrs[0]).expect("addr");

                    // Cold run populates the local cache.
                    let rs = engine.new_state();
                    engine
                        .clone()
                        .result_addr(rs, &head, OutputMatcher::All, &ResultOptions::default())
                        .await
                        .expect("cold run");

                    // Warm run, fresh request state: the full-hit descent that
                    // used to be one synchronous poll per chain.
                    let rs = engine.new_state();
                    engine
                        .clone()
                        .result_addr(rs, &head, OutputMatcher::All, &ResultOptions::default())
                        .await
                        .expect("warm run");
                });
            })
            .expect("spawn deep-chain thread")
            .join()
            .expect("deep-chain thread panicked");
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
                        let head = hmodel::htaddr::parse_addr(&addrs[0]).expect("addr");
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
        use std::sync::Arc;

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        // A 12-line log, default tail of 10 → lines 3..=12 with start_line 3.
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        let full: String = (1..=12).map(|i| format!("line{i}\n")).collect();
        std::fs::write(&log_path, &full)?;

        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log: Arc::new(hcore::hartifactcontent::FileContent::new(&log_path)),
        })
        .context("driver run")
        .context("execute //pkg:a");

        let out = classify_failure(&rs, &addr, false, e);
        // Own failure → cheap marker propagated upward.
        assert!(downcast_chain_ref::<UpstreamFailed>(&out).is_some());
        // …and the rich record carries the last 10 lines read from the log file,
        // tagged with the real starting line number.
        let recorded = rs.get_failure(&addr).expect("failure must be recorded");
        let tail = recorded.log_tail.as_ref().expect("log tail");
        assert_eq!(
            tail.text,
            "line3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12"
        );
        assert_eq!(tail.start_line, 3);
        Ok(())
    }

    #[tokio::test]
    async fn classify_drops_log_tail_for_interactive_targets() -> anyhow::Result<()> {
        // Interactive targets stream their output live to the user's terminal, so
        // the captured log tail must NOT be re-rendered in the failure box.
        use crate::engine::error::ProcessFailed;
        use std::sync::Arc;

        let engine = engine_with(vec![static_target("//pkg:a", &[], &[])])?;
        let rs = engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("log.txt");
        std::fs::write(&log_path, "line9\nline10\n")?;

        let e = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log: Arc::new(hcore::hartifactcontent::FileContent::new(&log_path)),
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
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;
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
            ..Default::default()
        }
    }

    /// [`engine_with_home`] with an explicit `parallelism`, and targets built
    /// from the resulting `max_workers` — so a test can size a selection against
    /// [`Engine::top_level_spawn_limit`] without hardcoding the machine.
    fn engine_with_parallelism(
        parallelism: usize,
        targets: impl FnOnce(usize) -> Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: Some(parallelism),
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets(engine.max_workers))?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
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
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// How many collected events match `pred`. Cache hit/miss assertions want a
    /// *count*, not an `any` — the bug these tests pin was one event emitted
    /// twice for one target, which every `any` in the suite happily accepted.
    fn count_kind(events: &[BuildEvent], pred: impl Fn(&BuildEventKind) -> bool) -> usize {
        events.iter().filter(|e| pred(&e.kind)).count()
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
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

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
        // Exactly one: the optimistic probe under the read lock stays silent on a
        // miss, and only the settled re-check under the write lock emits — a cold
        // target must not count as two misses in the hit/miss stats.
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:a")
            ),
            1,
            "expected exactly one LocalCacheMiss, got {events:?}"
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
                BuildEventKind::ResultEnd { addr, error: None, .. } if addr == "//pkg:a"
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
        // bypassing `Engine::result`. The RequestConfig announcement must still fire
        // (so the TUI paints the worker indicator), and exactly once.
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        let (res, events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("fresh target must resolve");

        let max_workers: Vec<usize> = events
            .iter()
            .filter_map(|e| match &e.kind {
                BuildEventKind::RequestConfig { max_workers, .. } => Some(*max_workers),
                _ => None,
            })
            .collect();
        assert_eq!(
            max_workers.len(),
            1,
            "expected exactly one RequestConfig event, got {events:?}"
        );
        assert!(max_workers[0] >= 1, "worker count must be positive");
        Ok(())
    }

    #[tokio::test]
    async fn warm_cache_emits_local_cache_hit_and_no_execute() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        // First resolve populates the cache (same engine ⇒ same home/cache).
        let (first, _) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");

        // Second resolve on the same engine must hit the local cache.
        let (second, events) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");

        // Exactly one: a hit returns from the first probe, so the second probe
        // never runs — and a warm target must not also count a miss.
        assert_eq!(
            count_kind(
                &events,
                |e| matches!(e, BuildEventKind::LocalCacheHit { addr } if addr == "//pkg:a")
            ),
            1,
            "warm resolve must emit exactly one LocalCacheHit, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, BuildEventKind::LocalCacheMiss { .. })),
            "warm resolve must not emit LocalCacheMiss, got {events:?}"
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

    /// Two requests racing one cold addr produce **one** miss and **one** hit
    /// between them — never a target counted as both.
    ///
    /// This is the other half of the double-count bug. Both requests probe under
    /// the read lock and both miss; one wins the write lock, executes and
    /// caches, and the loser's re-probe under that same lock hits. When the probe
    /// itself emitted the miss, the loser reported `Miss` *and* `Hit` for one
    /// target — inflating the miss count and letting `built + cached` exceed the
    /// target count. Only the winner's settled miss may be emitted.
    ///
    /// The winner is nondeterministic, but the totals across both streams are
    /// not: whatever the interleaving (including the degenerate sequential one),
    /// exactly one execute happens and exactly one of each event.
    #[tokio::test]
    async fn concurrent_cold_resolves_emit_one_miss_and_one_hit() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "true")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

        // Separate `RequestState`s: the per-request memoizer dedups within a
        // request, so only distinct requests can reach the lock protocol at all.
        let (a, b) = tokio::join!(
            resolve_collecting_events(&engine, &addr),
            resolve_collecting_events(&engine, &addr),
        );
        a.0.expect("first concurrent resolve must succeed");
        b.0.expect("second concurrent resolve must succeed");

        let events: Vec<BuildEvent> = a.1.into_iter().chain(b.1).collect();
        let count = |pred: fn(&BuildEventKind) -> bool| count_kind(&events, pred);
        assert_eq!(
            count(|e| matches!(e, BuildEventKind::LocalCacheMiss { addr } if addr == "//pkg:a")),
            1,
            "expected exactly one LocalCacheMiss across both requests, got {events:?}"
        );
        assert_eq!(
            count(|e| matches!(e, BuildEventKind::LocalCacheHit { addr } if addr == "//pkg:a")),
            1,
            "the loser's re-probe is the only hit, got {events:?}"
        );
        assert_eq!(
            count(|e| matches!(e, BuildEventKind::ExecuteStart { addr, .. } if addr == "//pkg:a")),
            1,
            "the write lock must let exactly one request execute, got {events:?}"
        );
        Ok(())
    }

    /// A `cache = False` target can never hit, so it must not show up in the
    /// hit/miss stats at all — every consumer (TUI, CI summary, GHA, telemetry)
    /// folds these events, and counting a target that *cannot* be cached as a
    /// "miss" misreports cache effectiveness on every run.
    #[tokio::test]
    async fn uncacheable_target_emits_no_cache_hit_or_miss_events() -> anyhow::Result<()> {
        let target = pluginstatictarget::Target {
            cache: Some(hcore::htvalue::Value::Bool(false)),
            ..static_target_run("//pkg:nocache", "true")
        };
        let (engine, _home) = engine_with_home(vec![target])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:nocache")?;

        // Resolve twice: neither the cold nor the repeat run may emit any
        // cache hit/miss event for an uncacheable target.
        for pass in ["cold", "repeat"] {
            let (res, events) = resolve_collecting_events(&engine, &addr).await;
            res.unwrap_or_else(|e| panic!("{pass} resolve must succeed: {e:#}"));
            assert!(
                !events.iter().any(|e| matches!(
                    &e.kind,
                    BuildEventKind::LocalCacheHit { .. }
                        | BuildEventKind::LocalCacheMiss { .. }
                        | BuildEventKind::RemoteCacheHit { .. }
                        | BuildEventKind::RemoteCacheMiss { .. }
                )),
                "{pass}: uncacheable target must emit no cache events, got {events:?}"
            );
            // It executes every time, announced with `cache: false` so views can
            // tell an uncacheable execute from a cache-miss execute.
            assert!(
                events.iter().any(|e| matches!(
                    &e.kind,
                    BuildEventKind::ExecuteStart { addr, cache: false, .. } if addr == "//pkg:nocache"
                )),
                "{pass}: expected ExecuteStart{{cache:false}}, got {events:?}"
            );
        }

        // `--force` is the other way a resolution becomes uncacheable, and it
        // shares the same early return today. Pin it separately so splitting
        // that branch (e.g. teaching force to probe the cache in order to
        // invalidate it) cannot silently put forced targets back in the stats.
        let (forced_engine, _forced_home) =
            engine_with_home(vec![static_target_run("//pkg:forced", "true")])?;
        let forced_addr = hmodel::htaddr::parse_addr("//pkg:forced")?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rs = forced_engine.new_state_with_events(true, Some(tx));
        Arc::clone(&forced_engine)
            .result_addr(
                rs.clone(),
                &forced_addr,
                OutputMatcher::All,
                &ResultOptions {
                    force: true,
                    ..Default::default()
                },
            )
            .await
            .expect("forced resolve must succeed");
        drop(rs);
        let mut forced_events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            forced_events.push(ev);
        }
        assert!(
            !forced_events.iter().any(|e| matches!(
                &e.kind,
                BuildEventKind::LocalCacheHit { .. }
                    | BuildEventKind::LocalCacheMiss { .. }
                    | BuildEventKind::RemoteCacheHit { .. }
                    | BuildEventKind::RemoteCacheMiss { .. }
            )),
            "a forced resolve must emit no cache events, got {forced_events:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failing_target_carries_error_in_execute_and_result_end() -> anyhow::Result<()> {
        let (engine, _home) = engine_with_home(vec![static_target_run("//pkg:a", "false")])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

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
                BuildEventKind::ResultEnd { addr, error: Some(_), .. } if addr == "//pkg:a"
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
                OutputMatcher::All,
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
                OutputMatcher::All,
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
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;

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
                OutputMatcher::All,
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
                OutputMatcher::All,
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

    // ----------------------------------------------------------------------
    // Per-addr result-lock self-deadlock regression (mem_locked_result).
    // ----------------------------------------------------------------------

    use crate::engine::driver::targetdef::{CacheConfig, Output};
    use crate::engine::driver::{
        ApplyTransitiveResponse, ConfigRequest as DriverConfigRequest,
        ConfigResponse as DriverConfigResponse, Driver as RawDriver, ParseResponse, RunRequest,
        RunResponse,
    };
    use async_trait::async_trait;

    /// Raw driver whose `run` produces one cacheable Raw output per `(group,
    /// name)` in `outputs` and counts executions. Lets the per-addr result-lock
    /// paths be exercised without spawning a subprocess or writing real files.
    struct BlockingDriver {
        exec_count: SArc<AtomicUsize>,
        /// `(output group, artifact name)` pairs this target emits.
        outputs: SArc<Vec<(String, String)>>,
        /// When set, `run` hands `execute_cache` a sandbox-cleanup job (the way a
        /// real sandboxing bridge does) whose body records the name of the thread
        /// it ran on. That name is the only direct evidence of which background
        /// lane the rmdir was routed to.
        cleanup_thread: Option<SArc<std::sync::OnceLock<String>>>,
    }

    #[async_trait]
    impl RawDriver for BlockingDriver {
        fn config(&self, _req: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
            Ok(DriverConfigResponse {
                name: "blocking".to_string(),
            })
        }
        fn schema(&self) -> crate::engine::driver::DriverSchema {
            crate::engine::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            req: ParseRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            Ok(ParseResponse {
                target_def: TargetDef {
                    addr: req.target_spec.addr.clone(),
                    labels: vec![],
                    raw_def: SArc::new(()),
                    inputs: vec![],
                    outputs: self
                        .outputs
                        .iter()
                        .map(|(group, _)| Output {
                            group: group.clone(),
                            paths: vec![],
                        })
                        .collect(),
                    support_files: vec![],
                    cache: CacheConfig::on(false),
                    pty: false,
                    hash: vec![1, 2, 3, 4],
                    transparent: false,
                },
            })
        }
        async fn apply_transitive(
            &self,
            req: ApplyTransitiveRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            Ok(ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            _req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            self.exec_count.fetch_add(1, Ordering::SeqCst);
            Ok(RunResponse {
                artifacts: self
                    .outputs
                    .iter()
                    .map(|(group, name)| outputartifact::OutputArtifact {
                        group: group.clone(),
                        name: name.clone(),
                        r#type: outputartifact::Type::Output,
                        content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                            data: b"hi".to_vec(),
                            path: name.clone(),
                            x: false,
                        }),
                        hashout: "feedface".to_string(),
                    })
                    .collect(),
                sandbox_cleanup: self.cleanup_thread.clone().map(|cell| {
                    Box::new(move || {
                        let name = std::thread::current()
                            .name()
                            .unwrap_or("<unnamed>")
                            .to_string();
                        drop(cell.set(name));
                        Ok(())
                    }) as crate::engine::driver::SandboxCleanupJob
                }),
                sandbox_guards: vec![],
            })
        }
        async fn run_shell<'a, 'io>(
            &self,
            _req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            anyhow::bail!("run_shell not supported by BlockingDriver")
        }
    }

    /// Provider serving exactly one `TargetSpec` (driven by `blocking`).
    struct OneTargetProvider {
        spec: TargetSpec,
    }

    impl crate::engine::provider::Provider for OneTargetProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "onetarget".to_string(),
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
            let spec = self.spec.clone();
            Box::pin(async move {
                if req.addr == spec.addr {
                    Ok(GetResponse { target_spec: spec })
                } else {
                    Err(GetError::NotFound)
                }
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// Engine with the `blocking` driver + a single cacheable target `//pkg:a`.
    /// Holds the cache/lock dirs in the returned `TempDir` (kept alive by caller).
    fn blocking_engine(
        exec_count: SArc<AtomicUsize>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Addr)> {
        blocking_engine_outputs(exec_count, vec![("main".to_string(), "out".to_string())])
    }

    /// Like [`blocking_engine`] but with a custom set of `(group, name)` outputs,
    /// for exercising multi-output / partial-cache behavior.
    fn blocking_engine_outputs(
        exec_count: SArc<AtomicUsize>,
        outputs: Vec<(String, String)>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Addr)> {
        blocking_engine_full(exec_count, outputs, "a", None, None)
    }

    /// Full form.
    ///
    /// `target_name` names the single target under `//pkg`. `cleanup_thread`
    /// makes the driver hand back a sandbox-cleanup job that records the thread
    /// it ran on. `wrap_cache` wraps the engine's `LocalCache`, which is how a
    /// test observes the thread the post-write trim ran on.
    /// Decorates the engine's `LocalCache` — how a test observes the thread the
    /// post-write trim ran on.
    type CacheWrapper<'a> = &'a dyn Fn(
        SArc<dyn crate::engine::local_cache::LocalCache>,
    ) -> SArc<dyn crate::engine::local_cache::LocalCache>;

    fn blocking_engine_full(
        exec_count: SArc<AtomicUsize>,
        outputs: Vec<(String, String)>,
        target_name: &str,
        cleanup_thread: Option<SArc<std::sync::OnceLock<String>>>,
        wrap_cache: Option<CacheWrapper<'_>>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir, Addr)> {
        let dir = tempdir()?;
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            // sqlite-direct (no in-memory layer) so `list_target_entries` /
            // `delete` reflect writes synchronously — the partial-cache test
            // discovers the hashin and drops a blob by name deterministically.
            mem_cache: crate::engine::MemCacheOptions {
                capacity_bytes: 0,
                ..Default::default()
            },
            ..Default::default()
        })?;
        if let Some(wrap) = wrap_cache {
            engine.local_cache = wrap(engine.local_cache.clone());
        }
        engine.register_driver(|_| {
            Box::new(BlockingDriver {
                exec_count,
                outputs: SArc::new(outputs),
                cleanup_thread,
            })
        })?;
        let addr = Addr::new(
            PkgBuf::from("pkg"),
            target_name.to_string(),
            Default::default(),
        );
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: "blocking".to_string(),
            ..Default::default()
        };
        engine.register_provider(move |_| Box::new(OneTargetProvider { spec }))?;
        Ok((Arc::new(engine), dir, addr))
    }

    /// Resolve one cacheable target through the **real** production cache stack
    /// (`Mem(Spill(SQLite, FS))`, mem tier at its default) at the given spill
    /// threshold, and hand back its single result artifact. Held `TempDir` is
    /// returned because the cache lives under it.
    async fn resolve_one_artifact_at_spill(
        spill_threshold_bytes: u64,
    ) -> (Arc<dyn Content>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            spill_threshold_bytes,
            ..Default::default()
        })
        .expect("engine");
        engine
            .register_driver(|_| {
                Box::new(BlockingDriver {
                    exec_count: SArc::new(AtomicUsize::new(0)),
                    outputs: SArc::new(vec![("main".to_string(), "out_main".to_string())]),
                    cleanup_thread: None,
                })
            })
            .expect("driver");
        let addr = Addr::new(PkgBuf::from("pkg"), "a".to_string(), Default::default());
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: "blocking".to_string(),
            ..Default::default()
        };
        engine
            .register_provider(move |_| Box::new(OneTargetProvider { spec }))
            .expect("provider");
        let engine = Arc::new(engine);

        let r = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("resolves");
        assert_eq!(r.artifacts.len(), 1, "the single output is surfaced");
        (Arc::clone(&r.artifacts[0]), dir)
    }

    /// The direct-open fast path, end to end through the real cache stack.
    ///
    /// Every tier has to answer for a consumer to get anything: the artifact is
    /// a `GuardedArtifact` over a `CacheArtifact` over `Mem(Spill(SQLite, FS))`,
    /// and a single tier defaulting to `None` — as all of them did — turns the
    /// whole path off with nothing failing. The unit tests pin the tiers; this
    /// pins that they compose, which is the property that was actually broken.
    ///
    /// The spill threshold is lowered to 64 bytes so the packed tar lands in the
    /// FS blob store without the test writing megabytes. The mem tier is left at
    /// its default — it is what production runs, and it is the tier sitting on
    /// top of the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cached_artifact_exposes_its_blob_file_for_direct_open() {
        let (artifact, _dir) = resolve_one_artifact_at_spill(64).await;

        let path = artifact
            .file_path()
            .expect("a spilled cache blob must reach the consumer as a path");
        let via_path = std::fs::read(&path).expect("open the cache blob directly");
        assert!(!via_path.is_empty(), "the path must name the real blob");

        // Same bytes either way — the fast path is a different route to the
        // artifact, not a different artifact.
        let mut via_stream = Vec::new();
        std::io::Read::read_to_end(&mut artifact.reader().expect("reader"), &mut via_stream)
            .expect("stream the artifact");
        assert_eq!(
            via_path, via_stream,
            "direct open and the byte stream must serve the same artifact"
        );
    }

    /// The property that makes handing out a bare `PathBuf` safe at all: an fd
    /// opened from it keeps serving the bytes it was opened on, even after the
    /// same cache key is rewritten underneath it.
    ///
    /// This is the half of the safety argument that is not about locking.
    /// `AtomicFileWriter` finishes by `rename`ing over the destination, so a
    /// rewrite (a lazy remote-cache pull, a rebuild) swaps the *inode* the path
    /// names while an already-open fd keeps the old one alive — POSIX unlink
    /// semantics, identical on all three supported targets. Without that, a
    /// consumer that opened the file could observe a torn mix of two revisions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_fd_opened_from_the_path_survives_the_blob_being_rewritten() {
        let (artifact, _dir) = resolve_one_artifact_at_spill(64).await;
        let path = artifact.file_path().expect("spilled blob has a path");

        let mut fd = std::fs::File::open(&path).expect("open before the rewrite");
        let original = std::fs::read(&path).expect("read blob");

        // Replace the blob the way the cache does — write a sibling, rename over.
        let tmp = path.with_extension("rewrite");
        std::fs::write(&tmp, b"a completely different revision").expect("write");
        std::fs::rename(&tmp, &path).expect("rename over the live blob");

        let mut held = Vec::new();
        std::io::Read::read_to_end(&mut fd, &mut held).expect("read through the held fd");
        assert_eq!(
            held, original,
            "the open fd must still serve the revision it was opened on"
        );
        assert_ne!(
            std::fs::read(&path).expect("read"),
            original,
            "precondition: the path itself now names different bytes"
        );
    }

    /// The shape ~every production artifact actually has, which the test above
    /// does not cover: at the **default** spill threshold a small artifact is a
    /// sqlite row with no file anywhere, so the honest answer is `None` and the
    /// consumer must fall back to the byte stream.
    ///
    /// This is the direction that turns a working read into a hard error if it
    /// regresses — a consumer opens what it is handed and never falls back — and
    /// it is exactly what a well-meaning "optimization" in the mem tier (answer
    /// from residency!) would break, with every other test in this area still
    /// green. `_golist`, the only artifact plugin-go reads across the seam, takes
    /// this branch and never the one above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_small_cached_artifact_reports_no_file_and_still_streams() {
        let (artifact, _dir) =
            resolve_one_artifact_at_spill(crate::engine::config::DEFAULT_SPILL_THRESHOLD_BYTES)
                .await;

        assert!(
            artifact.file_path().is_none(),
            "a sub-threshold blob lives in sqlite; naming a path would be naming nothing"
        );

        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut artifact.reader().expect("reader"), &mut bytes)
            .expect("stream the artifact");
        assert!(
            !bytes.is_empty(),
            "the stream must still serve the artifact when there is no file"
        );
    }

    /// P6.2: each background job class must reach its own lane.
    ///
    /// The two `Lane::` literals — the sandbox rmdir in `execute_cache`'s
    /// `defer!` and the batched post-write trim in `DeferredTrims::drop` — are the
    /// whole of the routing decision, and every other lane test drives `enqueue`
    /// with a lane the test itself supplies. Swap those two literals and the lanes
    /// invert while the rest of the suite stays green.
    ///
    /// Asserted on the *thread each job actually ran on*, not on the argument
    /// passed to `enqueue`, so a `sender()` that ignored its lane would fail here
    /// too. `Thread::name` reports the requested name on every supported target
    /// (Linux's 16-byte `PR_SET_NAME` truncation affects `/proc`, not this).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_background_job_class_runs_on_its_own_lane() {
        use crate::engine::local_cache_test_double::ForwardingCache;

        let exec_count = SArc::new(AtomicUsize::new(0));
        let rmdir_thread: SArc<std::sync::OnceLock<String>> = SArc::new(std::sync::OnceLock::new());
        let trim_thread: SArc<std::sync::OnceLock<String>> = SArc::new(std::sync::OnceLock::new());
        let wrap_trim = SArc::clone(&trim_thread);
        let (engine, _dir, addr) = blocking_engine_full(
            SArc::clone(&exec_count),
            vec![("main".to_string(), "out".to_string())],
            "p62_lane_routing",
            Some(SArc::clone(&rmdir_thread)),
            Some(&|inner| {
                // Records the thread of the first `list_target_entries` call.
                // In this test that call can only come from
                // `try_trim_after_write`, which starts with an unlocked
                // revision count — so it is reached whether or not the trim
                // goes on to take the write lock.
                SArc::new(ForwardingCache::new(inner).on_list_target_entries({
                    let wrap_trim = SArc::clone(&wrap_trim);
                    move |_| {
                        drop(
                            wrap_trim.set(
                                std::thread::current()
                                    .name()
                                    .unwrap_or("<unnamed>")
                                    .to_string(),
                            ),
                        );
                    }
                }))
            }),
        )
        .expect("engine");

        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("cold build resolves");
        assert_eq!(exec_count.load(Ordering::SeqCst), 1, "target executed once");

        // The trim is only *submitted* when the request state drops, so it cannot
        // have run yet. This is the deferral the lane split has to compose with —
        // if it ever regresses to running inline, the assertion below would be
        // measuring the calling thread instead of a lane.
        assert!(
            trim_thread.get().is_none(),
            "post-write trim must not run before the request state drops"
        );

        // Releases the request, which submits the trim batch, then waits for both
        // lanes to drain through the counter they share.
        drain_bg(rs).await;

        assert_eq!(
            rmdir_thread.get().map(String::as_str),
            Some("heph-sandbox-cleaner"),
            "the sandbox rmdir must run on the reclaim lane"
        );
        assert_eq!(
            trim_thread.get().map(String::as_str),
            Some("heph-cache-gc"),
            "the batched post-write cache.history trim must run on the bookkeeping lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_addr_two_output_variants_resolve_concurrently_completes() {
        // Reproduction. Two distinct `mem_result` cells for one addr — `All` and
        // `Exact["main"]` — both keep the produced artifact and so both hold a
        // riding read. On a cold cache the shared (memoized) meta wakes both at
        // once; both take coexisting shared reads, both miss, both then contend
        // the exclusive per-addr write. Pre-fix, the write-waiter blocks forever
        // on the sibling's riding read (single-process self-deadlock). Post-fix,
        // the addr-keyed `mem_locked_result` single-flight hands both callers one
        // shared read, so they complete.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine(SArc::clone(&exec_count)).expect("engine");
        let rs = engine.new_state();

        let t1 = tokio::spawn(enclose!((engine, rs, addr) async move {
            engine
                .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                .await
        }));
        let t2 = tokio::spawn(enclose!((engine, rs, addr) async move {
            engine
                .result_addr(
                    rs,
                    &addr,
                    OutputMatcher::Exact(vec!["main".to_string()]),
                    &ResultOptions::default(),
                )
                .await
        }));

        let (j1, j2) = tokio::time::timeout(Duration::from_secs(5), async {
            (t1.await.expect("t1 join"), t2.await.expect("t2 join"))
        })
        .await
        .expect("resolves must not self-deadlock on the per-addr result lock");

        let r1 = j1.expect("All variant resolves");
        let r2 = j2.expect("Exact[main] variant resolves");
        assert_eq!(r1.artifacts.len(), 1, "All must surface the 'main' output");
        assert_eq!(
            r2.artifacts.len(),
            1,
            "Exact[main] must surface the 'main' output"
        );
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "execute must be single-flighted across both output variants"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn distinct_requests_same_addr_share_cache_and_complete() {
        // The single-flight is per-`RequestStateData`: two separate requests get
        // separate `mem_locked_result` cells and still go through the real flock.
        // That's legitimate cross-request serialization (not the self-deadlock):
        // both complete, and the second hits the shared on-disk cache (keyed by
        // hashin), so execute runs exactly once across the two requests.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine(SArc::clone(&exec_count)).expect("engine");

        let r1 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("first request resolves");
        let r2 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("second request resolves");

        assert_eq!(r1.artifacts.len(), 1);
        assert_eq!(r2.artifacts.len(), 1);
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "second request must hit the cross-request on-disk cache, not re-execute"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zero_output_cacheable_target_hits_cache_on_second_run() {
        // Regression: a cacheable target that produces NO artifacts (a gate/check
        // like `go_lint_gate` / `go_format_check`) must still write a manifest so
        // a later run is a cache hit. Pre-fix, `execute_and_cache_inner` skipped
        // `cache_locally` whenever there was nothing to store, so no manifest was
        // ever written and every run re-executed ("0 cached").
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) =
            blocking_engine_outputs(SArc::clone(&exec_count), vec![]).expect("engine");

        let r1 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("first run resolves");
        let r2 = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("second run resolves");

        assert!(r1.artifacts.is_empty(), "target declares no outputs");
        assert!(r2.artifacts.is_empty(), "target declares no outputs");
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "second run must hit the cache, not re-execute a zero-output target"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_hit_pulls_only_requested_output_not_every_group() {
        // Lazy-pull invariant: requesting one output group must NOT require the
        // other groups' blobs to be present. A target with two outputs (`main`,
        // `extra`) is built once; we then delete `extra`'s blob locally (the
        // manifest stays — modelling a partial/remote cache that only pulled
        // `main`). A fresh request for just `main` must hit the cache and return
        // it WITHOUT re-executing. The earlier all-outputs resolution would have
        // missed here (every group's blob required present) and re-run execute.
        let exec_count = SArc::new(AtomicUsize::new(0));
        let (engine, _dir, addr) = blocking_engine_outputs(
            SArc::clone(&exec_count),
            vec![
                ("main".to_string(), "out_main".to_string()),
                ("extra".to_string(), "out_extra".to_string()),
            ],
        )
        .expect("engine");

        // Build everything once (both blobs + manifest now cached locally).
        let built = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("initial build resolves");
        assert_eq!(built.artifacts.len(), 2, "All surfaces both output groups");
        assert_eq!(exec_count.load(Ordering::SeqCst), 1);

        // Drop the `extra` group's blob, keeping the manifest (partial cache).
        // Derive the cache key from `meta` (the input hash — computed from inputs,
        // no cache read) rather than enumerating the cache: the sqlite writer is
        // async and a fresh read connection may not yet see the just-written rows.
        let hashin = Arc::clone(&engine)
            .meta(engine.new_state(), &addr)
            .await
            .expect("meta")
            .hashin;
        engine
            .local_cache
            .delete(&addr, &hashin, "out_extra")
            .expect("delete extra blob");

        // Request only `main` in a fresh request: must hit, not re-execute.
        let r = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::Exact(vec!["main".to_string()]),
                &ResultOptions::default(),
            )
            .await
            .expect("Exact[main] resolves against the partial cache");
        assert_eq!(
            r.artifacts.len(),
            1,
            "only the requested 'main' is surfaced"
        );
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "requesting one output must not re-execute just because another \
             group's blob is absent — the pull stays lazy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cache_hit_reads_manifest_once() {
        // T1.1: on a full cache hit the manifest must be read + deserialized
        // EXACTLY ONCE and reused for the per-caller output read. Pre-fix the
        // presence-probe and the per-caller read each parsed the manifest (two
        // backend reads per hit); now the probe stashes the parsed manifest on
        // `LockedResolution` and the caller filters its outputs from it.
        use crate::engine::local_cache::MANIFEST_V1;
        use crate::engine::local_cache_test_double::ForwardingCache;

        let dir = tempdir().expect("tempdir");
        let mut engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            // sqlite-direct (no in-memory tier) so every manifest read reaches the
            // counter instead of being served from an LRU on the second touch.
            mem_cache: crate::engine::MemCacheOptions {
                capacity_bytes: 0,
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("engine");
        let manifest_reads = SArc::new(AtomicUsize::new(0));
        engine.local_cache =
            SArc::new(ForwardingCache::new(engine.local_cache.clone()).on_reader({
                let manifest_reads = SArc::clone(&manifest_reads);
                move |_, _, name| {
                    if name == MANIFEST_V1 {
                        manifest_reads.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        let exec_count = SArc::new(AtomicUsize::new(0));
        engine
            .register_driver(enclose!(
                (exec_count) | _ | {
                    Box::new(BlockingDriver {
                        exec_count,
                        outputs: SArc::new(vec![("main".to_string(), "out".to_string())]),
                        cleanup_thread: None,
                    })
                }
            ))
            .expect("driver");
        let addr = Addr::new(PkgBuf::from("pkg"), "a".to_string(), Default::default());
        let spec = TargetSpec {
            addr: addr.clone(),
            driver: "blocking".to_string(),
            ..Default::default()
        };
        engine
            .register_provider(move |_| Box::new(OneTargetProvider { spec }))
            .expect("provider");
        let engine = Arc::new(engine);

        // Cold build: writes the manifest + blob. (Miss path reads happen here;
        // we only assert on the subsequent hit.)
        let cold_rs = engine.new_state();
        engine
            .clone()
            .result_addr(
                cold_rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("initial build resolves");
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "cold build executes once"
        );

        // The cold build's cache write records a fire-and-forget history-trim GC
        // on the background lane (see `try_trim_after_write`), which itself reads
        // the manifest. Drain it before resetting the counter so that lagging read
        // is never attributed to the hit below — otherwise the count races to 2.
        // The trim is only *submitted* when the request state drops, and holds a
        // background slot until then, so the request goes first.
        drain_bg(cold_rs).await;

        // Fresh request → full cache hit. The manifest backing read must happen
        // exactly once for the whole resolution.
        manifest_reads.store(0, Ordering::SeqCst);
        let r = engine
            .clone()
            .result_addr(
                engine.new_state(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("warm request hits the cache");
        assert_eq!(r.artifacts.len(), 1, "the single output is surfaced");
        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            1,
            "warm request must not re-execute"
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            1,
            "the manifest must be read + parsed exactly once per cache hit, not twice"
        );
    }

    // ─── Codegen write-back / fixpoint / frozen ──────────────────────────────

    /// Engine wired with the exec driver AND the `@heph/fs` provider+driver, so
    /// the `//@heph/introspect:outputs` magic input resolves. Returns the engine
    /// and the workspace-root `TempDir` (which doubles as the home/cache root).
    fn engine_with_home_fs(
        targets: Vec<pluginstatictarget::Target>,
    ) -> anyhow::Result<(Arc<Engine>, tempfile::TempDir)> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // `bash` driver wraps the `run` string into `bash -u -e -c <script>`,
        // so codegen targets can run real shell. The `@heph/fs` provider+driver
        // (auto-registered by `Engine::new`) resolves the synthesized
        // introspect-outputs inputs.
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_bash()))?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok((Arc::new(engine), root))
    }

    /// A codegen target: `out` group `""` over `paths`, the given `codegen` mode
    /// (`"copy"`/`"in_place"`), depending on the magic introspect-outputs input
    /// so its declared output paths become `@heph/fs` inputs. Runs under the
    /// `bash` driver so `run` may be a shell script (cwd = sandbox `ws/<pkg>`).
    fn codegen_run_target(
        addr: &str,
        codegen: &str,
        paths: &[&str],
        run: &str,
    ) -> pluginstatictarget::Target {
        let mut out = HashMap::new();
        out.insert(
            "".to_string(),
            paths.iter().map(|s| s.to_string()).collect(),
        );
        let mut deps = HashMap::new();
        deps.insert(
            "".to_string(),
            vec!["//@heph/introspect:outputs".to_string()],
        );
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "bash".to_string(),
            run: Some(run.to_string()),
            out,
            codegen: Some(codegen.to_string()),
            deps,
            labels: vec![],
            ..Default::default()
        }
    }

    /// Like [`codegen_run_target`] but with extra deps beyond the introspect
    /// target — used to put a *cacheable* sibling in an in_place target's dep
    /// closure.
    fn codegen_run_target_with_deps(
        addr: &str,
        codegen: &str,
        paths: &[&str],
        run: &str,
        extra_deps: &[&str],
    ) -> pluginstatictarget::Target {
        let mut t = codegen_run_target(addr, codegen, paths, run);
        if let Some(d) = t.deps.get_mut("") {
            d.extend(extra_deps.iter().map(|s| (*s).to_string()));
        }
        t
    }

    /// Cacheable bash target that takes `deps` and does nothing else. Its `hashin`
    /// therefore tracks those deps exactly.
    fn bash_target(addr: &str, deps: &[&str]) -> pluginstatictarget::Target {
        let mut deps_map = HashMap::new();
        deps_map.insert(
            "".to_string(),
            deps.iter().map(|s| (*s).to_string()).collect(),
        );
        pluginstatictarget::Target {
            addr: addr.to_string(),
            driver: "bash".to_string(),
            run: Some("true".to_string()),
            out: HashMap::new(),
            codegen: None,
            deps: deps_map,
            labels: vec![],
            ..Default::default()
        }
    }

    /// The invariant behind the fix, on its own: a hash-only request answers
    /// `HashUnknownError` for a cacheable target it is not already cached for,
    /// rather than taking the exclusive per-addr lock to build it.
    ///
    /// This is what makes nesting one inside a live resolution safe — the nested
    /// request shares the engine's single `ResultLock` with its caller but not
    /// the `mem_locked_result` memoizer that makes per-addr acquisition
    /// idempotent, so any write acquire is a self-deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hash_only_request_refuses_to_build_instead_of_locking() -> anyhow::Result<()> {
        // `meta` hashes a target from its INPUTS, so the uncached cacheable
        // target has to be a dep — that is the one a real recompute would build.
        let (engine, _root) = engine_with_home(vec![
            static_target("//pkg:dep", &[], &[]),
            static_target("//pkg:top", &[], &["//pkg:dep"]),
        ])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:top")?;

        // Nothing is cached yet, so resolving for real would execute `//pkg:dep`.
        let err = Arc::clone(&engine)
            .meta(engine.new_hash_only_state(addr.clone()), &addr)
            .await
            .err()
            .expect("a hash-only request must not build an uncached target");
        assert!(
            downcast_chain_ref::<HashUnknownError>(&err).is_some(),
            "expected HashUnknownError, got: {err:#}"
        );

        // And it is not a blanket refusal: once the target IS cached, the same
        // hash-only request answers from the cache under a shared read.
        let rs = engine.new_state();
        Arc::clone(&engine)
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("real resolve");
        drop(rs);
        Arc::clone(&engine)
            .meta(engine.new_hash_only_state(addr.clone()), &addr)
            .await
            .expect("a cached target is answerable without building");
        Ok(())
    }

    /// A hash-only probe that hits the cache must not report that hit.
    ///
    /// These are nested recomputes (the in_place write-back guard and fixpoint
    /// re-deriving a `hashin`), not resolutions anyone asked for, and the real
    /// request wrapping them probes the same addrs itself. They carry no event
    /// sender, so the TUI and the CI summary never saw them — but hooks are
    /// dispatched regardless of the sender, so telemetry and the GHA summary
    /// counted each nested probe as another cache hit.
    #[tokio::test]
    async fn hash_only_probes_do_not_report_cache_hits_to_hooks() -> anyhow::Result<()> {
        use crate::engine::hook::Hook;

        #[derive(Default)]
        struct HitCounter(std::sync::atomic::AtomicUsize);
        impl Hook for HitCounter {
            fn name(&self) -> String {
                "hit-counter".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                if matches!(ev.kind, BuildEventKind::LocalCacheHit { .. }) {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            fn on_close(&self) {}
        }

        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine
            .register_managed_driver(|_| Box::new(hplugin_exec::pluginexec::Driver::new_exec()))?;
        // A dep is what makes the probe run at all: `meta` hashes a target from
        // its inputs, so it resolves `//pkg:dep` for its `hashout` — and that
        // resolution is what probes the cache under the hash-only request.
        let provider = pluginstatictarget::Provider::new(vec![
            static_target("//pkg:dep", &[], &[]),
            static_target("//pkg:top", &[], &["//pkg:dep"]),
        ])?;
        engine.register_provider(move |_| Box::new(provider))?;
        let counter = Arc::new(HitCounter::default());
        engine.register_hook(Arc::clone(&counter) as Arc<dyn Hook>)?;
        let engine = Arc::new(engine);
        let addr = hmodel::htaddr::parse_addr("//pkg:top")?;

        // Warm the cache; the cold run reports no hit.
        let rs = engine.new_state();
        Arc::clone(&engine)
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("cold resolve");
        drop(rs);
        let hits = || counter.0.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(hits(), 0, "a cold resolve reports no cache hit");

        // Hash-only probes hit the cache — and stay silent about it.
        for _ in 0..3 {
            Arc::clone(&engine)
                .meta(engine.new_hash_only_state(addr.clone()), &addr)
                .await
                .expect("cached target answerable without building");
        }
        assert_eq!(hits(), 0, "hash-only probes must not report cache hits");

        // A real warm resolve still does — one per cached target it resolves.
        let rs = engine.new_state();
        Arc::clone(&engine)
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
            .expect("warm resolve");
        drop(rs);
        assert_eq!(
            hits(),
            2,
            "a real warm resolve reports one hit per cached target (top + dep)"
        );
        Ok(())
    }

    /// An in_place target whose dep closure contains a **cacheable** target
    /// hashing the same file it rewrites must not deadlock against itself.
    ///
    /// `maybe_store_fixpoint` recomputes the target's `hashin` on a *fresh*
    /// `RequestState` — deliberately, so the `@heph/fs` inputs re-read the
    /// just-written tree. But the outer request is still holding a riding **read**
    /// guard on every cacheable addr it resolved, including `//pkg:probe`. The
    /// write-back changed `in.txt`, so on the fresh request `//pkg:probe` hashes
    /// differently, misses, and asks for the exclusive **write** lock on an addr
    /// the outer request will not release until this call returns. The fresh
    /// request also runs on its own uncancelled token, so Ctrl-C cannot break it.
    ///
    /// `go_lint_fix` is exactly this shape: an in_place fixer over the same files
    /// its cacheable analyze dep hashes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_place_fixpoint_does_not_deadlock_on_a_cacheable_dep_it_rewrote()
    -> anyhow::Result<()> {
        let src = hbuiltins::pluginfs::file_addr("pkg/in.txt").format();
        let (engine, root) = engine_with_home_fs(vec![
            bash_target("//pkg:probe", &[&src]),
            codegen_run_target_with_deps(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                "printf '%s\\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
                &["//pkg:probe"],
            ),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        // Lowercase and newline-less, so run 1 provably changes the bytes — which
        // is what moves `//pkg:probe`'s hashin on the fixpoint recompute.
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let (res, _events) = tokio::time::timeout(
            Duration::from_secs(60),
            resolve_collecting_events(&engine, &addr),
        )
        .await
        .expect("in_place fixpoint recompute deadlocked against its own riding read locks");
        res.expect("fmt resolves");

        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "the in_place write-back must still land",
        );
        Ok(())
    }

    /// A write-back-guard failure must land in the failure registry — not just
    /// the event stream.
    ///
    /// The bug this pins: a guard failure that showed in the TUI's failed tab
    /// (events) while the run exited with an empty registry and nothing printed.
    ///
    /// The *trigger* changed with the guard's contract. It used to mutate a
    /// cacheable **dependency's** input mid-run, which made the guard's whole-
    /// `hashin` recompute fail with a `HashUnknownError` marker in its chain —
    /// a marker `classify_failure` deliberately never records. The guard now
    /// asks only about the files it is about to overwrite
    /// ([`Engine::check_in_place_inputs_unchanged`]), so a moved *dep* no longer
    /// fails it at all — see
    /// `a_moved_dependency_no_longer_blocks_the_write_back`, which pins that
    /// deliberate narrowing. So this mutates the in_place file itself, which is
    /// what the guard is for, and still asserts the two things that were broken:
    /// the failure is recorded, and the tree is not touched.
    ///
    /// The marker route is no longer reachable from here — the guard resolves
    /// only cache-off `@heph/fs` targets now, and those never miss at a hash —
    /// but the seal is kept in the guard as defence, not decoration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_place_guard_failure_is_recorded_not_just_evented() -> anyhow::Result<()> {
        let escape = tempdir()?;
        let started = escape.path().join("started");
        let release = escape.path().join("release");
        let src = hbuiltins::pluginfs::file_addr("pkg/other.txt").format();
        let (engine, root) = engine_with_home_fs(vec![
            // Cacheable dep hashing a file the test mutates mid-run.
            bash_target("//pkg:probe", &[&src]),
            codegen_run_target_with_deps(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                &format!(
                    "touch {started}; until [ -f {release} ]; do sleep 0.05; done; \
                     printf '%s\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
                    started = started.display(),
                    release = release.display(),
                ),
                &["//pkg:probe"],
            ),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;
        std::fs::write(pkg_dir.join("other.txt"), b"v1")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let rs = engine.new_state();
        let resolve = tokio::spawn(enclose!((engine, rs) async move {
            engine
                .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                .await
        }));

        // The run is provably in flight; move the very file the target is about
        // to overwrite, then let it finish. The guard re-reads that file and
        // sees a hash the run never transformed.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !started.exists() {
            assert!(std::time::Instant::now() < deadline, "run never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::write(pkg_dir.join("in.txt"), b"edited by someone else")?;
        std::fs::write(&release, b"go")?;

        let res = tokio::time::timeout(Duration::from_secs(60), resolve)
            .await
            .expect("run must finish")
            .expect("join");
        res.err()
            .expect("the guard must refuse: it cannot confirm the tree");

        let failures = rs.take_failures();
        assert_eq!(
            failures.len(),
            1,
            "the guard failure must be recorded so the exit path renders it"
        );
        assert_eq!(failures[0].addr.format(), "//pkg:fmt");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"edited by someone else",
            "a refused write-back must leave the newer bytes alone"
        );
        Ok(())
    }

    /// A moved **dependency** no longer blocks an in_place write-back.
    ///
    /// This is the one behaviour the narrowed guard gives up, and it is given up
    /// on purpose, so it is pinned rather than left to be rediscovered. The guard
    /// protects the files a target is about to *overwrite*; a target does not
    /// overwrite its dependency. A dep that moved mid-run makes this target's
    /// output stale, which the next run recomputes — it does not destroy
    /// anything, which is what the guard is for.
    ///
    /// The old whole-`hashin` recompute failed here, and paid for that by
    /// re-resolving the target's entire transitive closure under a fresh request
    /// on *every* in_place target of *every* run, cache hits included: 45.5s
    /// against 8.8s on a fully cached 2000-package run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_moved_dependency_no_longer_blocks_the_write_back() -> anyhow::Result<()> {
        let escape = tempdir()?;
        let started = escape.path().join("started");
        let release = escape.path().join("release");
        let src = hbuiltins::pluginfs::file_addr("pkg/other.txt").format();
        let (engine, root) = engine_with_home_fs(vec![
            bash_target("//pkg:probe", &[&src]),
            codegen_run_target_with_deps(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                &format!(
                    "touch {started}; until [ -f {release} ]; do sleep 0.05; done; \
                     printf '%s\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
                    started = started.display(),
                    release = release.display(),
                ),
                &["//pkg:probe"],
            ),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;
        std::fs::write(pkg_dir.join("other.txt"), b"v1")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let rs = engine.new_state();
        let resolve = tokio::spawn(enclose!((engine, rs) async move {
            engine
                .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                .await
        }));

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !started.exists() {
            assert!(std::time::Instant::now() < deadline, "run never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The dep's input moves — NOT the file being overwritten.
        std::fs::write(pkg_dir.join("other.txt"), b"v2")?;
        std::fs::write(&release, b"go")?;

        let res = tokio::time::timeout(Duration::from_secs(60), resolve)
            .await
            .expect("run must finish")
            .expect("join");
        res.expect("a moved dep must not fail the write-back");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "the write-back must land: nothing it overwrites has changed"
        );
        Ok(())
    }

    /// in_place does NOT restrict outputs to pre-existing inputs: a run that
    /// creates a net-new file matching its output glob succeeds and the file is
    /// written back to the tree. (The mode distinction is `copy` = generated /
    /// gitignored / glob-excluded vs `in_place` = committed / globbable — not
    /// "may only touch existing files".)
    #[tokio::test]
    async fn in_place_allows_net_new_files() -> anyhow::Result<()> {
        // out glob `pkg/*.txt`; nothing on disk → empty input set. The run script
        // (cwd = ws_dir/pkg) creates `created.txt`, a net-new output file.
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:gen",
            "in_place",
            &["*.txt"],
            "echo hi > created.txt",
        )])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:gen")?;

        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("in_place target may emit net-new files");
        assert_eq!(
            std::fs::read(root.path().join("pkg/created.txt"))?,
            b"hi\n",
            "net-new in_place output must be written back to the tree",
        );
        Ok(())
    }

    /// The on-disk exec bit is part of the `@heph/fs` (content + exec-bit) input
    /// hash, so the in_place write-back must apply the generated artifact's `x`
    /// even when the bytes are unchanged — otherwise a `+x`-only change never
    /// lands on disk and the recomputed fixpoint key would disagree with what ran.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_back_applies_exec_bit_on_unchanged_content() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:mkexec",
            "in_place",
            &["*.sh"],
            // Rewrite identical bytes, then mark the file executable in the
            // sandbox. The bytes match what's already on disk, so write-back
            // takes the unchanged path; only the exec bit differs and must be
            // reconciled onto the tree.
            "printf 'echo hi\\n' > run.sh && chmod +x run.sh",
        )])?;
        // Seed the exact bytes the run produces, but non-executable.
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))?;

        let addr = hmodel::htaddr::parse_addr("//pkg:mkexec")?;
        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("mkexec target resolves");

        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "write-back must apply the exec bit even when content is unchanged (mode {mode:o})",
        );
        Ok(())
    }

    /// The headline guarantee: re-running an idempotent in_place transform over
    /// the already-transformed tree is a no-op cache hit (no re-execution).
    ///
    /// Run 1 normalizes a lowercase, newline-less source into uppercase-with-
    /// trailing-newline, writes it back, and registers a fixpoint cache revision
    /// keyed on the post-write-back tree state. Run 2 reads the now-normalized
    /// tree and must HIT the cache — the `fmt` target emits no `ExecuteStart`.
    /// (The `@heph/fs` *inputs* are still re-read each run, so we assert
    /// specifically that the `fmt` target did not execute.)
    ///
    /// The transform changes the file CONTENT on run 1 (`hello` → `HELLO\n`), so
    /// the fixpoint key provably differs from the primary key — `@heph/fs` hashes
    /// by (content, exec-bit), and the changed bytes alone separate the two keys.
    /// This makes the stored fixpoint revision observable (≥ 2 cache revisions).
    #[tokio::test]
    async fn fixpoint_hit() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:fmt",
            "in_place",
            // Package-relative; stored as `FilePath(pkg/in.txt)`.
            &["in.txt"],
            // cwd = ws/pkg. Uppercase + ensure exactly one trailing newline. The
            // command substitution strips trailing newlines, `printf '%s\n'`
            // re-adds one, so f(f(x)) == f(x); on the FIRST run over a newline-
            // less lowercase seed it also changes the content.
            "printf '%s\\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
        )])?;
        // Seed a lowercase, newline-less source → run 1 yields "HELLO\n", a
        // guaranteed content change (case + trailing newline).
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let fmt = addr.format();
        let fmt_executed = |evs: &[BuildEvent]| {
            evs.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if *addr == fmt),
            )
        };

        // Run 1: executes, writes back the normalized file, stores the fixpoint.
        let (first, ev1) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");
        assert!(fmt_executed(&ev1), "first run must execute, got {ev1:?}");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "in_place run 1 must transform the tree file"
        );
        // Run 2: reads the already-transformed tree → fixpoint cache hit.
        // Because the transform changed the file CONTENT, run 2's hashin (over the
        // uppercased bytes) can only match the fixpoint revision, never the
        // primary (which is keyed over the lowercase seed). So "no execute on run
        // 2" is proof that the fixpoint revision was stored — no need to count
        // cache entries (the background GC may legitimately reclaim the now-stale
        // primary once the fixpoint exists).
        let (second, ev2) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");
        assert!(
            !fmt_executed(&ev2),
            "second run over the transformed tree must be a cache hit (no execute), got {ev2:?}"
        );
        assert!(
            ev2.iter()
                .any(|e| matches!(&e.kind, BuildEventKind::LocalCacheHit { addr } if *addr == fmt)),
            "second run must record a local cache hit for fmt, got {ev2:?}"
        );
        // The tree is unchanged by the no-op second run.
        assert_eq!(std::fs::read(pkg_dir.join("in.txt"))?, b"HELLO\n");
        Ok(())
    }

    /// The in_place write-back guard must survive an uncacheable (non-fs)
    /// dependency in the target's meta chain.
    ///
    /// The guard re-hashes the target on a fresh hash-only request. #193
    /// forbade hash-only requests from taking the result lock and answered
    /// `HashUnknownError` for EVERY non-fs target that would need it — but
    /// the self-deadlock it closed requires the outer request to be riding a
    /// read guard on the addr, and uncacheable resolutions hand out no guard
    /// at all (`LockedResolution { guard: None }`). The blanket raise made
    /// the guard structurally unable to verify any in_place target whose
    /// meta chain crosses a cache-off dep — `heph r lint //...` over
    /// toolchains reached through `//@heph/bin:*` (hostbin is cache-off)
    /// failed on every already-linted target.
    #[tokio::test]
    async fn in_place_write_back_survives_an_uncacheable_dep() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            // A cache-off, non-fs dependency — the hostbin shape.
            pluginstatictarget::Target {
                addr: "//pkg:tool".to_string(),
                driver: "bash".to_string(),
                run: Some("true".to_string()),
                out: HashMap::new(),
                codegen: None,
                deps: HashMap::new(),
                labels: vec![],
                cache: Some(hcore::htvalue::Value::Bool(false)),
                ..Default::default()
            },
            codegen_run_target_with_deps(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                "printf '%s\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
                &["//pkg:tool"],
            ),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        let fmt = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let (res, _ev) = resolve_collecting_events(&engine, &fmt).await;
        res.with_context(
            || "the write-back guard must be able to re-hash across an uncacheable dep",
        )?;
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "the in_place transform must have been written back"
        );

        // Second run: already-transformed tree — the cached-hit path runs the
        // same guard (this is the `r lint //...` on an already-linted repo).
        let (res, _ev) = resolve_collecting_events(&engine, &fmt).await;
        res.with_context(|| "the guard must also pass on the cached re-run")?;
        Ok(())
    }

    /// is_top gate: a codegen target reached only as a DEPENDENCY (not the
    /// directly-requested target) must NOT write its tree back. Locks the
    /// `is_top` memoizer-key fix — only the top-level frame materializes.
    #[tokio::test]
    async fn in_place_dep_is_not_written_back() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            codegen_run_target(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                "printf '%s\\n' \"$(tr a-z A-Z < in.txt)\" > in.txt.tmp && mv in.txt.tmp in.txt",
            ),
            // A consumer that depends on the in_place target but is itself a
            // plain (non-codegen) target. Uses the `bash` driver registered by
            // engine_with_home_fs.
            pluginstatictarget::Target {
                addr: "//pkg:consumer".to_string(),
                driver: "bash".to_string(),
                run: Some("true".to_string()),
                out: HashMap::new(),
                codegen: None,
                deps: HashMap::from([("".to_string(), vec!["//pkg:fmt".to_string()])]),
                labels: vec![],
                ..Default::default()
            },
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("in.txt"), b"hello")?;

        // Resolve the consumer (top-level). fmt is pulled in only as a dep
        // (is_top=false), so its tree must remain untouched.
        let consumer = hmodel::htaddr::parse_addr("//pkg:consumer")?;
        let (res, _ev) = resolve_collecting_events(&engine, &consumer).await;
        res.expect("consumer must resolve");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"hello",
            "an in_place target reached only as a dependency must NOT write its tree back"
        );

        // Sanity: requesting fmt directly DOES write it back.
        let fmt = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let (res, _ev) = resolve_collecting_events(&engine, &fmt).await;
        res.expect("fmt must resolve");
        assert_eq!(
            std::fs::read(pkg_dir.join("in.txt"))?,
            b"HELLO\n",
            "a directly-requested in_place target must write its tree back"
        );
        Ok(())
    }

    /// Multi-file in_place: a glob output covering several files transforms each
    /// of them, and a re-run over the transformed tree is a no-op cache hit —
    /// exercising the per-file write-back walk and the multi-file fixpoint.
    #[tokio::test]
    async fn fixpoint_hit_multi_file() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:fmt",
            "in_place",
            &["*.txt"],
            // cwd = ws/pkg. Normalize every .txt file in place.
            "for f in *.txt; do printf '%s\\n' \"$(tr a-z A-Z < \"$f\")\" > \"$f.t\" && mv \"$f.t\" \"$f\"; done",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("a.txt"), b"aaa")?;
        std::fs::write(pkg_dir.join("b.txt"), b"bbb")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let fmt = addr.format();
        let fmt_executed = |evs: &[BuildEvent]| {
            evs.iter().any(
                |e| matches!(&e.kind, BuildEventKind::ExecuteStart { addr, .. } if *addr == fmt),
            )
        };

        let (first, ev1) = resolve_collecting_events(&engine, &addr).await;
        first.expect("first resolve must succeed");
        assert!(fmt_executed(&ev1), "first run must execute");
        assert_eq!(std::fs::read(pkg_dir.join("a.txt"))?, b"AAA\n");
        assert_eq!(std::fs::read(pkg_dir.join("b.txt"))?, b"BBB\n");

        let (second, ev2) = resolve_collecting_events(&engine, &addr).await;
        second.expect("second resolve must succeed");
        assert!(
            !fmt_executed(&ev2),
            "second run over the transformed multi-file tree must be a cache hit, got {ev2:?}"
        );
        Ok(())
    }

    /// End-to-end provenance: a `copy` codegen target's output is EXCLUDED from a
    /// later `@heph/fs` glob over the same tree (so it is never double-sourced),
    /// while an `in_place` output stays visible.
    ///
    /// Nothing is registered up front. Running the target is what claims its
    /// output — the property that makes this usable: declare a codegen target, run
    /// it, and the file it writes is not source, with no command to remember and
    /// no ordering to get right.
    ///
    /// Asserted unconditionally. The xattr version of this test could only assert
    /// on a filesystem that persisted extended attributes and silently passed
    /// everywhere else — including on the tmpfs where the feature was most likely
    /// to be broken.
    #[tokio::test]
    async fn claimed_copy_output_excluded_from_later_glob() -> anyhow::Result<()> {
        // Nothing registered up front: the claim has to come from the act of
        // generating the file.
        let (engine, root) = engine_with_home_fs(vec![
            codegen_run_target("//pkg:cp", "copy", &["*.gen"], "echo generated > out.gen"),
            codegen_run_target("//pkg:ip", "in_place", &["keep.txt"], "true"),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("keep.txt"), b"keep\n")?;

        // Materialize the copy output, and write back the in_place file.
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:cp")?)
            .await
            .0
            .expect("copy target resolves");
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:ip")?)
            .await
            .0
            .expect("in_place target resolves");

        // A glob over the claimed copy output must yield nothing.
        let gen_glob = hbuiltins::pluginfs::glob_addr("pkg/*.gen", &[]);
        let (res, _) = resolve_collecting_events(&engine, &gen_glob).await;
        let gen_res = res.expect("glob over generated files resolves");
        // A glob over the unclaimed in_place output must still see it.
        let keep_glob = hbuiltins::pluginfs::glob_addr("pkg/keep.txt", &[]);
        let (res, _) = resolve_collecting_events(&engine, &keep_glob).await;
        let keep_res = res.expect("glob over in_place output resolves");

        assert!(
            gen_res.artifacts.is_empty(),
            "claimed copy output must be excluded from a later glob, got {} artifacts",
            gen_res.artifacts.len(),
        );
        assert!(
            !keep_res.artifacts.is_empty(),
            "unclaimed in_place output must remain visible to a glob",
        );
        Ok(())
    }

    /// A generated file rewritten by an outside tool the way formatters do —
    /// write a temp file, rename over it, replacing the inode — is still excluded
    /// from a later glob.
    ///
    /// This is the regression the whole mechanism exists for. The xattr lived on
    /// the inode, so `gofmt -w`, an editor save, or a `git checkout` erased it and
    /// the generated file re-entered the graph as source. The claim is declared,
    /// so nothing done to the file can revoke it.
    #[tokio::test]
    async fn claim_survives_an_outside_rewrite_of_the_generated_file() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:cp",
            "copy",
            &["*.gen"],
            "echo generated > out.gen",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;

        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:cp")?)
            .await
            .0
            .expect("copy target resolves");
        let gen_file = pkg_dir.join("out.gen");
        assert!(gen_file.exists(), "copy output must be written to the tree");

        // Rewrite it from outside heph, replacing the inode.
        let tmp = pkg_dir.join("out.gen.tmp");
        std::fs::write(&tmp, b"rewritten by some other tool\n")?;
        std::fs::rename(&tmp, &gen_file)?;

        let gen_glob = hbuiltins::pluginfs::glob_addr("pkg/*.gen", &[]);
        let (res, _) = resolve_collecting_events(&engine, &gen_glob).await;
        let gen_res = res.expect("glob over generated files resolves");
        assert!(
            gen_res.artifacts.is_empty(),
            "a rewritten generated file is still generated, got {} artifacts",
            gen_res.artifacts.len(),
        );
        Ok(())
    }

    /// A net-new `copy` codegen target materializes its file into the workspace
    /// root, and an `in_place` target's re-emitted file exists there too. Neither
    /// carries any heph-written metadata: which of them is generated is answered
    /// by the claim set the run registered, not by anything on the file.
    #[tokio::test]
    async fn writeback_materializes_both_modes() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            // Copy: generates a net-new file. The introspect input is a glob
            // (`pkg/*.gen`) so the not-yet-existing output doesn't error at
            // input resolution the way a `file()` over a missing path would.
            codegen_run_target("//pkg:cp", "copy", &["*.gen"], "echo generated > out.gen"),
            // In-place: re-emits an existing tracked source file untouched.
            codegen_run_target("//pkg:ip", "in_place", &["src.txt"], "true"),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("src.txt"), b"src\n")?;

        let cp_addr = hmodel::htaddr::parse_addr("//pkg:cp")?;
        let ip_addr = hmodel::htaddr::parse_addr("//pkg:ip")?;

        let (res, _e) = resolve_collecting_events(&engine, &cp_addr).await;
        res.expect("copy codegen target must resolve");
        let (res, _e) = resolve_collecting_events(&engine, &ip_addr).await;
        res.expect("in_place codegen target must resolve");

        assert!(
            root.path().join("pkg/out.gen").exists(),
            "copy codegen file must be written to the tree"
        );
        assert!(
            root.path().join("pkg/src.txt").exists(),
            "in_place codegen file must exist in the tree"
        );
        let claims = engine.codegen_claims.snapshot();
        assert!(
            claims.claims(std::path::Path::new("pkg/out.gen")),
            "the copy output is claimed"
        );
        assert!(
            !claims.claims(std::path::Path::new("pkg/src.txt")),
            "the in_place output is a tracked source and must NOT be claimed"
        );
        Ok(())
    }

    /// An `in_place` target must NOT write back into a tree file that another
    /// `copy` codegen target claims. Here `//pkg:cp` generates `out.gen`;
    /// `//pkg:ip` (in_place) then regenerates `out.gen` with different bytes. The
    /// guard leaves the copy-owned file untouched.
    #[tokio::test]
    async fn in_place_does_not_clobber_copy_controlled_file() -> anyhow::Result<()> {
        let (engine, root) = engine_with_home_fs(vec![
            codegen_run_target("//pkg:cp", "copy", &["*.gen"], "echo copyowned > out.gen"),
            // in_place over the same path, emitting DIFFERENT bytes.
            codegen_run_target(
                "//pkg:ip",
                "in_place",
                &["*.gen"],
                "echo clobbered > out.gen",
            ),
        ])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;

        // Copy first: writes out.gen. Then in_place tries to overwrite it.
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:cp")?)
            .await
            .0
            .expect("copy target resolves");
        resolve_collecting_events(&engine, &hmodel::htaddr::parse_addr("//pkg:ip")?)
            .await
            .0
            .expect("in_place target resolves");

        assert_eq!(
            std::fs::read(pkg_dir.join("out.gen"))?,
            b"copyowned\n",
            "in_place must not clobber a copy-controlled tree file",
        );
        Ok(())
    }

    /// `--frozen` on an in_place fmt target: when the tree does not yet match the
    /// generated output, the check fails with a typed `FrozenCheckError` and
    /// nothing is written; once the tree matches, it succeeds.
    #[tokio::test]
    async fn frozen_fails_on_dirty() -> anyhow::Result<()> {
        // The run script normalizes the input (uppercases) in place, so the
        // generated output differs from a lowercase tree file but matches an
        // already-uppercase one. Each scenario uses its OWN engine/root and runs
        // the target exactly once — avoiding the sandbox reuse that a second
        // execute on the same no-args addr would trigger.
        let run = "tr a-z A-Z < in.txt > in.txt.tmp && mv in.txt.tmp in.txt";
        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        let frozen_opts = ResultOptions {
            frozen: true,
            ..Default::default()
        };

        let seed_engine = |seed: &'static [u8]| {
            let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
                "//pkg:fmt",
                "in_place",
                &["in.txt"],
                run,
            )])?;
            let pkg_dir = root.path().join("pkg");
            std::fs::create_dir_all(&pkg_dir)?;
            std::fs::write(pkg_dir.join("in.txt"), seed)?;
            anyhow::Ok((engine, root))
        };

        // Dirty tree (lowercase): frozen must fail with a typed FrozenCheckError
        // and write nothing.
        let (engine, root) = seed_engine(b"hello\n")?;
        let tree_file = root.path().join("pkg/in.txt");
        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .err()
            .expect("frozen check on a dirty tree must error");
        drop(rs);
        // The top-level frame surfaces the recorded `TargetFailure` whose `source`
        // anyhow chain carries the original `FrozenCheckError`.
        let tf = err
            .downcast_ref::<TargetFailure>()
            .expect("top-level error must be a recorded TargetFailure");
        assert!(
            downcast_chain_ref::<crate::engine::error::FrozenCheckError>(tf.source.as_ref())
                .is_some(),
            "frozen failure must carry a FrozenCheckError, got: {err:#}"
        );
        assert_eq!(
            std::fs::read(&tree_file)?,
            b"hello\n",
            "frozen mode must not modify the tree"
        );

        // Clean tree (already uppercase): frozen must pass.
        let (engine, _root) = seed_engine(b"HELLO\n")?;
        let rs = engine.new_state();
        engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .expect("frozen check on a clean tree must succeed");
        Ok(())
    }

    /// `--frozen` must also catch an exec-bit-only divergence: the on-disk bytes
    /// match the generated output but the exec bit differs. Since `@heph/fs` now
    /// hashes (content + exec-bit), this is real drift a non-frozen run would
    /// write back, so frozen must fail rather than report the tree clean.
    #[cfg(unix)]
    #[tokio::test]
    async fn frozen_fails_on_exec_bit_drift() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let addr = hmodel::htaddr::parse_addr("//pkg:mkexec")?;
        let frozen_opts = ResultOptions {
            frozen: true,
            ..Default::default()
        };
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:mkexec",
            "in_place",
            &["*.sh"],
            // Identical bytes to the seed, but executable in the sandbox.
            "printf 'echo hi\\n' > run.sh && chmod +x run.sh",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))?;

        let rs = engine.new_state();
        let err = engine
            .clone()
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &frozen_opts)
            .await
            .err()
            .expect("frozen check must fail on exec-bit-only drift");
        drop(rs);
        let tf = err
            .downcast_ref::<TargetFailure>()
            .expect("top-level error must be a recorded TargetFailure");
        assert!(
            downcast_chain_ref::<crate::engine::error::FrozenCheckError>(tf.source.as_ref())
                .is_some(),
            "frozen failure must carry a FrozenCheckError, got: {err:#}"
        );
        // Frozen never writes: the tree file stays non-executable.
        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 == 0,
            "frozen mode must not chmod the tree (mode {mode:o})"
        );
        Ok(())
    }

    /// Mirror of `write_back_applies_exec_bit_on_unchanged_content` for the strip
    /// direction: an in_place target that emits a NON-executable file over a
    /// byte-identical executable tree file must clear the exec bit on write-back
    /// (x=false is part of the hash identity just as x=true is).
    #[cfg(unix)]
    #[tokio::test]
    async fn write_back_strips_exec_bit_on_unchanged_content() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (engine, root) = engine_with_home_fs(vec![codegen_run_target(
            "//pkg:rmexec",
            "in_place",
            &["*.sh"],
            // Identical bytes, but explicitly non-executable in the sandbox.
            "printf 'echo hi\\n' > run.sh && chmod -x run.sh",
        )])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        let script = pkg_dir.join("run.sh");
        std::fs::write(&script, b"echo hi\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

        let addr = hmodel::htaddr::parse_addr("//pkg:rmexec")?;
        let (res, _events) = resolve_collecting_events(&engine, &addr).await;
        res.expect("rmexec target resolves");

        let mode = std::fs::metadata(&script)?.permissions().mode();
        assert!(
            mode & 0o111 == 0,
            "write-back must strip the exec bit even when content is unchanged (mode {mode:o})",
        );
        Ok(())
    }

    /// A codegen target with MULTIPLE output groups must write back EVERY group's
    /// artifact. The write-back loop is driven off the cached artifacts (looking
    /// up each group's mode), so it covers all of them — not just the first one
    /// found for a group.
    #[tokio::test]
    async fn multi_group_codegen_writes_all_groups() -> anyhow::Result<()> {
        let target = pluginstatictarget::Target {
            addr: "//pkg:fmt".to_string(),
            driver: "bash".to_string(),
            run: Some(
                "tr a-z A-Z < a.txt > a.t && mv a.t a.txt; \
                 tr a-z A-Z < b.txt > b.t && mv b.t b.txt"
                    .to_string(),
            ),
            out: HashMap::from([
                ("ga".to_string(), vec!["a.txt".to_string()]),
                ("gb".to_string(), vec!["b.txt".to_string()]),
            ]),
            codegen: Some("in_place".to_string()),
            deps: HashMap::from([(
                "".to_string(),
                vec!["//@heph/introspect:outputs".to_string()],
            )]),
            labels: vec![],
            ..Default::default()
        };
        let (engine, root) = engine_with_home_fs(vec![target])?;
        let pkg_dir = root.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir)?;
        std::fs::write(pkg_dir.join("a.txt"), b"aaa\n")?;
        std::fs::write(pkg_dir.join("b.txt"), b"bbb\n")?;

        let addr = hmodel::htaddr::parse_addr("//pkg:fmt")?;
        resolve_collecting_events(&engine, &addr)
            .await
            .0
            .expect("multi-group codegen target must resolve");

        assert_eq!(
            std::fs::read(pkg_dir.join("a.txt"))?,
            b"AAA\n",
            "output group `ga` must be written back",
        );
        assert_eq!(
            std::fs::read(pkg_dir.join("b.txt"))?,
            b"BBB\n",
            "output group `gb` must be written back",
        );
        Ok(())
    }

    /// in_place targets persist two cache revisions per state (primary +
    /// fixpoint), so their GC `history` is doubled at eval time (on the def both
    /// GC paths read). copy / plain targets keep their declared history.
    #[tokio::test]
    async fn in_place_doubles_gc_history() -> anyhow::Result<()> {
        let (engine, _root) = engine_with_home_fs(vec![
            codegen_run_target("//pkg:fmt", "in_place", &["in.txt"], "true"),
            codegen_run_target("//pkg:gen", "copy", &["*.gen"], "echo x > out.gen"),
        ])?;
        let rs = engine.new_state();

        let fmt = Arc::clone(&engine)
            .get_def(rs.clone(), &hmodel::htaddr::parse_addr("//pkg:fmt")?)
            .await?;
        assert_eq!(
            fmt.target_def.cache.history, 2,
            "in_place target must double the default history (1 → 2)",
        );

        let gen_def = Arc::clone(&engine)
            .get_def(rs.clone(), &hmodel::htaddr::parse_addr("//pkg:gen")?)
            .await?;
        assert_eq!(
            gen_def.target_def.cache.history, 1,
            "copy target keeps its declared history",
        );
        Ok(())
    }

    // ----------------------------------------------------------------------
    // Terminal forwarding across transparent groups.
    //
    // `TtyReader` (crates/tui/src/tui/tty.rs) owns fd 0 exclusively and the TUI
    // pause is not refcounted, so at most one target per request may hold the
    // terminal. These tests pin the rule that enforces it: a run is interactive
    // only when it resolves to exactly one target that executes.
    // ----------------------------------------------------------------------

    /// Observes the [`InteractiveWrapper`] the way fd 0 experiences it: how many
    /// targets asked for the terminal, and how many held it *at the same time*.
    /// `max_live > 1` is the two-`TtyReader`s-on-one-fd bug.
    #[derive(Default)]
    struct TerminalProbe {
        calls: AtomicUsize,
        live: AtomicUsize,
        max_live: AtomicUsize,
    }

    /// Wrapper that records its own concurrency, then runs the target.
    ///
    /// The assertions that matter are on `calls`, which is 0-or-1 by
    /// construction and so deterministic. The sleep only sharpens `max_live`:
    /// it is an await point every sibling reaches before any of them finishes,
    /// so with enough execute-semaphore permits (hence `parallelism: Some(4)`)
    /// a shared terminal reads as 2 rather than 1. It cannot turn a red
    /// `calls` assertion green.
    fn probe_wrapper(probe: SArc<TerminalProbe>) -> InteractiveWrapper {
        SArc::new(move |inner: InteractiveInner| {
            let probe = SArc::clone(&probe);
            Box::pin(async move {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                let live = probe.live.fetch_add(1, Ordering::SeqCst) + 1;
                probe.max_live.fetch_max(live, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                let res = inner(None, None, None).await;
                probe.live.fetch_sub(1, Ordering::SeqCst);
                res
            })
        })
    }

    /// Test driver for the terminal tests. A spec carrying `group = [addr, …]`
    /// parses as a transparent group over those addrs; anything else is a leaf
    /// that executes (and so reaches the interactive wrapper). A leaf carrying
    /// `fail = true` returns a [`ProcessFailed`] over a real log file, so the
    /// recorded failure's log tail can be asserted.
    struct TerminalDriver {
        runs: SArc<AtomicUsize>,
        shell_runs: SArc<AtomicUsize>,
        log_path: std::path::PathBuf,
    }

    /// Label the driver stamps on a leaf whose `run` must fail; `TargetSpec`
    /// config is not visible from `run`, so the marker rides on the `TargetDef`.
    const FAIL_LABEL: &str = "terminal-test:fail";

    #[async_trait]
    impl RawDriver for TerminalDriver {
        fn config(&self, _req: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
            Ok(DriverConfigResponse {
                name: "terminal".to_string(),
            })
        }
        fn schema(&self) -> crate::engine::driver::DriverSchema {
            crate::engine::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            req: ParseRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            let transparent = req.target_spec.config.contains_key("group");
            let key = if transparent { "group" } else { "deps" };
            let members = match req.target_spec.config.get(key) {
                Some(hcore::htvalue::Value::List(items)) => items
                    .iter()
                    .map(|v| match v {
                        hcore::htvalue::Value::String(s) => {
                            crate::engine::driver::TargetAddr::parse(
                                s,
                                &req.target_spec.addr.package,
                            )
                        }
                        other => anyhow::bail!("member must be a string, got {other:?}"),
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?,
                _ => vec![],
            };
            let inputs = members
                .into_iter()
                .enumerate()
                .map(|(i, r#ref)| Input {
                    r#ref,
                    mode: crate::engine::driver::targetdef::InputMode::Standard,
                    origin_id: format!("group:{i}"),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                })
                .collect();
            let fails = matches!(
                req.target_spec.config.get("fail"),
                Some(hcore::htvalue::Value::Bool(true))
            );
            Ok(ParseResponse {
                target_def: TargetDef {
                    addr: req.target_spec.addr.clone(),
                    labels: if fails {
                        vec![FAIL_LABEL.to_string()]
                    } else {
                        vec![]
                    },
                    raw_def: SArc::new(()),
                    inputs,
                    outputs: if transparent {
                        vec![]
                    } else {
                        vec![Output {
                            group: "main".to_string(),
                            paths: vec![],
                        }]
                    },
                    support_files: vec![],
                    cache: if transparent {
                        CacheConfig::off()
                    } else {
                        CacheConfig::on(false)
                    },
                    pty: false,
                    hash: req.target_spec.addr.format().into_bytes(),
                    transparent,
                },
            })
        }
        async fn apply_transitive(
            &self,
            req: ApplyTransitiveRequest,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            Ok(ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if req.target.labels.iter().any(|l| l == FAIL_LABEL) {
                return Err(anyhow::Error::new(crate::engine::error::ProcessFailed {
                    status: "exit status: 1".to_string(),
                    log: SArc::new(hcore::hartifactcontent::FileContent::new(&self.log_path)),
                })
                .context("driver run"));
            }
            Ok(RunResponse {
                artifacts: vec![outputartifact::OutputArtifact {
                    group: "main".to_string(),
                    name: "out".to_string(),
                    r#type: outputartifact::Type::Output,
                    content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                        data: b"hi".to_vec(),
                        path: "out".to_string(),
                        x: false,
                    }),
                    hashout: "feedface".to_string(),
                }],
                sandbox_cleanup: None,
                sandbox_guards: vec![],
            })
        }
        async fn run_shell<'a, 'io>(
            &self,
            _req: RunRequest<'a, 'io>,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<RunResponse> {
            self.shell_runs.fetch_add(1, Ordering::SeqCst);
            Ok(RunResponse {
                artifacts: vec![],
                sandbox_cleanup: None,
                sandbox_guards: vec![],
            })
        }
    }

    /// Provider serving a fixed set of `TargetSpec`s. Listable, so the batch
    /// (matcher) path can be driven through it as well as `result_addr`.
    struct SpecsProvider {
        targets: Vec<TargetSpec>,
    }

    impl crate::engine::provider::Provider for SpecsProvider {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "specs".to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            let items: Vec<anyhow::Result<ListResponse>> = self
                .targets
                .iter()
                .filter(|t| t.addr.package == req.package)
                .map(|t| {
                    Ok(ListResponse {
                        addr: t.addr.clone(),
                    })
                })
                .collect();
            Box::pin(async move {
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
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
            let mut pkgs: Vec<PkgBuf> = Vec::new();
            for t in &self.targets {
                if !pkgs.contains(&t.addr.package) {
                    pkgs.push(t.addr.package.clone());
                }
            }
            let items: Vec<anyhow::Result<ListPackageResponse>> = pkgs
                .into_iter()
                .map(|pkg| Ok(ListPackageResponse { pkg }))
                .collect();
            Box::pin(async move {
                Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = _> + Send>)
            })
        }
        fn get<'a>(
            &'a self,
            req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            let found = self.targets.iter().find(|t| t.addr == req.addr).cloned();
            Box::pin(async move {
                match found {
                    Some(target_spec) => Ok(GetResponse { target_spec }),
                    None => Err(GetError::NotFound),
                }
            })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    /// A transparent group over `members`.
    fn group_spec(addr: &str, members: &[&str]) -> anyhow::Result<TargetSpec> {
        Ok(TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr)?,
            driver: "terminal".to_string(),
            config: HashMap::from([(
                "group".to_string(),
                hcore::htvalue::Value::List(
                    members
                        .iter()
                        .map(|m| hcore::htvalue::Value::String((*m).to_string()))
                        .collect(),
                ),
            )]),
            ..Default::default()
        })
    }

    /// An executable leaf; `fail` makes its `run` return a `ProcessFailed`.
    fn leaf_spec(addr: &str, fail: bool) -> anyhow::Result<TargetSpec> {
        let mut config = HashMap::new();
        if fail {
            config.insert("fail".to_string(), hcore::htvalue::Value::Bool(true));
        }
        Ok(TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr)?,
            driver: "terminal".to_string(),
            config,
            ..Default::default()
        })
    }

    /// An executable leaf with real (non-group) dependencies — the path
    /// `inputs_result_exec` resolves with `ResultOptions::default()`.
    fn leaf_with_deps_spec(addr: &str, deps: &[&str]) -> anyhow::Result<TargetSpec> {
        Ok(TargetSpec {
            addr: hmodel::htaddr::parse_addr(addr)?,
            driver: "terminal".to_string(),
            config: HashMap::from([(
                "deps".to_string(),
                hcore::htvalue::Value::List(
                    deps.iter()
                        .map(|d| hcore::htvalue::Value::String((*d).to_string()))
                        .collect(),
                ),
            )]),
            ..Default::default()
        })
    }

    struct TerminalHarness {
        engine: Arc<Engine>,
        probe: SArc<TerminalProbe>,
        runs: SArc<AtomicUsize>,
        shell_runs: SArc<AtomicUsize>,
        _root: tempfile::TempDir,
        _logs: tempfile::TempDir,
    }

    /// Engine wired to [`TerminalDriver`] + [`SpecsProvider`], with a 12-line
    /// process log on disk for the failing-leaf case.
    fn terminal_harness(targets: Vec<TargetSpec>) -> anyhow::Result<TerminalHarness> {
        let root = tempdir()?;
        let logs = tempdir()?;
        let log_path = logs.path().join("log.txt");
        std::fs::write(
            &log_path,
            (1..=12).map(|i| format!("line{i}\n")).collect::<String>(),
        )?;

        let runs = SArc::new(AtomicUsize::new(0));
        let shell_runs = SArc::new(AtomicUsize::new(0));
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            // Enough workers that concurrent members are genuinely concurrent —
            // otherwise the execute semaphore would serialize them and hide the
            // shared-terminal overlap this suite is looking for.
            parallelism: Some(4),
            ..Default::default()
        })?;
        engine.register_driver(enclose!((runs, shell_runs, log_path) move |_| {
            Box::new(TerminalDriver {
                runs,
                shell_runs,
                log_path,
            })
        }))?;
        engine.register_provider(move |_| Box::new(SpecsProvider { targets }))?;
        Ok(TerminalHarness {
            engine: Arc::new(engine),
            probe: SArc::new(TerminalProbe::default()),
            runs,
            shell_runs,
            _root: root,
            _logs: logs,
        })
    }

    impl TerminalHarness {
        fn opts(&self, shell: bool) -> ResultOptions {
            ResultOptions {
                shell,
                interactive: Some(probe_wrapper(SArc::clone(&self.probe))),
                ..Default::default()
            }
        }
    }

    /// Reproduction for the shared-tty bug: a group with two uncached members
    /// used to propagate `interactive` to both, so both built a `TtyReader` on
    /// fd 0, both paused/resumed the TUI, and whichever finished first restored
    /// blocking mode on the fd its sibling was still reading. No member may get
    /// the terminal — and in particular two of them may never hold it at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_member_group_forwards_the_terminal_to_no_member() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(
            h.runs.load(Ordering::SeqCst),
            2,
            "both members must still execute",
        );
        assert_eq!(
            h.probe.max_live.load(Ordering::SeqCst),
            0,
            "no group member may own the terminal",
        );
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            0,
            "the interactive wrapper must not reach any group member",
        );
        Ok(())
    }

    /// The rule is "exactly one target that executes", not "groups are never
    /// interactive": a group used as an alias is its member, at any nesting
    /// depth. `//pkg:g0` → `//pkg:g1` → `//pkg:a` must hand the terminal to
    /// `//pkg:a` exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_member_group_forwards_the_terminal_through_nesting() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g0", &["//pkg:g1"])?,
            group_spec("//pkg:g1", &["//pkg:a"])?,
            leaf_spec("//pkg:a", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g0")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            1,
            "a nested single-member group is its member: it keeps the terminal",
        );
        assert_eq!(
            h.probe.max_live.load(Ordering::SeqCst),
            1,
            "…and exactly one holder at a time",
        );
        Ok(())
    }

    /// `--shell` on a group that is not a single target is refused at the group
    /// frame, with a message naming the group, its members, and the command to
    /// run instead. Previously the member frames bailed with "cannot use --shell
    /// in non-interactive mode", which is false — the user *is* on a terminal.
    #[tokio::test]
    async fn shell_on_multi_member_group_names_the_members() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        let err = Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(true))
            .await
            .err()
            .expect("--shell on a multi-member group must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--shell needs exactly one target"),
            "msg: {msg}"
        );
        assert!(
            msg.contains("//pkg:g is a group with 2 members"),
            "msg: {msg}"
        );
        assert!(msg.contains("members: //pkg:a, //pkg:b"), "msg: {msg}");
        assert!(msg.contains("try: heph run --shell //pkg:a"), "msg: {msg}");
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            0,
            "no member may be entered once --shell is refused",
        );
        Ok(())
    }

    /// …and `--shell` on a single-member group shells into that member, because
    /// an alias *is* its target.
    #[tokio::test]
    async fn shell_on_single_member_group_shells_into_the_member() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a"])?,
            leaf_spec("//pkg:a", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(true))
            .await?;

        assert_eq!(
            h.shell_runs.load(Ordering::SeqCst),
            1,
            "the single member must be shelled into",
        );
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            1,
            "…through the interactive wrapper, exactly once",
        );
        Ok(())
    }

    /// `classify_failure` drops the captured log tail for interactive targets on
    /// the grounds that their output already streamed to the terminal. A group
    /// member never streamed anything, so while `interactive` was propagated to
    /// members a failing one lost its log tail — `heph run //pkg:group` gave
    /// *worse* diagnostics than `heph run //...`. Not forwarding the terminal
    /// restores it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failing_member_of_multi_member_group_keeps_its_log_tail() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", true)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;
        let failing = hmodel::htaddr::parse_addr("//pkg:a")?;

        Arc::clone(&h.engine)
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &h.opts(false))
            .await
            .err()
            .expect("a failing member must fail the group");

        let recorded = rs.get_failure(&failing).expect("failure must be recorded");
        let tail = recorded
            .log_tail
            .as_ref()
            .expect("a non-interactive member keeps its process log tail");
        assert!(tail.text.contains("line12"), "tail: {}", tail.text);
        Ok(())
    }

    /// The general rule the transparent-group gate is an instance of:
    /// dependencies never inherit the terminal.
    ///
    /// Characterization rather than a mutation-provable assertion —
    /// deps are built by `meta` while computing the parent's `hashin`, and
    /// `meta` is memoized per addr with no `opts` in scope, so there is no line
    /// to flip. (Verified: deliberately handing the wrapper to
    /// `inputs_result_exec` changes nothing, because by then every dep is
    /// already a memoizer hit.) The test guards against a future refactor that
    /// threads the caller's options into either path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deps_never_inherit_the_terminal() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            leaf_with_deps_spec("//pkg:root", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:root")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(h.runs.load(Ordering::SeqCst), 3, "root plus both deps run");
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            1,
            "only the requested target may own the terminal",
        );
        assert_eq!(h.probe.max_live.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// Third leg: a batch (matcher) request is many targets, so none of them
    /// gets the terminal — `Engine::result` clears `interactive` up front.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn batch_matcher_forwards_the_terminal_to_nobody() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let matcher = Matcher::Package(PkgBuf::from("pkg"));

        Arc::clone(&h.engine)
            .result(rs, &matcher, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(h.runs.load(Ordering::SeqCst), 2, "both targets must run");
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            0,
            "a multi-target selection gives the terminal to nobody",
        );
        Ok(())
    }

    /// The composition the commit claims to handle "at any nesting depth": the
    /// terminal is forwarded through a single-member group and then dropped by
    /// the multi-member group beneath it. This is the only path where the
    /// `take()` runs on a wrapper that was actually forwarded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_member_group_under_single_member_group_drops_the_terminal() -> anyhow::Result<()>
    {
        let h = terminal_harness(vec![
            group_spec("//pkg:g0", &["//pkg:g1"])?,
            group_spec("//pkg:g1", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g0")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(h.runs.load(Ordering::SeqCst), 2, "both members must run");
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            0,
            "the forwarded terminal must be dropped at the multi-member frame",
        );
        Ok(())
    }

    /// A group listing the same member twice (different output filters) is
    /// still one executing target: both entries share the addr-keyed
    /// `mem_locked_result` / `mem_execute_cache` cells, so there is exactly one
    /// execute and exactly one terminal holder. Counting input *entries* rather
    /// than distinct members would drop the terminal here and print "a group
    /// with 2 members: //pkg:a, //pkg:a".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn group_listing_one_member_twice_is_still_a_single_target() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a", "//pkg:a"])?,
            leaf_spec("//pkg:a", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(h.runs.load(Ordering::SeqCst), 1, "one target, one execute");
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            1,
            "one distinct member is one target: it keeps the terminal",
        );
        Ok(())
    }

    /// An empty group is legal (`group(name = "g")` with no `deps`). It runs
    /// nothing, so it needs no terminal — and nothing may index `inputs[0]`.
    #[tokio::test]
    async fn empty_group_runs_nothing_and_takes_no_terminal() -> anyhow::Result<()> {
        let h = terminal_harness(vec![group_spec("//pkg:g", &[])?])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;

        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(h.probe.calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// …and `--shell` on it still says something actionable, even with no
    /// member to name.
    #[tokio::test]
    async fn shell_on_empty_group_reports_zero_members() -> anyhow::Result<()> {
        let h = terminal_harness(vec![group_spec("//pkg:g", &[])?])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        let err = Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(true))
            .await
            .err()
            .expect("--shell on an empty group must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("is a group with 0 members"), "msg: {msg}");
        assert!(msg.contains("the group is empty"), "msg: {msg}");
        assert!(
            msg.contains("try: heph run --shell"),
            "an actionable message must still name an action: {msg}",
        );
        Ok(())
    }

    /// `--shell` on a group nested inside a single-member group is the *same*
    /// user error as at the top, and must render the same way. It is a property
    /// of the request, not a target failure: `classify_failure` propagates it
    /// unchanged, so no failure is recorded and the CLI does not print a
    /// failed-target box for a group that never executed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shell_error_from_a_nested_group_is_not_recorded_as_a_target_failure()
    -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g0", &["//pkg:g1"])?,
            group_spec("//pkg:g1", &["//pkg:a", "//pkg:b"])?,
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:g0")?;

        let err = Arc::clone(&h.engine)
            .result_addr(rs.clone(), &addr, OutputMatcher::All, &h.opts(true))
            .await
            .err()
            .expect("--shell on a nested multi-member group must fail");
        assert!(
            downcast_chain_ref::<ShellNeedsSingleTarget>(&err).is_some(),
            "the request-shape error must survive the nesting: {err:#}",
        );
        assert!(
            msg_names_group(&err, "//pkg:g1"),
            "the message must name the group that is not a single target: {err:#}",
        );
        assert!(
            rs.take_failures().is_empty(),
            "a request-shape error is nobody's target failure",
        );
        Ok(())
    }

    fn msg_names_group(err: &anyhow::Error, addr: &str) -> bool {
        format!("{err:#}").contains(addr)
    }

    /// `--shell` with a multi-target selection is refused once, naming the
    /// selection — not once per matched target with "cannot use --shell in
    /// non-interactive mode", which is false for a user sitting at a terminal.
    #[tokio::test]
    async fn shell_with_a_multi_target_selection_names_the_selection() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            leaf_spec("//pkg:a", false)?,
            leaf_spec("//pkg:b", false)?,
        ])?;
        let rs = h.engine.new_state();
        let matcher = Matcher::Package(PkgBuf::from("pkg"));

        let err = Arc::clone(&h.engine)
            .result(rs, &matcher, OutputMatcher::All, &h.opts(true))
            .await
            .err()
            .expect("--shell on a selection must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--shell needs exactly one target"),
            "msg: {msg}"
        );
        assert!(msg.contains("selects many"), "msg: {msg}");
        assert!(
            !msg.contains("non-interactive mode"),
            "the user is on a terminal; do not claim otherwise: {msg}",
        );
        assert_eq!(
            h.runs.load(Ordering::SeqCst),
            0,
            "nothing may run once --shell is refused",
        );
        Ok(())
    }

    /// `--shell` on a single target with no terminal attached is refused with
    /// a typed error naming the addr and the next action — not a bare
    /// `anyhow::bail!` string with neither.
    #[tokio::test]
    async fn shell_without_a_terminal_names_the_addr_and_the_next_step() -> anyhow::Result<()> {
        let h = terminal_harness(vec![leaf_spec("//pkg:a", false)?])?;
        let rs = h.engine.new_state();
        let addr = hmodel::htaddr::parse_addr("//pkg:a")?;
        let opts = ResultOptions {
            shell: true,
            interactive: None,
            ..Default::default()
        };

        let err = Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &opts)
            .await
            .err()
            .expect("--shell with no terminal must fail");

        let typed = downcast_chain_ref::<ShellNeedsSingleTarget>(&err)
            .expect("must be the typed shell error, not a bare anyhow string");
        assert!(
            matches!(typed, ShellNeedsSingleTarget::NotInteractive { addr } if addr.format() == "//pkg:a"),
            "the error must carry the addr that was refused: {err:#}"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("//pkg:a"), "msg: {msg}");
        assert!(
            msg.contains("try: run `heph run --shell //pkg:a`"),
            "an actionable message must name the next command: {msg}"
        );
        assert_eq!(
            h.runs.load(Ordering::SeqCst),
            0,
            "nothing may run once --shell is refused",
        );
        Ok(())
    }

    /// The warm path: a single-member group whose member is already cached
    /// executes nothing, so it takes no terminal — `probe.calls` counts
    /// executes, and the whole suite rests on that. `--shell` still shells in,
    /// because the shell path is never served from cache.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cached_single_member_group_takes_no_terminal_but_still_shells() -> anyhow::Result<()> {
        let h = terminal_harness(vec![
            group_spec("//pkg:g", &["//pkg:a"])?,
            leaf_spec("//pkg:a", false)?,
        ])?;
        let addr = hmodel::htaddr::parse_addr("//pkg:g")?;

        // Cold: executes, and takes the terminal.
        let rs = h.engine.new_state();
        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;
        assert_eq!(h.runs.load(Ordering::SeqCst), 1);
        assert_eq!(h.probe.calls.load(Ordering::SeqCst), 1);

        // Warm: cache hit, nothing executes, nothing needs the terminal.
        let rs = h.engine.new_state();
        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(false))
            .await?;
        assert_eq!(
            h.runs.load(Ordering::SeqCst),
            1,
            "warm run must not execute"
        );
        assert_eq!(
            h.probe.calls.load(Ordering::SeqCst),
            1,
            "a cache hit needs no terminal",
        );

        // …but `--shell` is never a cache hit.
        let rs = h.engine.new_state();
        Arc::clone(&h.engine)
            .result_addr(rs, &addr, OutputMatcher::All, &h.opts(true))
            .await?;
        assert_eq!(
            h.shell_runs.load(Ordering::SeqCst),
            1,
            "--shell on a cached alias still shells into the member",
        );
        assert_eq!(h.probe.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    /// The codegen write-back must not run on a tokio worker.
    ///
    /// It is the heaviest synchronous read on the result path: it walks every
    /// generated file, pulls its bytes out of the cache, reads the tree file back
    /// and (when frozen) diffs the two. The cache read underneath additionally
    /// parks the calling thread until any queued sqlite write to that artifact
    /// has been committed — and the artifacts it walks come straight from
    /// `cache_locally`, which queues them.
    ///
    /// The content records whether it was walked inside a `blocking::run` job
    /// (`hcore::blocking::in_blocking_job` is the witness — tokio's blocking
    /// threads carry no distinguishing name). Asserted positively: "not on a
    /// worker" would pass vacuously, since `#[tokio::test]` runs the body on
    /// the test thread.
    #[tokio::test]
    async fn codegen_write_back_runs_off_the_runtime_workers() -> anyhow::Result<()> {
        use crate::engine::driver::targetdef::path;

        /// A one-file tar that records whether its walk ran inside a
        /// `blocking::run` job.
        struct ThreadRecordingTar {
            bytes: Vec<u8>,
            in_job: Arc<std::sync::Mutex<Option<bool>>>,
        }

        impl Content for ThreadRecordingTar {
            fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
                Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
            }
            fn walk(
                &self,
            ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>>
            {
                *self.in_job.lock().expect("witness slot") =
                    Some(hcore::blocking::in_blocking_job());
                Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
                    std::io::Cursor::new(self.bytes.clone()),
                )?))
            }
            fn hashout(&self) -> anyhow::Result<String> {
                Ok("HO".to_string())
            }
        }

        let root = tempdir()?;
        let engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;

        let mut packer = hcore::hartifactcontent::tar::TarPacker::new();
        packer.create_raw(b"generated\n".to_vec(), "gen.txt", false);
        let mut bytes = Vec::new();
        packer.pack(&mut bytes)?;
        let in_job = Arc::new(std::sync::Mutex::new(None));

        let def = LinkedTargetDef {
            target: Arc::new(TargetDef {
                addr: hmodel::htaddr::parse_addr("//pkg:gen")?,
                labels: Vec::new(),
                raw_def: Arc::new(()),
                inputs: Vec::new(),
                outputs: vec![crate::engine::driver::targetdef::Output {
                    group: "out".to_string(),
                    paths: vec![path::Path {
                        content: path::Content::FilePath("gen.txt".to_string()),
                        codegen_tree: path::CodegenMode::Copy,
                        collect: false,
                    }],
                }],
                support_files: Vec::new(),
                cache: crate::engine::driver::targetdef::CacheConfig::on(false),
                pty: false,
                hash: Vec::new(),
                transparent: false,
            }),
            inputs: Vec::new(),
        };
        let cached = vec![ResultArtifact {
            content: Arc::new(ThreadRecordingTar {
                bytes,
                in_job: Arc::clone(&in_job),
            }),
            group: "out".to_string(),
            r#type: ManifestArtifactType::Output,
        }];

        let wrote = engine
            .materialize_codegen(true, &def, &cached, false)
            .await?;

        assert!(wrote, "the tree must actually be written back");
        assert_eq!(
            std::fs::read_to_string(root.path().join("gen.txt"))?,
            "generated\n",
        );
        let recorded = *in_job.lock().expect("witness slot");
        assert_eq!(
            recorded,
            Some(true),
            "the codegen write-back must run inside a blocking::run job (None = never walked)"
        );
        Ok(())
    }
}
