//! `RemoteDriver`: implements [`hplugin::driver::Driver`] by forwarding to an
//! out-of-process plugin over a [`Mux`].
//!
//! `parse`/`apply_transitive` (the target-def path) are wired; they round-trip
//! `TargetDef` including the opaque `raw_def` via the serialization contract
//! (`RawDefBytes` + `def_de`). `run`/`run_shell` are deferred: they need the
//! `driver-support` `ManagedDriver` sandbox-materialization integration (inputs
//! written into the shared sandbox dir, outputs collected as paths) — the next
//! slice.

use crate::host::{HostCallbackHandler, HostInner};
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, Driver,
    DriverSchema, ParseRequest, ParseResponse, RunRequest, RunResponse,
};
use plugin_abi::convert;
use plugin_abi::mux::{Body, Mux};
use plugin_abi::pb;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct RemoteDriver {
    name: String,
    mux: Arc<Mux>,
    _inner: Arc<HostInner>,
}

impl RemoteDriver {
    /// Connect over an established duplex byte stream. `name` is the driver name
    /// (from the handshake) returned by `config`.
    pub fn connect<R, W>(read: R, write: W, name: impl Into<String>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inner = Arc::new(HostInner::default());
        let handler = Arc::new(HostCallbackHandler {
            inner: Arc::clone(&inner),
        });
        let mux = Mux::start(read, write, handler);
        Self::from_parts(mux, inner, name.into())
    }

    pub(crate) fn from_parts(mux: Arc<Mux>, inner: Arc<HostInner>, name: String) -> Self {
        Self {
            name,
            mux,
            _inner: inner,
        }
    }
}

#[async_trait]
impl Driver for RemoteDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    fn schema(&self) -> DriverSchema {
        // Driver config schema is for BUILD-file tooling; not surfaced remotely
        // yet. A config-less default is correct for execution paths.
        DriverSchema::default()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let body = Body::ParseReq(pb::ParseRequest {
            request_id: req.request_id,
            target_spec: Some(convert::target_spec_to_pb(req.target_spec.as_ref())),
            driver: self.name.clone(),
        });
        match self.mux.call_cancellable(body, ctoken.cancelled()).await? {
            Body::ParseResp(pr) => Ok(ParseResponse {
                target_def: convert::target_def_from_pb(pr.target_def.unwrap_or_default())?,
            }),
            other => anyhow::bail!("unexpected parse response: {other:?}"),
        }
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        let body = Body::ApplyTransitiveReq(pb::ApplyTransitiveRequest {
            request_id: req.request_id,
            target_def: Some(convert::target_def_to_pb(&req.target_def)?),
            sandbox: Some(convert::sandbox_to_pb(&req.sandbox)),
            driver: self.name.clone(),
        });
        match self.mux.call_cancellable(body, ctoken.cancelled()).await? {
            Body::ApplyTransitiveResp(r) => Ok(ApplyTransitiveResponse {
                target_def: convert::target_def_from_pb(r.target_def.unwrap_or_default())?,
            }),
            other => anyhow::bail!("unexpected apply_transitive response: {other:?}"),
        }
    }

    async fn run<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!(
            "remote driver run() not yet implemented — needs ManagedDriver sandbox materialization"
        )
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!("remote driver run_shell() not yet implemented")
    }
}
