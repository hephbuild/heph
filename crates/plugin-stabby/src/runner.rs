//! Host side of a **plugin-exported** exec runner: wrap a `DynRunner` from a
//! loaded cdylib as an `ExecRunner` the registry can hold beside `local`,
//! `wrap` and `session`.
//!
//! This is the direction the seam was missing. `plugin-sdk/runnerhost` lets a
//! plugin *use* the host's runners; this lets a plugin *be* one. The registry
//! was built for it — by-name dispatch, a public `register` with a collision
//! guard, `shutdown_all` — and until now nothing could reach it, so the only
//! way a plugin could run a target elsewhere was to name a builtin in its
//! `runner.json`.
//!
//! Naming a builtin stays the right answer whenever `session` fits (the devenv
//! plugin holds a `devenv shell` open and gets descriptor passing, pooling and
//! signal fidelity for free). This is for the lifecycles it does not fit: a
//! per-exec `docker exec` carrying the target's own cwd, which a static wrap
//! prefix cannot produce.

use crate::abi::{DynRunner, StableRunnerDyn};
use crate::seam::panic_text;
use hexecrunner::SpecRewrite;
use hexecrunner::registry::{ExecRunner, RunnerCtx};
use hexecrunner::wire::{RunnerReply, RunnerRequest};
use stabby::vec::Vec as SVec;

/// A cdylib's runner, seen by the host registry as any other `ExecRunner`.
pub struct PluginRunner {
    name: String,
    runner: DynRunner,
    /// Read once at registration: it is constant per runner, and asking across
    /// the seam on every exec would pay a call to learn something that cannot
    /// have changed.
    supplies_environment: bool,
}

// SAFETY: the handle is an owned `Box<dyn StableRunner + Send + Sync>` on the
// plugin side; the `dynptr!` wrapper does not carry the auto traits itself, so
// they are asserted here as they are for every other handle crossing this seam.
unsafe impl Send for PluginRunner {}
// SAFETY: as above.
unsafe impl Sync for PluginRunner {}

impl PluginRunner {
    /// Wrap a loaded plugin's runner. `name` is what a `runner.json` selects.
    pub fn new(name: String, runner: DynRunner) -> Self {
        let supplies_environment = runner.supplies_environment();
        Self {
            name,
            runner,
            supplies_environment,
        }
    }
}

#[async_trait::async_trait]
impl ExecRunner for PluginRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn supplies_environment(&self) -> bool {
        self.supplies_environment
    }

    async fn prepare(
        &self,
        ctx: &RunnerCtx<'_>,
        rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite> {
        let req = RunnerRequest {
            addr: ctx.addr.to_string(),
            fingerprint: ctx.fingerprint.to_string(),
            // Re-serialized rather than passed as a parsed value: the plugin
            // owns its own config shape, so the host has nothing to say about
            // what is inside it.
            config_json: ctx.config.to_string(),
            rewrite,
        };
        let reply = self
            .runner
            .prepare(SVec::from(req.encode().as_slice()))
            .await;
        match RunnerReply::decode(&reply)? {
            RunnerReply::Ok(rewrite) => Ok(rewrite),
            // The plugin's own message, raised here unchanged so a failure reads
            // the same whether the runner is builtin or exported.
            RunnerReply::Err(msg) => Err(anyhow::anyhow!(msg)),
        }
    }

    fn shutdown(&self) {
        // Teardown, with nowhere to report to — but a panic crossing the seam is
        // a non-unwinding abort, so it is caught here rather than taking the
        // process down while it is already on its way out.
        let name = &self.name;
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.runner.shutdown()))
        {
            tracing::error!(
                runner = %name,
                panic = %panic_text(payload.as_ref()),
                "plugin exec runner panicked during shutdown"
            );
        }
    }
}
