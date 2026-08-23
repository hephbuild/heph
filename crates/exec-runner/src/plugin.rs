//! The **exec-runner component**: what a plugin exports to serve environments.
//!
//! A runner is its own component kind, beside a provider, a driver and a hook —
//! not a driver that answers extra methods. It does not parse, build, or carry a
//! config schema, and its name is its own: a plugin can export a `docker` runner
//! with no driver at all, and a runner target built by any driver can name it.
//!
//! ## Why it is addressed by session id
//!
//! [`ExecRunner`] hands back an [`ExecSession`] — a live object. That cannot
//! cross a stable boundary: what a session owns is a shell, a socket, a pid. So
//! the component trait is id-shaped instead. The plugin keeps its sessions and
//! the host holds only the id it was given.
//!
//! [`prepare_spec`](ExecRunnerPlugin::prepare_spec) is **per target process**,
//! and that is the point of the whole component: it is what makes the runner the
//! party that starts the process. One `devenv shell` is opened once, and every
//! target's exec is routed into it — decided per spawn, not once per
//! environment.

use std::sync::Arc;

use hcore::hasync::Cancellable;
use hproc::proc_exec::Spec;

use crate::{
    ExecRunner, ExecSession, OpenRequest, OpenedSession, SessionCaps, SessionDescription,
    SpawnError, TeardownJob,
};

/// A runner as a plugin implements it.
#[async_trait::async_trait]
pub trait ExecRunnerPlugin: Send + Sync {
    /// Acquire the session for `req.key`.
    ///
    /// Called at most once per distinct environment — the host's pool
    /// single-flights it — and never on the per-target path. It may be slow: a
    /// cold devenv evaluation is tens of seconds.
    async fn open_session(
        &self,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<OpenedSession>;

    /// Transform one spawn's spec so the process is created in this environment.
    ///
    /// Note that stdio never crosses the seam — `StdioSpec::Fd` owns a
    /// descriptor — so the host re-applies the real stdio to whatever comes
    /// back. Merging the session's own environment is the runner's job here:
    /// the host applies nothing on its behalf.
    async fn prepare_spec(&self, session_id: &str, spec: Spec) -> anyhow::Result<Spec>;

    /// Release the session. Best-effort and idempotent: the host also calls it
    /// on paths where a failure can only be logged.
    async fn close_session(&self, session_id: &str) -> anyhow::Result<()>;
}

/// Adapt a plugin's runner into the host's [`ExecRunner`].
///
/// The same adapter serves an in-process runner and a cdylib one, because a
/// cdylib's host-side wrapper implements the same trait. One implementation, one
/// set of semantics, whichever side of the seam the runner lives on.
pub struct PluginExecRunner {
    plugin: Arc<dyn ExecRunnerPlugin>,
}

impl PluginExecRunner {
    pub fn new(plugin: Arc<dyn ExecRunnerPlugin>) -> Self {
        Self { plugin }
    }
}

#[async_trait::async_trait]
impl ExecRunner for PluginExecRunner {
    async fn open(
        &self,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let opened = self
            .plugin
            .open_session(req.clone(), ctoken)
            .await
            .map_err(|e| e.context(format!("opening exec session for {}", req.runner_addr)))?;

        Ok(Arc::new(PluginSession {
            plugin: Arc::clone(&self.plugin),
            session_id: opened.session_id,
            caps: opened.caps,
            description: opened.description,
            base_env: opened.base_env,
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
}

/// A session whose every `prepare` crosses back into the plugin.
struct PluginSession {
    plugin: Arc<dyn ExecRunnerPlugin>,
    session_id: String,
    caps: SessionCaps,
    description: SessionDescription,
    base_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// So `close` and `teardown` cannot both act. Teardown is reachable from two
    /// paths by design (orderly and abort) and the plugin must not be told twice.
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ExecSession for PluginSession {
    async fn prepare(&self, spec: Spec) -> Result<Spec, SpawnError> {
        self.plugin
            .prepare_spec(&self.session_id, spec)
            .await
            // No `ProgramNotFound` to classify: the program has not been looked
            // up yet, and only the runner knows why it refused.
            .map_err(|e| SpawnError::SessionDied {
                key: self.description.key.clone(),
                reason: format!("{e:#}"),
            })
    }

    fn base_env(&self) -> Option<&[(std::ffi::OsString, std::ffi::OsString)]> {
        self.base_env.as_deref()
    }

    fn caps(&self) -> &SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &SessionDescription {
        &self.description
    }

    async fn close(&self) -> anyhow::Result<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        self.plugin.close_session(&self.session_id).await
    }

    fn teardown(&self) -> Option<TeardownJob> {
        // Nothing synchronous to hand back: closing this session means talking
        // to the plugin, which is async. The orderly path calls `close`.
        //
        // On hard abort nothing async runs, and that is covered elsewhere: a
        // process the plugin spawned was registered with the HOST's supervisor
        // (`heph_plugin_set_supervisor`), which kills the group on exit. So the
        // shell or container does not outlive heph even when `close` never
        // runs — it is reaped rather than asked to leave.
        None
    }
}
