//! `RemoteManagedDriver`: the host side of remote target execution.
//!
//! It implements driver-support's `ManagedDriver`, so the host's
//! `ManagedDriverBridge`/`ManagedDriverFuse` performs all sandbox setup +
//! input materialization (as for in-process managed drivers) and then calls
//! `run` here. We forward the already-materialized run — only paths + metadata,
//! never input bytes (the guest shares the filesystem) — to the out-of-process
//! plugin and return its output artifacts.
//!
//! Stdio is not yet proxied (build drivers like go's don't use the run streams);
//! streaming the target's stdin/out/err is a follow-up.

use crate::host::{HostCallbackHandler, HostInner};
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ManagedRunRequest, ManagedRunResponse,
};
use hplugin::driver::inputartifact;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, DriverSchema,
    ParseRequest, ParseResponse,
};
use plugin_abi::convert;
use plugin_abi::mux::{Body, Mux};
use plugin_abi::pb;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct RemoteManagedDriver {
    name: String,
    mux: Arc<Mux>,
    _inner: Arc<HostInner>,
}

impl RemoteManagedDriver {
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
        Self {
            name: name.into(),
            mux,
            _inner: inner,
        }
    }
}

fn managed_input_to_pb(mi: &ManagedRunInput) -> pb::ManagedRunInput {
    let ty = match mi.input.artifact.r#type {
        inputartifact::Type::Dep => pb::InputArtifactType::Dep,
        inputartifact::Type::Support => pb::InputArtifactType::Support,
    };
    pb::ManagedRunInput {
        r#type: ty as i32,
        origin_id: mi.input.origin_id.clone(),
        source_addr: Some(convert::addr_to_pb(&mi.input.source_addr)),
        filters: mi.input.filters.clone(),
        annotations: mi
            .input
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        unpack_root: mi.unpack_root.to_string_lossy().into_owned(),
        list_path: mi
            .list_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    }
}

#[async_trait]
impl ManagedDriver for RemoteManagedDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    fn schema(&self) -> DriverSchema {
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
        });
        match self.mux.call_cancellable(body, ctoken.cancelled()).await? {
            Body::ApplyTransitiveResp(r) => Ok(ApplyTransitiveResponse {
                target_def: convert::target_def_from_pb(r.target_def.unwrap_or_default())?,
            }),
            other => anyhow::bail!("unexpected apply_transitive response: {other:?}"),
        }
    }

    fn supports_shell(&self) -> bool {
        true
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, ctoken, false).await
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, ctoken, true).await
    }
}

impl RemoteManagedDriver {
    async fn dispatch_run(
        &self,
        req: ManagedRunRequest<'_, '_>,
        ctoken: &(dyn Cancellable + Send + Sync),
        shell: bool,
    ) -> anyhow::Result<ManagedRunResponse> {
        let body = Body::ManagedRunReq(pb::ManagedRunRequest {
            request_id: req.request.request_id.clone(),
            target: Some(convert::target_def_to_pb(req.request.target)?),
            tree_root_path: req.request.tree_root_path.to_string_lossy().into_owned(),
            hashin: req.request.hashin.to_string(),
            sandbox_dir: req.sandbox_dir.to_string_lossy().into_owned(),
            sandbox_ws_dir: req.sandbox_ws_dir.to_string_lossy().into_owned(),
            sandbox_pkg_dir: req.sandbox_pkg_dir.to_string_lossy().into_owned(),
            inputs: req.inputs.iter().map(managed_input_to_pb).collect(),
            shell,
        });
        match self.mux.call_cancellable(body, ctoken.cancelled()).await? {
            Body::ManagedRunResp(r) => Ok(ManagedRunResponse {
                artifacts: r
                    .artifacts
                    .into_iter()
                    .map(convert::output_artifact_from_pb)
                    .collect(),
            }),
            other => anyhow::bail!("unexpected managed run response: {other:?}"),
        }
    }
}
