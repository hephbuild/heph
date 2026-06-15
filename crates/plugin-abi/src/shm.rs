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

use iceoryx2::prelude::*;
use std::io;
use std::pin::Pin;
use std::sync::mpsc as smpsc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// Max bytes per iceoryx2 sample; larger writes are split across samples.
const MAX_CHUNK: usize = 64 * 1024;

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
            let send_svc = node
                .service_builder(&send_service.as_str().try_into()?)
                .publish_subscribe::<[u8]>()
                .open_or_create()?;
            let recv_svc = node
                .service_builder(&recv_service.as_str().try_into()?)
                .publish_subscribe::<[u8]>()
                .open_or_create()?;
            let publisher = send_svc
                .publisher_builder()
                .initial_max_slice_len(MAX_CHUNK)
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
    loop {
        let mut idle = true;

        // Outbound: publish queued chunks.
        loop {
            match wrx.try_recv() {
                Ok(chunk) => {
                    idle = false;
                    if publish(publisher, &chunk).is_err() {
                        return;
                    }
                }
                Err(smpsc::TryRecvError::Empty) => break,
                Err(smpsc::TryRecvError::Disconnected) => return,
            }
        }

        // Inbound: forward received payloads.
        loop {
            match subscriber.receive() {
                Ok(Some(sample)) => {
                    idle = false;
                    if rtx.send(sample.payload().to_vec()).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }

        if idle {
            std::thread::sleep(Duration::from_micros(50));
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
