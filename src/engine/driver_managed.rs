use anyhow::Context;
use std::{fs, io};
use std::fs::File;
use std::path::PathBuf;
use async_trait::async_trait;
use crate::engine::driver::{outputartifact, ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver, ParseRequest, ParseResponse, RunInput, RunRequest, RunResponse};
use crate::engine::Engine;
use crate::{defer, hartifactcontent, hasync};
use crate::engine::driver::outputartifact::Content::TarPath;
use crate::engine::driver::outputartifact::Type::Output;
use crate::engine::driver::targetdef::path::Content;
use crate::hasync::Cancellable;
use xxhash_rust::xxh3::Xxh3;
use std::io::Write;

pub struct ManagedRunInput {
    pub input: RunInput,
    pub list_path: PathBuf,
}

pub struct ManagedRunRequest<'a> {
    pub request: RunRequest<'a>,
    pub sandbox_dir: PathBuf,
    pub sandbox_ws_dir: PathBuf,
    pub sandbox_pkg_dir: PathBuf,
    pub inputs: Vec<ManagedRunInput>,
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

struct HashingWriter<W: Write> {
    inner: W,
    hasher: Xxh3,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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

    async fn run<'a>(&self, mut req: RunRequest<'a>, ctoken: &(dyn Cancellable + Send + Sync)) -> anyhow::Result<RunResponse> {
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
            match fs::remove_dir_all(&sandbox_dir) {
                Ok(_) => (),
                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                Err(err) => {
                    eprintln!("failed to clean up sandbox: {err}")
                },
            };
        }

        let ws_dir = sandbox_dir.join("ws");
        fs::create_dir_all(&ws_dir)?;

        let mut inputs = Vec::new();
        for input in std::mem::take(&mut req.inputs) {
            let input_list = ws_dir.join(format!("input_{}.list", input.origin_id));
            hartifactcontent::unpack::unpack(input.artifact.content.as_ref(), ws_dir.as_path(), input_list.as_path())?;
            inputs.push(ManagedRunInput {
                input,
                list_path: input_list,
            });
        }

        let target = req.target;
        let hashin = req.hashin.clone();

        let sandbox_pkg_dir = &ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(sandbox_pkg_dir)?;

        let mut res = self.driver.run(ManagedRunRequest{
            sandbox_dir: sandbox_dir.clone(),
            sandbox_ws_dir: ws_dir.clone(),
            sandbox_pkg_dir: sandbox_pkg_dir.clone(),
            request: req,
            inputs,
        }, ctoken).await.with_context(|| "driver run")?;

        for output in &target.outputs {
            if !output.paths.iter().any(|path| path.collect) {
                continue;
            }

            let mut tar = hartifactcontent::tar::TarPacker::new();

            for path in &output.paths {
                if !path.collect {
                    continue;
                }

                match &path.content {
                    Content::FilePath(fp) => {
                        let source = sandbox_pkg_dir.join(fp);
                        tar.create_file(source.to_string_lossy().into_owned(), fp.clone());
                    }
                    Content::DirPath(dir) => {
                        let dir_full = sandbox_pkg_dir.join(dir);
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
                        let full_pattern = sandbox_pkg_dir.join(pattern).to_string_lossy().into_owned();
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

            let artifacts_dir = self.home.join("artifacts");
            fs::create_dir_all(&artifacts_dir)?;
            let tarpath = artifacts_dir
                .join(format!("{}-{}.tar", hashin, output.group))
                .to_string_lossy()
                .into_owned();
            let tarf = File::create(std::path::Path::new(&tarpath))?;

            let mut hw = HashingWriter { inner: tarf, hasher: Xxh3::new() };

            tar.pack(&mut hw).with_context(|| "pack")?;

            res.artifacts.push(outputartifact::OutputArtifact{
                group: output.group.clone(),
                name: format!("{}.tar", output.group),
                r#type: Output,
                content: TarPath(tarpath),
                hashout: format!("{:x}", hw.hasher.digest())
            });
        }

        Ok(RunResponse{
            artifacts: res.artifacts
        })
    }
}
