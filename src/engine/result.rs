use crate::engine::driver::targetdef::TargetDef;
use crate::engine::driver::{outputartifact, ParseRequest};
use crate::engine::provider::{GetError, GetRequest, GetResponse, TargetSpec};
use crate::engine::error::TargetNotFoundError;
use crate::engine::request_state::RequestState;
use crate::engine::Engine;
use crate::htaddr::Addr;
use crate::hmemoizer::WrappedError;
use enclose::enclose;
use std::sync::Arc;

use std::{fmt, fs, io};
use anyhow::Context;
use futures::TryStreamExt;
use itertools::Itertools;
use tokio::task::JoinSet;
use crate::defer;
use crate::engine::link::LinkedTargetDef;
use crate::engine::local_cache::ManifestArtifactType;
use crate::hartifactcontent::Content;
use crate::htmatcher::Matcher;

impl fmt::Debug for dyn Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Artifact")
    }
}

#[derive(Clone)]
pub struct ArtifactMeta {
    pub hashout: String,
}

#[derive(Clone)]
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
    pub async fn result_addr(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr, outputs: OutputMatcher, opts: &ResultOptions) -> anyhow::Result<EResult> {
        let key = format!("{}:{}", addr.format(), outputs.cache_key());
        let opts = *opts;
        let res = rs.mem_result.process(key, enclose!((self => engine, rs, addr, outputs) move || async move {
            engine.inner_result_addr(rs, &addr, outputs, &opts).await.map_err(WrappedError::from)
        })).await?;

        Ok(res)
    }

    pub async fn result(self: Arc<Self>, rs: Arc<RequestState>, matcher: &Matcher, opts: &ResultOptions) -> anyhow::Result<Vec<EResult>> {
        let mut set = JoinSet::new();
        let opts = *opts;

        let stream = self.query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            set.spawn(enclose!((self => engine, rs) async move {
                engine.result_addr(rs, &addr, OutputMatcher::All, &opts).await
            }));
        }

        let mut all_res: Vec<EResult> = vec!();
        while let Some(res) = set.join_next().await {
            all_res.push(res??)
        }

        Ok(all_res)
    }

    async fn inner_result_addr(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr, outputs: OutputMatcher, opts: &ResultOptions) -> anyhow::Result<EResult> {
        let spec = self.get_spec(rs.clone(), addr).await?;
        let def = self.get_def(rs.clone(), addr).await?;

        let def = self.clone().link(rs.clone(), def).await.with_context(|| "link")?;

        let output_names = match outputs {
            OutputMatcher::None => anyhow::Ok(Vec::<String>::new()),
            OutputMatcher::All => Ok(def.target.outputs.iter().map(|o| o.group.clone()).unique().collect()),
            OutputMatcher::Exact(names) => {
                let mut all_output_names = def.target.outputs.iter().map(|o| o.group.clone()).unique();
                for name in &names {
                    if !all_output_names.contains(name) {
                        anyhow::bail!("output not found: {}", name);
                    }
                }

                Ok(names)
            }
        }?;

        let meta = self.clone().meta(rs.clone(), addr).await?;

        self.execute_and_cache(rs, &def, output_names, &ExecuteOptions{
            hashin: &meta.hashin,
            spec: &spec,
            def: &def,
            force: opts.force,
        }).await
    }

    async fn execute_and_cache(self: Arc<Self>, rs: Arc<RequestState>, def: &LinkedTargetDef, outputs: Vec<String>, opts: &ExecuteOptions<'_>) -> anyhow::Result<EResult> {
        if !opts.force && opts.def.target.cache
            && let Some(res) = self.result_from_cache(rs.clone(), def, opts, outputs.clone()).await? {
                return Ok(res)
            }

        let (artifacts, sandbox_dir) = self.clone().execute(rs.clone(), &def.target.addr, opts.spec, opts.def, opts.hashin).await?;
        let artifacts_meta = artifacts.iter().filter(|a| a.r#type == outputartifact::Type::Output).map(|a| Ok(ArtifactMeta { hashout: a.hashout()? })).collect::<anyhow::Result<Vec<_>>>()?;

        defer! {
            match fs::remove_dir_all(&sandbox_dir) {
                Ok(_) => (),
                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                Err(err) => {
                    eprintln!("failed to clean up sandbox: {err}")
                },
            };
        }

        let cached_artifacts = self.cache_locally(&rs.ctoken, &def.target.addr, opts.hashin.as_str(), artifacts, !opts.def.target.cache).await?;

        Ok(EResult {
            artifacts: cached_artifacts.into_iter().filter(|a| a.r#type == ManifestArtifactType::Output && outputs.contains(&a.group)).map(|a| Arc::new(a) as Arc<dyn Content>).collect(),
            artifacts_meta,
        })
    }

    async fn result_from_cache(&self, rs: Arc<RequestState>, def: &LinkedTargetDef, opts: &ExecuteOptions<'_>, outputs: Vec<String>) -> anyhow::Result<Option<EResult>> {
        let (cached_artifacts, artifacts_meta) = match self.artifacts_from_local_cache(&rs.ctoken, def, opts.hashin.as_str(), outputs).await? {
            Some(res) => res,
            None => return Ok(None),
        };

        Ok(Some(EResult {
            artifacts: cached_artifacts.into_iter().map(|a| Arc::new(a) as Arc<dyn Content>).collect(),
            artifacts_meta,
        }))
    }

    pub async fn get_def(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<TargetDef> {
        let spec = self.get_spec(rs.clone(), addr).await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver.driver.parse(ParseRequest{
            request_id: rs.request_id.clone(),
            target_spec: spec,
        }, &rs.ctoken).await?;

        if res.target_def.hash.is_empty() {
            anyhow::bail!("missing hash");
        }

        Ok(res.target_def)
    }

    pub async fn get_spec(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<TargetSpec> {
        for provider in self.providers.iter() {
            let spec = match provider.provider.get(GetRequest{
                request_id: rs.request_id.clone(),
                addr: addr.clone(),
                states: vec![], // TODO
            }, &rs.ctoken).await {
                Ok(GetResponse{target_spec}) => {
                    target_spec
                }
                Err(GetError::NotFound) => {
                    continue
                },
                Err(GetError::Other(e)) => anyhow::bail!(e)
            };

            return anyhow::Ok(spec)
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
        };

        let engine = Arc::new(Engine::new(cfg)?);
        let rs = engine.new_state();
        let addr = Addr {
            package: PkgBuf::from("non"),
            name: "existent".to_string(),
            args: Default::default(),
        };

        let result = engine.clone().result_addr(rs, &addr, OutputMatcher::None, &ResultOptions::default()).await;
        assert!(result.is_err());
        let err = result.err().unwrap();

        assert_eq!(err.to_string(), "target not found: //non:existent");
        
        use crate::hmemoizer::WrappedError;
        let is_target_not_found = err.downcast_ref::<TargetNotFoundError>().is_some() ||
            err.downcast_ref::<WrappedError>().and_then(|w| w.0.downcast_ref::<TargetNotFoundError>()).is_some();
        assert!(is_target_not_found);

        Ok(())
    }
}