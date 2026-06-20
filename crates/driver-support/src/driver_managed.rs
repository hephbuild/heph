use anyhow::Context;
use async_trait::async_trait;
use hcore::hartifactcontent;
use hcore::hasync::{self, Cancellable};
use hplugin::driver::inputartifact;
use hplugin::driver::outputartifact::Content::TarPath;
use hplugin::driver::targetdef::path::{self, Content};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunInput, RunRequest, outputartifact,
};
use hplugin::provider::TargetSpec;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};
use xxhash_rust::xxh3::Xxh3;

// ---------------------------------------------------------------------
// Shared types: trait + request/response shapes
// ---------------------------------------------------------------------

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
    /// Config schema, forwarded by the bridge's [`Driver::schema`]. A config-less
    /// driver returns `DriverSchema::default()`. See
    /// [`hplugin::driver::Driver::schema`].
    fn schema(&self) -> hplugin::driver::DriverSchema;
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
    /// Whether this driver implements its own `run_shell`. Default false:
    /// the bridge substitutes a synthetic shell `TargetSpec`, parses it
    /// on the configured shell fallback driver, and dispatches that
    /// driver's `run_shell` inside the already-materialized sandbox.
    fn supports_shell(&self) -> bool {
        false
    }
    async fn run_shell<'a, 'io>(
        &self,
        _req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        anyhow::bail!(
            "run_shell called on a ManagedDriver with supports_shell()=false; the bridge must dispatch to the shell fallback"
        )
    }
}

// ---------------------------------------------------------------------
// Shell fallback wiring (consumed by the bridge in `heph-driver-bridge`
// and by the OS/FUSE sandbox runners)
// ---------------------------------------------------------------------

/// Fallback used when the wrapped `ManagedDriver` returns false from
/// `supports_shell()`. The bridge swaps in `spec_template` (with the
/// original target's addr) and dispatches `run_shell` on `driver`
/// inside the already-materialized sandbox.
pub struct ShellFallback {
    pub driver: Arc<dyn ManagedDriver>,
    pub spec_template: Arc<TargetSpec>,
}

// ---------------------------------------------------------------------
// Shared helpers (used by ManagedDriverOs here + ManagedDriverFuse in
// `heph-driver-bridge`)
// ---------------------------------------------------------------------

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading state from run_inner; not part of public API"
)]
pub async fn invoke_inner<'a, 'io>(
    driver: &dyn ManagedDriver,
    mut req: RunRequest<'a, 'io>,
    ctoken: &(dyn Cancellable + Send + Sync),
    shell: bool,
    sandbox_dir: PathBuf,
    ws_dir: PathBuf,
    sandbox_pkg_dir: PathBuf,
    inputs: Vec<ManagedRunInput>,
    shell_fallback: &ShellFallback,
) -> anyhow::Result<ManagedRunResponse> {
    // Some inner drivers (e.g. pluginexec writing `init.sh`) read
    // `req.request.sandbox_dir` for filesystem ops. Keep both consistent
    // with the (maybe-redirected) sandbox_dir we just built.
    req.sandbox_dir = sandbox_dir.clone();
    let mreq = ManagedRunRequest {
        sandbox_dir,
        sandbox_ws_dir: ws_dir,
        sandbox_pkg_dir,
        request: req,
        inputs,
    };
    let res = if shell {
        if driver.supports_shell() {
            driver
                .run_shell(mreq, ctoken)
                .await
                .with_context(|| "driver run_shell")?
        } else {
            run_shell_fallback(mreq, ctoken, shell_fallback)
                .await
                .with_context(|| "shell fallback")?
        }
    } else {
        driver
            .run(mreq, ctoken)
            .await
            .with_context(|| "driver run")?
    };
    Ok(res)
}

/// Run an interactive shell on `shell_fallback` inside the
/// already-materialized sandbox. The fallback parses a synthetic
/// `TargetSpec` (the configured template stamped with the original
/// target's `addr`), then runs its own `run_shell` against that def
/// reusing every sandbox path and input the original driver was given.
async fn run_shell_fallback<'a, 'io>(
    mreq: ManagedRunRequest<'a, 'io>,
    ctoken: &(dyn Cancellable + Send + Sync),
    shell_fallback: &ShellFallback,
) -> anyhow::Result<ManagedRunResponse> {
    let ManagedRunRequest {
        request,
        sandbox_dir,
        sandbox_ws_dir,
        sandbox_pkg_dir,
        inputs,
    } = mreq;
    let RunRequest {
        request_id,
        target,
        tree_root_path,
        inputs: run_inputs,
        hashin,
        stdin,
        stdout,
        stderr,
        sandbox_dir: req_sandbox_dir,
        events,
    } = request;

    let mut synthetic = (*shell_fallback.spec_template).clone();
    synthetic.addr = target.addr.clone();

    let parse_resp = shell_fallback
        .driver
        .parse(
            ParseRequest {
                request_id: request_id.clone(),
                target_spec: Arc::new(synthetic),
            },
            ctoken,
        )
        .await
        .with_context(|| "parse synthetic shell spec on fallback driver")?;

    // Reuse the original `TargetDef` (preserves addr/inputs/outputs/hash
    // metadata the fallback's `run_shell` may read off of `req.target`)
    // but swap `raw_def` so pluginexec's `def::<TargetDef>()` downcast
    // sees its own type.
    let mut new_target = target.clone();
    new_target.raw_def = parse_resp.target_def.raw_def;

    let new_req = RunRequest {
        request_id,
        target: &new_target,
        tree_root_path,
        inputs: run_inputs,
        hashin,
        stdin,
        stdout,
        stderr,
        sandbox_dir: req_sandbox_dir,
        events,
    };
    let new_mreq = ManagedRunRequest {
        request: new_req,
        sandbox_dir,
        sandbox_ws_dir,
        sandbox_pkg_dir,
        inputs,
    };
    shell_fallback.driver.run_shell(new_mreq, ctoken).await
}

pub fn collect_outputs(
    res: &mut ManagedRunResponse,
    target: &hplugin::driver::targetdef::TargetDef,
    hashin: &str,
    ws_dir: &Path,
    sandbox_dir: &Path,
) -> anyhow::Result<()> {
    for output in &target.outputs {
        if !output.paths.iter().any(|path| path.collect) {
            continue;
        }
        let mut tar = hartifactcontent::tar::TarPacker::new();
        for path in &output.paths {
            if !path.collect {
                continue;
            }
            add_path_to_tar(&mut tar, ws_dir, path, &output.group)?;
        }
        let tarpath = pack_to_artifact_tar(sandbox_dir, hashin, &output.group, tar)?;
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
            add_path_to_tar(&mut tar, ws_dir, path, "support")?;
        }
        let (tarpath, hashout) = pack_to_artifact_tar(sandbox_dir, hashin, "support", tar)?;
        res.artifacts.push(outputartifact::OutputArtifact {
            group: String::new(),
            name: "support.tar".to_string(),
            r#type: outputartifact::Type::SupportFile,
            content: TarPath(tarpath),
            hashout,
        });
    }
    Ok(())
}

/// Input annotation key opting a dep into source_map.json generation.
/// Absent (the default) means the input is excluded — source_map.json is
/// only emitted for targets whose inputs explicitly request it (e.g. the
/// go plugin's golist deps). Value must be the string `"true"`.
pub const SOURCE_MAP_ANNOTATION: &str = "source_map";

fn source_map_enabled(input: &RunInput) -> bool {
    input
        .annotations
        .get(SOURCE_MAP_ANNOTATION)
        .is_some_and(|v| v == "true")
}

/// Build the source_map.json contents from the opted-in inputs. Only inputs
/// carrying the `source_map=true` annotation contribute; everything else is
/// skipped so the map (and the file) stays empty by default. Returns an empty
/// map when no input opts in — callers skip writing the file in that case.
pub(crate) fn build_source_map(
    inputs: &[ManagedRunInput],
    ws_dir: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut source_map: BTreeMap<String, String> = BTreeMap::new();
    for managed_input in inputs {
        if !source_map_enabled(&managed_input.input) {
            continue;
        }
        if managed_input.unpack_root != ws_dir {
            continue;
        }
        if matches!(
            managed_input.input.artifact.r#type,
            inputartifact::Type::Support
        ) {
            continue;
        }
        let source_addr_str = managed_input.input.source_addr.format();
        let filters = &managed_input.input.filters;
        // Walk artifact directly instead of reading list_path: after group
        // expansion, multiple inputs share parent origin_id → list_path_for
        // gives them one shared file (opened append). Reading that shared
        // list per-input would let the last-iterated input's source_addr
        // overwrite earlier ones for paths only the earlier inputs produced.
        let content = managed_input.input.artifact.content.as_ref();
        for entry in content
            .walk()
            .with_context(|| format!("walk content for source_map (source={source_addr_str})"))?
        {
            let entry = entry
                .with_context(|| format!("read entry for source_map (source={source_addr_str})"))?;
            if !filters.is_empty() && !filters.iter().any(|f| Path::new(f) == entry.path.as_path())
            {
                continue;
            }
            let rel = entry.path.to_string_lossy().into_owned();
            source_map.insert(rel, source_addr_str.clone());
        }
    }
    Ok(source_map)
}

/// Write `source_map.json` into the sandbox package dir, but only when at
/// least one input opted in. With no opted-in inputs the map is empty and the
/// file is skipped entirely — consumers (e.g. golist) treat a missing file as
/// an empty map.
pub fn write_source_map(
    inputs: &[ManagedRunInput],
    ws_dir: &Path,
    sandbox_pkg_dir: &Path,
) -> anyhow::Result<()> {
    let source_map = build_source_map(inputs, ws_dir)?;
    if source_map.is_empty() {
        return Ok(());
    }
    let source_map_json =
        serde_json::to_string(&source_map).with_context(|| "serialize source_map")?;
    fs::write(sandbox_pkg_dir.join("source_map.json"), source_map_json)
        .with_context(|| "write source_map.json")?;
    Ok(())
}

pub fn resolve_unpack_root(input: &RunInput, sandbox_dir: &Path, ws_dir: &Path) -> PathBuf {
    match input.artifact.r#type {
        inputartifact::Type::Support => ws_dir.to_path_buf(),
        inputartifact::Type::Dep => input
            .annotations
            .get("unpack_root")
            .map(|root| sandbox_dir.join(format!("exec_{root}")))
            .unwrap_or_else(|| ws_dir.to_path_buf()),
    }
}

pub fn list_path_for(input: &RunInput, list_dir: &Path) -> Option<PathBuf> {
    match input.artifact.r#type {
        inputartifact::Type::Dep => Some(list_dir.join(format!("input_{}.list", input.origin_id))),
        inputartifact::Type::Support => None,
    }
}

// ---------------------------------------------------------------------
// Output collection helpers
// ---------------------------------------------------------------------

struct HashingWriter<W: Write> {
    inner: W,
    hasher: Xxh3,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
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
                let md = fs::symlink_metadata(&matched).with_context(|| {
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
    let tarf = File::create(Path::new(&tarpath))
        .with_context(|| format!("create output tar {tarpath:?}"))?;

    let mut hw = HashingWriter {
        inner: tarf,
        hasher: Xxh3::new(),
    };

    tar.pack(&mut hw).with_context(|| "pack")?;

    Ok((tarpath, format!("{:x}", hw.hasher.digest())))
}

#[cfg(test)]
mod source_map_tests {
    use super::*;
    use hcore::hartifactcontent::tar::{TarPacker, TarWalker};
    use hcore::hartifactcontent::{Content, WalkEntry};
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::inputartifact::{InputArtifact, Type};
    use std::io::{Cursor, Read};

    struct TarBytes(Vec<u8>);

    impl Content for TarBytes {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            Ok(Box::new(Cursor::new(self.0.clone())))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(TarWalker::new(Cursor::new(self.0.clone()))?))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn pack_files(files: &[(&str, &str)]) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut packer = TarPacker::new();
        for (rel, body) in files {
            let abs = dir.path().join(rel);
            if let Some(p) = abs.parent() {
                fs::create_dir_all(p).expect("mkdir");
            }
            fs::write(&abs, body).expect("write");
            packer.create_file(abs.to_str().unwrap().to_string(), rel.to_string());
        }
        let mut buf = Vec::new();
        packer.pack(&mut buf).expect("pack tar");
        buf
    }

    fn make_input(origin_id: &str, source_addr: &str, files: &[(&str, &str)]) -> ManagedRunInput {
        let tar = pack_files(files);
        ManagedRunInput {
            input: RunInput {
                artifact: InputArtifact {
                    r#type: Type::Dep,
                    origin_id: origin_id.to_string(),
                    content: Arc::new(TarBytes(tar)),
                },
                origin_id: origin_id.to_string(),
                source_addr: parse_addr(source_addr).expect("parse addr"),
                filters: vec![],
                annotations: BTreeMap::from([(
                    SOURCE_MAP_ANNOTATION.to_string(),
                    "true".to_string(),
                )]),
            },
            list_path: Some(PathBuf::from("/dev/null")),
            unpack_root: PathBuf::from("/ws"),
        }
    }

    // Regression: when group expansion (engine/expand.rs) inlines multiple
    // child inputs under one parent origin_id, list_path_for assigns them all
    // the same list file (opened with append=true at unpack). The old
    // build_source_map read that shared file per-input and let the
    // last-iterated input's source_addr overwrite the correct mapping for
    // paths only the earlier input actually produced. Now we walk each
    // artifact directly, so each file is mapped to the input that really
    // contributed it.
    #[test]
    fn build_source_map_distinguishes_inputs_sharing_origin_id() {
        let ws_dir = PathBuf::from("/ws");
        let inputs = vec![
            make_input(
                "dep|srcfiles|0",
                "//pkg:_wasm",
                &[("pkg/resources/ajv.wasm.br", "wasm")],
            ),
            make_input(
                "dep|srcfiles|0",
                "//pkg:_schemas",
                &[("pkg/resources/mock-data/x.json", "{}")],
            ),
        ];
        let m = build_source_map(&inputs, &ws_dir).expect("build_source_map");
        assert_eq!(
            m.get("pkg/resources/ajv.wasm.br").map(String::as_str),
            Some("//pkg:_wasm"),
            "ajv.wasm.br must map to _wasm, not _schemas (last-write-wins bug): {:?}",
            m
        );
        assert_eq!(
            m.get("pkg/resources/mock-data/x.json").map(String::as_str),
            Some("//pkg:_schemas"),
        );
    }

    #[test]
    fn build_source_map_respects_filters() {
        let ws_dir = PathBuf::from("/ws");
        let mut input = make_input(
            "dep|f|0",
            "//pkg:_t",
            &[("pkg/a.txt", "a"), ("pkg/b.txt", "b")],
        );
        input.input.filters = vec!["pkg/a.txt".to_string()];
        let m = build_source_map(&[input], &ws_dir).expect("build_source_map");
        assert!(m.contains_key("pkg/a.txt"));
        assert!(
            !m.contains_key("pkg/b.txt"),
            "filtered paths must not appear in source_map: {:?}",
            m
        );
    }

    #[test]
    fn build_source_map_skips_inputs_without_opt_in() {
        let ws_dir = PathBuf::from("/ws");
        let mut input = make_input("dep|t|0", "//pkg:_t", &[("pkg/a.txt", "a")]);
        // Default: no opt-in annotation → excluded entirely.
        input.input.annotations.remove(SOURCE_MAP_ANNOTATION);
        let m = build_source_map(&[input], &ws_dir).expect("build_source_map");
        assert!(
            m.is_empty(),
            "inputs without the source_map opt-in must not contribute: {:?}",
            m
        );
    }

    #[test]
    fn build_source_map_skips_non_ws_unpack_root() {
        let ws_dir = PathBuf::from("/ws");
        let mut input = make_input("dep|t|0", "//pkg:_t", &[("pkg/a.txt", "a")]);
        input.unpack_root = PathBuf::from("/sandbox/exec_tools");
        let m = build_source_map(&[input], &ws_dir).expect("build_source_map");
        assert!(
            m.is_empty(),
            "inputs unpacked outside ws_dir must not contribute: {:?}",
            m
        );
    }
}
