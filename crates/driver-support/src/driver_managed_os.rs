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
        // materialize anything — the copy path would otherwise silently
        // last-write-wins. Off the worker: the check enumerates every input's
        // entry paths, which for a cache-backed input is a header scan over a
        // sqlite blob and can park on that key's queued write.
        let groups = detect_output_collisions_blocking(groups)
            .await
            .with_context(|| format!("output-collision check for {}", req.target.addr.format()))?;

        let mut inputs: Vec<ManagedRunInput> = Vec::new();
        let mut holds: Vec<crate::stage::StageHold> = Vec::new();
        for (unpack_root, group) in groups {
            fs::create_dir_all(&unpack_root)
                .with_context(|| format!("create unpack root {:?}", unpack_root))?;
            for input in group {
                let list_path = list_path_for(&input, &list_dir);
                match self.stage_dir.as_deref() {
                    Some(stage_dir) if crate::stage::is_read_only(&input.annotations) => {
                        let hold = crate::stage::stage_and_link(
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
                        // Held until the run's guards drop, so `gc` cannot
                        // clear the entry under its links (N3). On an error
                        // anywhere below, `holds` drops with this frame.
                        holds.extend(hold);
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

        let sandbox_pkg_dir = ws_dir.join(req.target.addr.package.as_str());
        fs::create_dir_all(&sandbox_pkg_dir)
            .with_context(|| format!("create pkg dir: {:?}", sandbox_pkg_dir))?;

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

        // The stage holds ride out with the response: the engine drops them
        // with the rest of the run's guards, after it has read the sandbox.
        let sandbox_guards = holds
            .into_iter()
            .map(|h| Box::new(h) as hplugin::driver::SandboxGuard)
            .collect();

        if shell {
            return Ok(RunResponse {
                artifacts: vec![],
                sandbox_cleanup: sandbox_cleanup.take(),
                sandbox_guards,
            });
        }

        res.artifacts
            .extend(collect_outputs(target, hashin, &ws_dir, &sandbox_dir).await?);

        Ok(RunResponse {
            artifacts: res.artifacts,
            sandbox_cleanup: sandbox_cleanup.take(),
            sandbox_guards,
        })
    }
}

#[cfg(all(test, unix))]
#[expect(
    clippy::panic_in_result_fn,
    reason = "tests assert inside `anyhow::Result` bodies so setup failures carry context"
)]
mod tests {
    use super::*;
    use crate::driver_managed::{ManagedRunRequest, ManagedRunResponse};
    use async_trait::async_trait;
    use hcore::hasync::StdCancellationToken;
    use hplugin::driver::inputartifact::{InputArtifact, Type};
    use hplugin::driver::targetdef::TargetDef;
    use hplugin::driver::{
        ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse,
        ParseRequest, ParseResponse,
    };

    /// A driver whose run succeeds or fails, and does nothing else.
    struct Outcome(bool);

    #[async_trait]
    impl ManagedDriver for Outcome {
        fn config(&self, _: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "outcome".to_string(),
            })
        }
        fn schema(&self) -> hplugin::driver::DriverSchema {
            hplugin::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            _: ParseRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            anyhow::bail!("not parsed in this test")
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
            anyhow::ensure!(self.0, "the run failed");
            Ok(ManagedRunResponse { artifacts: vec![] })
        }
    }

    fn os(ok: bool, stage: &std::path::Path) -> ManagedDriverOs {
        let fallback = Arc::new(ShellFallback {
            driver: Arc::new(Outcome(true)),
            spec_template: Arc::new(hplugin::provider::TargetSpec::default()),
        });
        ManagedDriverOs {
            driver: Arc::new(Box::new(Outcome(ok))),
            shell_fallback: fallback,
            stage_dir: Some(stage.to_path_buf()),
        }
    }

    fn read_only_input() -> RunInput {
        let content = crate::stage::tests::content("abc", &[("sdk/bin/go", "go", true)]);
        RunInput {
            artifact: InputArtifact {
                r#type: Type::Dep,
                origin_id: "sdk".to_string(),
                content,
            },
            origin_id: "sdk".to_string(),
            source_addr: hmodel::htaddr::parse_addr("//sdk:go").expect("addr"),
            filters: vec![],
            annotations: BTreeMap::from([(
                crate::stage::READ_ONLY_ANNOTATION.to_string(),
                "true".to_string(),
            )]),
        }
    }

    /// A successful run hands its stage holds out as `sandbox_guards`, which
    /// keep `gc` off the entry until they drop; a failing run drops them with
    /// its frame, so nothing stays held (R16).
    #[tokio::test]
    async fn stage_holds_ride_the_guards_and_drop_on_failure() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let stage = tmp.path().join("stage");
        let target = TargetDef::default();
        let id = "t".to_string();
        let ct = StdCancellationToken::new();

        let mut req = RunRequest::new(&id, &target, tmp.path().join("ok"));
        req.inputs = vec![read_only_input()];
        let res = os(true, &stage).run_inner(req, &ct, false).await?;
        assert_eq!(res.sandbox_guards.len(), 1, "one held entry");
        assert_eq!(crate::stage::clear_stage(&stage).0, 0, "held by the guard");
        drop(res);
        assert_eq!(crate::stage::clear_stage(&stage).0, 1, "released with it");

        let mut req = RunRequest::new(&id, &target, tmp.path().join("fail"));
        req.inputs = vec![read_only_input()];
        let failed = os(false, &stage).run_inner(req, &ct, false).await;
        assert!(failed.is_err(), "the run fails");
        assert_eq!(
            crate::stage::clear_stage(&stage).0,
            1,
            "a failed run holds nothing"
        );
        Ok(())
    }
}
