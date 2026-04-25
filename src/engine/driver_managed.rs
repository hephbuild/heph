use std::{fs, io};
use std::path::PathBuf;
use async_trait::async_trait;
use crate::engine::driver::{outputartifact, ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver, ParseRequest, ParseResponse, RunRequest, RunResponse};
use crate::engine::Engine;
use crate::{defer, hartifactcontent, hasync};
use crate::engine::driver::outputartifact::Type::Output;
use crate::engine::driver::targetdef::path::Content;
use crate::hartifactcontent::Content::TarPath;
use crate::hasync::Cancellable;

pub struct ManagedRunRequest<'a> {
    pub request: RunRequest<'a>,
    pub sandbox_dir: PathBuf,
    pub sandbox_ws_dir: PathBuf,
    pub sandbox_pkg_dir: PathBuf,
}
pub struct ManagedRunResponse {
    pub artifacts: Vec<outputartifact::OutputArtifact>,
}

#[async_trait]
pub trait ManagedDriver: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    async fn parse(&self, req: ParseRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse>;
    async fn apply_transitive(&self, req: ApplyTransitiveRequest, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse>;
    async fn run<'a>(&self, req: ManagedRunRequest<'a>, ctoken: &(dyn hasync::Cancellable + Send + Sync)) -> anyhow::Result<ManagedRunResponse>;
}

pub struct ManagedDriverBridge {
    pub home: PathBuf,
    pub driver: Box<dyn ManagedDriver>
}

impl Engine {
    pub fn new_managed_driver(&self, driver: Box<dyn ManagedDriver>) -> ManagedDriverBridge {
        ManagedDriverBridge{
            home: self.home.clone(),
            driver,
        }
    }
}

#[async_trait]
impl Driver for ManagedDriverBridge {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        self.driver.config(req)
    }

    async fn parse(&self, req: ParseRequest, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ParseResponse> {
        self.driver.parse(req, ctoken).await
    }

    async fn apply_transitive(&self, req: ApplyTransitiveRequest, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<ApplyTransitiveResponse> {
        self.driver.apply_transitive(req, ctoken).await
    }

    async fn run<'a>(&self, req: RunRequest<'a>, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<RunResponse> {
        let sandbox_dir = {
            let mut dir = self.home.join("sandbox");

            for c in req.target.addr.package.components() {
                dir = dir.join(c)
            }

            if req.target.addr.args.is_empty() {
                dir.join(format!("__target_{}", req.target.addr.name))
            } else {
                dir.join(format!("__target_{}_{}",  req.target.addr.name, req.target.addr.hash_str()))
            }
        };

        match fs::remove_dir_all(&sandbox_dir) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }?;
        fs::create_dir_all(&sandbox_dir)?;
        defer! {
            // match fs::remove_dir_all(&sandbox_dir) {
            //     Ok(_) => (),
            //     Err(err) if err.kind() == io::ErrorKind::NotFound => (),
            //     Err(err) => {
            //         eprintln!("failed to clean up sandbox: {err}")
            //     },
            // };
        }

        let ws_dir = sandbox_dir.join("ws");
        fs::create_dir_all(&ws_dir)?;

        for input in &req.inputs {
            hartifactcontent::unpack::unpack(&input.artifact.content, ws_dir.as_path())?;
        }

        let target = req.target;

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)?;

        let mut res = self.driver.run(ManagedRunRequest{
            sandbox_dir: sandbox_dir.clone(),
            sandbox_ws_dir: ws_dir.clone(),
            sandbox_pkg_dir,
            request: req,
        }, ctoken).await?;

        for output in &target.outputs {
            if !output.paths.iter().any(|path| path.collect) {
                continue;
            }

            let mut tar = hartifactcontent::pack_tar::TarPacker::new();

            for path in &output.paths {
                if !path.collect {
                    continue;
                }

                match &path.content {
                    Content::FilePath(fp) => {
                        let source = ws_dir.join(fp);
                        tar.create_file(source.to_string_lossy().into_owned(), fp.clone());
                    }
                    Content::DirPath(dir) => {
                        let dir_full = ws_dir.join(dir);
                        for entry in walkdir::WalkDir::new(&dir_full) {
                            let entry = entry?;
                            if entry.file_type().is_file() {
                                let source = entry.path().to_string_lossy().into_owned();
                                let rel = entry.path().strip_prefix(&ws_dir)?.to_string_lossy().into_owned();
                                tar.create_file(source, rel);
                            }
                        }
                    }
                    Content::Glob(pattern) => {
                        let full_pattern = ws_dir.join(pattern).to_string_lossy().into_owned();
                        for matched in glob::glob(&full_pattern)? {
                            let matched = matched?;
                            if matched.is_file() {
                                let source = matched.to_string_lossy().into_owned();
                                let rel = matched.strip_prefix(&ws_dir)?.to_string_lossy().into_owned();
                                tar.create_file(source, rel);
                            }
                        }
                    }
                }
            }

            let tarpath = String::from("/tmp/somepath");
            tar.pack(std::path::Path::new(&tarpath))?;

            res.artifacts.push(outputartifact::OutputArtifact{
                group: output.group.clone(),
                name: format!("{}.tar", output.group),
                r#type: Output,
                content: TarPath(tarpath),
            });
        }

        Ok(RunResponse{
            artifacts: res.artifacts
        })
    }
}
