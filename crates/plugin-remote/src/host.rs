//! Host-side callback service: serves the `AbiHost` surface (result / note_dep
//! / query / open_artifact / release_lease) that a remote plugin calls back
//! into while serving `get`/`parse`. Each call is dispatched against the
//! per-request scope registered by the host-side Provider/Driver for the
//! duration of that request.

use crate::lease::LeaseTable;
use async_trait::async_trait;
use hcore::hartifactcontent::Content;
use hplugin::provider::ProviderExecutor;
use plugin_abi::convert;
use plugin_abi::mux::{Body, InboundHandler, Mux};
use plugin_abi::pb;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

/// The per-request callback target: the engine executor for that request.
pub(crate) struct Scope {
    pub executor: Arc<dyn ProviderExecutor>,
}

#[derive(Default)]
pub(crate) struct HostInner {
    scopes: Mutex<HashMap<String, Arc<Scope>>>,
    /// Mints unique callback-scope ids. The engine's request_id is shared across
    /// a whole request tree, so concurrent provider `get`s within one request
    /// (e.g. plugingo's import-closure fan-out) would collide on it — one get's
    /// unregister would tear down a sibling's still-live scope, and they carry
    /// different executors (different cycle-detection parent) anyway. So each
    /// call gets its own scope id, which is what travels on the wire as the
    /// request_id and comes back on every callback.
    scope_seq: std::sync::atomic::AtomicU64,
    pub leases: LeaseTable,
}

impl HostInner {
    /// A fresh, process-unique scope id for one host→guest call.
    pub fn fresh_scope_id(&self) -> String {
        let n = self
            .scope_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("scope-{n}")
    }

    pub fn register(&self, scope_id: String, executor: Arc<dyn ProviderExecutor>) {
        self.scopes
            .lock()
            .expect("scopes")
            .insert(scope_id, Arc::new(Scope { executor }));
    }

    pub fn unregister(&self, scope_id: &str) {
        self.scopes.lock().expect("scopes").remove(scope_id);
    }

    fn scope(&self, scope_id: &str) -> Option<Arc<Scope>> {
        self.scopes.lock().expect("scopes").get(scope_id).cloned()
    }
}

pub(crate) struct HostCallbackHandler {
    pub inner: Arc<HostInner>,
}

fn err_frame(message: String) -> Body {
    Body::Error(pb::Error {
        kind: pb::error::Kind::Other as i32,
        message,
    })
}

/// True if `e`'s chain contains a dependency-cycle error.
fn is_cycle(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<hplugin::error::CycleError>(e).is_some()
}

/// Build an Error frame whose kind reflects the real error type (downcast, never
/// message-matched), so the guest can reconstruct a typed error.
fn err_frame_from(e: &anyhow::Error) -> Body {
    let kind = if is_cycle(e) {
        pb::error::Kind::Cycle
    } else if hplugin::error::is_cancelled(e) {
        pb::error::Kind::Cancelled
    } else {
        pb::error::Kind::Other
    };
    Body::Error(pb::Error {
        kind: kind as i32,
        message: e.to_string(),
    })
}

#[async_trait]
impl InboundHandler for HostCallbackHandler {
    async fn handle(&self, id: u64, body: Body, mux: Arc<Mux>) {
        match body {
            Body::ResultReq(req) => self.handle_result(id, req, &mux).await,
            Body::NoteDepReq(req) => self.handle_note_dep(id, req, &mux).await,
            Body::QueryReq(req) => self.handle_query(id, req, &mux).await,
            Body::OpenArtifactReq(req) => self.handle_open_artifact(id, req, &mux).await,
            Body::ReleaseLeaseReq(req) => {
                self.inner.leases.release(&req.lease_id);
                mux.send_body(id, Body::ReleaseLeaseResp(pb::ReleaseLeaseResponse {}));
            }
            Body::Cancel(_) => { /* host-as-server: cooperative cancel is M2 */ }
            other => {
                mux.send_body(
                    id,
                    err_frame(format!("unhandled inbound callback: {other:?}")),
                );
            }
        }
    }
}

impl HostCallbackHandler {
    async fn handle_result(&self, id: u64, req: pb::ResultRequest, mux: &Arc<Mux>) {
        let Some(scope) = self.inner.scope(&req.request_id) else {
            mux.send_body(
                id,
                err_frame(format!("unknown request scope {}", req.request_id)),
            );
            return;
        };
        let addr = convert::addr_from_pb(req.addr.unwrap_or_default());
        match scope.executor.result(&addr).await {
            Ok(eres) => {
                let artifacts: Vec<Arc<dyn Content>> = eres.artifacts.clone();
                let mut handles = Vec::with_capacity(artifacts.len());
                for (idx, art) in artifacts.iter().enumerate() {
                    let hashout = eres
                        .artifacts_meta
                        .get(idx)
                        .map(|m| m.hashout.clone())
                        .or_else(|| art.hashout().ok())
                        .unwrap_or_default();
                    handles.push(pb::ArtifactHandle {
                        handle_id: idx.to_string(),
                        group: String::new(),
                        name: String::new(),
                        hashout,
                        byte_size: art.byte_size().unwrap_or(0),
                        support: false,
                    });
                }
                let lease_id = self.inner.leases.insert(artifacts);
                mux.send_body(
                    id,
                    Body::ResultResp(pb::ResultResponse {
                        lease_id,
                        artifacts: handles,
                    }),
                );
            }
            Err(e) => mux.send_body(id, err_frame_from(&e)),
        }
    }

    async fn handle_note_dep(&self, id: u64, req: pb::NoteDepRequest, mux: &Arc<Mux>) {
        let Some(scope) = self.inner.scope(&req.request_id) else {
            mux.send_body(
                id,
                err_frame(format!("unknown request scope {}", req.request_id)),
            );
            return;
        };
        let addr = convert::addr_from_pb(req.addr.unwrap_or_default());
        // Edge-only registration (cheap); cycle is detected by type, not message.
        let resp = match scope.executor.note_dep(&addr).await {
            Ok(()) => pb::NoteDepResponse {
                ok: true,
                cycle: false,
                message: String::new(),
            },
            Err(e) => pb::NoteDepResponse {
                ok: false,
                cycle: is_cycle(&e),
                message: e.to_string(),
            },
        };
        mux.send_body(id, Body::NoteDepResp(resp));
    }

    async fn handle_query(&self, id: u64, req: pb::QueryRequest, mux: &Arc<Mux>) {
        let Some(scope) = self.inner.scope(&req.request_id) else {
            mux.send_body(
                id,
                err_frame(format!("unknown request scope {}", req.request_id)),
            );
            return;
        };
        let matcher = convert::matcher_from_pb(req.matcher.unwrap_or_default());
        match scope.executor.query(&matcher, &req.extra_skip).await {
            Ok(addrs) => mux.send_body(
                id,
                Body::QueryResp(pb::QueryResponse {
                    addrs: addrs.iter().map(convert::addr_to_pb).collect(),
                }),
            ),
            Err(e) => mux.send_body(id, err_frame_from(&e)),
        }
    }

    async fn handle_open_artifact(&self, id: u64, req: pb::OpenArtifactRequest, mux: &Arc<Mux>) {
        let idx: usize = req.handle_id.parse().unwrap_or(usize::MAX);
        let Some(content) = self.inner.leases.get(&req.lease_id, idx) else {
            mux.send_body(
                id,
                err_frame(format!(
                    "unknown artifact {}#{}",
                    req.lease_id, req.handle_id
                )),
            );
            return;
        };
        // Content::reader is sync; read on a blocking thread. M1 reads the whole
        // artifact into one chunk; offset/chunked streaming is M3.
        let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let mut r = content.reader()?;
            let mut buf = Vec::new();
            r.read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await;
        match bytes {
            Ok(Ok(buf)) => {
                mux.send_body(id, Body::StreamItem(pb::StreamItem { item: buf.into() }));
                mux.send_body(id, Body::StreamEnd(pb::StreamEnd { error: None }));
            }
            Ok(Err(e)) => mux.send_body(id, err_frame_from(&e)),
            Err(e) => mux.send_body(id, err_frame(format!("artifact read task failed: {e}"))),
        }
    }
}
