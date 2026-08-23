//! Turn a [`ManagedDriver`] that serves exec sessions into an [`ExecRunner`].
//!
//! This is the whole host side of the exec-runner lane, and it is deliberately
//! generic over `ManagedDriver` rather than over the plugin ABI: the same
//! adapter serves an in-process driver and a cdylib one, because a cdylib's
//! host-side wrapper *is* a `ManagedDriver`. One implementation, one set of
//! semantics, whichever side of the seam the driver lives on.
//!
//! What the runner gets out of it is ownership of process creation. `prepare`
//! is per target, so a runner can hold one shell open and route every target's
//! exec through it — deciding per spawn, not only per environment.

use std::sync::Arc;

use hcore::hasync::Cancellable;
use hexec_runner::{
    ExecRunner, ExecSession, OpenRequest, SessionCaps, SessionDescription, SpawnError, TeardownJob,
};
use hproc::proc_exec::Spec;

use crate::driver_managed::ManagedDriver;

/// An [`ExecRunner`] backed by a driver.
pub struct DriverExecRunner {
    driver: Arc<dyn ManagedDriver>,
}

impl DriverExecRunner {
    pub fn new(driver: Arc<dyn ManagedDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait::async_trait]
impl ExecRunner for DriverExecRunner {
    async fn open(
        &self,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        if !self.driver.serves_exec_sessions() {
            // Refuse rather than degrade. A driver that cannot serve the
            // environment must not leave the target running in the host's,
            // because the target's key already asserts the runner's.
            anyhow::bail!(
                "the driver of runner target {} does not serve exec sessions. It may be an older \
                 plugin built before the exec-runner lane existed — rebuild or update it.",
                req.runner_addr,
            );
        }

        let opened = self
            .driver
            .open_session(req.clone(), ctoken)
            .await
            .map_err(|e| e.context(format!("opening exec session for {}", req.runner_addr)))?;

        Ok(Arc::new(DriverSession {
            driver: Arc::clone(&self.driver),
            session_id: opened.session_id,
            caps: opened.caps,
            description: opened.description,
            base_env: opened.base_env,
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
}

/// A session whose every `prepare` crosses back into the driver.
struct DriverSession {
    driver: Arc<dyn ManagedDriver>,
    session_id: String,
    caps: SessionCaps,
    description: SessionDescription,
    base_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// So `close` and `teardown` cannot both act. Teardown is reachable from
    /// two paths by design (orderly and abort) and the driver must not be told
    /// twice.
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ExecSession for DriverSession {
    async fn prepare(&self, spec: Spec) -> Result<Spec, SpawnError> {
        self.driver
            .prepare_spec(&self.session_id, spec)
            .await
            // The driver is the only party that knows why; there is no
            // `ProgramNotFound` to classify here, because the program has not
            // been looked up yet.
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
        self.driver.close_session(&self.session_id).await
    }

    fn teardown(&self) -> Option<TeardownJob> {
        // Nothing synchronous to hand back: closing this session means talking
        // to the driver, which is async. The orderly path calls `close`.
        //
        // On hard abort nothing async runs, and that is covered elsewhere: a
        // process the plugin spawned was registered with the HOST's supervisor
        // (`heph_plugin_set_supervisor`), which kills the group on exit. So the
        // shell or container does not outlive heph even when `close` never
        // runs — it is just reaped rather than asked to leave.
        None
    }
}
