//! Third-party drivers as a real cdylib — a test fixture for `crates/bin-e2e`
//! (`cdylib_entrypoint.rs`, `plugin_dylib.rs`), never shipped.
//!
//! The second driver, `allot`, reports the run's allotment: its `parse` puts
//! the target's `resources` on the def with the SDK helper, and its `run`
//! writes what `RunRequest.allotment` carried into its one output,
//! `allotment.txt` — every granted pool as `<pool>=<rendered amount>`, in
//! name order and space-separated, or `told-nothing`. It is
//! what lets `allotment_crosses_the_plugin_seam` see the field across a real
//! `dlopen`, which a `convert.rs` round-trip cannot.
//!
//! The third, `stream`, writes its log through the writers of a run handed
//! `log_capture` in gated phases, for `run_log.rs`'s `log_capture_streams_live`.
//!
//! The first driver, `tool`: its own `parse` declares the entrypoint (no exec
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
//!   are a tty, its args) to `$HEPH_TEST_OUT`, and the device `tty` names to
//!   `$HEPH_TEST_OUT.tty`, then
//! - exits 42, or with `HEPH_TEST_MODE=wait` counts each TERM into
//!   `$HEPH_TEST_OUT.term`, prints `trap-ready`, and exits 7 once
//!   `$HEPH_TEST_OUT.stop` exists, or with `HEPH_TEST_MODE=stop` prints its and
//!   its parent's pids, stops itself, and exits 4 once continued.

use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hcore::process::Exit;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{CacheConfig, Entrypoint, Output, TargetDef, Workdir};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, DriverSchema,
    ParseRequest, ParseResponse, Resources, RunStdio,
};
use plugin_sdk::foreground::{ForegroundCommand, PathPolicy, RunnerRef, spawn_foreground};
use plugin_sdk::stabby::abi::{DynFunctionRegistry, PluginComponents};
use plugin_sdk::stabby::{create_config_from_bytes, named_managed_driver};
use std::ffi::OsString;

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
/usr/bin/tty > "$HEPH_TEST_OUT.tty"
if [ "$HEPH_TEST_MODE" = stop ]; then
  echo "tool-pid-$$."
  echo "heph-pid-$PPID."
  kill -STOP $$
  echo "resumed-""after-fg"
  exit 4
fi
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
                entrypoint: Some(Entrypoint::new(true, Workdir::Caller).with_pty()),
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
        let (fg, on_pty) = match options.stdio {
            RunStdio::Inherit(fg) => (fg, None),
            RunStdio::Pty(p) => (p.foreground, Some((p.slave, p.stops, p.continues))),
            _ => anyhow::bail!("an entrypoint run is in the caller's terminal"),
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
        let (cmd, channels) = match on_pty {
            Some((slave, stops, continues)) => (cmd.with_slave(slave), Some((stops, continues))),
            None => (cmd, None),
        };
        let (child, _) = spawn_foreground(RunnerRef::local(), cmd, &PathPolicy::default(), ctoken)
            .await
            .context("spawn the tool")?;
        let child = child.context("spawn the tool")?;
        let exit = match channels {
            Some((stops, continues)) => child.wait_pty(fg.signals, stops, continues, ctoken).await,
            None => child.wait(fg.signals, ctoken).await,
        }
        .context("wait for the tool")?;
        if exit == Exit::Code(0) {
            return Ok(ManagedRunResponse { artifacts: vec![] });
        }
        Err(hplugin::error::ProcessFailed::new(format!("{exit:?}"), Some(exit)).into())
    }
}

/// The `allot` driver's output, relative to the target's package.
const ALLOT_OUT: &str = "allotment.txt";

/// `<pkg>/allotment.txt`, the workspace-relative path of the output of a
/// target in package `pkg`.
fn allot_out(pkg: &str) -> String {
    if pkg.is_empty() {
        ALLOT_OUT.to_string()
    } else {
        format!("{pkg}/{ALLOT_OUT}")
    }
}

struct Allot;

#[async_trait::async_trait]
impl ManagedDriver for Allot {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "allot".to_string(),
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
        let addr = &req.target_spec.addr;
        let resources = Resources::from_config(&req.target_spec.config)
            .with_context(|| format!("{}: resources", addr.format()))?;
        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::FilePath(allot_out(addr.package.as_str())),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                hash: b"entrypoint-test-allot".to_vec(),
                resources,
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
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let seen = match &req.request.allotment {
            Some(a) => {
                let pools: Vec<String> = a.iter().map(|(p, n)| format!("{p}={n}")).collect();
                format!("{}\n", pools.join(" "))
            }
            None => "told-nothing\n".to_string(),
        };
        let out = req
            .sandbox_ws_dir
            .join(allot_out(req.request.target.addr.package.as_str()));
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&out, seen).with_context(|| format!("write {}", out.display()))?;
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// How long a `stream` run waits for each gate before it gives up: a test that
/// never opens one fails on the error rather than hanging its heph.
const STREAM_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The `stream` driver's lines, one pair per phase: what `log_capture_streams_live`
/// expects the live file, and then the stored log, to hold.
const STREAM_PHASES: [(&[u8], &[u8]); 2] = [(b"out-1\n", b"err-1\n"), (b"out-2\n", b"err-2\n")];

/// The third driver, `stream`: the run's log as the host captures it. It
/// refuses a run not handed `log_capture`, then for each phase writes a line
/// to stdout and one to stderr through the run's writers and waits for the
/// gate `<root>/<pkg>/<name>.<phase>` to exist, so a test can read the live
/// log between phases. A target whose name starts with `fail` then fails.
/// Its targets are cached (locally), so a success's log is promoted into the
/// cache entry's `Log`, where another heph (`heph log`) reads it.
struct Stream;

#[async_trait::async_trait]
impl ManagedDriver for Stream {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "stream".to_string(),
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
                hash: b"entrypoint-test-stream".to_vec(),
                cache: CacheConfig::on(false),
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
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        use tokio::io::AsyncWriteExt as _;

        let mut request = req.request;
        anyhow::ensure!(
            request.log_capture,
            "stream: the host did not hand this run log_capture"
        );
        let stdout = request.stdout.take().context("stream: no stdout writer")?;
        let stderr = request.stderr.take().context("stream: no stderr writer")?;
        let addr = &request.target.addr;
        let gates = request.tree_root_path.join(addr.package.as_str());
        for (phase, (out, err)) in (1..).zip(STREAM_PHASES) {
            stdout
                .write_all(out)
                .await
                .context("stream: write stdout")?;
            stdout.flush().await.context("stream: flush stdout")?;
            stderr
                .write_all(err)
                .await
                .context("stream: write stderr")?;
            stderr.flush().await.context("stream: flush stderr")?;
            wait_for_gate(gates.join(format!("{}.{phase}", addr.name))).await?;
        }
        if addr.name.starts_with("fail") {
            let exit = Exit::Code(3);
            return Err(hplugin::error::ProcessFailed::new(format!("{exit:?}"), Some(exit)).into());
        }
        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Wait until `gate` exists, polled on a thread of its own: the cdylib's
/// runtime has no timer, and blocking the task would block a host worker.
async fn wait_for_gate(gate: std::path::PathBuf) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let found = loop {
            if gate.exists() {
                break Ok(());
            }
            if started.elapsed() > STREAM_GATE_TIMEOUT {
                break Err(anyhow::anyhow!(
                    "stream: {} never appeared within {STREAM_GATE_TIMEOUT:?}",
                    gate.display()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // The run gone means nobody is waiting any more.
        drop(tx.send(found));
    });
    rx.await.context("stream: the gate thread ended")?
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
    let created = create_config_from_bytes(&cfg).and_then(|_| {
        Ok([
            named_managed_driver(Tool)?,
            named_managed_driver(Allot)?,
            named_managed_driver(Stream)?,
        ])
    });
    match created {
        Ok(created) => created.into_iter().for_each(|d| drivers.push(d)),
        // The create entry cannot fail across the ABI; the workspace then fails
        // on the first target naming the driver ("unknown driver").
        Err(e) => tracing::error!(error = %format!("{e:#}"), "entrypoint test plugin: create"),
    }
    PluginComponents {
        drivers,
        ..PluginComponents::empty()
    }
}

// The supervisor entry tracks the tool's pid with the host's sidecar through
// this cdylib's own copy of `proc`.
plugin_sdk::export_host_entries!();
