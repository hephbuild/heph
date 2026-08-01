use anyhow::Context;
use hcore::hartifactcontent::tar_index::TarIndex;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ShellFallback, collect_outputs,
    detect_output_collisions_blocking, invoke_inner, list_path_for, resolve_unpack_root,
    unpack_blocking, write_source_map_blocking,
};
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

        // Reject two distinct targets producing the same sandbox file before we
        // register any slot — overlapping layer paths would otherwise silently
        // shadow by registration order. Off the worker: the check enumerates
        // every input's entry paths, which for a cache-backed input is a header
        // scan over a sqlite blob and can park on that key's queued write.
        let groups = detect_output_collisions_blocking(groups)
            .await
            .with_context(|| format!("output-collision check for {}", req.target.addr.format()))?;

        let mut slot_guards: Vec<sandboxfuse::SlotGuard> = Vec::new();
        let mut inputs: Vec<ManagedRunInput> = Vec::new();

        for (unpack_root, group) in groups {
            fs::create_dir_all(&unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            let (slot, group) = try_register_slot_blocking(
                Arc::clone(&self.fs),
                self.fuse_lower.clone(),
                unpack_root.clone(),
                list_dir.clone(),
                group,
            )
            .await?;
            match slot {
                Some(guard) => {
                    slot_guards.push(guard);
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
                        // Off the worker, through the same shared helper the OS
                        // runner uses — see `unpack_blocking`.
                        unpack_blocking(
                            Arc::clone(&input.artifact.content),
                            unpack_root.clone(),
                            list_path.clone(),
                            input.filters.clone(),
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
        let inputs = write_source_map_blocking(inputs, &ws_dir, &sandbox_pkg_dir).await?;

        let target = req.target;
        let hashin = req.hashin;

        let mut res = invoke_inner(
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
        .await?;

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
            .extend(collect_outputs(target, hashin, &ws_dir, &sandbox_dir).await?);

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards: erase(slot_guards),
        })
    }
}

/// Layers plus the list files a group's inputs want written.
type SlotLayers = (Vec<Arc<sandboxfuse::Layer>>, Vec<(PathBuf, Vec<PathBuf>)>);

/// Build a group's FUSE layers off the runtime workers, then register the slot
/// here on the caller's frame.
///
/// [`build_slot_layers`] is the expensive half — a `par_iter` that opens a
/// seekable reader and builds a `TarIndex` per input, i.e. cache reads that park
/// on any queued sqlite write to those keys. The rayon workers parking is their
/// contract; the tokio worker pinned on the join for the whole group is the
/// runtime stall `hcore::blocking` exists to remove.
///
/// **The registration deliberately stays out of the job.** `register_slot` is a
/// map insert keyed by the sandbox prefix, and that prefix is stable per addr
/// rather than per attempt — while an `hcore::blocking` job runs to completion
/// even when its caller's future is dropped. Registering inside the job would
/// let an abandoned attempt insert its layers over a *successor's* live slot,
/// and then, when its unwanted `SlotGuard` was dropped on the pool thread,
/// remove the prefix outright — the successor's sandbox would lose its inputs
/// mid-run. Done here, a slot is only ever registered by a frame that is alive
/// to own its guard. The insert itself is one `RwLock` write; there is nothing
/// to offload.
async fn try_register_slot_blocking(
    fs: Arc<sandboxfuse::LayeredFs>,
    fuse_lower: PathBuf,
    unpack_root: PathBuf,
    list_dir: PathBuf,
    group: Vec<RunInput>,
) -> anyhow::Result<(Option<sandboxfuse::SlotGuard>, Vec<RunInput>)> {
    let prefix = unpack_root
        .strip_prefix(&fuse_lower)
        .map_err(|_e| {
            anyhow::anyhow!(
                "unpack_root {:?} not under FUSE mount {:?}",
                unpack_root,
                fuse_lower
            )
        })?
        .to_path_buf();

    let (built, group) = hcore::blocking::run(move || {
        let built = build_slot_layers(&unpack_root, &list_dir, &group)?;
        anyhow::Ok((built, group))
    })
    .await?;

    let Some((layers, list_writes)) = built else {
        return Ok((None, group));
    };
    let guard = fs.register_slot(prefix, layers);
    for (list_path, abs_paths) in list_writes {
        write_list_file(&list_path, &abs_paths)
            .with_context(|| format!("write list file {:?} for fuse-slot group", list_path))?;
    }
    Ok((Some(guard), group))
}

/// Build one `TarIndex`-backed [`sandboxfuse::Layer`] per input, plus the list
/// files the group's inputs want written. `None` if any input lacks a seekable
/// reader (the FUSE path requires random-access bytes) — the caller falls back
/// to unpack-into-upper for that group.
///
/// Synchronous and I/O-bound: reached from `run_inner` only through
/// [`try_register_slot_blocking`].
fn build_slot_layers(
    unpack_root: &Path,
    list_dir: &Path,
    group: &[RunInput],
) -> anyhow::Result<Option<SlotLayers>> {
    use rayon::prelude::*;
    type Built = (Arc<sandboxfuse::Layer>, Option<(PathBuf, Vec<PathBuf>)>);
    let results: Vec<anyhow::Result<Option<Built>>> = group
        .par_iter()
        .map(|input| -> anyhow::Result<Option<Built>> {
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
                let filters = input.filters.clone();
                index
                    .entries
                    .retain(|p, _| filters.iter().any(|f| Path::new(f) == p));
            }
            let list_write = list_path_for(input, list_dir).map(|list_path| {
                let abs_paths: Vec<PathBuf> =
                    index.entries.keys().map(|p| unpack_root.join(p)).collect();
                (list_path, abs_paths)
            });
            let content = input.artifact.content.clone();
            let origin_id = input.origin_id.clone();
            let opener: sandboxfuse::LayerOpener = Box::new(move || {
                content.seekable_reader()?.ok_or_else(|| {
                    anyhow::anyhow!("seekable_reader returned None for origin_id={origin_id}")
                })
            });
            Ok(Some((
                Arc::new(sandboxfuse::Layer::new(index, opener)),
                list_write,
            )))
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
    struct ThreadRecordingTar {
        bytes: Vec<u8>,
        in_job: Arc<std::sync::Mutex<Option<bool>>>,
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
            Ok(Some(Box::new(std::io::Cursor::new(self.bytes.clone()))))
        }
    }

    /// Registering a FUSE slot must not run on a tokio worker.
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
    async fn slot_registration_runs_off_the_runtime_workers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fuse_lower = dir.path().join("lower");
        let unpack_root = fuse_lower.join("sandbox/ws");
        let list_dir = dir.path().join("list");
        fs::create_dir_all(&unpack_root).expect("mkdir unpack root");
        fs::create_dir_all(&list_dir).expect("mkdir list dir");

        let mut packer = TarPacker::new();
        packer.create_raw(b"payload".to_vec(), "pkg/x.txt", false);
        let mut bytes = Vec::new();
        packer.pack(&mut bytes).expect("pack");
        let in_job = Arc::new(std::sync::Mutex::new(None));

        let input = RunInput {
            artifact: InputArtifact {
                r#type: Type::Dep,
                origin_id: "dep|a|0".to_string(),
                content: Arc::new(ThreadRecordingTar {
                    bytes,
                    in_job: Arc::clone(&in_job),
                }),
            },
            origin_id: "dep|a|0".to_string(),
            source_addr: hmodel::htaddr::parse_addr("//pkg:_a").expect("addr"),
            filters: vec![],
            annotations: BTreeMap::new(),
        };

        let fs_handle = Arc::new(sandboxfuse::LayeredFs::new_empty(dir.path().join("upper")));
        let (slot, group) =
            try_register_slot_blocking(fs_handle, fuse_lower, unpack_root, list_dir, vec![input])
                .await
                .expect("register slot");

        assert!(slot.is_some(), "a seekable input must register a slot");
        assert_eq!(group.len(), 1, "the group must come back to the caller");
        let recorded = *in_job.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            recorded,
            Some(true),
            "slot registration must run inside a blocking::run job (None = never opened)"
        );
    }
}
