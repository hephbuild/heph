use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::driver::{ApplyTransitiveRequest, ParseRequest, outputartifact};
use crate::engine::error::TargetNotFoundError;
use crate::engine::provider::{GetError, GetRequest, GetResponse, ProviderExecutor, TargetSpec};
use crate::engine::request_state::RequestState;
use crate::hmemoizer::unwrap_arc_err;
use crate::htaddr::Addr;
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
    ) -> futures::future::BoxFuture<'a, anyhow::Result<EResult>> {
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
}

impl fmt::Debug for dyn Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Artifact")
    }
}

pub struct ExtendedTargetDef {
    pub target_def: TargetDef,
    pub applied_transitive: Sandbox,
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

#[derive(Default, Clone, Copy)]
pub struct ResultOptions {
    pub force: bool,
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
}

impl Engine {
    #[async_recursion]
    pub async fn result_addr(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        outputs: OutputMatcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<EResult> {
        // Cycle check: fires for every caller (including those awaiting an in-flight future)
        // before the memoizer blocks, preventing memoizer deadlocks on dependency cycles.
        if let Some(ref parent) = rs.parent {
            let mut dag = rs.data.dep_dag.lock().await;
            dag.add_dep(parent, addr).map_err(|_e| {
                anyhow::anyhow!("cyclic dependency detected: {} → {}", parent, addr)
            })?;
        }

        // Set addr as parent so all sub-calls carry the right context for cycle detection.
        // Done outside the memoizer so context setup isn't buried in the deduplication boundary.
        let rs = rs.with_parent(addr.clone());

        // Transparent targets (groups) never execute — inline their deps' results.
        // Handled before the memoizer: nothing to deduplicate for groups, and calling
        // result_addr recursively here is safe because #[async_recursion] boxes the future,
        // breaking the Send inference cycle that would occur inside the memoizer closure.
        let def = self.get_def(rs.clone(), addr).await?;
        if def.target_def.transparent {
            let opts = *opts;
            let futures: Vec<_> = def
                .target_def
                .inputs
                .iter()
                .map(|input| {
                    let dep_addr = input.r#ref.r#ref.clone();
                    enclose!((self => engine, rs) async move {
                        engine
                            .result_addr(rs, &dep_addr, OutputMatcher::All, &opts)
                            .await
                    })
                })
                .collect();
            let results = futures::future::try_join_all(futures).await?;
            let mut merged = EResult::default();
            for r in results {
                merged.artifacts.extend(r.artifacts);
                merged.artifacts_meta.extend(r.artifacts_meta);
            }
            return Ok(merged);
        }

        let key = format!("{}:{}", addr.format(), outputs.cache_key());
        let opts = *opts;
        let res = rs.data.mem_result.once(key, enclose!((self => engine, rs, addr, outputs) move || async move {
            engine.inner_result_addr(rs, &addr, outputs, &opts).await.with_context(|| format!("result: {}", addr))
        })).await.map_err(unwrap_arc_err)?;

        Ok(res)
    }

    pub async fn result(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        matcher: &Matcher,
        opts: &ResultOptions,
    ) -> anyhow::Result<Vec<EResult>> {
        let mut set = JoinSet::new();
        let opts = *opts;

        let stream = self.query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            set.spawn(enclose!((self => engine, rs) async move {
                engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await
            }));
        }

        let mut all_res: Vec<EResult> = vec![];
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
        let spec = self.get_spec(rs.clone(), addr).await?;
        let def = self.get_def(rs.clone(), addr).await?;

        let def = self
            .clone()
            .link(rs.clone(), def.target_def.clone())
            .await
            .with_context(|| "link")?;

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

        let meta = self.clone().meta(rs.clone(), addr).await?;

        self.execute_and_cache(
            rs,
            &def,
            output_names,
            &ExecuteOptions {
                hashin: &meta.hashin,
                spec: &spec,
                def: &def,
                force: opts.force,
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
        let use_tmp_cache = !opts.def.target.cache;
        let key = format!("{}:{}", addr.format(), hashin);

        rs.data
            .mem_execute_cache
            .once(
                key,
                enclose!((self => engine, rs) move || async move {
                    let (artifacts, sandbox_dir) = engine
                        .clone()
                        .execute(rs.clone(), &addr, &spec, &def, &hashin)
                        .await?;

                    let artifacts_meta = artifacts
                        .iter()
                        .filter(|a| a.r#type == outputartifact::Type::Output)
                        .map(|a| Ok(ArtifactMeta { hashout: a.hashout()? }))
                        .collect::<anyhow::Result<Vec<_>>>()?;

                    defer! {
                        match fs::remove_dir_all(&sandbox_dir) {
                            Ok(_) => (),
                            Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                            Err(err) => eprintln!("failed to clean up sandbox: {err}"),
                        }
                    }

                    engine
                        .cache_locally(&rs.ctoken, &addr, &hashin, artifacts, use_tmp_cache)
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
            .artifacts_from_local_cache(&rs.ctoken, def, opts.hashin.as_str(), outputs)
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
        &self,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        let key = addr.format();
        rs.data
            .mem_def
            .once(
                key,
                enclose!((rs, addr) move || async move {
                    let engine = rs
                        .engine
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
                    engine.get_def_inner(rs, &addr).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn get_def_inner(
        &self,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<ExtendedTargetDef>> {
        let spec = self.get_spec(rs.clone(), addr).await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver
            .driver
            .parse(
                ParseRequest {
                    request_id: rs.request_id.clone(),
                    target_spec: (*spec).clone(),
                },
                &rs.ctoken,
            )
            .await
            .with_context(|| "parse")?;
        let def = res.target_def;

        let all_transitive = self
            .collect_transitive_deps(rs.clone(), &def.inputs)
            .await?;

        let def = if all_transitive.empty() {
            def
        } else {
            let res = driver
                .driver
                .apply_transitive(
                    ApplyTransitiveRequest {
                        request_id: rs.request_id.clone(),
                        target_def: def,
                        sandbox: all_transitive.clone(),
                    },
                    &rs.ctoken,
                )
                .await
                .with_context(|| "apply transitive")?;

            res.target_def
        };

        if def.hash.is_empty() {
            anyhow::bail!("missing hash");
        }

        Ok(Arc::new(ExtendedTargetDef {
            target_def: def,
            applied_transitive: all_transitive,
        }))
    }

    async fn collect_transitive_deps(
        &self,
        rs: Arc<RequestState>,
        inputs: &[Input],
    ) -> anyhow::Result<Sandbox> {
        let mut sb = Sandbox::default();

        for (i, input) in inputs.iter().enumerate() {
            let spec = self
                .get_spec(rs.clone(), &input.r#ref.r#ref)
                .await
                .with_context(|| format!("get spec: {:?}", input.r#ref))?;

            // For transparent targets (groups), use the pre-computed applied_transitive
            // which already recursively aggregates all nested deps' transitives.
            // For all other targets, use spec.transitive directly.
            // Important: avoid calling get_def on non-transparent targets here — get_def
            // calls collect_transitive_deps which would re-enter the mem_def memoizer
            // and deadlock on cyclic dep graphs.
            let transitive = if spec.driver == crate::plugingroup::DRIVER_NAME {
                let dep_def = self
                    .get_def(rs.clone(), &input.r#ref.r#ref)
                    .await
                    .with_context(|| format!("get def for group: {:?}", input.r#ref))?;
                dep_def.applied_transitive.clone()
            } else {
                spec.transitive.clone()
            };

            if transitive.empty() {
                continue;
            }

            let id = format!("_transitive_{}_{}", spec.addr.hash_str(), i);

            sb.merge_sandbox(transitive, id);
        }

        Ok(sb)
    }

    pub async fn get_spec(
        &self,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<TargetSpec>> {
        let key = addr.format();
        rs.data
            .mem_spec
            .once(
                key,
                enclose!((rs, addr) move || async move {
                    let engine = rs
                        .engine
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("engine dropped"))?;
                    engine.get_spec_inner(&rs, &addr).await
                }),
            )
            .await
            .map_err(|arc| {
                // Preserve TargetNotFoundError so callers can downcast_ref to it even when
                // the Arc is shared across concurrent memoizer waiters.
                if arc.downcast_ref::<TargetNotFoundError>().is_some() {
                    TargetNotFoundError { addr: addr.clone() }.into()
                } else {
                    unwrap_arc_err(arc)
                }
            })
    }

    async fn get_spec_inner(
        &self,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<TargetSpec>> {
        let executor: Arc<dyn ProviderExecutor> = Arc::new(EngineProviderExecutor {
            engine: rs.engine.clone(),
            rs: rs.clone(),
        });

        for provider in self.providers.iter() {
            let spec = match provider
                .provider
                .get(
                    GetRequest {
                        request_id: rs.request_id.clone(),
                        addr: addr.clone(),
                        states: vec![], // TODO
                        executor: Arc::clone(&executor),
                    },
                    &rs.ctoken,
                )
                .await
            {
                Ok(GetResponse { target_spec }) => target_spec,
                Err(GetError::NotFound) => continue,
                Err(GetError::Other(e)) => anyhow::bail!(e),
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
    use crate::htpkg::PkgBuf;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_engine_result_not_found() -> anyhow::Result<()> {
        let root = tempdir()?;
        let cfg = Config {
            root: root.path().to_path_buf(),
            parallelism: None,
        };

        let engine = Arc::new(Engine::new(cfg)?);
        let rs = engine.new_state();
        let addr = Addr {
            package: PkgBuf::from("non"),
            name: "existent".to_string(),
            args: Default::default(),
        };

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
}
