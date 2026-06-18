use anyhow::Context;
use hcore::hartifactcontent;
use hcore::hartifactcontent::tar_index::TarIndex;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ShellFallback, collect_outputs, invoke_inner, list_path_for,
    resolve_unpack_root, write_source_map,
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

        let mut slot_guards: Vec<sandboxfuse::SlotGuard> = Vec::new();
        let mut inputs: Vec<ManagedRunInput> = Vec::new();

        for (unpack_root, group) in groups {
            fs::create_dir_all(&unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            match try_register_slot(&self.fs, &self.fuse_lower, &unpack_root, &list_dir, &group)? {
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
                        let filters = input.filters.clone();
                        let predicate: Option<&dyn Fn(&Path) -> bool> = if filters.is_empty() {
                            None
                        } else {
                            Some(&|rel: &Path| filters.iter().any(|f| Path::new(f) == rel))
                        };
                        hartifactcontent::unpack::unpack(
                            input.artifact.content.as_ref(),
                            unpack_root.as_path(),
                            list_path.as_deref(),
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

        write_source_map(&inputs, &ws_dir, &sandbox_pkg_dir)?;

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

        collect_outputs(&mut res, target, hashin, &ws_dir, &sandbox_dir)?;

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards: erase(slot_guards),
        })
    }
}

/// Build per-input `TarIndex`es and register a slot on the shared FUSE
/// filesystem. Returns `None` if any input lacks a seekable reader (the
/// FUSE path requires random-access bytes) — caller falls back to
/// unpack-into-upper for that group.
fn try_register_slot(
    fs: &Arc<sandboxfuse::LayeredFs>,
    fuse_lower: &Path,
    unpack_root: &Path,
    list_dir: &Path,
    group: &[RunInput],
) -> anyhow::Result<Option<sandboxfuse::SlotGuard>> {
    use rayon::prelude::*;
    type Built = (sandboxfuse::Layer, Option<(PathBuf, Vec<PathBuf>)>);
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
            Ok(Some((sandboxfuse::Layer::new(index, opener), list_write)))
        })
        .collect();

    let mut layers: Vec<sandboxfuse::Layer> = Vec::with_capacity(group.len());
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

    let prefix = unpack_root.strip_prefix(fuse_lower).map_err(|_e| {
        anyhow::anyhow!(
            "unpack_root {:?} not under FUSE mount {:?}",
            unpack_root,
            fuse_lower
        )
    })?;
    let guard = fs.register_slot(prefix.to_path_buf(), layers);

    for (list_path, abs_paths) in list_writes {
        write_list_file(&list_path, &abs_paths)
            .with_context(|| format!("write list file {:?} for fuse-slot group", list_path))?;
    }
    Ok(Some(guard))
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
