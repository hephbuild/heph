//! Downloading a URL whose body may be large, from blocking code.
//!
//! reqwest's default client bounds the **whole** request, body included, by
//! 30s, so a tens-of-MB artifact on a slow-but-healthy link fails on every
//! attempt. [`Fetcher`] bounds the connect and each read instead: a transfer
//! that keeps making progress finishes however long it takes, and a stalled one
//! still fails. The blocking client offers only the total bound, so this runs
//! the async client on a runtime of its own.

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
        std::thread::scope(|s| {
            s.spawn(|| self.get_on_own_runtime(url, w, on_content_length))
                .join()
                .map_err(|_e| anyhow::anyhow!("download thread for {url} panicked"))?
        })
    }

    fn get_on_own_runtime(
        &self,
        url: &str,
        w: &mut (impl Write + Send),
        on_content_length: impl FnOnce(u64),
    ) -> anyhow::Result<u64> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build download runtime")?;
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .connect_timeout(self.connect_timeout)
                .read_timeout(self.read_timeout)
                .build()
                .context("build download client")?;
            let mut resp = client
                .get(url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .with_context(|| format!("GET {url}"))?;
            if let Some(len) = resp.content_length() {
                on_content_length(len);
            }
            let mut written = 0u64;
            while let Some(chunk) = resp
                .chunk()
                .await
                .with_context(|| format!("download {url}"))?
            {
                w.write_all(&chunk)
                    .with_context(|| format!("write body of {url}"))?;
                written += chunk.len() as u64;
            }
            w.flush().with_context(|| format!("write body of {url}"))?;
            Ok(written)
        })
    }
}

/// [`Fetcher::get`] with the default timeouts, ignoring the content length.
pub fn get(url: &str, w: &mut (impl Write + Send)) -> anyhow::Result<u64> {
    Fetcher::default().get(url, w, |_| {})
}

/// [`get`], also returning the lowercase hex SHA-256 of the body, computed as
/// it streams into `w` — so a large artifact can be checked against a pinned
/// checksum without holding it in memory or reading it back.
pub fn get_sha256(url: &str, w: &mut (impl Write + Send)) -> anyhow::Result<String> {
    let mut hw = HashingWriter {
        inner: w,
        hasher: Sha256::new(),
    };
    get(url, &mut hw)?;
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
}
