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

    async fn run<'a, 'io>(
        &self,
        req: RunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        match self.pick(&req.inputs)? {
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
        match self.pick(&req.inputs)? {
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
    fn pick(&self, inputs: &[RunInput]) -> anyhow::Result<Pick> {
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

    struct NoShellDriver;

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
                    addr: Default::default(),
                    labels: vec![],
                    raw_def: Arc::new("recorded-shell-def".to_string()),
                    inputs: vec![],
                    outputs: vec![],
                    support_files: vec![],
                    cache: hplugin::driver::targetdef::CacheConfig::off(),
                    pty: false,
                    hash: vec![],
                    transparent: false,
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

        let target_def = TargetDef {
            addr: Default::default(),
            labels: vec![],
            raw_def: Arc::new(()),
            inputs: vec![],
            outputs: vec![],
            support_files: vec![],
            cache: hplugin::driver::targetdef::CacheConfig::off(),
            pty: false,
            hash: vec![],
            transparent: false,
        };
        let request_id = "shell-fallback-test".to_string();
        let req = RunRequest {
            request_id: &request_id,
            target: &target_def,
            tree_root_path: tmp.path().to_path_buf(),
            inputs: vec![],
            hashin: "",
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox.clone(),
        };

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
