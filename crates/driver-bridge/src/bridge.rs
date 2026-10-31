use crate::driver_managed_fuse::ManagedDriverFuse;
use crate::fuseconfig::FuseMode;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ShellFallback};
use hdriver_support::driver_managed_os::ManagedDriverOs;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver,
    ParseRequest, ParseResponse, RunInput, RunRequest, RunResponse,
};
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------
// Router: picks Os or Fuse per target based on FuseConfig + input walk
// ---------------------------------------------------------------------

/// Hardcoded auto-mode threshold. Below this total input byte size, the
/// cost of FUSE mount setup outweighs unpack-copy. Tuned to roughly the
/// observed macFUSE per-mount cost amortized across small targets.
const AUTO_FUSE_BYTE_THRESHOLD: u64 = 1024 * 1024;

pub struct ManagedDriverBridge {
    cfg: FuseMode,
    os: ManagedDriverOs,
    fuse: Option<ManagedDriverFuse>,
}

/// FUSE wiring the engine hands the bridge when a `LayeredFs` is available.
/// Keeps the bridge's FUSE fields private to this crate while letting the
/// engine supply the engine-owned bits (home dir, shared fs, lower/upper).
pub struct FuseSlot {
    pub home: PathBuf,
    pub fs: Arc<hsandboxfuse::LayeredFs>,
    pub fuse_lower: PathBuf,
    pub fuse_upper: PathBuf,
}

impl ManagedDriverBridge {
    /// Assemble a bridge from its inner driver, the shell fallback, the resolved
    /// FUSE config, and (when FUSE is available) the engine-supplied slot.
    pub fn new(
        driver: Box<dyn ManagedDriver>,
        shell_fallback: Arc<ShellFallback>,
        cfg: FuseMode,
        home: PathBuf,
        fuse: Option<FuseSlot>,
    ) -> Self {
        let driver = Arc::new(driver);
        let os = ManagedDriverOs {
            driver: driver.clone(),
            shell_fallback: shell_fallback.clone(),
            stage_dir: Some(home.join("stage")),
        };
        let fuse = fuse.map(|f| ManagedDriverFuse {
            driver: driver.clone(),
            shell_fallback: shell_fallback.clone(),
            home: f.home,
            fs: f.fs,
            fuse_lower: f.fuse_lower,
            fuse_upper: f.fuse_upper,
        });
        Self { cfg, os, fuse }
    }

    /// Test helper: build an OS-only bridge (no FUSE) without an `Engine`.
    /// Production code paths construct bridges via `Engine::new_managed_driver`.
    #[cfg(test)]
    pub(crate) fn new_os_for_test_with_shell_fallback(
        driver: Box<dyn ManagedDriver>,
        shell_fallback: Arc<ShellFallback>,
    ) -> Self {
        Self {
            cfg: FuseMode::Off,
            os: ManagedDriverOs {
                driver: Arc::new(driver),
                shell_fallback,
                stage_dir: None,
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

    fn schema(&self) -> hplugin::driver::DriverSchema {
        self.os.driver.schema()
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

    fn supports_shell(&self) -> bool {
        self.os.driver.supports_shell()
    }

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let pick = self.pick(&req)?;
        htelemetry::telemetry::record_sandbox(matches!(pick, Pick::Fuse));
        match pick {
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
        let pick = self.pick(&req)?;
        htelemetry::telemetry::record_sandbox(matches!(pick, Pick::Fuse));
        // An interactive sandbox shell was actually entered (not just `--shell`
        // requested then errored upstream).
        htelemetry::telemetry::record_shell_session();
        match pick {
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
    ///
    /// An entrypoint's run is always the OS copy, whatever the config (Open
    /// 8): its sandbox is a unique per-invocation directory the engine
    /// creates, owns through a lock file inside it and removes itself, while
    /// the FUSE runner redirects `sandbox_dir` into its upper side and owns
    /// that teardown. Its read-only inputs are linked from the stage exactly
    /// as a build's are (R29).
    fn pick(&self, req: &RunRequest<'_, '_>) -> anyhow::Result<Pick> {
        if req.target.entrypoint.is_some() {
            return Ok(Pick::Os);
        }
        let inputs: &[RunInput] = &req.inputs;
        match self.cfg {
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

#[cfg(test)]
mod shell_fallback_tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
    use hplugin::driver::targetdef::TargetDef;
    use hplugin::provider::TargetSpec;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(super) struct NoShellDriver;

    #[async_trait]
    impl ManagedDriver for NoShellDriver {
        fn config(&self, _: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "noshell".to_string(),
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
        fn schema(&self) -> hplugin::driver::DriverSchema {
            hplugin::driver::DriverSchema::default()
        }
        async fn parse(
            &self,
            _: ParseRequest,
            _: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<ParseResponse> {
            self.parse_called.store(true, Ordering::SeqCst);
            Ok(ParseResponse {
                target_def: TargetDef {
                    raw_def: Arc::new("recorded-shell-def".to_string()),
                    ..Default::default()
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
        let mut config: std::collections::HashMap<String, hcore::htvalue::Value> =
            std::collections::HashMap::new();
        config.insert("run".to_string(), hcore::htvalue::Value::List(vec![]));
        let fallback = Arc::new(ShellFallback {
            driver: Arc::new(RecordingShellDriver {
                parse_called: parse_called.clone(),
                run_shell_called: run_shell_called.clone(),
            }),
            spec_template: Arc::new(TargetSpec {
                addr: Default::default(),
                driver: "exec".to_string(),
                config,
                ..Default::default()
            }),
        });
        let bridge = ManagedDriverBridge::new_os_for_test_with_shell_fallback(
            Box::new(NoShellDriver),
            fallback,
        );

        let ctoken = StdCancellationToken::new();
        let tmp = tempfile::tempdir()?;
        let sandbox = tmp.path().join("sandbox");
        fs::create_dir_all(&sandbox)?;

        let target_def = TargetDef::default();
        let request_id = "shell-fallback-test".to_string();
        let mut req = RunRequest::new(&request_id, &target_def, sandbox.clone());
        req.tree_root_path = tmp.path().to_path_buf();

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
mod pick_tests {
    use super::*;
    use hplugin::driver::targetdef::{Entrypoint, TargetDef, Workdir};

    /// An entrypoint's run never goes through FUSE, even with `fuse.enabled`
    /// forced on: its sandbox is the engine's own per-invocation directory
    /// (Open 8). A build with the same config still asks for FUSE.
    #[test]
    fn entrypoint_always_picks_the_os_runner() {
        let fallback = Arc::new(ShellFallback {
            driver: Arc::new(shell_fallback_tests::NoShellDriver),
            spec_template: Arc::new(hplugin::provider::TargetSpec::default()),
        });
        let bridge = ManagedDriverBridge::new(
            Box::new(shell_fallback_tests::NoShellDriver),
            fallback,
            FuseMode::On,
            PathBuf::from("/nonexistent-home"),
            None,
        );
        let id = "pick".to_string();
        let build = TargetDef::default();
        let req = RunRequest::new(&id, &build, "/nonexistent-sandbox");
        assert!(
            bridge.pick(&req).is_err(),
            "a build with fuse forced on and no mount is refused"
        );
        let entrypoint = TargetDef {
            entrypoint: Some(Entrypoint::new(false, Workdir::Caller)),
            ..Default::default()
        };
        let req = RunRequest::new(&id, &entrypoint, "/nonexistent-sandbox");
        assert!(matches!(bridge.pick(&req), Ok(Pick::Os)));
    }
}
