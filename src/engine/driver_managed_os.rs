use crate::engine::driver::{RunInput, RunRequest, RunResponse};
use crate::engine::driver_managed::{
    ManagedDriver, ManagedRunInput, ShellFallback, build_source_map, collect_outputs, invoke_inner,
    list_path_for, resolve_unpack_root,
};
use crate::hartifactcontent;
use crate::hasync::Cancellable;
use anyhow::Context;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// OS-backed managed driver: materializes every input artifact to disk via
/// `hartifactcontent::unpack`. No FUSE involvement.
pub struct ManagedDriverOs {
    pub(crate) driver: Arc<Box<dyn ManagedDriver>>,
    pub(crate) shell_fallback: Arc<ShellFallback>,
}

impl ManagedDriverOs {
    pub(crate) async fn run_inner<'a, 'io>(
        &self,
        mut req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<RunResponse> {
        let sandbox_dir = req.sandbox_dir.clone();
        let cleanup_dir = sandbox_dir.clone();
        let mut sandbox_cleanup: Option<crate::engine::sandbox_cleaner::SandboxCleanupJob> =
            Some(Box::new(move || {
                crate::engine::sandbox_cleaner::remove_dir_all(&cleanup_dir)
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

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)
            .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

        let source_map = build_source_map(&inputs, &ws_dir)?;
        let source_map_json =
            serde_json::to_string(&source_map).with_context(|| "serialize source_map")?;
        fs::write(sandbox_pkg_dir.join("source_map.json"), source_map_json)
            .with_context(|| "write source_map.json")?;

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
                fuse_slot_guards: Vec::new(),
            });
        }

        collect_outputs(&mut res, target, hashin, &ws_dir, &sandbox_dir)?;

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            fuse_slot_guards: Vec::new(),
        })
    }
}
