use anyhow::Context;
use async_trait::async_trait;
use hcore::hartifactcontent;
use hcore::hasync::{self, Cancellable};
use hmodel::htaddr::Addr;
use hplugin::driver::inputartifact;
use hplugin::driver::outputartifact::Content::TarPath;
use hplugin::driver::targetdef::path::{self, Content};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunInput, RunRequest, outputartifact,
};
use hplugin::provider::TargetSpec;
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

/// Tar, hash and stage every collectable output of a finished run, through
/// `hcore::blocking::run`.
///
/// This reads, tars and xxh3-hashes **every output byte** the target produced.
/// Run inline in an `async fn` it holds a tokio worker for that whole time, and
/// with `2 * ncpu` execute permits against `ncpu` workers, enough targets landing
/// at once park every one of them — the reactor, the timer wheel and every
/// in-flight transfer stop with them. A worker-second lost here is lost to the
/// entire runtime, not just to this target.
///
/// The clones are of the output *declarations* (a handful of small structs), not
/// of anything they name, and they buy the `'static` the pool needs.
pub async fn collect_outputs(
    target: &hplugin::driver::targetdef::TargetDef,
    hashin: &str,
    ws_dir: &Path,
    sandbox_dir: &Path,
) -> anyhow::Result<Vec<outputartifact::OutputArtifact>> {
    let outputs = target.outputs.clone();
    let support_files = target.support_files.clone();
    let hashin = hashin.to_string();
    let (ws_dir, sandbox_dir) = (ws_dir.to_path_buf(), sandbox_dir.to_path_buf());
    hcore::blocking::run(move || {
        collect_outputs_inner(&outputs, &support_files, &hashin, &ws_dir, &sandbox_dir)
    })
    .await
}

/// The synchronous body of [`collect_outputs`], split out so it can be handed to
/// `hcore::blocking::run` whole.
fn collect_outputs_inner(
    target_outputs: &[hplugin::driver::targetdef::Output],
    support_files: &[hplugin::driver::targetdef::path::Path],
    hashin: &str,
    ws_dir: &Path,
    sandbox_dir: &Path,
) -> anyhow::Result<Vec<outputartifact::OutputArtifact>> {
    let mut artifacts = Vec::new();
    for output in target_outputs {
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
        artifacts.push(outputartifact::OutputArtifact {
            group: output.group.clone(),
            name: format!("{}.tar", output.group),
            r#type: outputartifact::Type::Output,
            content: TarPath(tarpath.0),
            hashout: tarpath.1,
        });
    }
    if !support_files.is_empty() {
        let mut tar = hartifactcontent::tar::TarPacker::new();
        for path in support_files {
            add_path_to_tar(&mut tar, ws_dir, path, "support")?;
        }
        let (tarpath, hashout) = pack_to_artifact_tar(sandbox_dir, hashin, "support", tar)?;
        artifacts.push(outputartifact::OutputArtifact {
            group: String::new(),
            name: "support.tar".to_string(),
            r#type: outputartifact::Type::SupportFile,
            content: TarPath(tarpath),
            hashout,
        });
    }
    Ok(artifacts)
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
        // Enumerate the artifact's own paths directly instead of reading
        // list_path: after group expansion, multiple inputs share parent
        // origin_id → list_path_for gives them one shared file (opened append).
        // Reading that shared list per-input would let the last-iterated input's
        // source_addr overwrite earlier ones for paths only the earlier inputs
        // produced. `entry_paths` is header-only for tar-backed content, so this
        // maps paths without reading the file bytes.
        let content = managed_input.input.artifact.content.as_ref();
        for rel in content.entry_paths().with_context(|| {
            format!("enumerate content for source_map (source={source_addr_str})")
        })? {
            if !filters.is_empty() && !filters.iter().any(|f| Path::new(f) == rel.as_path()) {
                continue;
            }
            source_map.insert(rel.to_string_lossy().into_owned(), source_addr_str.clone());
        }
    }
    Ok(source_map)
}

/// Write `source_map.json` into the sandbox package dir, but only when at
/// least one input opted in. With no opted-in inputs the map is empty and the
/// file is skipped entirely — consumers (e.g. golist) treat a missing file as
/// an empty map.
/// [`write_source_map`] through `hcore::blocking::run`, taking and returning the inputs so
/// the job can own them.
///
/// Cheap when nothing opts in — but an input that does costs an `entry_paths()`
/// per artifact, which for tar-backed cache content is a header scan over a
/// sqlite blob. That is real I/O and it does not belong on a tokio worker.
pub async fn write_source_map_blocking(
    inputs: Vec<ManagedRunInput>,
    ws_dir: &Path,
    sandbox_pkg_dir: &Path,
) -> anyhow::Result<Vec<ManagedRunInput>> {
    let (ws_dir, sandbox_pkg_dir) = (ws_dir.to_path_buf(), sandbox_pkg_dir.to_path_buf());
    hcore::blocking::run(move || {
        write_source_map(&inputs, &ws_dir, &sandbox_pkg_dir)?;
        anyhow::Ok(inputs)
    })
    .await
}

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

/// Reject two *different* producer targets writing the same file into one
/// sandbox. Keyed on producer identity (`source_addr`), per the sandbox
/// isolation model: a materialized path has exactly one owning target. A path
/// claimed by the *same* `source_addr` more than once — a diamond dependency, or
/// a target depended on twice — resolves to a single owner and is allowed.
///
/// Only file and symlink paths are compared — directory entries are excluded,
/// so shared parent directories never trip the check (two targets may populate
/// disjoint files under a common dir). `filters` are honored so a
/// partially-consumed input only claims the paths it exposes.
///
/// Path enumeration goes through [`Content::entry_paths`], which the tar-backed
/// cache artifact implements as a header-only scan (seeking past file data), so
/// the check stays cheap and does not read the input bytes the FUSE path is
/// designed never to touch — the format detail stays behind the trait.
///
/// Runs on the execution (cache-miss) path before any input is materialized, so
/// a collision fails fast with both producers named rather than silently
/// last-write-wins in the sandbox (regular deps `File::create`-truncate; FUSE
/// layers shadow by registration order — neither surfaces the conflict).
///
/// Synchronous and I/O-bound: call it from
/// [`detect_output_collisions_blocking`] rather than directly from an `async fn`.
pub fn detect_output_collisions(groups: &BTreeMap<PathBuf, Vec<RunInput>>) -> anyhow::Result<()> {
    // Absolute sandbox path -> the target that claimed it.
    let mut owners: HashMap<PathBuf, Addr> = HashMap::new();
    for (unpack_root, inputs) in groups {
        for input in inputs {
            let producer = &input.source_addr;
            let paths = input.artifact.content.entry_paths().with_context(|| {
                format!(
                    "enumerate paths for output-collision check \
                     (origin_id={}, source_addr={})",
                    input.origin_id,
                    producer.format(),
                )
            })?;
            for rel in paths {
                if !input.filters.is_empty()
                    && !input.filters.iter().any(|f| Path::new(f) == rel.as_path())
                {
                    continue;
                }
                let abs = unpack_root.join(&rel);
                match owners.get(&abs) {
                    Some(existing) if existing != producer => {
                        // Two fs-provider inputs (e.g. a `glob` and a specific
                        // `file`) claiming the same path expose the *same*
                        // workspace source bytes — the fs provider only surfaces
                        // existing files, it never materializes — so this overlap
                        // is benign (last-writer-wins is safe when the bytes
                        // match). Allow it, keeping the first owner. Mirrors the
                        // `validate` codegen-overlap exemption. Any collision
                        // involving a non-fs producer is still a hard error.
                        if is_fs_source(existing) && is_fs_source(producer) {
                            continue;
                        }
                        anyhow::bail!(
                            "output collision: {} is produced by two different targets \
                             ({} and {}); a sandbox file may be provided by only one target",
                            abs.display(),
                            existing.format(),
                            producer.format(),
                        )
                    }
                    // Same producer (diamond / depended twice) — one owner, allowed.
                    Some(_) => {}
                    None => {
                        owners.insert(abs, producer.clone());
                    }
                }
            }
        }
    }
    Ok(())
}

/// [`hartifactcontent::unpack::unpack`] through `hcore::blocking::run`.
///
/// Unpacking reads every byte of the artifact out of the cache and writes the
/// whole tree to disk. Inline in an `async fn` it holds a runtime worker for the
/// entire materialization — and the cache read underneath it additionally parks
/// that thread on any queued sqlite write to the same key
/// (`LocalCacheSQLite::reader`). With `2 * ncpu` execute permits against `ncpu`
/// workers, enough targets in this window park every one of them and the
/// reactor, the timer wheel and the TUI stop with them.
///
/// The captures are an `Arc` bump, two paths and the (usually empty) filter
/// list. Shared by both sandbox runners and by [`crate::stage`] so the discipline
/// has one definition — and one test — rather than three copies, only some of
/// which run on a host with a FUSE mount.
pub async fn unpack_blocking(
    content: Arc<dyn hartifactcontent::Content>,
    dst: PathBuf,
    list_path: Option<PathBuf>,
    filters: Vec<String>,
) -> anyhow::Result<()> {
    hcore::blocking::run(move || {
        let pred = |rel: &Path| filters.iter().any(|f| Path::new(f) == rel);
        let predicate: Option<&dyn Fn(&Path) -> bool> = if filters.is_empty() {
            None
        } else {
            Some(&pred)
        };
        hartifactcontent::unpack::unpack(content.as_ref(), &dst, list_path.as_deref(), predicate)
    })
    .await
}

/// [`detect_output_collisions`] through `hcore::blocking::run`, taking and returning the
/// groups so the job can own them.
///
/// The check is not the cheap map walk it looks like: `entry_paths()` on a
/// cache-backed input is a header scan over a sqlite blob, and it goes through
/// `LocalCacheSQLite::seekable_reader`, which parks the calling thread until any
/// queued write to that key has been committed by the single writer thread.
/// Inline in an `async fn` that is exactly the runtime-worker stall
/// `hcore::blocking` exists to remove; inside a blocking job the park is the
/// backend's documented contract.
pub async fn detect_output_collisions_blocking(
    groups: BTreeMap<PathBuf, Vec<RunInput>>,
) -> anyhow::Result<BTreeMap<PathBuf, Vec<RunInput>>> {
    hcore::blocking::run(move || {
        detect_output_collisions(&groups)?;
        anyhow::Ok(groups)
    })
    .await
}

/// The fs provider's package. Its `file`/`glob` targets only expose existing
/// workspace source (never materialize), so two of them claiming the same
/// sandbox path provide identical bytes — a benign overlap. Kept as a literal to
/// avoid a `driver-support` → `builtins` dependency (would cycle); mirrors
/// `hbuiltins::pluginfs::is_fs_addr`.
const FS_PROVIDER_PKG: &str = "@heph/fs";

fn is_fs_source(addr: &Addr) -> bool {
    addr.package.as_str() == FS_PROVIDER_PKG
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
            // A single-file output must resolve to a file/symlink. Without this
            // check a path that materialized as a directory is registered blindly
            // and only fails deep in `tar.pack` with a cryptic `Is a directory`
            // (EISDIR). lstat here so the consumer gets a path-and-group message.
            let md = fs::symlink_metadata(&source).with_context(|| {
                format!("lstat declared file output {fp:?} (group={group_for_err})")
            })?;
            let ft = md.file_type();
            anyhow::ensure!(
                ft.is_file() || ft.is_symlink(),
                "declared file output {fp:?} (group={group_for_err}) is a directory, \
                 not a file; declare it as a directory or glob output instead"
            );
            tar.create_file(source.to_string_lossy().into_owned(), fp.clone());
        }
        Content::DirPath(dir) => {
            let dir_full = ws_dir.join(dir);
            // Symmetric to the FilePath guard: a directory output declared on a
            // path that materialized as a file would otherwise be single-walked
            // and packed silently. lstat so the consumer gets a clear message.
            let md = fs::symlink_metadata(&dir_full).with_context(|| {
                format!("lstat declared directory output {dir:?} (group={group_for_err})")
            })?;
            anyhow::ensure!(
                md.file_type().is_dir(),
                "declared directory output {dir:?} (group={group_for_err}) is a file, \
                 not a directory; declare it as a file or glob output instead"
            );
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

    fn out_path(content: path::Content) -> path::Path {
        path::Path {
            content,
            codegen_tree: path::CodegenMode::None,
            collect: false,
        }
    }

    // A single-file output declared on a path that materialized as a directory
    // must fail with a legible message naming the path and group — not the raw
    // `Is a directory` (EISDIR) buried in `tar.pack`.
    #[test]
    fn file_output_pointing_at_dir_errors_clearly() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("out")).expect("mkdir");
        let p = out_path(path::Content::FilePath("out".to_string()));
        let mut tar = TarPacker::new();
        let err = add_path_to_tar(&mut tar, dir.path(), &p, "bin").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("is a directory"), "got: {msg}");
        assert!(
            msg.contains("out") && msg.contains("bin"),
            "must name path+group: {msg}"
        );
    }

    // Symmetric: a directory output declared on a file must fail clearly rather
    // than silently single-walking the file.
    #[test]
    fn dir_output_pointing_at_file_errors_clearly() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("out"), b"x").expect("write");
        let p = out_path(path::Content::DirPath("out".to_string()));
        let mut tar = TarPacker::new();
        let err = add_path_to_tar(&mut tar, dir.path(), &p, "data").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("is a file"), "got: {msg}");
        assert!(
            msg.contains("out") && msg.contains("data"),
            "must name path+group: {msg}"
        );
    }

    fn group(inputs: Vec<ManagedRunInput>) -> BTreeMap<PathBuf, Vec<RunInput>> {
        let mut m: BTreeMap<PathBuf, Vec<RunInput>> = BTreeMap::new();
        for mi in inputs {
            m.entry(mi.unpack_root.clone()).or_default().push(mi.input);
        }
        m
    }

    // Two *different* targets writing the same sandbox path must fail, naming
    // both producers — not silently last-write-wins.
    #[test]
    fn collision_between_two_targets_errors() {
        let groups = group(vec![
            make_input("dep|a|0", "//pkg:_a", &[("pkg/x.txt", "1")]),
            make_input("dep|b|0", "//pkg:_b", &[("pkg/x.txt", "2")]),
        ]);
        let err = detect_output_collisions(&groups).expect_err("expected collision");
        let msg = format!("{err:#}");
        assert!(msg.contains("two different targets"), "got: {msg}");
        assert!(
            msg.contains("//pkg:_a") && msg.contains("//pkg:_b"),
            "must name both producers: {msg}"
        );
        assert!(msg.contains("pkg/x.txt"), "must name the path: {msg}");
    }

    // Two fs-provider inputs (a glob and a specific file) claiming the same path
    // expose the same workspace source, so the overlap is allowed.
    #[test]
    fn two_fs_provider_inputs_same_path_allowed() {
        let groups = group(vec![
            make_input(
                "dep|glob|0",
                "//@heph/fs:glob@p=mgmt/tsconfig*.json",
                &[("mgmt/tsconfig.lib.json", "{}")],
            ),
            make_input(
                "dep|file|0",
                "//@heph/fs:file@f=mgmt/tsconfig.lib.json",
                &[("mgmt/tsconfig.lib.json", "{}")],
            ),
        ]);
        detect_output_collisions(&groups).expect("two fs deps overlapping must be allowed");
    }

    // The exemption is fs-only: an fs input colliding with a real (non-fs)
    // producer is still a hard error.
    #[test]
    fn fs_vs_non_fs_collision_still_errors() {
        let groups = group(vec![
            make_input(
                "dep|glob|0",
                "//@heph/fs:glob@p=mgmt/*.json",
                &[("mgmt/x.json", "1")],
            ),
            make_input("dep|gen|0", "//mgmt:gen", &[("mgmt/x.json", "2")]),
        ]);
        let err = detect_output_collisions(&groups).expect_err("fs vs non-fs must collide");
        assert!(
            format!("{err:#}").contains("two different targets"),
            "{err:#}"
        );
    }

    // The same target contributing a path twice (diamond dep / depended twice)
    // is one owner — allowed.
    #[test]
    fn same_target_twice_is_allowed() {
        let groups = group(vec![
            make_input("dep|a|0", "//pkg:_a", &[("pkg/x.txt", "1")]),
            make_input("dep|a|1", "//pkg:_a", &[("pkg/x.txt", "1")]),
        ]);
        detect_output_collisions(&groups).expect("same producer must not collide");
    }

    // Different targets under a shared directory but at disjoint files: no
    // collision (only file paths are compared, not the parent dir).
    #[test]
    fn disjoint_files_under_shared_dir_ok() {
        let groups = group(vec![
            make_input("dep|a|0", "//pkg:_a", &[("pkg/a.txt", "1")]),
            make_input("dep|b|0", "//pkg:_b", &[("pkg/b.txt", "2")]),
        ]);
        detect_output_collisions(&groups).expect("disjoint files must not collide");
    }

    // The same relative path in two *different* unpack roots is two distinct
    // sandbox paths — not a collision.
    #[test]
    fn same_rel_path_different_unpack_root_ok() {
        let mut a = make_input("dep|a|0", "//pkg:_a", &[("pkg/x.txt", "1")]);
        let mut b = make_input("dep|b|0", "//pkg:_b", &[("pkg/x.txt", "2")]);
        a.unpack_root = PathBuf::from("/sandbox/exec_toolsA");
        b.unpack_root = PathBuf::from("/sandbox/exec_toolsB");
        let groups = group(vec![a, b]);
        detect_output_collisions(&groups).expect("distinct roots must not collide");
    }

    // A filtered input only claims the paths it actually exposes: a path it
    // filters out cannot collide with another target's file at that path.
    #[test]
    fn filtered_out_path_does_not_collide() {
        let mut a = make_input(
            "dep|a|0",
            "//pkg:_a",
            &[("pkg/x.txt", "1"), ("pkg/y.txt", "1")],
        );
        a.input.filters = vec!["pkg/y.txt".to_string()];
        let groups = group(vec![
            a,
            make_input("dep|b|0", "//pkg:_b", &[("pkg/x.txt", "2")]),
        ]);
        detect_output_collisions(&groups).expect("filtered-out path must not collide");
    }

    /// Records whether its bytes were read inside a `blocking::run` job, then
    /// behaves like `TarBytes`.
    ///
    /// The witness for every "runs off the runtime workers" test in this module:
    /// `entry_paths` (the default) and `unpack` both go through `walk`, so one
    /// recorder covers path enumeration and full materialization alike.
    struct ThreadRecordingTar {
        inner: TarBytes,
        in_job: Arc<std::sync::Mutex<Option<bool>>>,
    }

    impl Content for ThreadRecordingTar {
        fn reader(&self) -> anyhow::Result<Box<dyn Read>> {
            self.inner.reader()
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            record_in_blocking_job(&self.in_job);
            self.inner.walk()
        }
        fn hashout(&self) -> anyhow::Result<String> {
            self.inner.hashout()
        }
    }

    /// Record whether this runs inside a `blocking::run` job into `slot`.
    ///
    /// A free function returning `()` rather than a lock inside `walk`: `walk`
    /// returns a `Result`, and `clippy::unwrap_in_result` (denied in this crate)
    /// rejects an `expect` there. Poisoning is ignored — the `Option` is still
    /// consistent, and a poisoned slot would only hide the assertion.
    fn record_in_blocking_job(slot: &std::sync::Mutex<Option<bool>>) {
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(hcore::blocking::in_blocking_job());
    }

    /// A `ThreadRecordingTar` over `files`, plus the slot it reports into.
    fn recording_content(
        files: &[(&str, &str)],
    ) -> (Arc<dyn Content>, Arc<std::sync::Mutex<Option<bool>>>) {
        let in_job = Arc::new(std::sync::Mutex::new(None));
        let content = Arc::new(ThreadRecordingTar {
            inner: TarBytes(pack_files(files)),
            in_job: Arc::clone(&in_job),
        });
        (content, in_job)
    }

    /// The witness itself: `hcore::blocking::in_blocking_job` is true only
    /// while a `blocking::run` job runs on the calling thread — a runtime
    /// worker, the test thread, and even a bare `spawn_blocking` all read
    /// false. (The old pool's `heph-blocking-*` thread names used to be the
    /// witness; tokio's blocking threads carry no distinguishing name.)
    ///
    /// Positive rather than negative on purpose — asserting "not on a worker"
    /// passes vacuously under `#[tokio::test]`, whose current-thread flavor
    /// runs the body on the test thread.
    fn assert_ran_on_blocking_pool(in_job: &Arc<std::sync::Mutex<Option<bool>>>, what: &str) {
        let recorded = *in_job.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            recorded,
            Some(true),
            "{what} must run inside a blocking::run job (None = never walked)"
        );
    }

    /// Sandbox materialization must not run on a tokio worker.
    ///
    /// `build_source_map` calls `entry_paths()` per opted-in input, which for
    /// tar-backed cache content is a header scan over a sqlite blob. Inline in an
    /// `async fn` that holds a worker; with `2 * ncpu` execute permits against
    /// `ncpu` workers, enough targets in this window park the whole runtime.
    ///
    /// Asserted where the work is actually observable: the content itself
    /// records whether it was enumerated inside a `blocking::run` job
    /// (`hcore::blocking::in_blocking_job` is the witness).
    #[tokio::test]
    async fn source_map_generation_runs_off_the_runtime_workers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws_dir = dir.path().join("ws");
        let pkg_dir = dir.path().join("ws/pkg");
        fs::create_dir_all(&pkg_dir).expect("mkdir");

        let (content, thread) = recording_content(&[("pkg/x.txt", "1")]);
        let mut input = make_input("dep|a|0", "//pkg:_a", &[("pkg/x.txt", "1")]);
        input.input.artifact.content = content;
        input.unpack_root = ws_dir.clone();

        let inputs = write_source_map_blocking(vec![input], &ws_dir, &pkg_dir)
            .await
            .expect("write source map");

        assert_eq!(inputs.len(), 1, "the inputs must come back to the caller");
        assert!(
            pkg_dir.join("source_map.json").exists(),
            "the map itself must still be written"
        );
        assert_ran_on_blocking_pool(&thread, "source map generation");
    }

    /// The output-collision check must not run on a tokio worker either.
    ///
    /// It looks like a map walk, but `entry_paths()` per input is a header scan
    /// over the artifact's bytes — for a cache-backed input, over a sqlite blob,
    /// through `seekable_reader`, which additionally parks the calling thread
    /// until any queued write to that key has been committed. Both sandbox
    /// runners call it before materializing anything, on every cache miss.
    #[tokio::test]
    async fn output_collision_check_runs_off_the_runtime_workers() {
        let (content, thread) = recording_content(&[("pkg/x.txt", "1")]);
        let mut input = make_input("dep|a|0", "//pkg:_a", &[("pkg/x.txt", "1")]);
        input.input.artifact.content = content;

        let groups = detect_output_collisions_blocking(group(vec![input]))
            .await
            .expect("no collision");

        assert_eq!(groups.len(), 1, "the groups must come back to the caller");
        assert_ran_on_blocking_pool(&thread, "the output-collision check");
    }

    /// Unpacking an input must not run on a tokio worker.
    ///
    /// This is the heaviest of the three: every byte of the artifact is read out
    /// of the cache and written to disk. Both sandbox runners and
    /// [`crate::stage`] route through this one helper so the discipline has a
    /// single definition — and this single test.
    #[tokio::test]
    async fn unpacking_an_input_runs_off_the_runtime_workers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("ws");
        fs::create_dir_all(&dst).expect("mkdir");
        let (content, thread) = recording_content(&[("pkg/x.txt", "hello")]);

        unpack_blocking(content, dst.clone(), None, Vec::new())
            .await
            .expect("unpack");

        assert_eq!(
            fs::read_to_string(dst.join("pkg/x.txt")).expect("unpacked file"),
            "hello",
            "the tree must still be materialized"
        );
        assert_ran_on_blocking_pool(&thread, "unpacking an input");
    }
}
