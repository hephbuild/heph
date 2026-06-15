//! Bidirectional, request-id-multiplexed frame mux over one connection.
//!
//! Symmetric: both ends use the same `Mux`. Each side issues ids for the
//! requests *it* initiates and routes inbound *response* frames to its own
//! pending table; inbound *request* frames are dispatched to the side's
//! [`InboundHandler`]. Because a side only ever matches response-typed frames
//! against ids it issued, the two id spaces never collide.
//!
//! On the host this carries plugin calls outbound + callbacks inbound; on the
//! guest, the reverse.

use crate::frame::{read_frame, write_frame};
use crate::pb;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Notify};

pub use pb::frame::Body;

/// Handles inbound *request* frames (the calls this side serves). The handler
/// sends its reply (and, for streamed methods, stream items + a stream-end)
/// back through `mux` tagged with the same `id`.
#[async_trait]
pub trait InboundHandler: Send + Sync + 'static {
    async fn handle(&self, id: u64, body: Body, mux: Arc<Mux>);
}

enum Pending {
    Unary(oneshot::Sender<Body>),
    Stream(mpsc::UnboundedSender<Body>),
}

/// True for frames that complete (or feed) a request this side issued.
fn is_response(body: &Body) -> bool {
    matches!(
        body,
        Body::Hello(_)
            | Body::Error(_)
            | Body::StreamItem(_)
            | Body::StreamEnd(_)
            | Body::ConfigResp(_)
            | Body::GetResp(_)
            | Body::GetErr(_)
            | Body::ProbeResp(_)
            | Body::ParseResp(_)
            | Body::ApplyTransitiveResp(_)
            | Body::RunResp(_)
            | Body::ManagedRunResp(_)
            | Body::ResultResp(_)
            | Body::NoteDepResp(_)
            | Body::QueryResp(_)
            | Body::WalkResp(_)
            | Body::ConfigGetResp(_)
            | Body::ReleaseLeaseResp(_)
    )
}

fn is_stream_terminal(body: &Body) -> bool {
    matches!(body, Body::StreamEnd(_) | Body::Error(_))
}

pub struct Mux {
    out: mpsc::UnboundedSender<pb::Frame>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    closed: AtomicBool,
    closed_notify: Notify,
}

impl Mux {
    /// Start the read and write tasks over the given half-streams, dispatching
    /// inbound requests to `handler`. Returns a handle for issuing outbound
    /// calls and sending responses.
    pub fn start<R, W>(read: R, write: W, handler: Arc<dyn InboundHandler>) -> Arc<Mux>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<pb::Frame>();
        let mux = Arc::new(Mux {
            out: out_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
        });

        // writer task
        tokio::spawn(writer_loop(write, out_rx));
        // reader task
        tokio::spawn(reader_loop(read, Arc::clone(&mux), handler));

        mux
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// True once the connection has closed (peer EOF or I/O error).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Resolves when the connection closes. A guest's `main` awaits this to
    /// stay alive for the lifetime of the connection and exit on disconnect.
    pub async fn wait_closed(&self) {
        loop {
            if self.is_closed() {
                return;
            }
            let notified = self.closed_notify.notified();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.closed_notify.notify_waiters();
    }

    /// Send a frame with an explicit id (used by handlers to reply).
    pub fn send_body(&self, id: u64, body: Body) {
        drop(self.out.send(pb::Frame {
            id,
            body: Some(body),
        }));
    }

    /// Issue a unary request and await its single response body.
    pub async fn call(&self, body: Body) -> anyhow::Result<Body> {
        self.call_cancellable(body, std::future::pending()).await
    }

    /// Like [`Mux::call`], but races the request against `cancel`. On cancel,
    /// drops the pending waiter and sends a `Cancel` frame for the request id so
    /// the serving side can abort, then returns an error. `cancel` is a bare
    /// future (e.g. `ctoken.cancelled()`) so the mux needs no cancellation dep.
    pub async fn call_cancellable<F>(&self, body: Body, cancel: F) -> anyhow::Result<Body>
    where
        F: std::future::Future<Output = ()>,
    {
        let id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("mux pending").insert(id, Pending::Unary(tx));
        self.send_body(id, body);
        tokio::pin!(cancel);
        tokio::select! {
            resp = rx => {
                let resp = resp
                    .map_err(|_e| anyhow::anyhow!("plugin connection closed before response"))?;
                if let Body::Error(e) = resp {
                    anyhow::bail!("plugin error: {}", e.message);
                }
                Ok(resp)
            }
            () = &mut cancel => {
                self.pending.lock().expect("mux pending").remove(&id);
                self.send_body(id, Body::Cancel(pb::Cancel { request_id: id }));
                anyhow::bail!("cancelled")
            }
        }
    }

    /// Issue a streaming request. The returned receiver yields response bodies
    /// (`StreamItem`s) until a `StreamEnd`/`Error` arrives (which is forwarded
    /// as the final body, then the channel closes).
    pub fn call_stream(&self, body: Body) -> mpsc::UnboundedReceiver<Body> {
        let id = self.alloc_id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.pending.lock().expect("mux pending").insert(id, Pending::Stream(tx));
        self.send_body(id, body);
        rx
    }

    fn route_response(&self, id: u64, body: Body) {
        let mut pending = self.pending.lock().expect("mux pending");
        match pending.get(&id) {
            Some(Pending::Unary(_)) => {
                if let Some(Pending::Unary(tx)) = pending.remove(&id) {
                    drop(tx.send(body));
                }
            }
            Some(Pending::Stream(tx)) => {
                let terminal = is_stream_terminal(&body);
                drop(tx.send(body));
                if terminal {
                    pending.remove(&id);
                }
            }
            None => {
                tracing::warn!(id, "response for unknown request id");
            }
        }
    }
}

async fn writer_loop<W: AsyncWrite + Unpin>(mut write: W, mut rx: mpsc::UnboundedReceiver<pb::Frame>) {
    while let Some(frame) = rx.recv().await {
        if let Err(e) = write_frame(&mut write, &frame).await {
            tracing::warn!(error = %e, "frame write failed; closing writer");
            break;
        }
    }
}

async fn reader_loop<R: AsyncRead + Unpin>(
    mut read: R,
    mux: Arc<Mux>,
    handler: Arc<dyn InboundHandler>,
) {
    loop {
        match read_frame(&mut read).await {
            Ok(Some(frame)) => {
                let id = frame.id;
                let Some(body) = frame.body else { continue };
                if is_response(&body) {
                    mux.route_response(id, body);
                } else {
                    let mux = Arc::clone(&mux);
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        handler.handle(id, body, mux).await;
                    });
                }
            }
            Ok(None) => break, // peer closed
            Err(e) => {
                tracing::warn!(error = %e, "frame read failed; closing reader");
                break;
            }
        }
    }
    mux.mark_closed();
}
