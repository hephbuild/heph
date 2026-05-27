use crate::engine::Engine;
use crate::engine::config_file::{FuseConfig, FuseMode};
use crate::engine::driver::inputartifact;
use crate::engine::driver::outputartifact::Content::TarPath;
use crate::engine::driver::targetdef::path::{self, Content};
use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver,
    ParseRequest, ParseResponse, RunInput, RunRequest, RunResponse, outputartifact,
};
use crate::engine::driver_managed_fuse::ManagedDriverFuse;
use crate::engine::driver_managed_os::ManagedDriverOs;
use crate::engine::provider::TargetSpec;
use crate::hartifactcontent;
use crate::hasync::{self, Cancellable};
use crate::loosespecparser::TargetSpecValue;
use anyhow::Context;
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};
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
// Router: picks Os or Fuse per target based on FuseConfig + input walk
// ---------------------------------------------------------------------

/// Hardcoded auto-mode threshold. Below this total input byte size, the
/// cost of FUSE mount setup outweighs unpack-copy. Tuned to roughly the
/// observed macFUSE per-mount cost amortized across small targets.
const AUTO_FUSE_BYTE_THRESHOLD: u64 = 1024 * 1024;

pub struct ManagedDriverBridge {
    cfg: FuseConfig,
    os: ManagedDriverOs,
    fuse: Option<ManagedDriverFuse>,
}

/// Fallback used when the wrapped `ManagedDriver` returns false from
/// `supports_shell()`. The bridge swaps in `spec_template` (with the
/// original target's addr) and dispatches `run_shell` on `driver`
/// inside the already-materialized sandbox.
pub struct ShellFallback {
    pub driver: Arc<dyn ManagedDriver>,
    pub spec_template: Arc<TargetSpec>,
}

impl ShellFallback {
    /// Default: pluginexec with `run = []`. `wrap_run_shell` turns
    /// the single `bash` command into an interactive bash session via
    /// `bash_args_shell` + `init.sh`.
    pub fn default_exec() -> Arc<Self> {
        let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
        config.insert(
            "run".to_string(),
            TargetSpecValue::List(vec![]),
        );
        Arc::new(Self {
            driver: Arc::new(crate::pluginexec::Driver::new_exec()),
            spec_template: Arc::new(TargetSpec {
                addr: Default::default(),
                driver: "exec".to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        })
    }
}

impl Engine {
    pub fn new_managed_driver(&self, driver: Box<dyn ManagedDriver>) -> ManagedDriverBridge {
        let driver = Arc::new(driver);
        let shell_fallback = ShellFallback::default_exec();
        let os = ManagedDriverOs {
            driver: driver.clone(),
            shell_fallback: shell_fallback.clone(),
        };
        let fuse = self.fuse.layered_fs().map(|fs| ManagedDriverFuse {
            driver: driver.clone(),
            shell_fallback: shell_fallback.clone(),
            home: self.home.clone(),
            fs,
            fuse_lower: self.fuse.lower.clone(),
            fuse_upper: self.fuse.upper.clone(),
        });
        ManagedDriverBridge {
            cfg: self.cfg.fuse,
            os,
            fuse,
        }
    }
}

impl ManagedDriverBridge {
    /// Test helper: build an OS-only bridge (no FUSE) without an `Engine`.
    /// Production code paths construct bridges via `Engine::new_managed_driver`.
    #[cfg(test)]
    pub(crate) fn new_os_for_test(driver: Box<dyn ManagedDriver>) -> Self {
        Self::new_os_for_test_with_shell_fallback(driver, ShellFallback::default_exec())
    }

    #[cfg(test)]
    pub(crate) fn new_os_for_test_with_shell_fallback(
        driver: Box<dyn ManagedDriver>,
        shell_fallback: Arc<ShellFallback>,
    ) -> Self {
        Self {
            cfg: FuseConfig {
                enabled: Some(false),
            },
            os: ManagedDriverOs {
                driver: Arc::new(driver),
                shell_fallback,
            },
            fuse: None,
        }
    }
}

#[async_trait]
impl Driver for ManagedDriverBridge {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        self.os.driver.config(req)
    }

    async fn parse(
        &self,
        req: ParseRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        self.os.driver.parse(req, ctoken).await
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        self.os.driver.apply_transitive(req, ctoken).await
    }

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        match self.pick(&req.inputs)? {
            Pick::Os => self.os.run_inner(req, ctoken, false).await,
            Pick::Fuse => {
                let fuse = self.fuse.as_ref().expect("Pick::Fuse implies fuse is Some");
                fuse.run_inner(req, ctoken, false).await
            }
        }
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        match self.pick(&req.inputs)? {
            Pick::Os => self.os.run_inner(req, ctoken, true).await,
            Pick::Fuse => {
                let fuse = self.fuse.as_ref().expect("Pick::Fuse implies fuse is Some");
                fuse.run_inner(req, ctoken, true).await
            }
        }
    }
}

enum Pick {
    Os,
    Fuse,
}

impl ManagedDriverBridge {
    /// Decides os vs fuse based on `FuseConfig` and (auto only) input
    /// walk. Errors when `enabled=true` but no FUSE mount available.
    fn pick(&self, inputs: &[RunInput]) -> anyhow::Result<Pick> {
        match self.cfg.mode() {
            FuseMode::Off => Ok(Pick::Os),
            FuseMode::On => {
                if self.fuse.is_none() {
                    anyhow::bail!(
                        "fuse.enabled=true but no FUSE mount available — check support_check"
                    );
                }
                Ok(Pick::Fuse)
            }
            FuseMode::Auto => {
                if self.fuse.is_none() {
                    return Ok(Pick::Os);
                }
                let total: u64 = inputs
                    .iter()
                    .map(|i| i.artifact.content.byte_size().unwrap_or(0))
                    .sum();
                if total >= AUTO_FUSE_BYTE_THRESHOLD {
                    Ok(Pick::Fuse)
                } else {
                    Ok(Pick::Os)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Shared helpers (used by ManagedDriverOs + ManagedDriverFuse)
// ---------------------------------------------------------------------

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper threading state from run_inner; not part of public API"
)]
pub(crate) async fn invoke_inner<'a, 'io>(
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

pub(crate) fn collect_outputs(
    res: &mut ManagedRunResponse,
    target: &crate::engine::driver::targetdef::TargetDef,
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

pub(crate) fn build_source_map(
    inputs: &[ManagedRunInput],
    ws_dir: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut source_map: BTreeMap<String, String> = BTreeMap::new();
    for managed_input in inputs {
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
            let entry = entry.with_context(|| {
                format!("read entry for source_map (source={source_addr_str})")
            })?;
            if !filters.is_empty()
                && !filters.iter().any(|f| Path::new(f) == entry.path.as_path())
            {
                continue;
            }
            let rel = entry.path.to_string_lossy().into_owned();
            source_map.insert(rel, source_addr_str.clone());
        }
    }
    Ok(source_map)
}

pub(crate) fn resolve_unpack_root(input: &RunInput, sandbox_dir: &Path, ws_dir: &Path) -> PathBuf {
    match input.artifact.r#type {
        inputartifact::Type::Support => ws_dir.to_path_buf(),
        inputartifact::Type::Dep => input
            .annotations
            .get("unpack_root")
            .map(|root| sandbox_dir.join(format!("exec_{root}")))
            .unwrap_or_else(|| ws_dir.to_path_buf()),
    }
}

pub(crate) fn list_path_for(input: &RunInput, list_dir: &Path) -> Option<PathBuf> {
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
mod shell_fallback_tests {
    use super::*;
    use crate::engine::driver::targetdef::TargetDef;
    use crate::hasync::StdCancellationToken;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct NoShellDriver;

    #[async_trait]
    impl ManagedDriver for NoShellDriver {
        fn config(&self, _: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "noshell".to_string(),
            })
        }
        async fn parse(
            &self,
            _: ParseRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            unimplemented!("parse not exercised by shell fallback test")
        }
        async fn apply_transitive(
            &self,
            req: ApplyTransitiveRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            Ok(ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            _: ManagedRunRequest<'a, 'io>,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            unimplemented!("run not exercised by shell fallback test")
        }
        // supports_shell + run_shell inherit defaults — the bridge must
        // never invoke this driver's run_shell.
    }

    struct RecordingShellDriver {
        parse_called: Arc<AtomicBool>,
        run_shell_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ManagedDriver for RecordingShellDriver {
        fn config(&self, _: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "recording".to_string(),
            })
        }
        async fn parse(
            &self,
            _: ParseRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            self.parse_called.store(true, Ordering::SeqCst);
            Ok(ParseResponse {
                target_def: TargetDef {
                    addr: Default::default(),
                    labels: vec![],
                    raw_def: Arc::new("recorded-shell-def".to_string()),
                    inputs: vec![],
                    outputs: vec![],
                    support_files: vec![],
                    cache: false,
                    disable_remote_cache: false,
                    pty: false,
                    hash: vec![],
                    transparent: false,
                },
            })
        }
        async fn apply_transitive(
            &self,
            req: ApplyTransitiveRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ApplyTransitiveResponse> {
            Ok(ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            _: ManagedRunRequest<'a, 'io>,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            unimplemented!()
        }
        fn supports_shell(&self) -> bool {
            true
        }
        async fn run_shell<'a, 'io>(
            &self,
            _: ManagedRunRequest<'a, 'io>,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            self.run_shell_called.store(true, Ordering::SeqCst);
            Ok(ManagedRunResponse { artifacts: vec![] })
        }
    }

    #[tokio::test]
    async fn run_shell_dispatches_to_fallback_when_driver_does_not_support() -> anyhow::Result<()> {
        let parse_called = Arc::new(AtomicBool::new(false));
        let run_shell_called = Arc::new(AtomicBool::new(false));
        let fallback = Arc::new(ShellFallback {
            driver: Arc::new(RecordingShellDriver {
                parse_called: parse_called.clone(),
                run_shell_called: run_shell_called.clone(),
            }),
            spec_template: ShellFallback::default_exec().spec_template.clone(),
        });
        let bridge = ManagedDriverBridge::new_os_for_test_with_shell_fallback(
            Box::new(NoShellDriver),
            fallback,
        );

        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;
        let sandbox = tmp.path().join("sandbox");
        fs::create_dir_all(&sandbox)?;

        let target_def = TargetDef {
            addr: Default::default(),
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
        };
        let request_id = "shell-fallback-test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: tmp.path().to_path_buf(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };

        bridge.run_shell(req, &ctoken).await?;

        assert!(
            parse_called.load(Ordering::SeqCst),
            "fallback driver's parse must be called",
        );
        assert!(
            run_shell_called.load(Ordering::SeqCst),
            "fallback driver's run_shell must be called",
        );
        Ok(())
    }
}

#[cfg(test)]
mod source_map_tests {
    use super::*;
    use crate::engine::driver::inputartifact::{InputArtifact, Type};
    use crate::hartifactcontent::tar::{TarPacker, TarWalker};
    use crate::hartifactcontent::{Content, WalkEntry};
    use crate::htaddr::parse_addr;
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
                annotations: BTreeMap::new(),
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
