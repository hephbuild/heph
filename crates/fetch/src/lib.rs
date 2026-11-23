//! Downloading a URL whose body may be large.
//!
//! reqwest's default client bounds the **whole** request, body included, by
//! 30s, so a tens-of-MB artifact on a slow-but-healthy link fails on every
//! attempt. [`Fetcher`] bounds the connect and each read instead: a transfer
//! that keeps making progress finishes however long it takes, and a stalled one
//! still fails.
//!
//! [`Fetcher::get_async`] runs on the caller's tokio runtime, and dropping its
//! future stops the transfer and closes the connection. The blocking client
//! offers only the total bound, so the blocking entry points ([`Fetcher::get`],
//! [`get`], [`get_sha256`]) run that same future on a runtime of their own.

use std::future::Future;
use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};

/// Timeouts for a download. See the crate docs for why there is no total one.
#[derive(Debug, Clone, Copy)]
pub struct Fetcher {
    /// Bound on establishing the connection.
    pub connect_timeout: Duration,
    /// Bound on a single read making no progress.
    pub read_timeout: Duration,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
        }
    }
}

impl Fetcher {
    /// GET `url` and stream its body into `w`, returning the number of bytes
    /// written. `on_content_length` is called once with the announced body size,
    /// when the server sends one, before any byte reaches `w`.
    ///
    /// Blocks the calling thread. The request runs on a dedicated thread with its
    /// own runtime, so this is safe to call from within an async runtime (a
    /// nested `block_on` would otherwise panic) and needs no reactor from the
    /// caller — a plugin cdylib has none.
    pub fn get(
        &self,
        url: &str,
        w: &mut (impl Write + Send),
        on_content_length: impl FnOnce(u64) + Send,
    ) -> anyhow::Result<u64> {
        block_on_own_runtime(url, self.get_async(url, w, on_content_length))
    }

    /// [`get`](Self::get) on the caller's tokio runtime, which must have IO
    /// and time enabled. Dropping the future abandons the transfer and closes
    /// its connection.
    ///
    /// `w` is written synchronously, one received chunk at a time, on whichever
    /// worker polls the future.
    pub async fn get_async(
        &self,
        url: &str,
        w: &mut (impl Write + Send),
        on_content_length: impl FnOnce(u64) + Send,
    ) -> anyhow::Result<u64> {
        let mut body = self.open(url).await?;
        if let Some(len) = body.content_length() {
            on_content_length(len);
        }
        let mut written = 0u64;
        while let Some(chunk) = body.chunk().await? {
            w.write_all(&chunk)
                .with_context(|| format!("write body of {url}"))?;
            written += chunk.len() as u64;
        }
        w.flush().with_context(|| format!("write body of {url}"))?;
        Ok(written)
    }

    /// GET `url` on the caller's tokio runtime, which must have IO and time
    /// enabled, and return its body still to be read, once the status is a
    /// success. Dropping the [`Body`] abandons the transfer and closes its
    /// connection.
    ///
    /// For a caller that writes the body somewhere other than the polling
    /// worker; [`get_async`](Self::get_async) writes each chunk inline.
    pub async fn open<'u>(&self, url: &'u str) -> anyhow::Result<Body<'u>> {
        let client = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout)
            .build()
            .context("build download client")?;
        let resp = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("GET {url}"))?;
        Ok(Body { url, resp })
    }
}

/// The body of a successful GET, read one received chunk at a time. See
/// [`Fetcher::open`].
#[derive(Debug)]
pub struct Body<'u> {
    url: &'u str,
    resp: reqwest::Response,
}

impl Body<'_> {
    /// The body size the server announced, if it sent one.
    pub fn content_length(&self) -> Option<u64> {
        self.resp.content_length()
    }

    /// The next chunk of the body, or `None` once it has ended. Each read is
    /// bounded by the [`Fetcher`]'s `read_timeout`.
    ///
    /// Cancel-safe: dropping the future before it resolves loses no bytes;
    /// `plugin-http`'s download `select!`s on it.
    pub async fn chunk(&mut self) -> anyhow::Result<Option<bytes::Bytes>> {
        let url = self.url;
        self.resp
            .chunk()
            .await
            .with_context(|| format!("download {url}"))
    }
}

/// Run a download future to completion from blocking code.
///
/// The future runs on a dedicated thread with its own runtime, so this is safe
/// to call from within an async runtime (a nested `block_on` would otherwise
/// panic) and needs no reactor from the caller — a plugin cdylib has none.
fn block_on_own_runtime<T: Send>(
    url: &str,
    fut: impl Future<Output = anyhow::Result<T>> + Send,
) -> anyhow::Result<T> {
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build download runtime")?
                .block_on(fut)
        })
        .join()
        .map_err(|_e| anyhow::anyhow!("download thread for {url} panicked"))?
    })
}

/// [`Fetcher::get`] with the default timeouts, ignoring the content length.
pub fn get(url: &str, w: &mut (impl Write + Send)) -> anyhow::Result<u64> {
    Fetcher::default().get(url, w, |_| {})
}

/// [`get`], also returning the lowercase hex SHA-256 of the body, computed as
/// it streams into `w` — so a large artifact can be checked against a pinned
/// checksum without holding it in memory or reading it back.
pub fn get_sha256(url: &str, w: &mut (impl Write + Send)) -> anyhow::Result<String> {
    block_on_own_runtime(url, get_sha256_async(url, w))
}

/// [`get_sha256`] on the caller's tokio runtime; see [`Fetcher::get_async`].
pub async fn get_sha256_async(url: &str, w: &mut (impl Write + Send)) -> anyhow::Result<String> {
    let mut hw = HashingWriter {
        inner: w,
        hasher: Sha256::new(),
    };
    Fetcher::default().get_async(url, &mut hw, |_| {}).await?;
    Ok(hex::encode(hw.hasher.finalize()))
}

/// Writes through to `inner`, hashing every byte that lands.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(data)?;
        self.hasher.update(data.get(..n).unwrap_or(data));
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A body being written into `W` and hashed as it lands: [`get_sha256`]'s
/// writing half, for a caller that receives the body ([`Fetcher::open`]) on
/// one thread and writes it on another.
///
/// It owns all of its state, so it can move into a short blocking job for each
/// batch of chunks and back out again, rather than one job holding a thread for
/// the whole transfer.
pub struct Sha256Writer<W> {
    hw: HashingWriter<W>,
}

impl<W> std::fmt::Debug for Sha256Writer<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sha256Writer").finish_non_exhaustive()
    }
}

impl<W: Write> Sha256Writer<W> {
    pub fn new(w: W) -> Self {
        Self {
            hw: HashingWriter {
                inner: w,
                hasher: Sha256::new(),
            },
        }
    }

    /// Write the next `chunks` of `url`'s body, in order, and flush them: when
    /// this returns they have reached `W`'s sink, not just its buffer, so a
    /// body that then stalls is on disk up to its last received byte.
    pub fn write_chunks(
        &mut self,
        url: &str,
        chunks: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> anyhow::Result<()> {
        for chunk in chunks {
            self.hw
                .write_all(chunk.as_ref())
                .with_context(|| format!("write body of {url}"))?;
        }
        self.hw
            .flush()
            .with_context(|| format!("write body of {url}"))
    }

    /// Flush what was written and return its lowercase hex SHA-256.
    pub fn finish(mut self, url: &str) -> anyhow::Result<String> {
        self.hw
            .flush()
            .with_context(|| format!("write body of {url}"))?;
        Ok(hex::encode(self.hw.hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Serve one HTTP response on a local port: headers announcing `len` body
    /// bytes, then `body` one byte every `gap`, then hold the connection open
    /// for `hold`. Returns the URL.
    fn serve_dribble(len: usize, body: &'static [u8], gap: Duration, hold: Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "empty request");
            write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n"
            )
            .unwrap();
            for b in body {
                std::thread::sleep(gap);
                s.write_all(&[*b]).unwrap();
                s.flush().unwrap();
            }
            std::thread::sleep(hold);
        });
        format!("http://{addr}/heph-go-plugin_linux_amd64.so")
    }

    fn fetcher(read_timeout: Duration) -> Fetcher {
        Fetcher {
            read_timeout,
            ..Fetcher::default()
        }
    }

    /// A transfer taking far longer in total than the read timeout still
    /// completes, as long as every read makes progress — the regression was a
    /// 30s bound on the whole request (#463).
    #[test]
    fn tolerates_slow_but_progressing_body() {
        let body: &[u8] = b"0123456789";
        let url = serve_dribble(body.len(), body, Duration::from_millis(100), Duration::ZERO);
        let mut out = Vec::new();
        let mut announced = None;
        // ~1s total, 400ms per read.
        let n = fetcher(Duration::from_millis(400))
            .get(&url, &mut out, |len| announced = Some(len))
            .unwrap();
        assert_eq!(out, body);
        assert_eq!(n, body.len() as u64);
        assert_eq!(announced, Some(body.len() as u64));
    }

    /// The streamed hash is the SHA-256 of exactly the bytes written.
    #[test]
    fn get_sha256_hashes_the_written_body() {
        let body: &[u8] = b"test";
        let url = serve_dribble(body.len(), body, Duration::ZERO, Duration::ZERO);
        let mut out = Vec::new();
        let got = get_sha256(&url, &mut out).unwrap();
        assert_eq!(out, body);
        // sha256("test")
        assert_eq!(
            got,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    /// A body that stops arriving fails on the read timeout, and the error
    /// names the URL.
    #[test]
    fn fails_on_stalled_body_naming_the_url() {
        let url = serve_dribble(10, b"01", Duration::ZERO, Duration::from_secs(5));
        let mut out = Vec::new();
        let err = fetcher(Duration::from_millis(200))
            .get(&url, &mut out, |_| {})
            .expect_err("stalled body must time out");
        let msg = format!("{err:#}");
        assert!(msg.contains(&url), "{msg}");
    }

    /// An error status fails before any body is written.
    #[test]
    fn fails_on_error_status() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "empty request");
            s.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope",
            )
            .unwrap();
        });
        let url = format!("http://{addr}/missing");
        let mut out = Vec::new();
        let err = get(&url, &mut out).expect_err("404 must fail");
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(out.is_empty());
    }

    /// Batches written one after another hash as the whole body, and land in
    /// order.
    #[test]
    fn sha256_writer_hashes_across_batches() {
        let mut w = Sha256Writer::new(Vec::new());
        w.write_chunks("u", [&b"a"[..]]).unwrap();
        w.write_chunks("u", [&b"b"[..], &b"c"[..]]).unwrap();
        assert_eq!(w.hw.inner, b"abc");
        assert_eq!(
            w.finish("u").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
