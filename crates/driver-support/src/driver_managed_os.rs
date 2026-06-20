use crate::driver_managed::{
    ManagedDriver, ManagedRunInput, ShellFallback, collect_outputs,
    detect_output_collisions_blocking, invoke_inner, list_path_for, resolve_unpack_root,
    unpack_blocking, write_source_map_blocking,
};
use anyhow::Context;
use hcore::hasync::Cancellable;
use hplugin::driver::{RunInput, RunRequest, RunResponse};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
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

        // Build the sandbox (create dirs + materialize every input) under a
        // SandboxCreate scope so it renders as a per-target op in the TUI and is
        // flagged slow when it runs long. The scope covers only the build — not
        // the subprocess run that follows (that is the Execute op).
        let events = req.events.clone();
        let addr = req.target.addr.format();
        let addr_for_ctx = addr.clone();
        let pkg = req.target.addr.package.as_str().to_owned();
        let inputs_taken = std::mem::take(&mut req.inputs);
        let stage_dir = self.stage_dir.clone();
        let sandbox_dir_scope = sandbox_dir.clone();
        let (inputs, ws_dir, sandbox_pkg_dir) = hcore::events::emit_scope_tx(
            events,
            hcore::events::BuildEventKind::SandboxCreateStart { addr: addr.clone() },
            move |error| hcore::events::BuildEventKind::SandboxCreateEnd { addr, error },
            async move {
                let sandbox_dir = sandbox_dir_scope;
                let ws_dir = sandbox_dir.join("ws");
                fs::create_dir_all(&ws_dir)
                    .with_context(|| format!("create ws dir {:?}", ws_dir))?;

                let list_dir = sandbox_dir.join("list");
                fs::create_dir_all(&list_dir)
                    .with_context(|| format!("create list dir {:?}", list_dir))?;

                let mut groups: BTreeMap<PathBuf, Vec<RunInput>> = BTreeMap::new();
                for input in inputs_taken {
                    let unpack_root = resolve_unpack_root(&input, &sandbox_dir, &ws_dir);
                    groups.entry(unpack_root).or_default().push(input);
                }

                // Reject two distinct targets producing the same sandbox file before we
                // materialize anything — the copy path would otherwise silently
                // last-write-wins. Off the worker: the check enumerates every input's
                // entry paths, which for a cache-backed input is a header scan over a
                // sqlite blob and can park on that key's queued write.
                let groups = detect_output_collisions_blocking(groups)
                    .await
                    .with_context(|| format!("output-collision check for {addr_for_ctx}"))?;

                let mut inputs: Vec<ManagedRunInput> = Vec::new();
                for (unpack_root, group) in groups {
                    fs::create_dir_all(&unpack_root)
                        .with_context(|| format!("create unpack root {:?}", unpack_root))?;
                    for input in group {
                        let list_path = list_path_for(&input, &list_dir);
                        match stage_dir.as_deref() {
                            Some(stage_dir)
                                if crate::stage::is_read_only(&input.annotations) =>
                            {
                                crate::stage::stage_and_link(
                                    &input.artifact.content,
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
                                // Writes the input's entire tree to disk — off the
                                // worker, see `unpack_blocking`.
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
                            }
                        }
                        inputs.push(ManagedRunInput {
                            input,
                            list_path,
                            unpack_root: unpack_root.clone(),
                        });
                    }
                }

                let sandbox_pkg_dir = ws_dir.join(&pkg);
                fs::create_dir_all(&sandbox_pkg_dir)
                    .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

                let inputs = write_source_map_blocking(inputs, &ws_dir, &sandbox_pkg_dir).await?;

                Ok((inputs, ws_dir, sandbox_pkg_dir))
            },
        )
        .await?;

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

        res.artifacts
            .extend(collect_outputs(target, hashin, &ws_dir, &sandbox_dir).await?);

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards: Vec::new(),
        })
    }
}
