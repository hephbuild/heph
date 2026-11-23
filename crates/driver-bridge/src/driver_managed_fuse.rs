use anyhow::Context;
use hcore::hartifactcontent::tar_index::{IndexEntryKind, TarIndex};
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, OutputClaims, ShellFallback, collect_outputs, invoke_inner,
    list_path_for, resolve_unpack_root, unpack_blocking, write_source_map_blocking,
};
use hdriver_support::stop;
use hplugin::driver::{RunInput, RunRequest, RunResponse, SandboxGuard};
use hsandboxfuse as sandboxfuse;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// FUSE-backed managed driver. Holds a shared `LayeredFs` (one process-wide
/// mount) injected at construction; per-target inputs register/unregister
/// slots in that filesystem instead of being copied to disk.
pub struct ManagedDriverFuse {
    pub(crate) driver: Arc<Box<dyn ManagedDriver>>,
    pub(crate) shell_fallback: Arc<ShellFallback>,
    pub(crate) home: PathBuf,
    pub(crate) fs: Arc<sandboxfuse::LayeredFs>,
    pub(crate) fuse_lower: PathBuf,
    pub(crate) fuse_upper: PathBuf,
}

impl ManagedDriverFuse {
    pub(crate) async fn run_inner<'a, 'io>(
        &self,
        mut req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<RunResponse> {
        // Redirect sandbox_dir from `<home>/sandbox/...` to
        // `<fuse_lower>/...` so all writes go through the FUSE mount.
        let plain_sandbox_dir = req.sandbox_dir.clone();
        let plain_root = self.home.join("sandbox");
        let rel = plain_sandbox_dir
            .strip_prefix(&plain_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| plain_sandbox_dir.clone());
        let sandbox_dir = self.fuse_lower.join(&rel);

        // Cleanup targets the upper-side path directly, bypassing the
        // live FUSE mount. The FUSE handlers' `unlink` swallows
        // `IsADirectory` errors and marks the entry deleted without
        // removing the upper-side dir, so a recursive walk through the
        // mount leaves orphans that fail the outer `rmdir` with
        // ENOTEMPTY. Direct rm on upper avoids that path entirely.
        let upper_cleanup_dir = self.fuse_upper.join(&rel);
        let mut sandbox_cleanup: Option<hplugin::driver::SandboxCleanupJob> =
            Some(Box::new(move || {
                hcore::fsutil::remove_dir_all(&upper_cleanup_dir)
            }));

        let ws_dir = sandbox_dir.join("ws");
        fs::create_dir_all(&ws_dir).with_context(|| format!("create ws dir {:?}", ws_dir))?;

        let list_dir = sandbox_dir.join("list");
        fs::create_dir_all(&list_dir).with_context(|| format!("create list dir {:?}", list_dir))?;

        let mut groups: BTreeMap<PathBuf, Vec<RunInput>> = BTreeMap::new();
        for input in std::mem::take(&mut req.inputs) {
            let unpack_root = resolve_unpack_root(&input, &sandbox_dir, &ws_dir);
            groups.entry(unpack_root).or_default().push(input);
        }

        // Build every group's layers, then reject two distinct targets
        // producing the same sandbox file, before we register any slot —
        // overlapping layer paths would otherwise silently shadow by
        // registration order. See `prepare_groups_blocking`.
        let groups =
            prepare_groups_blocking(groups, list_dir.clone(), &req.target.addr, ctoken).await?;

        let mut slot_guards: Vec<sandboxfuse::SlotGuard> = Vec::new();
        let mut inputs: Vec<ManagedRunInput> = Vec::new();

        for PreparedGroup {
            unpack_root,
            inputs: group,
            layers,
        } in groups
        {
            fs::create_dir_all(&unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            match layers {
                Some(layers) => {
                    slot_guards.push(register_slot(
                        &self.fs,
                        &self.fuse_lower,
                        &unpack_root,
                        layers,
                    )?);
                    // Slot registered; inputs served via layers — no per-input unpack.
                    for input in group {
                        let list_path = list_path_for(&input, &list_dir);
                        inputs.push(ManagedRunInput {
                            input,
                            list_path,
                            unpack_root: unpack_root.clone(),
                        });
                    }
                }
                None => {
                    // Fallback (e.g. input lacks seekable reader): unpack
                    // into the FUSE upper layer just like OS mode.
                    for input in group {
                        let list_path = list_path_for(&input, &list_dir);
                        // Off the worker and stopped by the run's
                        // cancellation, through the same shared helper the OS
                        // runner uses — see `unpack_blocking`.
                        unpack_blocking(
                            Arc::clone(&input.artifact.content),
                            unpack_root.clone(),
                            list_path.clone(),
                            input.filters.clone(),
                            ctoken,
                        )
                        .await
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
                            list_path,
                            unpack_root: unpack_root.clone(),
                        });
                    }
                }
            }
        }

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)
            .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

        // Off the worker, as the OS runner already does it: an input that opts in
        // costs an `entry_paths()` per artifact, a header scan over a sqlite blob
        // that can park on that key's queued write.
        let inputs = write_source_map_blocking(inputs, &ws_dir, &sandbox_pkg_dir, ctoken).await?;

        let target = req.target;
        let hashin = req.hashin;

        let invoked = invoke_inner(
            &**self.driver,
            req,
            ctoken,
            shell,
            sandbox_dir.clone(),
            ws_dir.clone(),
            sandbox_pkg_dir.clone(),
            inputs,
            &self.shell_fallback,
        )
        .await;
        let mut res = match invoked {
            Ok(res) => res,
            // Read through the mount while the slot guards still hold the
            // inputs; packed under the *plain* sandbox dir, a real directory
            // that outlives the slot, for the engine to unpack into the
            // failure slot.
            Err(err) => {
                // A FUSE run writes nothing under the plain dir, so it may not
                // exist yet; the packs create only their own directory under
                // it (never a removed sandbox), so it is made here, by the run
                // itself, never by a job that could outlive it. A failure here
                // surfaces as the pack's own error, with its path.
                drop(fs::create_dir_all(&plain_sandbox_dir));
                let err = hdriver_support::failure_out::keep_failure_write_back(
                    err,
                    target,
                    hashin,
                    shell,
                    ctoken,
                    &ws_dir,
                    &plain_sandbox_dir,
                    // FUSE shows inputs as real directories: no stage links.
                    None,
                )
                .await;
                return Err(hdriver_support::failure_out::keep_failure_outputs(
                    err,
                    &target.failure_outputs,
                    hashin,
                    shell,
                    ctoken,
                    &ws_dir,
                    &plain_sandbox_dir,
                    &[],
                )
                .await);
            }
        };

        // Type-erase the slot guards into the contract's opaque `SandboxGuard`
        // so `hplugin` need not depend on `sandboxfuse`; the engine only holds
        // and drops them (drop deregisters the slot).
        let erase = |guards: Vec<sandboxfuse::SlotGuard>| -> Vec<SandboxGuard> {
            guards
                .into_iter()
                .map(|g| Box::new(g) as SandboxGuard)
                .collect()
        };

        if shell {
            return Ok(RunResponse {
                artifacts: vec![],
                sandbox_cleanup: sandbox_cleanup.take(),
                sandbox_guards: erase(std::mem::take(&mut slot_guards)),
            });
        }

        res.artifacts
            // FUSE shows inputs as real directories: no stage links to follow.
            .extend(collect_outputs(target, hashin, &ws_dir, &sandbox_dir, None, ctoken).await?);

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards: erase(slot_guards),
        })
    }
}

/// Layers plus the list files a group's inputs want written.
type SlotLayers = (Vec<Arc<sandboxfuse::Layer>>, Vec<(PathBuf, Vec<PathBuf>)>);

/// One unpack root's inputs, with the FUSE layers that serve them: `None` when
/// an input has no seekable reader, and the group is unpacked instead.
struct PreparedGroup {
    unpack_root: PathBuf,
    inputs: Vec<RunInput>,
    layers: Option<SlotLayers>,
}

/// Build every group's FUSE layers, then run the output-collision check, off
/// the runtime workers. Registers nothing: see [`register_slot`].
///
/// [`build_slot_layers`] is the expensive half — a `par_iter` that opens a
/// seekable reader and builds a `TarIndex` per input, i.e. cache reads that park
/// on any queued sqlite write to those keys. The rayon workers parking is their
/// contract; the tokio worker pinned on the join for the whole group is the
/// runtime stall `hcore::blocking` exists to remove.
///
/// The collision check reads a layered group's paths from its layers'
/// indexes, so each input's index is built once; enumerating them through
/// `entry_paths` would build it again. A group that falls back to unpacking
/// enumerates its inputs as the OS runner does. Groups are claimed in order,
/// and inputs in group order, as [`detect_output_collisions`] claims them.
///
/// [`detect_output_collisions`]: hdriver_support::driver_managed::detect_output_collisions
async fn prepare_groups_blocking(
    groups: BTreeMap<PathBuf, Vec<RunInput>>,
    list_dir: PathBuf,
    addr: &hmodel::htaddr::Addr,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<Vec<PreparedGroup>> {
    let addr = addr.clone();
    stop::run_blocking(ctoken, move |stop| {
        let mut prepared = Vec::with_capacity(groups.len());
        for (unpack_root, inputs) in groups {
            let layers = build_slot_layers(&unpack_root, &list_dir, &inputs, stop)?;
            prepared.push(PreparedGroup {
                unpack_root,
                inputs,
                layers,
            });
        }
        claim_outputs(&prepared, stop)
            .with_context(|| format!("output-collision check for {}", addr.format()))?;
        Ok(prepared)
    })
    .await
}

/// The output-collision check over prepared groups: a layered input's paths
/// come from its layer's index (filters already applied, directories left
/// out, as `entry_paths` leaves them out).
fn claim_outputs(prepared: &[PreparedGroup], stop: &dyn Fn() -> bool) -> anyhow::Result<()> {
    let mut claims = OutputClaims::default();
    for group in prepared {
        match &group.layers {
            Some((layers, _)) => {
                for (input, layer) in group.inputs.iter().zip(layers) {
                    let paths = layer
                        .index
                        .entries
                        .iter()
                        .filter(|(_, e)| !matches!(e.kind, IndexEntryKind::Dir))
                        .map(|(p, _)| p.as_path());
                    claims.claim(&group.unpack_root, &input.source_addr, paths)?;
                }
            }
            None => {
                for input in &group.inputs {
                    if stop() {
                        return Err(stop::cancelled());
                    }
                    claims.claim_input(&group.unpack_root, input)?;
                }
            }
        }
    }
    Ok(())
}

/// Register a group's slot and write its list files, here on the caller's
/// frame.
///
/// **The registration deliberately stays out of the blocking job.**
/// `register_slot` is a map insert keyed by the sandbox prefix, and that prefix
/// is stable per addr rather than per attempt — while an `hcore::blocking` job
/// runs to completion even when its caller's future is dropped. Registering
/// inside the job would let an abandoned attempt insert its layers over a
/// *successor's* live slot, and then, when its unwanted `SlotGuard` was dropped
/// on the pool thread, remove the prefix outright — the successor's sandbox
/// would lose its inputs mid-run. Done here, a slot is only ever registered by
/// a frame that is alive to own its guard. The insert itself is one `RwLock`
/// write; there is nothing to offload.
fn register_slot(
    fs: &Arc<sandboxfuse::LayeredFs>,
    fuse_lower: &Path,
    unpack_root: &Path,
    (layers, list_writes): SlotLayers,
) -> anyhow::Result<sandboxfuse::SlotGuard> {
    let prefix = unpack_root
        .strip_prefix(fuse_lower)
        .map_err(|_e| {
            anyhow::anyhow!(
                "unpack_root {:?} not under FUSE mount {:?}",
                unpack_root,
                fuse_lower
            )
        })?
        .to_path_buf();
    let guard = fs.register_slot(prefix, layers);
    for (list_path, abs_paths) in list_writes {
        write_list_file(&list_path, &abs_paths)
            .with_context(|| format!("write list file {:?} for fuse-slot group", list_path))?;
    }
    Ok(guard)
}

/// Build one `TarIndex`-backed [`sandboxfuse::Layer`] per input, plus the list
/// files the group's inputs want written. `None` if any input lacks a seekable
/// reader (the FUSE path requires random-access bytes) — the caller falls back
/// to unpack-into-upper for that group.
///
/// Synchronous and I/O-bound: reached from `run_inner` only through
/// [`prepare_groups_blocking`]. Writes nothing; `stop` is polled before
/// each input's index build so a cancelled run is not held up by the rest.
fn build_slot_layers(
    unpack_root: &Path,
    list_dir: &Path,
    group: &[RunInput],
    stop: &(dyn Fn() -> bool + Sync),
) -> anyhow::Result<Option<SlotLayers>> {
    use rayon::prelude::*;
    type Built = (Arc<sandboxfuse::Layer>, Option<(PathBuf, Vec<PathBuf>)>);
    let results: Vec<anyhow::Result<Option<Built>>> = group
        .par_iter()
        .map(|input| -> anyhow::Result<Option<Built>> {
            if stop() {
                return Err(stop::cancelled());
            }
            let Some(seekable) = input.artifact.content.seekable_reader().with_context(|| {
                format!(
                    "open seekable reader for input origin_id={} source_addr={}",
                    input.origin_id,
                    input.source_addr.format()
                )
            })?
            else {
                return Ok(None);
            };
            let mut index = TarIndex::build(seekable).with_context(|| {
                format!("build tar index for input origin_id={}", input.origin_id)
            })?;
            if !input.filters.is_empty() {
                let filters = hcore::hartifactcontent::PathFilter::new(&input.filters);
                index.entries.retain(|p, _| filters.matches(p));
            }
            let list_write = list_path_for(input, list_dir).map(|list_path| {
                let abs_paths: Vec<PathBuf> =
                    index.entries.keys().map(|p| unpack_root.join(p)).collect();
                (list_path, abs_paths)
            });
            let content = input.artifact.content.clone();
            // No plain file behind the blob (compressed, or inline in
            // sqlite): every open re-reads the seek table and a read decodes
            // a whole frame, so read whole frames and keep them.
            let chunked = content.file_path().is_none();
            let origin_id = input.origin_id.clone();
            let opener: sandboxfuse::LayerOpener = Box::new(move || {
                content.seekable_reader()?.ok_or_else(|| {
                    anyhow::anyhow!("seekable_reader returned None for origin_id={origin_id}")
                })
            });
            let mut layer = sandboxfuse::Layer::new(index, opener);
            if chunked {
                layer = layer.with_read_chunk(sandboxfuse::COMPRESSED_READ_CHUNK);
            }
            Ok(Some((Arc::new(layer), list_write)))
        })
        .collect();

    let mut layers: Vec<Arc<sandboxfuse::Layer>> = Vec::with_capacity(group.len());
    let mut list_writes: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for result in results {
        match result? {
            None => return Ok(None),
            Some((layer, list_write)) => {
                if let Some(lw) = list_write {
                    list_writes.push(lw);
                }
                layers.push(layer);
            }
        }
    }

    Ok(Some((layers, list_writes)))
}

fn write_list_file(list_path: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::io::BufWriter::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(list_path)?,
    );
    for p in paths {
        writeln!(f, "{}", p.display())?;
    }
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hartifactcontent::tar::TarPacker;
    use hcore::hartifactcontent::{Content, ReadSeek, WalkEntry};
    use hplugin::driver::inputartifact::{InputArtifact, Type};
    use std::collections::BTreeMap;

    /// A tar-backed input whose `seekable_reader` records whether it was
    /// opened inside a `blocking::run` job. Both the reader open and the
    /// `TarIndex` build happen inside `try_register_slot`'s `par_iter`, so
    /// this witnesses the whole fan-out. (A single-input `par_iter` runs on
    /// the calling thread — the job's — which is also what kept the old
    /// thread-name witness deterministic here.)
    ///
    /// It also counts its `seekable_reader` opens, and enumerates its paths as
    /// the cache artifact does — an index built over a seekable reader — so a
    /// second index build shows up as a second open.
    struct ThreadRecordingTar {
        bytes: Vec<u8>,
        in_job: Arc<std::sync::Mutex<Option<bool>>>,
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ThreadRecordingTar {
        fn new(files: &[&str]) -> Self {
            let mut packer = TarPacker::new();
            for path in files {
                packer.create_raw(b"payload".to_vec(), (*path).to_string(), false);
            }
            let mut bytes = Vec::new();
            packer.pack(&mut bytes).expect("pack");
            Self {
                bytes,
                in_job: Arc::default(),
                opens: Arc::default(),
            }
        }
    }

    impl Content for ThreadRecordingTar {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
                std::io::Cursor::new(self.bytes.clone()),
            )?))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("HO".to_string())
        }
        fn seekable_reader(&self) -> anyhow::Result<Option<Box<dyn ReadSeek + Send>>> {
            // Not an `expect`: this returns a `Result`, and a poisoned slot would
            // only hide the assertion rather than signal anything.
            let mut slot = self.in_job.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(hcore::blocking::in_blocking_job());
            drop(slot);
            self.opens
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Some(Box::new(std::io::Cursor::new(self.bytes.clone()))))
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
            let reader = self.seekable_reader()?.expect("seekable");
            Ok(TarIndex::build(reader)?.entry_paths())
        }
    }

    /// An input produced by `//pkg:_<name>`.
    fn tar_input(name: &str, content: ThreadRecordingTar) -> RunInput {
        RunInput {
            artifact: InputArtifact {
                r#type: Type::Dep,
                origin_id: format!("dep|{name}|0"),
                content: Arc::new(content),
            },
            origin_id: format!("dep|{name}|0"),
            source_addr: hmodel::htaddr::parse_addr(&format!("//pkg:_{name}")).expect("addr"),
            filters: vec![],
            annotations: BTreeMap::new(),
        }
    }

    /// Lower and list dirs for a test that prepares groups without a mount.
    fn fuse_dirs(dir: &Path) -> (PathBuf, PathBuf) {
        let unpack_root = dir.join("lower/sandbox/ws");
        let list_dir = dir.join("list");
        fs::create_dir_all(&unpack_root).expect("mkdir unpack root");
        fs::create_dir_all(&list_dir).expect("mkdir list dir");
        (unpack_root, list_dir)
    }

    /// Preparing a layered group builds each input's index once: the
    /// collision check reads the layers' indexes rather than enumerating the
    /// content again.
    #[tokio::test]
    async fn prepare_opens_each_layered_input_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (unpack_root, list_dir) = fuse_dirs(dir.path());
        let a = ThreadRecordingTar::new(&["pkg/a.txt"]);
        let b = ThreadRecordingTar::new(&["pkg/b.txt", "pkg/sub/c.txt"]);
        let opens = [Arc::clone(&a.opens), Arc::clone(&b.opens)];
        let groups = BTreeMap::from([(unpack_root, vec![tar_input("a", a), tar_input("b", b)])]);
        let ct = hcore::hasync::StdCancellationToken::new();
        let addr = hmodel::htaddr::parse_addr("//app:t").expect("addr");
        let prepared = prepare_groups_blocking(groups, list_dir, &addr, &ct)
            .await
            .expect("prepare");

        assert!(prepared[0].layers.is_some(), "every input is seekable");
        for opened in opens {
            assert_eq!(opened.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
    }

    /// Two targets producing the same file in a layered group fail the
    /// collision check with both named, and nothing comes back to register.
    #[tokio::test]
    async fn prepare_rejects_a_collision_between_layered_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (unpack_root, list_dir) = fuse_dirs(dir.path());
        let groups = BTreeMap::from([(
            unpack_root.clone(),
            vec![
                tar_input("a", ThreadRecordingTar::new(&["pkg/x.txt"])),
                tar_input("b", ThreadRecordingTar::new(&["pkg/x.txt"])),
            ],
        )]);
        let ct = hcore::hasync::StdCancellationToken::new();
        let addr = hmodel::htaddr::parse_addr("//app:t").expect("addr");
        let Err(err) = prepare_groups_blocking(groups, list_dir, &addr, &ct).await else {
            panic!("a collision must fail the preparation");
        };
        assert_eq!(
            format!("{err:#}"),
            format!(
                "output-collision check for //app:t: output collision: {} is produced by two \
                 different targets (//pkg:_a and //pkg:_b); a sandbox file may be provided by \
                 only one target",
                unpack_root.join("pkg/x.txt").display()
            )
        );
    }

    /// Writes `report.txt` into its package dir and exits 1.
    struct FailsWithReport;

    #[async_trait::async_trait]
    impl hdriver_support::driver_managed::ManagedDriver for FailsWithReport {
        fn config(
            &self,
            _: hplugin::driver::ConfigRequest,
        ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
            Ok(hplugin::driver::ConfigResponse {
                name: "fails".to_string(),
            })
        }
        fn schema(&self) -> hplugin::driver::DriverSchema {
            hplugin::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            _: hplugin::driver::ParseRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ParseResponse> {
            anyhow::bail!("not parsed in this test")
        }
        async fn apply_transitive(
            &self,
            req: hplugin::driver::ApplyTransitiveRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
            Ok(hplugin::driver::ApplyTransitiveResponse {
                target_def: req.target_def,
            })
        }
        async fn run<'a, 'io>(
            &self,
            req: hdriver_support::driver_managed::ManagedRunRequest<'a, 'io>,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<hdriver_support::driver_managed::ManagedRunResponse> {
            fs::write(req.sandbox_pkg_dir.join("report.txt"), "report")?;
            fs::write(req.sandbox_pkg_dir.join("fix.txt"), "fix")?;
            Err(anyhow::Error::new(hplugin::error::ProcessFailed::new(
                "exit status: 1",
                Some(hplugin::error::Exit::Code(1)),
            )))
        }
    }

    /// The FUSE runner keeps `failure_out` on a process exit exactly as the OS
    /// one does, read from the redirected workspace while the run's slot is
    /// live, and staged under the **plain** sandbox dir — a real directory the
    /// engine's sandbox roots name, which outlives the slot.
    ///
    /// Needs no FUSE mount: without one the redirected `lower` is a plain
    /// directory, and this is the runner's own code path either way. No
    /// `crates/e2e` test mounts FUSE, so this is that runner's coverage.
    #[tokio::test]
    async fn failure_out_is_staged_under_the_plain_sandbox() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        let plain = home.join("sandbox/app/__target_t");
        let driver: Box<dyn hdriver_support::driver_managed::ManagedDriver> =
            Box::new(FailsWithReport);
        let fuse = ManagedDriverFuse {
            driver: Arc::new(driver),
            shell_fallback: Arc::new(ShellFallback {
                driver: Arc::new(FailsWithReport),
                spec_template: Arc::new(hplugin::provider::TargetSpec::default()),
            }),
            home: home.clone(),
            fs: Arc::new(sandboxfuse::LayeredFs::new_empty(dir.path().join("upper"))),
            fuse_lower: dir.path().join("lower"),
            fuse_upper: dir.path().join("upper"),
        };
        let target = hplugin::driver::targetdef::TargetDef {
            addr: hmodel::htaddr::parse_addr("//app:t")?,
            failure_outputs: vec![hplugin::driver::targetdef::FailureOutput {
                group: String::new(),
                paths: vec![hplugin::driver::targetdef::path::Content::FilePath(
                    "app/report.txt".to_string(),
                )],
            }],
            outputs: vec![hplugin::driver::targetdef::Output {
                group: String::new(),
                paths: vec![hplugin::driver::targetdef::path::Path {
                    content: hplugin::driver::targetdef::path::Content::FilePath(
                        "app/fix.txt".to_string(),
                    ),
                    codegen_tree: hplugin::driver::targetdef::path::CodegenMode::InPlace {
                        write_on_failure: true,
                    },
                    collect: true,
                }],
            }],
            ..Default::default()
        };
        let id = "t".to_string();
        let ct = hcore::hasync::StdCancellationToken::new();
        let req = RunRequest::new(&id, &target, plain.clone());
        let Err(err) = fuse.run_inner(req, &ct, false).await else {
            anyhow::bail!("the run must fail");
        };
        let kept = hplugin::error::failure_out_tar(&err)
            .ok_or_else(|| anyhow::anyhow!("failure_out not kept: {err:#}"))?;
        let hplugin::driver::outputartifact::Content::TarPath(kept_tar) = &kept.content else {
            anyhow::bail!("failure_out is a tar");
        };
        assert!(
            PathBuf::from(&kept_tar.path).starts_with(&plain),
            "packed under the plain sandbox dir: {}",
            kept_tar.path
        );
        let mut report = String::new();
        for entry in hcore::hartifactcontent::Content::walk(kept)? {
            let entry = entry?;
            if entry.path == std::path::Path::new("app/report.txt")
                && let hcore::hartifactcontent::WalkEntryKind::File { mut data, .. } = entry.kind
            {
                std::io::Read::read_to_string(&mut data, &mut report)?;
            }
        }
        assert_eq!(report, "report");
        // The `in_place` fixes are packed under the plain sandbox dir too, as
        // a `writeback-` tar that outlives the slot.
        let write_back = hplugin::error::failure_write_back(&err).expect("fixes collected");
        let [art] = write_back.0.as_slice() else {
            anyhow::bail!("one write-back tar: {:?}", write_back);
        };
        let hplugin::driver::outputartifact::Content::TarPath(tar) = &art.content else {
            anyhow::bail!("a tar: {:?}", write_back);
        };
        let tar = PathBuf::from(&tar.path);
        assert!(tar.starts_with(&plain), "{}", tar.display());
        assert!(
            tar.file_name().and_then(|n| n.to_str()).is_some_and(
                |n| n.starts_with(hdriver_support::driver_managed::WRITE_BACK_TAR_PREFIX)
            ),
            "{}",
            tar.display()
        );
        assert!(tar.is_file(), "{}", tar.display());
        assert!(
            dir.path()
                .join("lower/app/__target_t/ws/app/report.txt")
                .exists(),
            "the run wrote through the redirected sandbox"
        );
        Ok(())
    }

    /// Building a FUSE slot's layers must not run on a tokio worker.
    ///
    /// It is the heaviest of the moved sites: a `par_iter` that opens a seekable
    /// reader and builds a `TarIndex` per input. The rayon workers parking on
    /// those cache reads is their contract — the tokio worker pinned on the join
    /// for the whole group is not.
    ///
    /// Needs no FUSE mount: `LayeredFs::new_empty` + `register_slot` are a map
    /// insert in both the real backend and the stub, so this runs on every
    /// supported target and under either `fuse-sandbox` setting.
    #[tokio::test]
    async fn slot_layers_build_off_the_runtime_workers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fuse_lower = dir.path().join("lower");
        let (unpack_root, list_dir) = fuse_dirs(dir.path());
        let content = ThreadRecordingTar::new(&["pkg/x.txt"]);
        let in_job = Arc::clone(&content.in_job);
        let groups = BTreeMap::from([(unpack_root, vec![tar_input("a", content)])]);

        let fs_handle = Arc::new(sandboxfuse::LayeredFs::new_empty(dir.path().join("upper")));
        let ct = hcore::hasync::StdCancellationToken::new();
        let addr = hmodel::htaddr::parse_addr("//app:t").expect("addr");
        let mut prepared = prepare_groups_blocking(groups, list_dir, &addr, &ct)
            .await
            .expect("prepare groups");
        let PreparedGroup {
            unpack_root,
            inputs,
            layers,
        } = prepared.remove(0);
        let layers = layers.expect("a seekable input must get layers");
        assert_eq!(inputs.len(), 1, "the group must come back to the caller");
        register_slot(&fs_handle, &fuse_lower, &unpack_root, layers).expect("register slot");
        let recorded = *in_job.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            recorded,
            Some(true),
            "slot registration must run inside a blocking::run job (None = never opened)"
        );
    }
}
