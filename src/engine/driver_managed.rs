use crate::engine::Engine;
use crate::engine::driver::outputartifact::Content::TarPath;
use crate::engine::driver::outputartifact::Type::Output;
use crate::engine::driver::targetdef::path::Content;
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver,
    ParseRequest, ParseResponse, RunInput, RunRequest, RunResponse, outputartifact,
};
use crate::hasync::Cancellable;
use crate::{hartifactcontent, hasync};
use anyhow::Context;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::{fs, io};
use xxhash_rust::xxh3::Xxh3;

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
    async fn parse(
        &self,
        req: ParseRequest,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse>;
    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse>;
    async fn run<'a>(
        &self,
        req: ManagedRunRequest<'a>,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse>;
}

pub struct ManagedDriverBridge {
    pub home: PathBuf,
    pub driver: Box<dyn ManagedDriver>,
}

impl Engine {
    pub fn new_managed_driver(&self, driver: Box<dyn ManagedDriver>) -> ManagedDriverBridge {
        ManagedDriverBridge {
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
        // n is the return value of write(), guaranteed <= buf.len() by the Write contract
        #[expect(
            clippy::indexing_slicing,
            reason = "n is guaranteed <= buf.len() by the Write::write contract"
        )]
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

    async fn parse(
        &self,
        req: ParseRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        self.driver.parse(req, ctoken).await
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        self.driver.apply_transitive(req, ctoken).await
    }

    async fn run<'a>(
        &self,
        mut req: RunRequest<'a>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let sandbox_dir = req.sandbox_dir.clone();
        let ws_dir = sandbox_dir.join("ws");
        fs::create_dir_all(&ws_dir).with_context(|| "create ws dir")?;

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)
            .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

        let mut inputs = Vec::new();
        for input in std::mem::take(&mut req.inputs) {
            let input_list = ws_dir.join(format!("input_{}.list", input.origin_id));
            hartifactcontent::unpack::unpack(
                input.artifact.content.as_ref(),
                ws_dir.as_path(),
                input_list.as_path(),
            )
            .with_context(|| "unpack")?;
            inputs.push(ManagedRunInput {
                input,
                list_path: input_list,
            });
        }

        // Apply filters and collect source_map entries.
        // BTreeMap so serialization to source_map.json is byte-deterministic across runs;
        // HashMap iter order varies per-process and breaks downstream output hashing/caching.
        let mut source_map: BTreeMap<String, String> = BTreeMap::new();
        for managed_input in &mut inputs {
            let filters = &managed_input.input.filters;
            let source_addr_str = managed_input.input.source_addr.format();
            let raw = fs::read_to_string(&managed_input.list_path)
                .with_context(|| format!("read list {:?}", managed_input.list_path))?;

            let mut kept: Vec<&str> = Vec::new();
            for line in raw.lines() {
                if line.is_empty() {
                    continue;
                }
                let rel = std::path::Path::new(line)
                    .strip_prefix(&ws_dir)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| line.to_string());
                let include = filters.is_empty() || filters.iter().any(|f| f == &rel);
                if include {
                    source_map.insert(rel, source_addr_str.clone());
                    kept.push(line);
                }
            }

            if !filters.is_empty() {
                let new_content = kept.join("\n") + if kept.is_empty() { "" } else { "\n" };
                fs::write(&managed_input.list_path, new_content)
                    .with_context(|| format!("rewrite list {:?}", managed_input.list_path))?;
            }
        }

        let source_map_json =
            serde_json::to_string(&source_map).with_context(|| "serialize source_map")?;
        fs::write(sandbox_pkg_dir.join("source_map.json"), source_map_json)
            .with_context(|| "write source_map.json")?;

        let target = req.target;
        let hashin = req.hashin.clone();

        let mut res = self
            .driver
            .run(
                ManagedRunRequest {
                    sandbox_dir: sandbox_dir.clone(),
                    sandbox_ws_dir: ws_dir.clone(),
                    sandbox_pkg_dir: sandbox_pkg_dir.clone(),
                    request: req,
                    inputs,
                },
                ctoken,
            )
            .await
            .with_context(|| "driver run")?;

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
                        let source = ws_dir.join(fp);
                        tar.create_file(source.to_string_lossy().into_owned(), fp.clone());
                    }
                    Content::DirPath(dir) => {
                        let dir_full = ws_dir.join(dir);
                        for entry in walkdir::WalkDir::new(&dir_full) {
                            let entry = entry?;
                            if entry.file_type().is_file() {
                                let source = entry.path().to_string_lossy().into_owned();
                                let rel = entry
                                    .path()
                                    .strip_prefix(&ws_dir)?
                                    .to_string_lossy()
                                    .into_owned();
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
                                let rel = matched
                                    .strip_prefix(&ws_dir)?
                                    .to_string_lossy()
                                    .into_owned();
                                tar.create_file(source, rel);
                            }
                        }
                    }
                }
            }

            let artifacts_dir = sandbox_dir.join("heph-collect-artifacts");
            fs::create_dir_all(&artifacts_dir)?;
            let tarpath = artifacts_dir
                .join(format!("{}-{}.tar", hashin, output.group))
                .to_string_lossy()
                .into_owned();
            let tarf = File::create(std::path::Path::new(&tarpath))?;

            let mut hw = HashingWriter {
                inner: tarf,
                hasher: Xxh3::new(),
            };

            tar.pack(&mut hw).with_context(|| "pack")?;

            res.artifacts.push(outputartifact::OutputArtifact {
                group: output.group.clone(),
                name: format!("{}.tar", output.group),
                r#type: Output,
                content: TarPath(tarpath),
                hashout: format!("{:x}", hw.hasher.digest()),
            });
        }

        Ok(RunResponse {
            artifacts: res.artifacts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::targetdef::TargetDef;
    use crate::engine::driver::{
        ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse,
        ParseRequest, ParseResponse, RunInput, inputartifact, outputartifact,
    };
    use crate::hasync;
    use crate::htaddr::Addr;
    use crate::htpkg::PkgBuf;
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    struct NoopManagedDriver;

    #[async_trait]
    impl ManagedDriver for NoopManagedDriver {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "noop".to_string(),
            })
        }
        async fn parse(
            &self,
            _req: ParseRequest,
            _ctoken: &(dyn hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            unimplemented!()
        }
        async fn apply_transitive(
            &self,
            _req: ApplyTransitiveRequest,
            _ctoken: &(dyn hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            unimplemented!()
        }
        async fn run<'a>(
            &self,
            _req: ManagedRunRequest<'a>,
            _ctoken: &(dyn hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            Ok(ManagedRunResponse { artifacts: vec![] })
        }
    }

    fn make_raw_input(
        origin_id: &str,
        rel_path: &str,
        source_addr: Addr,
        filters: Vec<String>,
    ) -> RunInput {
        RunInput {
            artifact: inputartifact::InputArtifact {
                r#type: inputartifact::Type::Dep,
                origin_id: origin_id.to_string(),
                content: Arc::new(outputartifact::OutputArtifact {
                    group: "".to_string(),
                    name: rel_path.to_string(),
                    r#type: outputartifact::Type::Output,
                    content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                        data: b"content".to_vec(),
                        path: rel_path.to_string(),
                        x: false,
                    }),
                    hashout: "hash".to_string(),
                }),
            },
            origin_id: origin_id.to_string(),
            source_addr,
            filters,
        }
    }

    fn make_target_def(pkg: &str) -> TargetDef {
        TargetDef {
            addr: Addr::new(PkgBuf::from(pkg), "t".to_string(), BTreeMap::new()),
            labels: vec![],
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: false,
            disable_remote_cache: false,
            pty: false,
            hash: vec![],
            transparent: false,
        }
    }

    fn bridge() -> ManagedDriverBridge {
        ManagedDriverBridge {
            home: std::path::PathBuf::from("/tmp"),
            driver: Box::new(NoopManagedDriver),
        }
    }

    fn ctoken() -> hasync::StdCancellationToken {
        hasync::StdCancellationToken::new()
    }

    #[tokio::test]
    async fn source_map_written_for_all_inputs() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "gen".to_string(), BTreeMap::new());
        let input = make_raw_input("dep0", "pkg/foo.go", src_addr, vec![]);
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![input],
            hashin: &"hash".to_string(),
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        let map_path = sandbox.join("ws").join("pkg").join("source_map.json");
        let content = fs::read_to_string(&map_path)?;
        let map: BTreeMap<String, String> = serde_json::from_str(&content)?;
        assert_eq!(map.get("pkg/foo.go").map(|s| s.as_str()), Some("//pkg:gen"));
        Ok(())
    }

    #[tokio::test]
    async fn filter_prunes_list_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "gen".to_string(), BTreeMap::new());
        // Input has file pkg/foo.go but filter only allows pkg/bar.go
        let input = make_raw_input(
            "dep0",
            "pkg/foo.go",
            src_addr,
            vec!["pkg/bar.go".to_string()],
        );
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![input],
            hashin: &"hash".to_string(),
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        let list_path = sandbox.join("ws").join("input_dep0.list");
        let list_content = fs::read_to_string(&list_path)?;
        assert!(
            list_content.is_empty(),
            "filtered file must be excluded from list: {list_content}"
        );

        // source_map must also be empty since nothing passed the filter
        let map_path = sandbox.join("ws").join("pkg").join("source_map.json");
        let map: BTreeMap<String, String> = serde_json::from_str(&fs::read_to_string(map_path)?)?;
        assert!(map.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn empty_source_map_when_no_inputs() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![],
            hashin: &"hash".to_string(),
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        let map_path = sandbox.join("ws").join("pkg").join("source_map.json");
        let map: BTreeMap<String, String> = serde_json::from_str(&fs::read_to_string(map_path)?)?;
        assert!(map.is_empty());
        Ok(())
    }

    // Regression: source_map.json was serialized from a HashMap, so its byte
    // representation varied across runs. Downstream targets hash the resulting
    // output tar, so non-determinism here caused unstable hashouts and broke
    // the local cache for targets like go_golist/go_embed.
    #[tokio::test]
    async fn source_map_json_bytes_are_deterministic_and_sorted() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "gen".to_string(), BTreeMap::new());
        // Many keys with names that would scatter under HashMap's RandomState.
        let inputs: Vec<RunInput> = ["c.go", "a.go", "b.go", "z.go", "m.go"]
            .iter()
            .enumerate()
            .map(|(i, f)| {
                make_raw_input(
                    &format!("dep{i}"),
                    &format!("pkg/{f}"),
                    src_addr.clone(),
                    vec![],
                )
            })
            .collect();
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs,
            hashin: &"hash".to_string(),
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        let map_path = sandbox.join("ws").join("pkg").join("source_map.json");
        let bytes = fs::read(&map_path)?;
        // Keys must appear in sorted order.
        let text = String::from_utf8(bytes.clone())?;
        let pos_a = text.find("pkg/a.go").expect("a present");
        let pos_b = text.find("pkg/b.go").expect("b present");
        let pos_c = text.find("pkg/c.go").expect("c present");
        let pos_m = text.find("pkg/m.go").expect("m present");
        let pos_z = text.find("pkg/z.go").expect("z present");
        assert!(
            pos_a < pos_b && pos_b < pos_c && pos_c < pos_m && pos_m < pos_z,
            "source_map.json keys must be sorted: {text}"
        );
        Ok(())
    }
}
