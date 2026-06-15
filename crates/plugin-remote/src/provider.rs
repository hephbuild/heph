//! `RemoteProvider`: implements the in-process [`hplugin::provider::Provider`]
//! trait by forwarding each method to an out-of-process plugin over a [`Mux`],
//! while serving the plugin's callbacks (result/query/…) against the per-request
//! executor. The engine registers it through the normal factory hooks and is
//! otherwise unaware the plugin is remote.

use crate::host::{HostCallbackHandler, HostInner};
use anyhow::Result;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
};
use plugin_abi::convert;
use plugin_abi::mux::{Body, Mux};
use plugin_abi::pb;
use prost::Message;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct RemoteProvider {
    name: String,
    mux: Arc<Mux>,
    inner: Arc<HostInner>,
}

impl RemoteProvider {
    /// Connect over an established duplex byte stream (e.g. a UDS socketpair).
    /// `name` is the provider name (from the handshake) returned by `config`.
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
        Self { name, mux, inner }
    }
}

impl Provider for RemoteProvider {
    fn config(&self, _req: ConfigRequest) -> Result<ConfigResponse> {
        // Name is learned at handshake; no round trip needed.
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<Box<dyn Iterator<Item = Result<ListResponse>> + Send>>> {
        Box::pin(async move {
            let body = Body::ListReq(pb::ListRequest {
                request_id: req.request_id,
                package: req.package.as_str().to_string(),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            });
            let mut rx = self.mux.call_stream(body);
            let mut out: Vec<Result<ListResponse>> = Vec::new();
            while let Some(b) = rx.recv().await {
                match b {
                    Body::StreamItem(si) => {
                        let lr = pb::ListResponse::decode(&si.item[..])?;
                        out.push(Ok(ListResponse {
                            addr: convert::addr_from_pb(lr.addr.unwrap_or_default()),
                        }));
                    }
                    Body::StreamEnd(se) => {
                        if let Some(e) = se.error {
                            anyhow::bail!("{}", e.message);
                        }
                        break;
                    }
                    Body::Error(e) => anyhow::bail!("{}", e.message),
                    _ => {}
                }
            }
            Ok(Box::new(out.into_iter()) as Box<dyn Iterator<Item = Result<ListResponse>> + Send>)
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>>> {
        Box::pin(async move {
            let body = Body::ListPackagesReq(pb::ListPackagesRequest {
                prefix: req.prefix.as_str().to_string(),
            });
            let mut rx = self.mux.call_stream(body);
            let mut out: Vec<Result<ListPackageResponse>> = Vec::new();
            while let Some(b) = rx.recv().await {
                match b {
                    Body::StreamItem(si) => {
                        let lpr = pb::ListPackageResponse::decode(&si.item[..])?;
                        out.push(Ok(ListPackageResponse {
                            pkg: PkgBuf::from(lpr.pkg),
                        }));
                    }
                    Body::StreamEnd(se) => {
                        if let Some(e) = se.error {
                            anyhow::bail!("{}", e.message);
                        }
                        break;
                    }
                    Body::Error(e) => anyhow::bail!("{}", e.message),
                    _ => {}
                }
            }
            Ok(Box::new(out.into_iter())
                as Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, std::result::Result<GetResponse, GetError>> {
        Box::pin(async move {
            let request_id = req.request_id.clone();
            // Register the executor so the plugin's result()/query() callbacks
            // for this request route back to it.
            self.inner
                .register(request_id.clone(), Arc::clone(&req.executor));
            let body = Body::GetReq(pb::GetRequest {
                request_id: req.request_id,
                addr: Some(convert::addr_to_pb(&req.addr)),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            });
            let res = self.mux.call_cancellable(body, ctoken.cancelled()).await;
            self.inner.unregister(&request_id);
            match res {
                Ok(Body::GetResp(gr)) => Ok(GetResponse {
                    target_spec: convert::target_spec_from_pb(gr.target_spec.unwrap_or_default()),
                }),
                Ok(Body::GetErr(ge)) => {
                    // Reconstruct the typed error from the kind (never the message).
                    match pb::get_error::Kind::try_from(ge.kind)
                        .unwrap_or(pb::get_error::Kind::Other)
                    {
                        pb::get_error::Kind::NotFound => Err(GetError::NotFound),
                        pb::get_error::Kind::Cycle => Err(GetError::Other(anyhow::Error::new(
                            hplugin::error::CycleError {
                                from: req.addr.clone(),
                                to: req.addr.clone(),
                            },
                        ))),
                        pb::get_error::Kind::Cancelled => {
                            Err(GetError::Other(anyhow::Error::new(hplugin::error::CancelledError)))
                        }
                        _ => Err(GetError::Other(anyhow::anyhow!("{}", ge.message))),
                    }
                }
                Ok(other) => Err(GetError::Other(anyhow::anyhow!(
                    "unexpected get response: {other:?}"
                ))),
                Err(e) => Err(GetError::Other(e)),
            }
        })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<ProbeResponse>> {
        Box::pin(async move {
            let body = Body::ProbeReq(pb::ProbeRequest {
                request_id: req.request_id,
                package: req.package.as_str().to_string(),
            });
            match self.mux.call_cancellable(body, ctoken.cancelled()).await? {
                Body::ProbeResp(pr) => Ok(ProbeResponse {
                    states: pr.states.into_iter().map(convert::state_from_pb).collect(),
                }),
                other => anyhow::bail!("unexpected probe response: {other:?}"),
            }
        })
    }
}
