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
        let groups = detect_output_collisions_blocking(groups, ctoken)
            .await
            .with_context(|| format!("output-collision check for {}", req.target.addr.format()))?;

        let mut inputs: Vec<ManagedRunInput> = Vec::new();
        let mut holds: Vec<crate::stage::StageHold> = Vec::new();
        // The list the last staged input appended to, still open: a
        // multi-output tool's inputs are adjacent and share one.
        let mut open_list: Option<crate::stage::ListFile> = None;
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
                            list_path
                                .as_deref()
                                .map(|p| crate::stage::ListFile::reuse(&mut open_list, p)),
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
                        // worker, and stopped by the run's cancellation, see
                        // `unpack_blocking`.
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
            // Here, while the sandbox is live and before the stage holds drop:
            // a staged input the target listed is still linked.
            Err(err) => {
                let err = crate::failure_out::keep_failure_write_back(
                    err,
                    target,
                    hashin,
                    shell,
                    ctoken,
                    &ws_dir,
                    &sandbox_dir,
                )
                .await;
                return Err(crate::failure_out::keep_failure_outputs(
                    err,
                    &target.failure_outputs,
                    hashin,
                    shell,
                    ctoken,
                    &ws_dir,
                    &sandbox_dir,
                    self.stage_dir.as_slice(),
                )
                .await);
            }
        };

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

        res.artifacts.extend(
            collect_outputs(
                target,
                hashin,
                &ws_dir,
                &sandbox_dir,
                self.stage_dir.as_deref(),
                ctoken,
            )
            .await?,
        );

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

    /// A driver whose run writes `report.txt` into its package dir, then
    /// succeeds (`None`) or fails with the error the function makes.
    struct Outcome(Option<fn() -> anyhow::Error>);

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
            req: ManagedRunRequest<'a, 'io>,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            fs::write(req.sandbox_pkg_dir.join("report.txt"), "report")?;
            match self.0 {
                Some(fail) => Err(fail()),
                None => Ok(ManagedRunResponse { artifacts: vec![] }),
            }
        }
        fn supports_shell(&self) -> bool {
            true
        }
        async fn run_shell<'a, 'io>(
            &self,
            req: ManagedRunRequest<'a, 'io>,
            ct: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ManagedRunResponse> {
            self.run(req, ct).await
        }
    }

    fn os(ok: bool, stage: &std::path::Path) -> ManagedDriverOs {
        os_failing(
            (!ok).then_some((|| anyhow::anyhow!("the run failed")) as fn() -> anyhow::Error),
            stage,
        )
    }

    fn os_failing(fail: Option<fn() -> anyhow::Error>, stage: &std::path::Path) -> ManagedDriverOs {
        let fallback = Arc::new(ShellFallback {
            driver: Arc::new(Outcome(None)),
            spec_template: Arc::new(hplugin::provider::TargetSpec::default()),
        });
        ManagedDriverOs {
            driver: Arc::new(Box::new(Outcome(fail))),
            shell_fallback: fallback,
            stage_dir: Some(stage.to_path_buf()),
        }
    }

    /// A token that reads cancelled once `0` exists: a cancellation that
    /// lands while the run is running, after it wrote that file. `.1` is its
    /// reason: the run's `timeout`, or (false) its request's cancellation.
    struct CancelledOnceExists(std::path::PathBuf, bool);

    impl Cancellable for CancelledOnceExists {
        fn is_cancelled(&self) -> bool {
            self.0.exists()
        }
        fn timed_out(&self) -> bool {
            self.1 && self.is_cancelled()
        }
        fn cancelled(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                while !self.0.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        }
        fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
            Arc::new(Self(self.0.clone(), self.1))
        }
    }

    /// A run cancelled by its own `timeout` (its token says so) keeps its
    /// `failure_out` with no process exit behind it, and writes nothing back;
    /// one cancelled by its request keeps nothing.
    #[tokio::test]
    async fn timeout_keeps_failure_out_and_writes_nothing_back() -> anyhow::Result<()> {
        use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as DefPath};
        let tmp = tempfile::tempdir()?;
        let stage = tmp.path().join("stage");
        let id = "t".to_string();
        let cancelled: fn() -> anyhow::Error = || anyhow::anyhow!("cancelled");
        let run = |name: &'static str, timed_out: bool| {
            let sandbox = tmp.path().join(name);
            let os = os_failing(Some(cancelled), &stage);
            let id = &id;
            async move {
                let mut target = keeps_report();
                target.timeout = Some(hplugin::driver::Timeout::new(
                    std::time::Duration::from_secs(3600),
                )?);
                target.outputs = vec![hplugin::driver::targetdef::Output {
                    group: String::new(),
                    paths: vec![DefPath {
                        content: Content::FilePath("app/report.txt".to_string()),
                        codegen_tree: CodegenMode::InPlace {
                            write_on_failure: true,
                        },
                        collect: true,
                    }],
                }];
                let ct = CancelledOnceExists(sandbox.join("ws/app/report.txt"), timed_out);
                let req = RunRequest::new(id, &target, sandbox.clone());
                let Err(err) = os.run_inner(req, &ct, false).await else {
                    anyhow::bail!("{name}: the run must fail");
                };
                anyhow::Ok((err, sandbox))
            }
        };

        // Cancelled as timed out, however far from its limit the clock says.
        let (err, _sandbox) = run("timed-out", true).await?;
        let kept = hplugin::error::failure_out_tar(&err).expect("kept on a timeout");
        assert_eq!(
            crate::failure_out::tar_files(kept)
                .get("app/report.txt")
                .map(String::as_str),
            Some("report")
        );
        assert!(
            hplugin::error::failure_write_back(&err).is_none(),
            "a timeout writes nothing back"
        );

        // The same run cancelled by its request.
        let (err, sandbox) = run("cancelled", false).await?;
        assert!(hplugin::error::failure_out_tar(&err).is_none());
        assert!(crate::failure_out::packed(&sandbox).is_empty());
        assert!(hplugin::error::failure_write_back(&err).is_none());
        Ok(())
    }

    /// A def in `//app` that keeps `app/report.txt` on failure.
    fn keeps_report() -> TargetDef {
        TargetDef {
            addr: hmodel::htaddr::parse_addr("//app:t").expect("addr"),
            failure_outputs: vec![hplugin::driver::targetdef::FailureOutput {
                group: String::new(),
                paths: vec![hplugin::driver::targetdef::path::Content::FilePath(
                    "app/report.txt".to_string(),
                )],
            }],
            ..TargetDef::default()
        }
    }

    /// A failure no process exit is behind — a driver's or heph's own, before
    /// or around the run — keeps nothing, and neither does a cancelled run or a
    /// `--shell`; a process exit keeps the report, packed into a tar under
    /// the sandbox (Surface table, I8).
    #[tokio::test]
    async fn failure_without_exit_collects_nothing() -> anyhow::Result<()> {
        use hplugin::error::{Exit, ProcessFailed};
        let tmp = tempfile::tempdir()?;
        let stage = tmp.path().join("stage");
        let target = keeps_report();
        let id = "t".to_string();
        let exit1: fn() -> anyhow::Error =
            || anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))));
        let no_exit: fn() -> anyhow::Error = || anyhow::anyhow!("credential source failed");
        let unknown_exit: fn() -> anyhow::Error =
            || anyhow::Error::new(ProcessFailed::new("exit status: ?", None));

        let run = |name: &'static str, fail: fn() -> anyhow::Error, cancel: bool, shell: bool| {
            let sandbox = tmp.path().join(name);
            let os = os_failing(Some(fail), &stage);
            let target = &target;
            let id = &id;
            async move {
                // Cancelled once the run has written its report: staging
                // stops on a token cancelled up front, before the run.
                let ct: Arc<dyn Cancellable + Send + Sync> = if cancel {
                    Arc::new(CancelledOnceExists(
                        sandbox.join("ws/app/report.txt"),
                        false,
                    ))
                } else {
                    Arc::new(StdCancellationToken::new())
                };
                let req = RunRequest::new(id, target, sandbox.clone());
                let err = match os.run_inner(req, &*ct, shell).await {
                    Ok(_) => anyhow::bail!("{name}: the run must fail"),
                    Err(e) => e,
                };
                anyhow::Ok((err, sandbox))
            }
        };

        for (name, fail, cancel, shell) in [
            ("no-exit", no_exit, false, false),
            ("unknown-exit", unknown_exit, false, false),
            ("cancelled", exit1, true, false),
            ("shell", exit1, false, true),
        ] {
            let (err, sandbox) = run(name, fail, cancel, shell).await?;
            assert!(hplugin::error::failure_out_tar(&err).is_none(), "{name}");
            assert!(
                crate::failure_out::packed(&sandbox).is_empty(),
                "{name}: nothing packed"
            );
            assert!(
                sandbox.join("ws/app/report.txt").exists(),
                "{name}: the run did write it"
            );
        }

        let (err, _sandbox) = run("exit-1", exit1, false, false).await?;
        let kept = hplugin::error::failure_out_tar(&err).expect("kept on an exit");
        assert_eq!(
            crate::failure_out::tar_files(kept)
                .get("app/report.txt")
                .map(String::as_str),
            Some("report")
        );
        assert_eq!(
            hplugin::error::process_exit(&err),
            Some(Exit::Code(1)),
            "the target's own error is still there"
        );
        Ok(())
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

    /// R29, for every driver: an entrypoint's read-only input is linked from
    /// the stage as a build's is, and held for as long as the run's guards.
    #[tokio::test]
    async fn entrypoint_read_only_input_is_linked_like_a_build() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let stage = tmp.path().join("stage");
        let target = TargetDef {
            entrypoint: Some(hplugin::driver::targetdef::Entrypoint::new(
                false,
                hplugin::driver::targetdef::Workdir::Caller,
            )),
            ..TargetDef::default()
        };
        let id = "t".to_string();
        let ct = StdCancellationToken::new();
        let sandbox = tmp.path().join("ep");

        let mut req = RunRequest::new(&id, &target, sandbox.clone());
        req.inputs = vec![read_only_input()];
        let res = os(true, &stage).run_inner(req, &ct, false).await?;
        assert_eq!(res.sandbox_guards.len(), 1, "the staged entry is held");
        let file = sandbox.join("ws/sdk/bin/go");
        let real = std::fs::canonicalize(&file)
            .with_context(|| format!("the input at {}", file.display()))?;
        assert!(
            real.starts_with(std::fs::canonicalize(&stage)?),
            "linked from the stage: {}",
            real.display()
        );
        assert_eq!(crate::stage::clear_stage(&stage).0, 0, "held by the guard");
        drop(res);
        assert_eq!(crate::stage::clear_stage(&stage).0, 1, "released with it");
        Ok(())
    }
}
