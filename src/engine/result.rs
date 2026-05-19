use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::{CycleError, TargetNotFoundError};
use crate::engine::provider::{
    GetError, GetRequest, GetResponse, ListRequest, ProviderExecutor, TargetSpec,
};
use crate::engine::request_state::RequestState;
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
                    if rs.skip_providers.contains(&provider.name) {
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

#[derive(Clone)]
pub enum OutputMatcher {
    None,
    All,
    Exact(Vec<String>),
}

impl OutputMatcher {
    fn cache_key(&self) -> String {
        match self {
            OutputMatcher::None => "none".to_string(),
            OutputMatcher::All => "all".to_string(),
            OutputMatcher::Exact(names) => {
                let mut sorted = names.clone();
                sorted.sort();
                format!("exact:{}", sorted.join(","))
            }
        }
    }
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
            let mut dag = rs.data.dep_dag.lock().expect("dep_dag mutex poisoned");
            dag.add_dep(parent, addr).map_err(|_e| {
                anyhow::Error::new(CycleError {
                    from: parent.clone(),
                    to: addr.clone(),
                })
            })?;
        }

        // Set addr as parent so all sub-calls carry the right context for cycle detection.
        // Done outside the memoizer so context setup isn't buried in the deduplication boundary.
        let rs = rs.with_parent(addr.clone());

        // Transparent targets (groups) never execute — inline their deps' results.
        // Handled before the memoizer: nothing to deduplicate for groups, and calling
        // result_addr recursively here is safe because #[async_recursion] boxes the future,
        // breaking the Send inference cycle that would occur inside the memoizer closure.
        let def = Arc::clone(&self).get_def(rs.clone(), addr).await?;
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
                    .artifacts_meta
                    .extend(r.artifacts_meta.iter().cloned());
            }
            return Ok(Arc::new(merged));
        }

        let key = format!("{}:{}", addr.format(), outputs.cache_key());
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
            set.spawn(enclose!((self => engine, rs, opts) async move {
                engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await
            }));
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
        let spec = Arc::clone(&self).get_spec(rs.clone(), addr).await?;
        let def = Arc::clone(&self).get_def(rs.clone(), addr).await?;

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

        Ok(EResult {
            artifacts: cached_artifacts
                .into_iter()
                .filter(|a| a.r#type == ManifestArtifactType::Output && outputs.contains(&a.group))
                .map(|a| Arc::new(a) as Arc<dyn Content>)
                .collect(),
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
        let key = format!("{}:{}", addr.format(), hashin);

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    let (artifacts, sandbox_dir) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin, interactive, shell)
                        .await?;

                    let artifacts_meta = artifacts
                        .iter()
                        .filter(|a| a.r#type == outputartifact::Type::Output)
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

                    engine
                        .cache_locally(rs.ctoken(), &addr, &hashin, artifacts, use_tmp_cache)
                        .await
                        .map(|cached| (cached, artifacts_meta))
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

        Ok(Some(EResult {
            artifacts: cached_artifacts
                .into_iter()
                .map(|a| Arc::new(a) as Arc<dyn Content>)
                .collect(),
            artifacts_meta,
        }))
    }

    #[async_recursion]
    pub async fn get_def(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        rs.data
            .mem_def
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.get_def_inner(rs, &addr, true).await
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
        self.get_def_inner(rs, addr, false).await
    }

    async fn get_def_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        apply_transitive: bool,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        let spec = Arc::clone(&self).get_spec(rs.clone(), addr).await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver
            .driver
            .parse(
                ParseRequest {
                    request_id: rs.request_id().to_string(),
                    target_spec: Arc::clone(&spec),
                },
                rs.ctoken(),
            )
            .await
            .with_context(|| "parse")?;
        let def = res.target_def;

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

    pub async fn get_spec(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<TargetSpec>> {
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
    ) -> anyhow::Result<Arc<TargetSpec>> {
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

            return anyhow::Ok(Arc::new(spec));
        }

        Err(TargetNotFoundError { addr: addr.clone() }.into())
    }
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
            .query(&Matcher::Package(PkgBuf::from("any")))
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

    #[tokio::test]
    async fn cycle_detection_returns_typed_cycle_error() -> anyhow::Result<()> {
        let root = tempdir()?;
        let engine = Arc::new(Engine::new(Config {
            root: root.path().to_path_buf(),
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
