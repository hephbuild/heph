//! A third-party entrypoint driver as a real cdylib — a test fixture for
//! `crates/bin-e2e` (`cdylib_entrypoint.rs`), never shipped.
//!
//! One driver, `tool`: its own `parse` declares the entrypoint (no exec
//! underneath), and its `run` spawns `/bin/sh -c SCRIPT sh <args>` through the
//! SDK's foreground spawn with everything the run's `RunOptions` carry. It is
//! the cdylib twin of `ForegroundTool` in `crates/e2e/tests/entrypoint.rs`,
//! which covers the same rows in-process across the stable-ABI seam; this one
//! adds what only a `dlopen` has — the symbol and layout checks, the cdylib's
//! own SDK statics, `tracing`, supervisor sink and runtime — and, through the
//! shipped binary, a terminal.
//!
//! The script reads its instructions from the caller's environment:
//! - writes what it saw (cwd, `HEPH_TEST_CALLER`, `HEPH_ROOT`, whether fds 0–2
//!   are a tty, its args) to `$HEPH_TEST_OUT`, then
//! - exits 42, or with `HEPH_TEST_MODE=wait` counts each TERM into
//!   `$HEPH_TEST_OUT.term`, prints `trap-ready`, and exits 7 once
//!   `$HEPH_TEST_OUT.stop` exists.

use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hcore::process::Exit;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::{Entrypoint, TargetDef, Workdir};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, DriverSchema,
    ParseRequest, ParseResponse, RunStdio,
};
use plugin_sdk::foreground::{ForegroundCommand, PathPolicy, RunnerRef, spawn_foreground};
use plugin_sdk::stabby::abi::{
    DynFunctionRegistry, DynLogSink, DynRunnerHost, DynSupervisor, PluginComponents,
};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_runner_host, install_supervisor,
    named_managed_driver,
};
use std::ffi::OsString;
use std::sync::Arc;

const SCRIPT: &str = r#"
tty=no
if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then tty=yes; fi
{
  echo "cwd=$(pwd -P)"
  echo "caller=$HEPH_TEST_CALLER"
  echo "root=$HEPH_ROOT"
  echo "tty=$tty"
  for a in "$@"; do echo "arg=[$a]"; done
} > "$HEPH_TEST_OUT"
if [ "$HEPH_TEST_MODE" = wait ]; then
  trap 'echo got-term >> "$HEPH_TEST_OUT.term"' TERM
  echo "trap-""ready"
  while [ ! -e "$HEPH_TEST_OUT.stop" ]; do sleep 0.05; done
  exit 7
fi
exit 42
"#;

struct Tool;

#[async_trait::async_trait]
impl ManagedDriver for Tool {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "tool".to_string(),
        })
    }

    fn schema(&self) -> DriverSchema {
        DriverSchema::default()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                entrypoint: Some(Entrypoint::new(true, Workdir::Caller)),
                hash: b"entrypoint-test-tool".to_vec(),
                ..Default::default()
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let options = req.request.options;
        let RunStdio::Inherit(fg) = options.stdio else {
            anyhow::bail!("an entrypoint run is in the caller's terminal");
        };
        let mut args: Vec<OsString> = vec!["-c".into(), SCRIPT.into(), "sh".into()];
        args.extend(options.args.unwrap_or_default());
        let cmd = ForegroundCommand::new(
            "/bin/sh",
            args,
            options.base_env.unwrap_or_default(),
            options.cwd.unwrap_or(req.sandbox_pkg_dir),
        )
        .with_foreground(&fg);
        let (child, _) = spawn_foreground(RunnerRef::local(), cmd, &PathPolicy::default(), ctoken)
            .await
            .context("spawn the tool")?;
        let exit = child
            .context("spawn the tool")?
            .wait(fg.signals, ctoken)
            .await
            .context("wait for the tool")?;
        if exit == Exit::Code(0) {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        Err(hplugin::error::ProcessFailed {
            status: format!("{exit:?}"),
            log: Arc::new(hcore::hartifactcontent::FileContent::new("/dev/null")),
            exit: Some(exit),
        }
        .into())
    }
}

/// Stable ABI create entry. The create config also records the host's
/// capabilities for the SDK, which refuses an entrypoint on a host without
/// them.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    let mut drivers = stabby::vec::Vec::new();
    match create_config_from_bytes(&cfg).and_then(|_| named_managed_driver(Tool)) {
        Ok(driver) => drivers.push(driver),
        // The create entry cannot fail across the ABI; the workspace then fails
        // on the first target naming the driver ("unknown driver").
        Err(e) => tracing::error!(error = %format!("{e:#}"), "entrypoint test plugin: create"),
    }
    PluginComponents {
        provider: stabby::option::Option::None(),
        drivers,
        functions: stabby::vec::Vec::new(),
        function_handle: stabby::option::Option::None(),
        hooks: stabby::vec::Vec::new(),
        runners: stabby::vec::Vec::new(),
        meta: stabby::vec::Vec::new(),
    }
}

/// Stable ABI log-sink entry: this cdylib's own `tracing` forwards to the host.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// ABI version entry, read by the host before anything else.
#[unsafe(no_mangle)]
pub extern "C" fn heph_plugin_abi_semver() -> *const std::ffi::c_char {
    plugin_sdk::stabby::abi_semver()
}

/// Stable ABI log-filter entry.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_filter(filter: stabby::vec::Vec<u8>) {
    plugin_sdk::stabby::install_log_filter(&filter);
}

/// Stable ABI supervisor entry: the tool's pid is tracked with the host's
/// sidecar through this cdylib's own copy of `proc`.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

/// Stable ABI exec-runner entry, for parity with the shipped cdylibs.
#[stabby::export]
pub extern "C" fn heph_plugin_set_runner_host(host: DynRunnerHost) {
    install_runner_host(host);
}
