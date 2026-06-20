use crate::driver_managed::{
    ManagedDriver, ManagedRunInput, ShellFallback, collect_outputs, invoke_inner, list_path_for,
    resolve_unpack_root, write_source_map,
};
use anyhow::Context;
use hcore::hartifactcontent;
use hcore::hasync::Cancellable;
use hplugin::driver::{RunInput, RunRequest, RunResponse};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// OS-backed managed driver: materializes every input artifact to disk via
/// `hartifactcontent::unpack`. No FUSE involvement.
///
/// Inputs flagged read-only (see [`crate::stage::READ_ONLY_ANNOTATION`]) are
/// staged once into the shared `stage_dir` and symlinked in, rather than copied
/// fresh into every consuming sandbox.
pub struct ManagedDriverOs {
    pub driver: Arc<Box<dyn ManagedDriver>>,
    pub shell_fallback: Arc<ShellFallback>,
    /// Shared `<home>/stage` root for read-only input staging. `None` disables
    /// staging (read-only inputs fall back to the plain copy path) — used by
    /// the standalone [`ManagedDriverOs::new`] constructor.
    pub stage_dir: Option<PathBuf>,
}

impl ManagedDriverOs {
    /// Construct a standalone OS-copy runner. The `ManagedDriverBridge` (in
    /// `heph-driver-bridge`) builds its `os` field directly to share one
    /// `Arc<driver>` with the FUSE runner; this is for callers that only want
    /// the OS path (e.g. tests).
    pub fn new(driver: Box<dyn ManagedDriver>, shell_fallback: Arc<ShellFallback>) -> Self {
        Self {
            driver: Arc::new(driver),
            shell_fallback,
            stage_dir: None,
        }
    }

    pub async fn run_inner<'a, 'io>(
        &self,
        mut req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<RunResponse> {
        let sandbox_dir = req.sandbox_dir.clone();
        let cleanup_dir = sandbox_dir.clone();
        let mut sandbox_cleanup: Option<hplugin::driver::SandboxCleanupJob> =
            Some(Box::new(move || {
                hcore::fsutil::remove_dir_all(&cleanup_dir)
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

        let mut inputs: Vec<ManagedRunInput> = Vec::new();
        for (unpack_root, group) in groups {
            fs::create_dir_all(&unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            for input in group {
                let list_path = list_path_for(&input, &list_dir);
                match self.stage_dir.as_deref() {
                    Some(stage_dir) if crate::stage::is_read_only(&input.annotations) => {
                        crate::stage::stage_and_link(
                            input.artifact.content.as_ref(),
                            stage_dir,
                            &input.source_addr.format(),
                            unpack_root.as_path(),
                            list_path.as_deref(),
                            &input.filters,
                            crate::stage::is_per_file(&input.annotations),
                            ctoken,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "stage read-only input origin_id={} source_addr={} into {:?}",
                                input.origin_id,
                                input.source_addr.format(),
                                unpack_root,
                            )
                        })?;
                    }
                    _ => {
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
                    }
                }
                inputs.push(ManagedRunInput {
                    input,
                    list_path,
                    unpack_root: unpack_root.clone(),
                });
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

        if shell {
            return Ok(RunResponse {
                artifacts: vec![],
                sandbox_cleanup: sandbox_cleanup.take(),
                sandbox_guards: Vec::new(),
            });
        }

        collect_outputs(&mut res, target, hashin, &ws_dir, &sandbox_dir)?;

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards: Vec::new(),
        })
    }
}
