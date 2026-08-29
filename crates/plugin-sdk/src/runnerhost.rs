//! Guest side of exec-runner forwarding: point this cdylib's copy of the
//! `execrunner` crate at the host's runner registry.
//!
//! A loaded cdylib statically links its OWN `execrunner`, whose host registry
//! `Engine::install_exec_runner_host` never touched — statics are not shared
//! across a dylib boundary. So a driver in here whose target names
//! `runner = "//tools/devenv:runner"` would fail with "no runner host is
//! installed in this component", which is the honest outcome but not a usable
//! one: it is exactly the shipped configuration for every cdylib plugin, and
//! the exec-runner seam refuses to degrade to a local spawn because that would
//! run the target outside the environment its cache key claims.
//!
//! The host hands the plugin a [`DynRunnerHost`] (via the
//! `heph_plugin_set_runner_host` symbol); [`install_runner_host`] installs a
//! `RunnerHost` that forwards every `prepare` across the seam.
//!
//! The whole `prepare` goes over, not just the resolution. Resolving means
//! *building* the runner target, and for a `session` runner it may launch the
//! environment and hold it open — so the host keeps the pool. Otherwise two
//! plugins in one build would each open their own `devenv shell` for the same
//! environment. A plugin sends a spec and gets a spec.

use hcore::hasync::Cancellable;
use hexecrunner::registry::ExecRunner as RunnerImpl;
use hexecrunner::wire::{PrepareReply, PrepareRequest, RunnerReply, RunnerRequest};
use hexecrunner::{PrepareOutcome, RunnerHost, SpecRewrite};
use hmodel::htaddr::Addr;
use hplugin_stabby::abi::{DynRunnerHost, StableRunnerHostDyn};
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::vec::Vec as SVec;
use std::sync::Arc;

/// Forwards `prepare` to the host across the stable ABI.
struct HostForwarder {
    host: DynRunnerHost,
}

// SAFETY: the handle is an owned `Box<dyn StableRunnerHost + Send + Sync>` on
// the host side; the `dynptr!` wrapper does not itself carry the auto traits, so
// they are asserted here exactly as the supervisor and log-sink forwarders do.
unsafe impl Send for HostForwarder {}
// SAFETY: as above.
unsafe impl Sync for HostForwarder {}

#[async_trait::async_trait]
impl RunnerHost for HostForwarder {
    /// Every request. A cdylib has exactly one upstream — the host that loaded
    /// it — so there is nothing to route between here; the routing by request
    /// id happens on the far side, against the engines that side knows about.
    fn owns(&self, _request_id: &str) -> bool {
        true
    }

    /// The handle lives as long as the cdylib, which is the process. There is
    /// no teardown to observe from this side.
    fn alive(&self) -> bool {
        true
    }

    async fn prepare(
        &self,
        request_id: &str,
        addr: &Addr,
        rewrite: SpecRewrite,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<PrepareOutcome> {
        // Cancellation is not forwarded: dropping this future is what the host
        // observes, and its seam wrapper aborts the spawned body on drop. A
        // second cancellation path would be a second thing to keep in sync.
        let req = PrepareRequest {
            request_id: request_id.to_string(),
            addr: addr.format(),
            rewrite,
        };
        let reply = self.host.prepare(SVec::from(req.encode().as_slice())).await;
        match PrepareReply::decode(&reply)? {
            PrepareReply::Ok {
                rewrite,
                supplies_environment,
            } => Ok(PrepareOutcome {
                rewrite,
                supplies_environment,
            }),
            // The host's error text, re-raised here so the driver's diagnostic
            // reads the same whether it ran in the binary or in a plugin.
            PrepareReply::Err(msg) => Err(anyhow::anyhow!(msg)),
        }
    }
}

/// Install the host's runner registry as this component's runner host.
///
/// Called from the cdylib's `heph_plugin_set_runner_host` export, which the
/// host invokes right after load.
pub fn install_runner_host(host: DynRunnerHost) {
    hexecrunner::install_host(Arc::new(HostForwarder { host }));
}

/// Guest side of a plugin-**exported** exec runner: wrap a real
/// [`hexecrunner::registry::ExecRunner`] as an ABI-stable
/// [`DynRunner`](hplugin_stabby::abi::DynRunner) the host registers beside its
/// builtins.
///
/// The mirror of [`install_runner_host`]. That one lets a plugin *use* the
/// host's runners; this lets a plugin *be* one, for a lifecycle no builtin
/// expresses — a per-exec `docker exec` carrying the target's own cwd, say.
/// When `session` fits, name it in a `runner.json` instead and inherit its
/// descriptor passing, pooling and signal fidelity.
pub fn make_dyn_runner(runner: Arc<dyn RunnerImpl>) -> hplugin_stabby::abi::DynRunner {
    hplugin_stabby::vtable::dynify(stabby::boxed::Box::new(GuestRunner { runner }))
}

/// The plugin's own runner, wrapped for the seam.
struct GuestRunner {
    runner: Arc<dyn RunnerImpl>,
}

impl hplugin_stabby::abi::StableRunner for GuestRunner {
    extern "C" fn prepare<'a>(&'a self, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let runner = Arc::clone(&self.runner);
        let fut = async move {
            let reply = match run_guest_prepare(runner.as_ref(), &req).await {
                Ok(rewrite) => RunnerReply::Ok(rewrite),
                Err(e) => RunnerReply::Err(format!("{e:#}")),
            };
            SVec::from(reply.encode().as_slice())
        };
        hplugin_stabby::vtable::dynify(stabby::boxed::Box::new(fut))
    }

    extern "C" fn supplies_environment(&self) -> bool {
        self.runner.supplies_environment()
    }

    extern "C" fn shutdown(&self) {
        self.runner.shutdown();
    }
}

/// Decode, run the plugin's runner, hand back its rewrite.
///
/// The cancellation token is this side's own: a cdylib runs in the host
/// process, so the host dropping the returned future is what stops the work,
/// and `RunnerCtx` still needs *a* token to hand the implementation.
async fn run_guest_prepare(
    runner: &dyn RunnerImpl,
    req: &[u8],
) -> anyhow::Result<hexecrunner::SpecRewrite> {
    let req = RunnerRequest::decode(req)?;
    let config: serde_json::Value = serde_json::from_str(&req.config_json)
        .map_err(|e| anyhow::anyhow!("exec runner {}: parse config: {e}", req.addr))?;
    let ctoken = hcore::hasync::StdCancellationToken::new();
    let ctx = hexecrunner::registry::RunnerCtx {
        addr: &req.addr,
        fingerprint: &req.fingerprint,
        config: &config,
        ctoken: &ctoken,
    };
    runner.prepare(&ctx, req.rewrite).await
}
