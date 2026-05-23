use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::{CycleError, TargetNotFoundError};
use crate::engine::provider::{
    GetError, GetRequest, GetResponse, ListRequest, ProviderExecutor, TargetSpec,
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
use crate::hartifactcontent::Content;
use crate::htmatcher::Matcher;
use anyhow::Context;
use futures::TryStreamExt;
use std::fmt;
use std::sync::{Arc, Weak};
use std::{fs, io};
use tokio::task::JoinSet;

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
}

#[derive(Clone)]
pub struct ArtifactMeta {
    pub hashout: String,
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

impl Engine {
    #[async_recursion]
    pub async fn result_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<Arc<EResult>> {
        if opts.shell && opts.interactive.is_none() {
            anyhow::bail!("cannot use --shell in non-interactive mode");
        }

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
        let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;
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
            let results = futures::future::try_join_all(futures).await?;
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
        let res = rs.data.mem_result.once(key, enclose!((self => engine, rs, addr, outputs) move || async move {
            engine.inner_result_addr(rs, &addr, outputs, &opts).await.map(Arc::new).with_context(|| format!("result: {}", addr))
        })).await.map_err(unwrap_arc_err)?;

        Ok(res)
    }

    pub async fn result(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<Vec<Arc<EResult>>> {
        let mut opts = opts.clone();
        if !matches!(matcher, Matcher::Addr(_)) {
            opts.interactive = None;
        }

        let mut set = JoinSet::new();

        let stream = Arc::clone(&self).query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            crate::hmemoizer::join_set_spawn(
                &mut set,
                enclose!((self => engine, rs, opts) async move {
                    engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await
                }),
            );
        }

        let mut all_res: Vec<Arc<EResult>> = vec![];
        while let Some(res) = set.join_next().await {
            all_res.push(res??)
        }

        Ok(all_res)
    }

    async fn inner_result_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<EResult> {
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
            rs,
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
    }

    async fn execute_and_cache(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        outputs: Vec<String>,
        opts: &ExecuteOptions<'_>,
    ) -> anyhow::Result<EResult> {
        if !opts.force
            && opts.def.target.cache
            && !opts.shell
            && let Some(res) = self
                .result_from_cache(rs.clone(), def, opts, outputs.clone())
                .await?
        {
            return Ok(res);
        }

        let (cached_artifacts, artifacts_meta) = self
            .clone()
            .execute_and_cache_inner(rs.clone(), opts)
            .await?;

        let mut artifacts: Vec<Arc<dyn Content>> = Vec::new();
        let mut support_artifacts: Vec<Arc<dyn Content>> = Vec::new();
        for a in cached_artifacts {
            match a.r#type {
                ManifestArtifactType::Output if outputs.contains(&a.group) => {
                    artifacts.push(Arc::new(a) as Arc<dyn Content>);
                }
                ManifestArtifactType::SupportFile => {
                    support_artifacts.push(Arc::new(a) as Arc<dyn Content>);
                }
                _ => {}
            }
        }
        Ok(EResult {
            artifacts,
            support_artifacts,
            artifacts_meta,
        })
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
        let use_tmp_cache = !opts.def.target.cache || opts.shell;
        let interactive = opts.interactive.clone();
        let shell = opts.shell;
        let key = (addr.clone(), hashin.clone());

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    crate::hmemoizer::set_phase("execute_cache:engine_execute");
                    let (artifacts, sandbox_dir) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin, interactive, shell)
                        .await?;

                    let artifacts_meta = artifacts
                        .iter()
                        .filter(|a| matches!(
                            a.r#type,
                            outputartifact::Type::Output | outputartifact::Type::SupportFile
                        ))
                        .map(|a| Ok(ArtifactMeta { hashout: a.hashout()? }))
                        .collect::<anyhow::Result<Vec<_>>>()?;

                    defer! {
                        let sandbox_dir = sandbox_dir.clone();
                        tokio::task::spawn_blocking(move || {
                            match fs::remove_dir_all(&sandbox_dir) {
                                Ok(_) => (),
                                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                                Err(err) => tracing::error!(error = %err, "failed to clean up sandbox"),
                            }
                        });
                    }

                    crate::hmemoizer::set_phase("execute_cache:cache_locally");
                    let out = engine
                        .cache_locally(rs.ctoken(), &addr, &hashin, artifacts, use_tmp_cache)
                        .await
                        .map(|cached| (cached, artifacts_meta));
                    crate::hmemoizer::clear_phase();
                    out
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn result_from_cache(
        &self,
        rs: Arc<RequestState>,
        def: &LinkedTargetDef,
        opts: &ExecuteOptions<'_>,
        outputs: Vec<String>,
    ) -> anyhow::Result<Option<EResult>> {
        let (cached_artifacts, artifacts_meta) = match self
            .artifacts_from_local_cache(rs.ctoken(), def, opts.hashin.as_str(), outputs)
            .await?
        {
            Some(res) => res,
            None => return Ok(None),
        };

        let mut artifacts: Vec<Arc<dyn Content>> = Vec::new();
        let mut support_artifacts: Vec<Arc<dyn Content>> = Vec::new();
        for a in cached_artifacts {
            match a.r#type {
                ManifestArtifactType::Output => {
                    artifacts.push(Arc::new(a) as Arc<dyn Content>);
                }
                ManifestArtifactType::SupportFile => {
                    support_artifacts.push(Arc::new(a) as Arc<dyn Content>);
                }
                ManifestArtifactType::Log => {}
            }
        }
        Ok(Some(EResult {
            artifacts,
            support_artifacts,
            artifacts_meta,
        }))
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
        }))
    }

    async fn collect_transitive_deps(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: &[Input],
    ) -> anyhow::Result<Sandbox> {
        let futures = inputs.iter().enumerate().map(|(i, input)| {
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

        let results = futures::future::try_join_all(futures).await?;

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

    async fn get_spec_inner(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<EngineTargetSpec>> {
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
                        states: vec![], // TODO
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
    use crate::hasync::Cancellable;
    use crate::htmatcher::Matcher;
    use crate::htpkg::PkgBuf;
    use futures::future::BoxFuture;
    use std::sync::Arc as SArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

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
        ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>>
        {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>) })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>,
        > {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _>>) })
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

    #[tokio::test]
    async fn skip_providers_excludes_provider_from_query() -> anyhow::Result<()> {
        let root = tempdir()?;
        let list_calls = SArc::new(AtomicUsize::new(0));
        let list_calls_clone = SArc::clone(&list_calls);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
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
            Some(crate::loosespecparser::TargetSpecValue::List(l)) => l,
            other => panic!("expected deps list, got {other:?}"),
        };
        let dep_strs: Vec<String> = deps
            .iter()
            .map(|v| match v {
                crate::loosespecparser::TargetSpecValue::String(s) => s.clone(),
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
                Some(crate::loosespecparser::TargetSpecValue::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        crate::loosespecparser::TargetSpecValue::String(s) => s.clone(),
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
    async fn cycle_detection_returns_typed_cycle_error() -> anyhow::Result<()> {
        let root = tempdir()?;
        let engine = Arc::new(Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
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
}
