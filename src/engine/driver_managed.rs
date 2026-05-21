use crate::engine::Engine;
use crate::engine::driver::inputartifact;
use crate::engine::driver::outputartifact::Content::TarPath;
use crate::engine::driver::targetdef::path::{self, Content};
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
use std::path::{Path, PathBuf};
use std::{fs, io};
use xxhash_rust::xxh3::Xxh3;

pub struct ManagedRunInput {
    pub input: RunInput,
    /// `None` for Support inputs — they are materialized into the sandbox
    /// but intentionally produce no list file so they stay out of SRC/list
    /// env routing in downstream drivers.
    pub list_path: Option<PathBuf>,
    pub unpack_root: PathBuf,
}

impl ManagedRunInput {
    /// List path for a Dep input. Errors when called on a Support input —
    /// a driver iterating its own Dep inputs by `origin_id` should never see one.
    pub fn require_list_path(&self) -> anyhow::Result<&Path> {
        self.list_path.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no list_path for input origin_id={} (support inputs have no list file)",
                self.input.origin_id,
            )
        })
    }
}

pub struct ManagedRunRequest<'a, 'io> {
    pub request: RunRequest<'a, 'io>,
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
    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse>;
    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
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

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        self.run_inner(req, ctoken, false).await
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        self.run_inner(req, ctoken, true).await
    }
}
impl ManagedDriverBridge {
    async fn run_inner<'a, 'io>(
        &self,
        mut req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<RunResponse> {
        let sandbox_dir = req.sandbox_dir.clone();
        let ws_dir = sandbox_dir.join("ws");
        fs::create_dir_all(&ws_dir).with_context(|| format!("create ws dir {:?}", ws_dir))?;

        let list_dir = sandbox_dir.join("list");
        fs::create_dir_all(&list_dir).with_context(|| format!("create list dir {:?}", list_dir))?;

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)
            .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

        let mut inputs = Vec::new();
        for input in std::mem::take(&mut req.inputs) {
            // Support inputs always materialize into ws_dir regardless of the
            // dep's `unpack_root` annotation — a tool dep with support files
            // would otherwise scatter them into `exec_tools/`, which defeats
            // the "available on disk to the consumer" purpose.
            let unpack_root = match input.artifact.r#type {
                inputartifact::Type::Support => &ws_dir,
                inputartifact::Type::Dep => {
                    if let Some(unpack_root) = input.annotations.get("unpack_root") {
                        &req.sandbox_dir.join(format!("exec_{}", unpack_root))
                    } else {
                        &ws_dir
                    }
                }
            };
            fs::create_dir_all(unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            // Support inputs share materialization with Dep inputs but
            // intentionally produce no list file, so they don't surface in
            // SRC_/LIST_/tool-bin env routing in downstream drivers.
            let input_list = match input.artifact.r#type {
                inputartifact::Type::Dep => {
                    Some(list_dir.join(format!("input_{}.list", input.origin_id)))
                }
                inputartifact::Type::Support => None,
            };
            let filters = &input.filters;
            let predicate: Option<&dyn Fn(&Path) -> bool> = if filters.is_empty() {
                None
            } else {
                Some(&|rel: &Path| filters.iter().any(|f| Path::new(f) == rel))
            };
            hartifactcontent::unpack::unpack(
                input.artifact.content.as_ref(),
                unpack_root.as_path(),
                input_list.as_deref(),
                predicate,
            )
            .with_context(|| {
                format!(
                    "unpack input origin_id={} source_addr={} into {:?}",
                    input.origin_id,
                    input.source_addr.format(),
                    unpack_root,
                )
            })?;
            inputs.push(ManagedRunInput {
                input,
                list_path: input_list,
                unpack_root: unpack_root.clone(),
            });
        }

        // No path-level overlap check here. Two inputs landing at the same
        // unpacked path is a legitimate user pattern (e.g.
        // `deps = {"root": file(x), "_": glob(...)}` where the glob expands
        // to include the root file; the dep groups are split for env-var
        // routing — `$SRC_ROOT` vs `$SRC_`). Targets are deterministic, so
        // `fs::File::create`'s truncate semantics produce the right bytes
        // regardless of write order. The address-level invariant — no two
        // engine inputs share the same `(r#ref, group[, mode])` — is
        // enforced upstream by `Sandbox::merge_sandbox`.
        //
        // BTreeMap so serialization to source_map.json is byte-deterministic across runs;
        // HashMap iter order varies per-process and breaks downstream output hashing/caching.
        // Only inputs unpacked into ws_dir contribute to source_map; tool/other-root inputs
        // are out-of-workspace and have no place on a source path → addr mapping.
        let mut source_map: BTreeMap<String, String> = BTreeMap::new();
        for managed_input in &inputs {
            if managed_input.unpack_root != ws_dir {
                continue;
            }
            // Support inputs are materialized but not "source" — they have no
            // list file and must not contribute to source_map.
            let Some(ref list_path) = managed_input.list_path else {
                continue;
            };
            let source_addr_str = managed_input.input.source_addr.format();
            let raw = fs::read_to_string(list_path)
                .with_context(|| format!("read list {:?}", list_path))?;

            for line in raw.lines() {
                if line.is_empty() {
                    continue;
                }
                let rel = Path::new(line)
                    .strip_prefix(&ws_dir)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| line.to_string());
                source_map.insert(rel, source_addr_str.clone());
            }
        }

        let source_map_json =
            serde_json::to_string(&source_map).with_context(|| "serialize source_map")?;
        fs::write(sandbox_pkg_dir.join("source_map.json"), source_map_json)
            .with_context(|| "write source_map.json")?;

        let target = req.target;
        let hashin = req.hashin;

        let mut res = {
            let req = ManagedRunRequest {
                sandbox_dir: sandbox_dir.clone(),
                sandbox_ws_dir: ws_dir.clone(),
                sandbox_pkg_dir: sandbox_pkg_dir.clone(),
                request: req,
                inputs,
            };

            if shell {
                self.driver.run_shell(req, ctoken)
            } else {
                self.driver.run(req, ctoken)
            }
        }
        .await
        .with_context(|| "driver run")?;

        if shell {
            return Ok(RunResponse { artifacts: vec![] });
        }

        for output in &target.outputs {
            if !output.paths.iter().any(|path| path.collect) {
                continue;
            }

            let mut tar = hartifactcontent::tar::TarPacker::new();

            for path in &output.paths {
                if !path.collect {
                    continue;
                }
                add_path_to_tar(&mut tar, &ws_dir, path, &output.group)?;
            }

            let tarpath = pack_to_artifact_tar(&sandbox_dir, hashin, &output.group, tar)?;

            res.artifacts.push(outputartifact::OutputArtifact {
                group: output.group.clone(),
                name: format!("{}.tar", output.group),
                r#type: outputartifact::Type::Output,
                content: TarPath(tarpath.0),
                hashout: tarpath.1,
            });
        }

        if !target.support_files.is_empty() {
            let mut tar = hartifactcontent::tar::TarPacker::new();
            for path in &target.support_files {
                add_path_to_tar(&mut tar, &ws_dir, path, "support")?;
            }

            let (tarpath, hashout) = pack_to_artifact_tar(&sandbox_dir, hashin, "support", tar)?;

            res.artifacts.push(outputartifact::OutputArtifact {
                group: String::new(),
                name: "support.tar".to_string(),
                r#type: outputartifact::Type::SupportFile,
                content: TarPath(tarpath),
                hashout,
            });
        }

        Ok(RunResponse {
            artifacts: res.artifacts,
        })
    }
}

fn add_path_to_tar(
    tar: &mut hartifactcontent::tar::TarPacker,
    ws_dir: &Path,
    path: &path::Path,
    group_for_err: &str,
) -> anyhow::Result<()> {
    match &path.content {
        Content::FilePath(fp) => {
            let source = ws_dir.join(fp);
            tar.create_file(source.to_string_lossy().into_owned(), fp.clone());
        }
        Content::DirPath(dir) => {
            let dir_full = ws_dir.join(dir);
            for entry in walkdir::WalkDir::new(&dir_full) {
                let entry = entry.with_context(|| {
                    format!("walk output dir {:?} (group={})", dir_full, group_for_err)
                })?;
                let ft = entry.file_type();
                if ft.is_file() || ft.is_symlink() {
                    let source = entry.path().to_string_lossy().into_owned();
                    let rel = entry
                        .path()
                        .strip_prefix(ws_dir)
                        .with_context(|| {
                            format!("strip ws prefix from {:?} (ws={:?})", entry.path(), ws_dir)
                        })?
                        .to_string_lossy()
                        .into_owned();
                    tar.create_file(source, rel);
                }
            }
        }
        Content::Glob(pattern) => {
            let full_pattern = ws_dir.join(pattern).to_string_lossy().into_owned();
            for matched in glob::glob(&full_pattern)
                .with_context(|| format!("compile output glob {full_pattern:?}"))?
            {
                let matched =
                    matched.with_context(|| format!("glob entry from {full_pattern:?}"))?;
                let md = std::fs::symlink_metadata(&matched).with_context(|| {
                    format!("lstat glob match {:?} (group={})", matched, group_for_err)
                })?;
                let ft = md.file_type();
                if ft.is_file() || ft.is_symlink() {
                    let source = matched.to_string_lossy().into_owned();
                    let rel = matched
                        .strip_prefix(ws_dir)
                        .with_context(|| {
                            format!(
                                "strip ws prefix from glob match {:?} (ws={:?})",
                                matched, ws_dir
                            )
                        })?
                        .to_string_lossy()
                        .into_owned();
                    tar.create_file(source, rel);
                }
            }
        }
    }
    Ok(())
}

fn pack_to_artifact_tar(
    sandbox_dir: &Path,
    hashin: &str,
    name_suffix: &str,
    tar: hartifactcontent::tar::TarPacker,
) -> anyhow::Result<(String, String)> {
    let artifacts_dir = sandbox_dir.join("heph-collect-artifacts");
    fs::create_dir_all(&artifacts_dir)
        .with_context(|| format!("create artifacts dir {:?}", artifacts_dir))?;
    let tarpath = artifacts_dir
        .join(format!("{}-{}.tar", hashin, name_suffix))
        .to_string_lossy()
        .into_owned();
    let tarf = File::create(std::path::Path::new(&tarpath))
        .with_context(|| format!("create output tar {tarpath:?}"))?;

    let mut hw = HashingWriter {
        inner: tarf,
        hasher: Xxh3::new(),
    };

    tar.pack(&mut hw).with_context(|| "pack")?;

    Ok((tarpath, format!("{:x}", hw.hasher.digest())))
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
        async fn run<'a, 'io>(
            &self,
            _req: ManagedRunRequest<'a, 'io>,
            _ctoken: &(dyn hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            Ok(ManagedRunResponse { artifacts: vec![] })
        }
        async fn run_shell<'a, 'io>(
            &self,
            _req: ManagedRunRequest<'a, 'io>,
            _ctoken: &(dyn hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            anyhow::bail!("run_shell not implemented for NoopManagedDriver")
        }
    }

    fn make_raw_input(
        origin_id: &str,
        rel_path: &str,
        source_addr: Addr,
        filters: Vec<String>,
    ) -> RunInput {
        make_raw_input_full(
            origin_id,
            rel_path,
            source_addr,
            filters,
            BTreeMap::new(),
            "hash",
        )
    }

    fn make_raw_input_with_annotations(
        origin_id: &str,
        rel_path: &str,
        source_addr: Addr,
        filters: Vec<String>,
        annotations: BTreeMap<String, String>,
    ) -> RunInput {
        make_raw_input_full(
            origin_id,
            rel_path,
            source_addr,
            filters,
            annotations,
            "hash",
        )
    }

    fn make_raw_input_full(
        origin_id: &str,
        rel_path: &str,
        source_addr: Addr,
        filters: Vec<String>,
        annotations: BTreeMap<String, String>,
        hashout: &str,
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
                    hashout: hashout.to_string(),
                }),
            },
            origin_id: origin_id.to_string(),
            source_addr,
            filters,
            annotations,
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
            hashin: "hash",
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
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        let list_path = sandbox.join("list").join("input_dep0.list");
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
            hashin: "hash",
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
            hashin: "hash",
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

    // Per-input `unpack_root` annotation routes each input under a distinct
    // sandbox subdir. Default → ws/, `unpack_root=tools` → tools/. Only the
    // ws/-rooted inputs contribute to source_map (tools are not "source").
    #[tokio::test]
    async fn unpack_root_annotation_overrides_destination() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "gen".to_string(), BTreeMap::new());
        let tool_addr = Addr::new(PkgBuf::from("pkg"), "t".to_string(), BTreeMap::new());

        let ws_input = make_raw_input("dep0", "pkg/foo.go", src_addr, vec![]);
        let tool_input = make_raw_input_with_annotations(
            "tool|cc|0",
            "pkg/bin/cc",
            tool_addr,
            vec![],
            BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
        );

        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![ws_input, tool_input],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        assert!(sandbox.join("ws").join("pkg").join("foo.go").exists());
        assert!(sandbox.join("list").join("input_dep0.list").exists());

        assert!(
            sandbox
                .join("exec_tools")
                .join("pkg")
                .join("bin")
                .join("cc")
                .exists()
        );
        assert!(sandbox.join("list").join("input_tool|cc|0.list").exists());

        // Tools must not bleed into ws/, and source deps must not leak into tools/.
        assert!(
            !sandbox
                .join("ws")
                .join("pkg")
                .join("bin")
                .join("cc")
                .exists()
        );
        assert!(
            !sandbox
                .join("exec_tools")
                .join("pkg")
                .join("foo.go")
                .exists()
        );

        // Tool input is omitted from source_map (out-of-workspace).
        let map: BTreeMap<String, String> = serde_json::from_str(&fs::read_to_string(
            sandbox.join("ws").join("pkg").join("source_map.json"),
        )?)?;
        assert_eq!(map.get("pkg/foo.go").map(|s| s.as_str()), Some("//pkg:gen"));
        assert!(!map.keys().any(|k| k.contains("cc")));
        Ok(())
    }

    // Path-level overlap across inputs is legitimate (e.g. user splits deps
    // into env-routed groups: `deps = {"root": f, "_": glob_including_f}`).
    // Bridge must silently accept it — targets are deterministic so the
    // truncating second write produces the right bytes regardless. Engine
    // invariants on input addresses live upstream in `Sandbox::merge_sandbox`.
    #[tokio::test]
    async fn overlapping_inputs_at_same_path_succeed() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        // Distinct source targets, same dest path — the `fs:file` + `fs:glob`
        // pattern. Must not bail.
        let src_file = Addr::new(
            PkgBuf::from("@heph/fs"),
            "file".to_string(),
            BTreeMap::new(),
        );
        let src_glob = Addr::new(
            PkgBuf::from("@heph/fs"),
            "glob".to_string(),
            BTreeMap::new(),
        );
        let a = make_raw_input("dep|root|0", "pkg/foo.yaml", src_file, vec![]);
        let b = make_raw_input("dep|_|0", "pkg/foo.yaml", src_glob, vec![]);
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![a, b],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;
        assert!(sandbox.join("ws").join("pkg").join("foo.yaml").exists());
        Ok(())
    }

    // Multi-output tool refs intentionally share one origin_id across N
    // RunInputs (the bridge merges them into one list file). Must not trip
    // any check.
    #[tokio::test]
    async fn shared_origin_multi_output_succeeds() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "t".to_string(), BTreeMap::new());

        // Two RunInputs sharing one origin_id, distinct paths.
        let a = make_raw_input_with_annotations(
            "tool||0",
            "pkg/bin/node",
            src_addr.clone(),
            vec![],
            BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
        );
        let b = make_raw_input_with_annotations(
            "tool||0",
            "pkg/bin/npm",
            src_addr,
            vec![],
            BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
        );
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![a, b],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?; // must not bail
        Ok(())
    }

    fn make_support_input(origin_id: &str, rel_path: &str, source_addr: Addr) -> RunInput {
        make_support_input_with_annotations(origin_id, rel_path, source_addr, BTreeMap::new())
    }

    fn make_support_input_with_annotations(
        origin_id: &str,
        rel_path: &str,
        source_addr: Addr,
        annotations: BTreeMap<String, String>,
    ) -> RunInput {
        RunInput {
            artifact: inputartifact::InputArtifact {
                r#type: inputartifact::Type::Support,
                origin_id: origin_id.to_string(),
                content: Arc::new(outputartifact::OutputArtifact {
                    group: "".to_string(),
                    name: rel_path.to_string(),
                    r#type: outputartifact::Type::SupportFile,
                    content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                        data: b"support".to_vec(),
                        path: rel_path.to_string(),
                        x: false,
                    }),
                    hashout: "support_hash".to_string(),
                }),
            },
            origin_id: origin_id.to_string(),
            source_addr,
            filters: vec![],
            annotations,
        }
    }

    // Support inputs land in ws/ alongside Dep inputs but produce no list file
    // (so SRC_/LIST_ env routing in pluginexec stays clean) and never appear in
    // source_map (they aren't "source"). Dep input on the same origin_id still
    // produces its own list file.
    #[tokio::test]
    async fn support_input_unpacked_but_no_list_or_source_map() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "gen".to_string(), BTreeMap::new());
        let dep = make_raw_input("dep||0", "pkg/foo.go", src_addr.clone(), vec![]);
        let sup = make_support_input("dep||0", "pkg/fixtures/seed.json", src_addr);
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![dep, sup],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        // Both files materialize under ws/.
        assert!(sandbox.join("ws").join("pkg").join("foo.go").exists());
        assert!(
            sandbox
                .join("ws")
                .join("pkg")
                .join("fixtures")
                .join("seed.json")
                .exists()
        );

        // Dep list exists and contains foo.go.
        let dep_list = sandbox.join("list").join("input_dep||0.list");
        let dep_content = fs::read_to_string(&dep_list)?;
        assert!(dep_content.contains("foo.go"), "got: {dep_content}");
        assert!(
            !dep_content.contains("seed.json"),
            "support file leaked into dep list: {dep_content}"
        );

        // No support_*.list file written for the Support input.
        assert!(
            !sandbox.join("list").join("support_dep||0.list").exists(),
            "support inputs must not produce list files"
        );

        // source_map only mentions the Dep file.
        let map: BTreeMap<String, String> = serde_json::from_str(&fs::read_to_string(
            sandbox.join("ws").join("pkg").join("source_map.json"),
        )?)?;
        assert_eq!(map.get("pkg/foo.go").map(|s| s.as_str()), Some("//pkg:gen"));
        assert!(
            !map.keys().any(|k| k.contains("seed.json")),
            "support file must not appear in source_map: {:?}",
            map
        );
        Ok(())
    }

    // A Support input must always land in ws/, even when the parent dep was
    // referenced as a tool (annotations carry `unpack_root=tools`). Otherwise
    // a tool's support files would scatter into `exec_tools/` instead of being
    // available to the consumer's working tree.
    #[tokio::test]
    async fn support_input_ignores_unpack_root_annotation() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        let src_addr = Addr::new(PkgBuf::from("pkg"), "tool".to_string(), BTreeMap::new());
        let sup = make_support_input_with_annotations(
            "tool|cc|0",
            "pkg/fixtures/seed.json",
            src_addr,
            BTreeMap::from([("unpack_root".to_string(), "tools".to_string())]),
        );
        let def = make_target_def("pkg");
        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![sup],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        bridge().run(req, &ct).await?;

        assert!(
            sandbox
                .join("ws")
                .join("pkg")
                .join("fixtures")
                .join("seed.json")
                .exists(),
            "support file must land in ws/, not exec_tools/"
        );
        assert!(
            !sandbox
                .join("exec_tools")
                .join("pkg")
                .join("fixtures")
                .join("seed.json")
                .exists(),
            "support file must not follow unpack_root=tools annotation"
        );
        Ok(())
    }

    // TargetDef.support_files declared paths must be collected at end of run
    // into one SupportFile-typed OutputArtifact alongside the regular Outputs.
    #[tokio::test]
    async fn target_support_files_collected_as_support_artifact() -> anyhow::Result<()> {
        use crate::engine::driver::targetdef::path::{CodegenMode, Content, Path as TargetPath};
        let dir = tempdir()?;
        let sandbox = dir.path().to_path_buf();
        fs::create_dir_all(sandbox.join("ws").join("pkg").join("fixtures"))?;
        fs::write(
            sandbox
                .join("ws")
                .join("pkg")
                .join("fixtures")
                .join("seed.json"),
            b"{}",
        )?;

        let mut def = make_target_def("pkg");
        def.support_files = vec![TargetPath {
            content: Content::FilePath("pkg/fixtures/seed.json".to_string()),
            codegen_tree: CodegenMode::None,
            collect: true,
        }];

        let ct = ctoken();
        let req = RunRequest {
            request_id: &"rid".to_string(),
            target: &def,
            tree_root_path: dir.path().to_path_buf(),
            inputs: vec![],
            hashin: "hash",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };
        let res = bridge().run(req, &ct).await?;
        let support_artifacts: Vec<_> = res
            .artifacts
            .iter()
            .filter(|a| a.r#type == outputartifact::Type::SupportFile)
            .collect();
        assert_eq!(support_artifacts.len(), 1);
        let a = support_artifacts.first().expect("support artifact present");
        assert_eq!(a.name, "support.tar");
        assert_eq!(a.group, "");
        // hashout populated from the tar bytes.
        assert!(!a.hashout.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    fn read_tar_entries(
        buf: &[u8],
    ) -> Vec<(
        std::path::PathBuf,
        tar::EntryType,
        Option<std::path::PathBuf>,
    )> {
        let mut archive = tar::Archive::new(std::io::Cursor::new(buf));
        let mut out = Vec::new();
        for entry in archive.entries().expect("entries") {
            let entry = entry.expect("entry");
            let path = entry.path().expect("path").into_owned();
            let et = entry.header().entry_type();
            let link = entry.link_name().ok().flatten().map(|p| p.into_owned());
            out.push((path, et, link));
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn add_path_to_tar_dirpath_includes_symlink() {
        use crate::engine::driver::targetdef::path::{CodegenMode, Path as TPath};

        let ws = tempdir().expect("ws tempdir");
        let sub = ws.path().join("out");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("real.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("real.txt", sub.join("link.txt")).unwrap();

        let p = TPath {
            content: Content::DirPath("out".to_string()),
            codegen_tree: CodegenMode::None,
            collect: true,
        };
        let mut packer = hartifactcontent::tar::TarPacker::new();
        add_path_to_tar(&mut packer, ws.path(), &p, "group").expect("add_path_to_tar");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let entries = read_tar_entries(&buf);
        let link = entries
            .iter()
            .find(|(p, _, _)| p.ends_with("link.txt"))
            .expect("link.txt entry");
        assert_eq!(link.1, tar::EntryType::Symlink);
        assert_eq!(link.2.as_deref(), Some(std::path::Path::new("real.txt")));
        assert!(
            entries
                .iter()
                .any(|(p, et, _)| p.ends_with("real.txt") && et.is_file())
        );
    }

    #[cfg(unix)]
    #[test]
    fn add_path_to_tar_glob_includes_symlink() {
        use crate::engine::driver::targetdef::path::{CodegenMode, Path as TPath};

        let ws = tempdir().expect("ws tempdir");
        let sub = ws.path().join("g");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("real.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("real.txt", sub.join("link.txt")).unwrap();

        let p = TPath {
            content: Content::Glob("g/*.txt".to_string()),
            codegen_tree: CodegenMode::None,
            collect: true,
        };
        let mut packer = hartifactcontent::tar::TarPacker::new();
        add_path_to_tar(&mut packer, ws.path(), &p, "group").expect("add_path_to_tar");

        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack");

        let entries = read_tar_entries(&buf);
        let link = entries
            .iter()
            .find(|(p, _, _)| p.ends_with("link.txt"))
            .expect("link.txt entry");
        assert_eq!(link.1, tar::EntryType::Symlink);
        assert_eq!(link.2.as_deref(), Some(std::path::Path::new("real.txt")));
    }
}
