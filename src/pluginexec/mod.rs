use tokio::process::Command;
use async_trait::async_trait;
use crate::engine;
use crate::engine::driver::{ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest, ParseResponse, RunRequest, RunResponse};
use crate::engine::driver::targetdef::TargetDef as EngineTargetDef;
use crate::hasync::Cancellable;

pub struct Driver {

}

struct TargetDef {
    pub run: Vec<String>,
}

impl Driver {
    pub fn new() -> Driver {
        Driver{}
    }
}

#[async_trait]
impl engine::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: "exec".parse()?,
        })
    }

    async fn parse(&self, req: ParseRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse> {
        Ok(ParseResponse {
            target_def: EngineTargetDef{
                addr: req.target_spec.addr,
                raw_def: Box::new(TargetDef{
                    // run: vec!["echo", "hello"].iter().map(|s| s.to_string()).collect(),
                    run: vec!["sh", "-c", "exit 1"].iter().map(|s| s.to_string()).collect(),
                }),
                inputs: vec![],
                outputs: vec![],
                support_files: vec![],
                cache: true,
                disable_remote_cache: false,
                pty: true,
                hash: vec![],
            }
        })
    }

    async fn apply_transitive(&self, req: ApplyTransitiveRequest, _ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse{
            target_def: req.target_def,
        })
    }

    async fn run<'a>(&self, req: RunRequest<'a>, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<TargetDef>();

        let mut child = Command::new(&def.run[0])
            .args(&def.run[1..])
            .spawn()?;

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) => {
                        if status.success() {
                            return Ok(RunResponse{ artifacts: vec![] })
                        }

                        Err(anyhow::anyhow!("command failed: {:?}", status))
                    }
                    Err(inner) => {
                        Err(anyhow::anyhow!(inner))
                    }
                }
            }
            _ = ctoken.cancelled() => {
                child.kill().await?;
                child.wait().await?;
                anyhow::bail!("cancelled")
            }
        }
    }
}
