//! Guest-side serving: drive an author's [`Provider`] from inbound frames, and
//! expose the host callbacks (`result`/`query`) as a [`ProviderExecutor`] so the
//! author's code calls back into the host exactly as in-process.

use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hartifactcontent::{Content, WalkEntry};
use hcore::hasync::StdCancellationToken;
use hplugin::eresult::{ArtifactMeta, EResult};
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunInput, ManagedRunRequest};
use hplugin::driver::{
    inputartifact, ApplyTransitiveRequest, ConfigRequest as DriverConfigRequest, Driver,
    ParseRequest, RunInput, RunRequest,
};
use std::path::PathBuf;
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
    serve_plugin(Some(provider), None, read, write)
}

/// Serve a driver-only plugin.
pub fn serve_driver<R, W>(driver: Arc<dyn Driver>, read: R, write: W) -> Arc<Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    serve_plugin(None, Some(driver), read, write)
}

/// Serve a single managed-driver plugin (target execution at the driver-support
/// `ManagedDriver` layer; the host materializes the sandbox).
pub fn serve_managed_driver<R, W>(managed: Arc<dyn ManagedDriver>, read: R, write: W) -> Arc<Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    serve_components(None, HashMap::from([(String::new(), managed)]), read, write)
}

/// Serve a plugin that may export a provider, a driver, or both (single driver).
pub fn serve_plugin<R, W>(
    provider: Option<Arc<dyn Provider>>,
    driver: Option<Arc<dyn Driver>>,
    read: R,
    write: W,
) -> Arc<Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(GuestHandler {
        provider,
        driver,
        managed: HashMap::new(),
        tokens: Mutex::new(HashMap::new()),
    });
    Mux::start(read, write, handler)
}

/// Serve a plugin that exposes a provider and/or several named managed drivers
/// over one connection (the plugin-go shape: provider + golist/embed/testmain).
/// Driver-targeted requests carry the driver name to route here.
pub fn serve_components<R, W>(
    provider: Option<Arc<dyn Provider>>,
    managed: HashMap<String, Arc<dyn ManagedDriver>>,
    read: R,
    write: W,
) -> Arc<Mux>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(GuestHandler {
        provider,
        driver: None,
        managed,
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
    provider: Option<Arc<dyn Provider>>,
    driver: Option<Arc<dyn Driver>>,
    managed: HashMap<String, Arc<dyn ManagedDriver>>,
    // frame-id -> cancellation token for the in-flight method call
    tokens: Mutex<HashMap<u64, Arc<StdCancellationToken>>>,
}

impl GuestHandler {
    /// Pick a managed driver by selector; fall back to the sole one when the
    /// selector is empty or there is exactly one registered.
    fn pick_managed(&self, sel: &str) -> Option<Arc<dyn ManagedDriver>> {
        if let Some(d) = self.managed.get(sel) {
            return Some(Arc::clone(d));
        }
        if self.managed.len() == 1 {
            return self.managed.values().next().cloned();
        }
        None
    }
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
            Body::ConfigReq(_) => {
                let name = if let Some(p) = &self.provider {
                    p.config(ConfigRequest {}).map(|r| r.name)
                } else if let Some(d) = &self.driver {
                    d.config(DriverConfigRequest {}).map(|r| r.name)
                } else if let Some(m) = self.managed.values().next() {
                    m.config(DriverConfigRequest {}).map(|r| r.name)
                } else {
                    Err(anyhow::anyhow!("plugin exports neither provider nor driver"))
                };
                match name {
                    Ok(name) => mux.send_body(id, Body::ConfigResp(pb::ConfigResponse { name })),
                    Err(e) => mux.send_body(id, err_frame(e.to_string())),
                }
            }
            Body::ListReq(req) => {
                let Some(provider) = self.provider.clone() else {
                    mux.send_body(id, stream_err("plugin has no provider".to_string()));
                    return;
                };
                let tok = self.new_token(id);
                let lreq = ListRequest {
                    request_id: req.request_id,
                    package: PkgBuf::from(req.package),
                    states: req.states.into_iter().map(convert::state_from_pb).collect(),
                };
                let res = provider.list(lreq, &*tok).await;
                self.stream_addrs(id, res, &mux);
                self.drop_token(id);
            }
            Body::ListPackagesReq(req) => {
                let Some(provider) = self.provider.clone() else {
                    mux.send_body(id, stream_err("plugin has no provider".to_string()));
                    return;
                };
                let tok = self.new_token(id);
                let lreq = ListPackagesRequest {
                    prefix: PkgBuf::from(req.prefix),
                };
                let res = provider.list_packages(lreq, &*tok).await;
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
                let Some(provider) = self.provider.clone() else {
                    mux.send_body(id, err_frame("plugin has no provider".to_string()));
                    return;
                };
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
                match provider.get(greq, &*tok).await {
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
                let Some(provider) = self.provider.clone() else {
                    mux.send_body(id, err_frame("plugin has no provider".to_string()));
                    return;
                };
                let tok = self.new_token(id);
                let preq = ProbeRequest {
                    request_id: req.request_id,
                    package: PkgBuf::from(req.package),
                };
                match provider.probe(preq, &*tok).await {
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
            Body::ParseReq(req) => {
                let tok = self.new_token(id);
                let sel = req.driver.clone();
                let preq = ParseRequest {
                    request_id: req.request_id,
                    target_spec: Arc::new(convert::target_spec_from_pb(
                        req.target_spec.unwrap_or_default(),
                    )),
                };
                let result = if let Some(d) = self.driver.clone() {
                    d.parse(preq, &*tok).await
                } else if let Some(m) = self.pick_managed(&sel) {
                    m.parse(preq, &*tok).await
                } else {
                    Err(anyhow::anyhow!("plugin has no driver"))
                };
                match result {
                    Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
                        Ok(td) => mux.send_body(
                            id,
                            Body::ParseResp(pb::ParseResponse {
                                target_def: Some(td),
                            }),
                        ),
                        Err(e) => mux.send_body(id, err_frame(e.to_string())),
                    },
                    Err(e) => mux.send_body(id, err_frame(e.to_string())),
                }
                self.drop_token(id);
            }
            Body::ApplyTransitiveReq(req) => {
                let tok = self.new_token(id);
                let target_def = match convert::target_def_from_pb(req.target_def.unwrap_or_default())
                {
                    Ok(td) => td,
                    Err(e) => {
                        mux.send_body(id, err_frame(e.to_string()));
                        self.drop_token(id);
                        return;
                    }
                };
                let sel = req.driver.clone();
                let areq = ApplyTransitiveRequest {
                    request_id: req.request_id,
                    target_def,
                    sandbox: convert::sandbox_from_pb(req.sandbox.unwrap_or_default()),
                };
                let result = if let Some(d) = self.driver.clone() {
                    d.apply_transitive(areq, &*tok).await
                } else if let Some(m) = self.pick_managed(&sel) {
                    m.apply_transitive(areq, &*tok).await
                } else {
                    Err(anyhow::anyhow!("plugin has no driver"))
                };
                match result {
                    Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
                        Ok(td) => mux.send_body(
                            id,
                            Body::ApplyTransitiveResp(pb::ApplyTransitiveResponse {
                                target_def: Some(td),
                            }),
                        ),
                        Err(e) => mux.send_body(id, err_frame(e.to_string())),
                    },
                    Err(e) => mux.send_body(id, err_frame(e.to_string())),
                }
                self.drop_token(id);
            }
            Body::ManagedRunReq(req) => {
                let Some(managed) = self.pick_managed(&req.driver) else {
                    mux.send_body(id, err_frame("plugin has no managed driver".to_string()));
                    return;
                };
                let tok = self.new_token(id);
                self.handle_managed_run(id, req, managed, &*tok, &mux).await;
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

fn run_input_from_pb(mi: &pb::ManagedRunInput) -> RunInput {
    let ty = match pb::InputArtifactType::try_from(mi.r#type).unwrap_or(pb::InputArtifactType::Dep) {
        pb::InputArtifactType::Support => inputartifact::Type::Support,
        _ => inputartifact::Type::Dep,
    };
    RunInput {
        artifact: inputartifact::InputArtifact {
            r#type: ty,
            origin_id: mi.origin_id.clone(),
            // Input bytes are on the shared filesystem (unpack_root); the guest
            // reads from disk, never from this Content.
            content: Arc::new(NullContent),
        },
        origin_id: mi.origin_id.clone(),
        source_addr: convert::addr_from_pb(mi.source_addr.clone().unwrap_or_default()),
        filters: mi.filters.clone(),
        annotations: mi
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn managed_input_from_pb(mi: pb::ManagedRunInput) -> ManagedRunInput {
    let input = run_input_from_pb(&mi);
    ManagedRunInput {
        input,
        list_path: mi.list_path.map(PathBuf::from),
        unpack_root: PathBuf::from(mi.unpack_root),
    }
}

/// Placeholder Content for materialized run inputs — the bytes live on the
/// shared filesystem (unpack_root), so this is never read.
struct NullContent;

impl Content for NullContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        anyhow::bail!("managed run input content is on disk (unpack_root), not streamed")
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        anyhow::bail!("managed run input content is on disk (unpack_root), not streamed")
    }
    fn hashout(&self) -> Result<String> {
        Ok(String::new())
    }
}

impl GuestHandler {
    async fn handle_managed_run(
        &self,
        id: u64,
        req: pb::ManagedRunRequest,
        managed: Arc<dyn ManagedDriver>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        mux: &Arc<Mux>,
    ) {
        let target = match convert::target_def_from_pb(req.target.unwrap_or_default()) {
            Ok(t) => t,
            Err(e) => {
                mux.send_body(id, err_frame(e.to_string()));
                return;
            }
        };
        let request_id = req.request_id;
        let hashin = req.hashin;
        let sandbox_dir = PathBuf::from(req.sandbox_dir);
        let run_inputs: Vec<RunInput> = req.inputs.iter().map(run_input_from_pb).collect();
        let managed_inputs: Vec<ManagedRunInput> =
            req.inputs.into_iter().map(managed_input_from_pb).collect();
        let rr = RunRequest {
            request_id: &request_id,
            target: &target,
            tree_root_path: PathBuf::from(req.tree_root_path),
            inputs: run_inputs,
            hashin: hashin.as_str(),
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: sandbox_dir.clone(),
        };
        let mrr = ManagedRunRequest {
            request: rr,
            sandbox_dir,
            sandbox_ws_dir: PathBuf::from(req.sandbox_ws_dir),
            sandbox_pkg_dir: PathBuf::from(req.sandbox_pkg_dir),
            inputs: managed_inputs,
        };
        let result = if req.shell {
            managed.run_shell(mrr, ctoken).await
        } else {
            managed.run(mrr, ctoken).await
        };
        match result {
            Ok(resp) => mux.send_body(
                id,
                Body::ManagedRunResp(pb::ManagedRunResponse {
                    artifacts: resp.artifacts.iter().map(convert::output_artifact_to_pb).collect(),
                }),
            ),
            Err(e) => mux.send_body(id, err_frame(e.to_string())),
        }
    }

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

impl MuxExecutor {
    /// Pull all bytes of one artifact via `open_artifact` (streamed as chunks).
    async fn fetch_artifact(&self, lease_id: &str, handle_id: &str) -> Result<Vec<u8>> {
        let mut rx = self.mux.call_stream(Body::OpenArtifactReq(pb::OpenArtifactRequest {
            lease_id: lease_id.to_string(),
            handle_id: handle_id.to_string(),
            offset: 0,
        }));
        let mut bytes = Vec::new();
        while let Some(b) = rx.recv().await {
            match b {
                Body::StreamItem(si) => bytes.extend_from_slice(&si.item),
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
        Ok(bytes)
    }
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
                    // Eagerly pull each artifact's bytes over the mux. M2: whole
                    // artifact in one fetch; lazy/offset chunking is M3. Most
                    // plugins read a tiny file (e.g. package.bin) so this is
                    // cheap in practice.
                    let mut artifacts: Vec<Arc<dyn Content>> = Vec::with_capacity(rr.artifacts.len());
                    let mut artifacts_meta = Vec::with_capacity(rr.artifacts.len());
                    for h in &rr.artifacts {
                        let bytes = self.fetch_artifact(&rr.lease_id, &h.handle_id).await?;
                        artifacts.push(Arc::new(RemoteContent {
                            bytes,
                            hashout: h.hashout.clone(),
                        }) as Arc<dyn Content>);
                        artifacts_meta.push(ArtifactMeta {
                            hashout: h.hashout.clone(),
                        });
                    }
                    // Bytes are now owned locally; release the host-side guards.
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

/// A host artifact materialized on the guest side: the bytes are fetched eagerly
/// over the mux, then read/walked locally. Artifacts are tar (the only content
/// type the cache produces today), so `walk` uses the tar walker.
struct RemoteContent {
    bytes: Vec<u8>,
    hashout: String,
}

impl Content for RemoteContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
    }
    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        Ok(Box::new(hcore::hartifactcontent::tar::TarWalker::new(
            std::io::Cursor::new(self.bytes.clone()),
        )?))
    }
    fn hashout(&self) -> Result<String> {
        Ok(self.hashout.clone())
    }
    fn byte_size(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }
}
