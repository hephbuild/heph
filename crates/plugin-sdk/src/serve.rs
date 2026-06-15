//! Guest-side serving: drive an author's [`Provider`] from inbound frames, and
//! expose the host callbacks (`result`/`query`) as a [`ProviderExecutor`] so the
//! author's code calls back into the host exactly as in-process.

use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
use hcore::hasync::StdCancellationToken;
use hplugin::eresult::{ArtifactMeta, EResult};
use hplugin::provider::{
    ConfigRequest, GetError, GetRequest, ListPackagesRequest, ListRequest, ProbeRequest, Provider,
    ProviderExecutor,
};
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use plugin_abi::convert;
use plugin_abi::mux::{Body, InboundHandler, Mux};
use plugin_abi::pb;
use prost::Message;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// Serve `provider` over an established duplex byte stream. Returns the mux
/// handle; keep it alive for the lifetime of the connection (the read/write
/// tasks run in the background and end on EOF).
pub fn serve<R, W>(provider: Arc<dyn Provider>, read: R, write: W) -> Arc<Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(GuestHandler {
        provider,
        tokens: Mutex::new(HashMap::new()),
    });
    Mux::start(read, write, handler)
}

/// Guest entry point: serve `provider` over the inherited fd 3 (set up by the
/// host's `spawn_plugin`) and block until the host disconnects. A plugin's
/// `main` typically does just `serve_inherited(Arc::new(MyProvider)).await`.
#[cfg(unix)]
pub async fn serve_inherited(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: fd 3 is the protocol socket the host passed us at spawn; we own it.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(std_stream)?;
    let (r, w) = stream.into_split();
    let mux = serve(provider, r, w);
    mux.wait_closed().await;
    Ok(())
}

struct GuestHandler {
    provider: Arc<dyn Provider>,
    // frame-id -> cancellation token for the in-flight method call
    tokens: Mutex<HashMap<u64, Arc<StdCancellationToken>>>,
}

impl GuestHandler {
    fn new_token(&self, id: u64) -> Arc<StdCancellationToken> {
        let tok = Arc::new(StdCancellationToken::new());
        self.tokens.lock().expect("tokens").insert(id, Arc::clone(&tok));
        tok
    }
    fn drop_token(&self, id: u64) {
        self.tokens.lock().expect("tokens").remove(&id);
    }
}

fn err_frame(message: String) -> Body {
    Body::Error(pb::Error {
        kind: pb::error::Kind::Other as i32,
        message,
    })
}

#[async_trait]
impl InboundHandler for GuestHandler {
    async fn handle(&self, id: u64, body: Body, mux: Arc<Mux>) {
        match body {
            Body::ConfigReq(_) => match self.provider.config(ConfigRequest {}) {
                Ok(resp) => mux.send_body(id, Body::ConfigResp(pb::ConfigResponse { name: resp.name })),
                Err(e) => mux.send_body(id, err_frame(e.to_string())),
            },
            Body::ListReq(req) => {
                let tok = self.new_token(id);
                let lreq = ListRequest {
                    request_id: req.request_id,
                    package: PkgBuf::from(req.package),
                    states: req.states.into_iter().map(convert::state_from_pb).collect(),
                };
                let res = self.provider.list(lreq, &*tok).await;
                self.stream_addrs(id, res, &mux);
                self.drop_token(id);
            }
            Body::ListPackagesReq(req) => {
                let tok = self.new_token(id);
                let lreq = ListPackagesRequest {
                    prefix: PkgBuf::from(req.prefix),
                };
                let res = self.provider.list_packages(lreq, &*tok).await;
                match res {
                    Ok(iter) => {
                        for item in iter {
                            match item {
                                Ok(lpr) => mux.send_body(
                                    id,
                                    Body::StreamItem(pb::StreamItem {
                                        item: pb::ListPackageResponse {
                                            pkg: lpr.pkg.as_str().to_string(),
                                        }
                                        .encode_to_vec().into(),
                                    }),
                                ),
                                Err(e) => {
                                    mux.send_body(id, stream_err(e.to_string()));
                                    self.drop_token(id);
                                    return;
                                }
                            }
                        }
                        mux.send_body(id, Body::StreamEnd(pb::StreamEnd { error: None }));
                    }
                    Err(e) => mux.send_body(id, stream_err(e.to_string())),
                }
                self.drop_token(id);
            }
            Body::GetReq(req) => {
                let tok = self.new_token(id);
                let executor: Arc<dyn ProviderExecutor> = Arc::new(MuxExecutor {
                    mux: Arc::clone(&mux),
                    request_id: req.request_id.clone(),
                });
                let greq = GetRequest {
                    request_id: req.request_id,
                    addr: convert::addr_from_pb(req.addr.unwrap_or_default()),
                    states: req.states.into_iter().map(convert::state_from_pb).collect(),
                    executor,
                };
                match self.provider.get(greq, &*tok).await {
                    Ok(gr) => mux.send_body(
                        id,
                        Body::GetResp(pb::GetResponse {
                            target_spec: Some(convert::target_spec_to_pb(&gr.target_spec)),
                        }),
                    ),
                    Err(GetError::NotFound) => mux.send_body(
                        id,
                        Body::GetErr(pb::GetError {
                            kind: pb::get_error::Kind::NotFound as i32,
                            message: String::new(),
                        }),
                    ),
                    Err(GetError::Other(e)) => mux.send_body(
                        id,
                        Body::GetErr(pb::GetError {
                            kind: pb::get_error::Kind::Other as i32,
                            message: e.to_string(),
                        }),
                    ),
                }
                self.drop_token(id);
            }
            Body::ProbeReq(req) => {
                let tok = self.new_token(id);
                let preq = ProbeRequest {
                    request_id: req.request_id,
                    package: PkgBuf::from(req.package),
                };
                match self.provider.probe(preq, &*tok).await {
                    Ok(pr) => mux.send_body(
                        id,
                        Body::ProbeResp(pb::ProbeResponse {
                            states: pr.states.iter().map(convert::state_to_pb).collect(),
                        }),
                    ),
                    Err(e) => mux.send_body(id, err_frame(e.to_string())),
                }
                self.drop_token(id);
            }
            Body::Cancel(c) => {
                if let Some(tok) = self.tokens.lock().expect("tokens").get(&c.request_id) {
                    tok.cancel();
                }
            }
            other => mux.send_body(id, err_frame(format!("unhandled inbound request: {other:?}"))),
        }
    }
}

impl GuestHandler {
    fn stream_addrs(
        &self,
        id: u64,
        res: Result<Box<dyn Iterator<Item = Result<hplugin::provider::ListResponse>> + Send>>,
        mux: &Arc<Mux>,
    ) {
        match res {
            Ok(iter) => {
                for item in iter {
                    match item {
                        Ok(lr) => mux.send_body(
                            id,
                            Body::StreamItem(pb::StreamItem {
                                item: pb::ListResponse {
                                    addr: Some(convert::addr_to_pb(&lr.addr)),
                                }
                                .encode_to_vec().into(),
                            }),
                        ),
                        Err(e) => {
                            mux.send_body(id, stream_err(e.to_string()));
                            return;
                        }
                    }
                }
                mux.send_body(id, Body::StreamEnd(pb::StreamEnd { error: None }));
            }
            Err(e) => mux.send_body(id, stream_err(e.to_string())),
        }
    }
}

fn stream_err(message: String) -> Body {
    Body::StreamEnd(pb::StreamEnd {
        error: Some(pb::Error {
            kind: pb::error::Kind::Other as i32,
            message,
        }),
    })
}

/// A `ProviderExecutor` that forwards `result`/`query` to the host over the mux.
struct MuxExecutor {
    mux: Arc<Mux>,
    request_id: String,
}

impl ProviderExecutor for MuxExecutor {
    fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, Result<Arc<EResult>>> {
        Box::pin(async move {
            let body = Body::ResultReq(pb::ResultRequest {
                request_id: self.request_id.clone(),
                addr: Some(convert::addr_to_pb(addr)),
            });
            match self.mux.call(body).await? {
                Body::ResultResp(rr) => {
                    let artifacts: Vec<Arc<dyn Content>> = rr
                        .artifacts
                        .iter()
                        .map(|h| {
                            Arc::new(RemoteContent {
                                hashout: h.hashout.clone(),
                                byte_size: h.byte_size,
                            }) as Arc<dyn Content>
                        })
                        .collect();
                    let artifacts_meta = rr
                        .artifacts
                        .iter()
                        .map(|h| ArtifactMeta {
                            hashout: h.hashout.clone(),
                        })
                        .collect();
                    // M1 does not read artifact bytes, so release the lease now.
                    // Lazy byte streaming + lease-on-drop lands in M2.
                    drop(
                        self.mux
                            .call(Body::ReleaseLeaseReq(pb::ReleaseLeaseRequest {
                                lease_id: rr.lease_id,
                            }))
                            .await,
                    );
                    Ok(Arc::new(EResult {
                        artifacts,
                        support_artifacts: vec![],
                        artifacts_meta,
                    }))
                }
                other => anyhow::bail!("unexpected result response: {other:?}"),
            }
        })
    }

    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Addr>>> {
        Box::pin(async move {
            let body = Body::QueryReq(pb::QueryRequest {
                request_id: self.request_id.clone(),
                matcher: Some(convert::matcher_to_pb(m)),
                extra_skip: extra_skip.to_vec(),
            });
            match self.mux.call(body).await? {
                Body::QueryResp(qr) => {
                    Ok(qr.addrs.into_iter().map(convert::addr_from_pb).collect())
                }
                other => anyhow::bail!("unexpected query response: {other:?}"),
            }
        })
    }
}

/// Host artifact handle on the guest side. M1 exposes only metadata; byte access
/// (`reader`/`walk`) streams over the mux in M2.
struct RemoteContent {
    hashout: String,
    byte_size: u64,
}

impl Content for RemoteContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        anyhow::bail!("remote artifact byte access lands in M2")
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        anyhow::bail!("remote artifact walk lands in M2")
    }
    fn hashout(&self) -> Result<String> {
        Ok(self.hashout.clone())
    }
    fn byte_size(&self) -> Option<u64> {
        Some(self.byte_size)
    }
}
