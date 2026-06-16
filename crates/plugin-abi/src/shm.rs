//! shm transport: an iceoryx2 shared-memory byte-pipe wrapped as
//! `AsyncRead`/`AsyncWrite`, so the existing [`crate::mux::Mux`] (and thus the
//! whole plugin protocol) runs over it unchanged.
//!
//! Two iceoryx2 publish-subscribe services carry the Frame byte stream, one per
//! direction; each side publishes on its "send" service and subscribes on its
//! "recv" service (host and guest pass them swapped). Ordering is FIFO per
//! single-publisher service, so the byte stream is preserved.
//!
//! iceoryx2 ports are `!Send`, so a single dedicated io-thread owns the node,
//! publisher and subscriber and bridges to the async world over channels:
//! `AsyncWrite` -> std mpsc -> publish; subscribe -> tokio mpsc -> `AsyncRead`.
//!
//! v1 copies bytes into/out of samples and the io-thread polls; that already
//! avoids the per-message syscall (the UDS cost). True zero-copy (rkyv payload
//! in the loaned sample) and event-driven wakeup (`Listener`/`WaitSet`) are
//! follow-up optimizations.

use iceoryx2::port::update_connections::UpdateConnections;
use iceoryx2::prelude::*;
use std::io;
use std::pin::Pin;
use std::sync::mpsc as smpsc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// Max bytes per iceoryx2 sample; larger writes are split across samples. Kept
/// small so a deep subscriber buffer (below) stays affordable — buffer memory is
/// `SUB_BUFFER * MAX_CHUNK` per service.
const MAX_CHUNK: usize = 4 * 1024;

/// Subscriber queue depth. pub/sub is lossy (a full queue drops samples), so this
/// must exceed the protocol's peak in-flight frame count or a Mux call stalls
/// forever on a dropped reply. `SUB_BUFFER * MAX_CHUNK` (= 32 MiB) per service.
const SUB_BUFFER: usize = 8192;

pub struct ShmReadHalf {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    leftover: Vec<u8>,
    pos: usize,
}

pub struct ShmWriteHalf {
    tx: smpsc::Sender<Vec<u8>>,
}

/// Open a shm byte-pipe. `send_service`/`recv_service` are the iceoryx2 service
/// names for this side (host and guest pass them swapped).
pub fn connect(
    send_service: &str,
    recv_service: &str,
) -> anyhow::Result<(ShmReadHalf, ShmWriteHalf)> {
    let (wtx, wrx) = smpsc::channel::<Vec<u8>>();
    let (rtx, rrx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (setup_tx, setup_rx) = smpsc::channel::<Result<(), String>>();

    let send_service = send_service.to_string();
    let recv_service = recv_service.to_string();

    // All iceoryx2 handles (!Send) live on this one thread.
    std::thread::spawn(move || {
        let ports = (|| -> anyhow::Result<_> {
            let node = NodeBuilder::new().create::<ipc::Service>()?;
            // pub/sub is lossy: a full subscriber buffer drops samples. A build's
            // concurrent request/callback fan-out bursts hundreds of frames, so
            // size the buffer (and loaned samples) generously to never overflow —
            // a dropped frame would stall a Mux call forever.
            let send_svc = node
                .service_builder(&send_service.as_str().try_into()?)
                .publish_subscribe::<[u8]>()
                .subscriber_max_buffer_size(SUB_BUFFER)
                .history_size(0)
                .open_or_create()?;
            let recv_svc = node
                .service_builder(&recv_service.as_str().try_into()?)
                .publish_subscribe::<[u8]>()
                .subscriber_max_buffer_size(SUB_BUFFER)
                .history_size(0)
                .open_or_create()?;
            let publisher = send_svc
                .publisher_builder()
                .initial_max_slice_len(MAX_CHUNK)
                .max_loaned_samples(8)
                .create()?;
            let subscriber = recv_svc.subscriber_builder().create()?;
            Ok((node, publisher, subscriber))
        })();

        match ports {
            Ok((_node, publisher, subscriber)) => {
                if setup_tx.send(Ok(())).is_err() {
                    return;
                }
                run_loop(&publisher, &subscriber, &wrx, &rtx);
            }
            Err(e) => {
                drop(setup_tx.send(Err(e.to_string())));
            }
        }
    });

    setup_rx
        .recv()
        .map_err(|_e| anyhow::anyhow!("shm io-thread died during setup"))?
        .map_err(|e| anyhow::anyhow!("shm setup: {e}"))?;

    Ok((
        ShmReadHalf {
            rx: rrx,
            leftover: Vec::new(),
            pos: 0,
        },
        ShmWriteHalf { tx: wtx },
    ))
}

type Publisher = iceoryx2::port::publisher::Publisher<ipc::Service, [u8], ()>;
type Subscriber = iceoryx2::port::subscriber::Subscriber<ipc::Service, [u8], ()>;

fn run_loop(
    publisher: &Publisher,
    subscriber: &Subscriber,
    wrx: &smpsc::Receiver<Vec<u8>>,
    rtx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    // Adaptive backoff: while frames are flowing (a build's hot resolve), spin so
    // round-trip latency is ~microseconds, not the per-hop poll interval. After a
    // stretch of idleness (between builds), back off to sleeping so an idle plugin
    // doesn't peg a core. The hot path is what the cost model cares about.
    let mut idle_iters: u32 = 0;
    let mut connected = false;
    loop {
        let mut idle = true;

        // Discover subscribers that connected after this publisher was created
        // (the host's publisher exists before the plugin subscribes). Only needed
        // until the link is live; once we've received anything the peer is
        // connected, so stop paying for it on the hot path.
        if !connected {
            // Discover a late-connecting subscriber; ignore the result.
            let _discovered = publisher.update_connections();
        }

        // Outbound: publish queued chunks. On disconnect, publish a 0-length EOF
        // marker so the peer's reader observes a clean close (shm has no implicit
        // EOF), then exit.
        loop {
            match wrx.try_recv() {
                Ok(chunk) => {
                    idle = false;
                    if publish(publisher, &chunk).is_err() {
                        return;
                    }
                }
                Err(smpsc::TryRecvError::Empty) => break,
                Err(smpsc::TryRecvError::Disconnected) => {
                    drop(publish(publisher, &[]));
                    return;
                }
            }
        }

        // Inbound: forward received payloads. A 0-length payload is the peer's EOF
        // marker → drop our forwarding channel (reader sees end) and exit.
        loop {
            match subscriber.receive() {
                Ok(Some(sample)) => {
                    let payload = sample.payload();
                    if payload.is_empty() {
                        return;
                    }
                    idle = false;
                    connected = true;
                    if rtx.send(payload.to_vec()).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }

        if idle {
            idle_iters = idle_iters.saturating_add(1);
            if idle_iters < 256 {
                std::hint::spin_loop();
            } else if idle_iters < 4096 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(200));
            }
        } else {
            idle_iters = 0;
        }
    }
}

fn publish(publisher: &Publisher, chunk: &[u8]) -> anyhow::Result<()> {
    let sample = publisher.loan_slice_uninit(chunk.len())?;
    let sample = sample.write_from_slice(chunk);
    sample.send()?;
    Ok(())
}

impl AsyncRead for ShmReadHalf {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.leftover.len() {
            let start = self.pos;
            let end = (start + buf.remaining()).min(self.leftover.len());
            if let Some(s) = self.leftover.get(start..end) {
                buf.put_slice(s);
                self.pos = end;
            }
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = chunk.len().min(buf.remaining());
                if let Some(s) = chunk.get(..n) {
                    buf.put_slice(s);
                }
                if n < chunk.len() {
                    self.leftover = chunk;
                    self.pos = n;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ShmWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        for chunk in buf.chunks(MAX_CHUNK) {
            self.tx
                .send(chunk.to_vec())
                .map_err(|_e| io::Error::new(io::ErrorKind::BrokenPipe, "shm writer closed"))?;
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static N: AtomicU32 = AtomicU32::new(0);

    /// Cross-process delivery: re-exec this test binary as a child (subscriber)
    /// and ping/pong over shm. shm_byte_pipe_roundtrip only proves the same
    /// process; this proves two processes actually share the iceoryx2 services.
    #[test]
    fn shm_cross_process_delivery() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        if std::env::var("HEPH_SHM_XTEST").is_ok() {
            // Child: subscribe h2g, publish g2h; echo ping->pong.
            rt.block_on(async {
                let (mut r, mut w) = connect("xt_g2h", "xt_h2g").expect("child connect");
                let mut b = [0u8; 4];
                r.read_exact(&mut b).await.expect("child read");
                w.write_all(b"pong").await.expect("child write");
                tokio::time::sleep(Duration::from_millis(300)).await;
            });
            return;
        }
        // Mimic heph's order: parent (publisher) connects BEFORE the child
        // (subscriber) — so update_connections must rediscover the late subscriber.
        let (mut r, mut w) = connect("xt_h2g", "xt_g2h").expect("parent connect");
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .env("HEPH_SHM_XTEST", "1")
            .args(["--exact", "shm::tests::shm_cross_process_delivery"])
            .spawn()
            .expect("spawn child");
        std::thread::sleep(Duration::from_millis(400)); // let child subscribe
        rt.block_on(async {
            w.write_all(b"ping").await.expect("parent write");
            let mut b = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(5), r.read_exact(&mut b))
                .await
                .expect("cross-process shm delivery timed out")
                .expect("parent read");
            assert_eq!(&b, b"pong");
        });
        let _status = child.wait();
    }

    #[tokio::test]
    async fn shm_byte_pipe_roundtrip() {
        let id = format!(
            "heph_shm_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let h2g = format!("{id}_h2g");
        let g2h = format!("{id}_g2h");

        // Host: send on h2g, recv on g2h. Guest: the reverse.
        let (mut host_r, mut host_w) = connect(&h2g, &g2h).expect("host shm");
        let (mut guest_r, mut guest_w) = connect(&g2h, &h2g).expect("guest shm");

        host_w.write_all(b"ping").await.expect("host write");
        let mut a = [0u8; 4];
        guest_r.read_exact(&mut a).await.expect("guest read");
        assert_eq!(&a, b"ping");

        guest_w.write_all(b"pong").await.expect("guest write");
        let mut b = [0u8; 4];
        host_r.read_exact(&mut b).await.expect("host read");
        assert_eq!(&b, b"pong");
    }
}
