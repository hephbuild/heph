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
//!
//! A URL a user hands heph to read (a `.hephsession` by `<ref>`) is untrusted:
//! [`Fetcher::https_capped`] refuses anything but `https://`, redirects
//! included, and stops at a size cap, into a temp file deleted on drop
//! ([`Fetcher::get_to_temp`]).

use std::future::Future;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};

/// Timeouts and limits for a download. See the crate docs for why there is no
/// total timeout.
#[derive(Debug, Clone, Copy)]
pub struct Fetcher {
    /// Bound on establishing the connection.
    pub connect_timeout: Duration,
    /// Bound on a single read making no progress.
    pub read_timeout: Duration,
    /// The most body bytes accepted: a larger announced length fails before
    /// any byte is written, and a body that grows past it fails as it does
    /// ([`TooLarge`]). `None` is no cap.
    pub max_bytes: Option<u64>,
    /// Refuse a URL, or a redirect, whose scheme is not `https` ([`NotHttps`]
    /// for the URL itself; reqwest refuses the redirect).
    pub https_only: bool,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            max_bytes: None,
            https_only: false,
        }
    }
}

/// The body is larger than [`Fetcher::max_bytes`]. Reached through
/// `anyhow::Error::downcast_ref`, so a caller can tell it from a network
/// failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooLarge {
    pub max_bytes: u64,
}

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the body is larger than the {} byte cap", self.max_bytes)
    }
}

impl std::error::Error for TooLarge {}

/// The URL is not `https://` and [`Fetcher::https_only`] is set. Nothing was
/// sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotHttps {
    pub url: String,
}

impl std::fmt::Display for NotHttps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refusing {}: only https:// URLs are fetched", self.url)
    }
}

impl std::error::Error for NotHttps {}

/// `url` as it may appear in a message: no userinfo, no query, no fragment.
/// A signed URL (`?X-Amz-Signature=…`) carries its credential in the query,
/// so every error this crate builds names the URL this way, and reqwest's own
/// errors lose theirs ([`reqwest::Error::without_url`]). Text that does not
/// parse as a URL is not echoed at all.
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            // Only fails for a URL that cannot have credentials.
            let _ok = u.set_username("");
            let _ok = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "the URL".to_owned(),
    }
}

impl Fetcher {
    /// The default timeouts, `https://` only, at most `max_bytes` of body.
    pub fn https_capped(max_bytes: u64) -> Self {
        Self {
            max_bytes: Some(max_bytes),
            https_only: true,
            ..Self::default()
        }
    }

    /// GET `url` into a new temp file in `dir`, deleted when the returned
    /// handle drops, including when the download fails part way.
    ///
    /// Blocks the calling thread, as [`get`](Self::get) does.
    pub fn get_to_temp(&self, url: &str, dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        let mut file = tempfile::NamedTempFile::new_in(dir).with_context(|| {
            format!(
                "create a temp file in {} for {}",
                dir.display(),
                redact_url(url)
            )
        })?;
        self.get(
            url,
            &mut std::io::BufWriter::new(file.as_file_mut()),
            |_| {},
        )?;
        Ok(file)
    }

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
                .with_context(|| format!("write body of {}", redact_url(url)))?;
            written += chunk.len() as u64;
        }
        w.flush()
            .with_context(|| format!("write body of {}", redact_url(url)))?;
        Ok(written)
    }

    /// GET `url` on the caller's tokio runtime, which must have IO and time
    /// enabled, and return its body still to be read, once the status is a
    /// success. Dropping the [`Body`] abandons the transfer and closes its
    /// connection.
    ///
    /// For a caller that writes the body somewhere other than the polling
    /// worker; [`get_async`](Self::get_async) writes each chunk inline.
    ///
    /// With [`max_bytes`](Self::max_bytes) set, an announced length over it
    /// fails here, before any byte is read, and [`Body::chunk`] fails on the
    /// chunk that would pass it.
    pub async fn open<'u>(&self, url: &'u str) -> anyhow::Result<Body<'u>> {
        self.open_with(url, RequestOptions::default()).await
    }

    /// [`open`](Self::open), sending what `opts` adds to the request.
    ///
    /// The auth header is sent to `url`'s origin only: a redirect to another
    /// host, port or scheme drops it (reqwest strips `Authorization` on a
    /// cross-origin hop), so a release asset that redirects to a CDN's signed
    /// URL does not hand that CDN the credential. `opts.headers` hold no
    /// secret ([`check_header`] refuses those) and follow every hop.
    pub async fn open_with<'u>(
        &self,
        url: &'u str,
        opts: RequestOptions<'_>,
    ) -> anyhow::Result<Body<'u>> {
        // Every message names the URL without what may be a credential.
        if self.https_only {
            let parsed = reqwest::Url::parse(url)
                .with_context(|| format!("parse URL {}", redact_url(url)))?;
            if parsed.scheme() != "https" {
                return Err(NotHttps {
                    url: redact_url(url),
                }
                .into());
            }
        }
        let client = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .read_timeout(self.read_timeout)
            .user_agent(user_agent())
            .https_only(self.https_only)
            .build()
            .context("build download client")?;
        let mut req = client.get(url);
        for (name, value) in opts.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        if let Some(auth) = opts.auth {
            req = req.basic_auth(&auth.username, Some(&auth.password));
        }
        let resp = req
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("GET {}", redact_url(url)))?;
        let body = Body {
            url,
            resp,
            max_bytes: self.max_bytes,
            read: 0,
        };
        if let (Some(len), Some(max)) = (body.content_length(), self.max_bytes)
            && len > max
        {
            return Err(body.too_large(max));
        }
        Ok(body)
    }
}

/// What a GET sends beyond its URL. See [`Fetcher::open_with`].
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestOptions<'a> {
    /// Sent as HTTP basic auth, to the URL's origin only.
    pub auth: Option<&'a BasicAuth>,
    /// Extra request headers, each already accepted by [`check_header`]. One
    /// named `User-Agent` replaces [`user_agent`].
    pub headers: &'a [(String, String)],
}

/// The `User-Agent` every download sends: `heph/<version>`. Some servers
/// refuse a request with none (GitHub's API answers 403), and reqwest sets
/// none of its own.
pub fn user_agent() -> String {
    format!("heph/{}", hcore::version::current())
}

/// Headers a caller may not set through [`RequestOptions::headers`], and why.
/// Secrets first: a header there is ordinary configuration, written in a BUILD
/// file and hashed into a key, so a credential in one would be both. The rest
/// frame the request, which is the client's job.
const REFUSED_HEADERS: &[(&str, &str)] = &[
    (
        "authorization",
        "it carries a credential; declare `credentials` instead",
    ),
    (
        "proxy-authorization",
        "it carries a credential; declare `credentials` instead",
    ),
    (
        "cookie",
        "it carries a credential; declare `credentials` instead",
    ),
    ("host", "the client derives it from the url"),
    ("content-length", "the client sets it"),
    ("transfer-encoding", "the client sets it"),
    ("connection", "the client manages the connection"),
];

/// Accept `name: value` as an extra request header, or say why not: a name or
/// value HTTP cannot carry, or a header in the refused set. Returns the name
/// lowercased, the one spelling two declarations of the same header share.
///
/// Never quotes `value`: an author who put a token in one anyway should not
/// find it copied into an error.
pub fn check_header(name: &str, value: &str) -> anyhow::Result<String> {
    let parsed = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_e| anyhow::anyhow!("{name:?} is not a valid HTTP header name"))?;
    let lower = parsed.as_str();
    if let Some((_, why)) = REFUSED_HEADERS.iter().find(|(n, _)| *n == lower) {
        anyhow::bail!("header {name:?} cannot be set here: {why}");
    }
    reqwest::header::HeaderValue::from_str(value).map_err(|_e| {
        anyhow::anyhow!("the value of header {name:?} holds bytes an HTTP header cannot carry")
    })?;
    Ok(lower.to_string())
}

/// The HTTP status a failed download was answered with, if the failure was a
/// status at all (rather than a connection, timeout or write error): read off
/// the error chain [`Fetcher::open`] returns.
pub fn http_status(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|e| e.downcast_ref::<reqwest::Error>()?.status())
        .map(|s| s.as_u16())
}

/// A username and password sent as HTTP basic auth. See [`Fetcher::open_with`].
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

/// Prints neither field: an auth value that reaches a log or an error chain
/// through `{:?}` would put the secret there, and a username is material too
/// (GitHub takes a token as one).
impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BasicAuth(<redacted>)")
    }
}

/// The body of a successful GET, read one received chunk at a time. See
/// [`Fetcher::open`].
#[derive(Debug)]
pub struct Body<'u> {
    url: &'u str,
    resp: reqwest::Response,
    /// [`Fetcher::max_bytes`].
    max_bytes: Option<u64>,
    /// Body bytes returned so far.
    read: u64,
}

impl Body<'_> {
    /// The body size the server announced, if it sent one.
    pub fn content_length(&self) -> Option<u64> {
        self.resp.content_length()
    }

    /// The next chunk of the body, or `None` once it has ended. Each read is
    /// bounded by the [`Fetcher`]'s `read_timeout`, and the body as a whole by
    /// its `max_bytes` ([`TooLarge`]).
    ///
    /// Cancel-safe: dropping the future before it resolves loses no bytes;
    /// `plugin-http`'s download `select!`s on it.
    pub async fn chunk(&mut self) -> anyhow::Result<Option<bytes::Bytes>> {
        let url = self.url;
        let chunk = self
            .resp
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("download {}", redact_url(url)))?;
        if let Some(chunk) = &chunk {
            let read = self.read.saturating_add(chunk.len() as u64);
            if let Some(max) = self.max_bytes.filter(|max| read > *max) {
                return Err(self.too_large(max));
            }
            self.read = read;
        }
        Ok(chunk)
    }

    fn too_large(&self, max_bytes: u64) -> anyhow::Error {
        anyhow::Error::new(TooLarge { max_bytes })
            .context(format!("download {}", redact_url(self.url)))
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
        .map_err(|_e| anyhow::anyhow!("download thread for {} panicked", redact_url(url)))?
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

/// The lowercase hex SHA-256 of everything left to read from `reader`, and how
/// many bytes that was. Streams in 256 KiB reads, so a large file is hashed
/// without being held in memory.
pub fn sha256_hex_of(reader: impl Read) -> std::io::Result<(String, u64)> {
    let mut hw = HashingWriter {
        inner: std::io::sink(),
        hasher: Sha256::new(),
    };
    let n = std::io::copy(
        &mut std::io::BufReader::with_capacity(256 * 1024, reader),
        &mut hw,
    )?;
    Ok((hex::encode(hw.hasher.finalize()), n))
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
                .with_context(|| format!("write body of {}", redact_url(url)))?;
        }
        self.hw
            .flush()
            .with_context(|| format!("write body of {}", redact_url(url)))
    }

    /// Flush what was written and return its lowercase hex SHA-256.
    pub fn finish(mut self, url: &str) -> anyhow::Result<String> {
        self.hw
            .flush()
            .with_context(|| format!("write body of {}", redact_url(url)))?;
        Ok(hex::encode(self.hw.hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// FIPS 180-2 vectors: empty input, `abc`, and a million `a`s, which spans
    /// several 256 KiB reads and ends on a partial one.
    #[test]
    fn sha256_hex_of_matches_fixed_vectors() {
        assert_eq!(
            sha256_hex_of(&b""[..]).unwrap(),
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
                0
            )
        );
        assert_eq!(
            sha256_hex_of(&b"abc"[..]).unwrap(),
            (
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
                3
            )
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex_of(million.as_slice()).unwrap(),
            (
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0".to_string(),
                1_000_000
            )
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

    /// `{:?}` of a credential prints neither half: `Selected` in plugin-http
    /// derives `Debug` around one.
    #[test]
    fn basic_auth_debug_prints_no_material() {
        let shown = format!(
            "{:?}",
            BasicAuth {
                username: "user-material".to_string(),
                password: "password-material".to_string(),
            }
        );
        assert!(!shown.contains("material"), "{shown}");
    }

    /// A signed URL's query and fragment, and any userinfo, never reach an
    /// error's text: not this crate's contexts, not reqwest's own message,
    /// not [`NotHttps`].
    #[test]
    fn errors_never_carry_a_query_fragment_or_userinfo() {
        const SECRETS: [&str; 3] = ["sekrit", "frag", "hunter2"];
        let check = |err: &anyhow::Error, base: &str| {
            for text in [format!("{err:#}"), format!("{err:?}")] {
                for s in SECRETS {
                    assert!(!text.contains(s), "{s} in: {text}");
                }
                assert!(text.contains(base), "{base} not in: {text}");
            }
        };
        let signed = |base: &str| format!("{base}?X-Amz-Signature=sekrit#frag");

        // An error status: reqwest names the URL in its own message.
        let addr = serve_raw(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope"
                .to_vec(),
        );
        let base = format!("http://{addr}/x");
        let err = get(&signed(&base), &mut Vec::new()).expect_err("404");
        check(&err, &base);

        // A connect failure.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let base = format!("http://{addr}/x");
        let err = get(&signed(&base), &mut Vec::new()).expect_err("refused");
        check(&err, &base);

        // A body over the cap.
        let addr = serve_raw(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\ntests".to_vec(),
        );
        let base = format!("http://{addr}/x");
        let err = capped(4)
            .get(&signed(&base), &mut Vec::new(), |_| {})
            .expect_err("over the cap");
        check(&err, &base);

        // Refused by scheme, userinfo included.
        let err = Fetcher::https_capped(1)
            .get(
                "http://u:hunter2@h.invalid/x?X-Amz-Signature=sekrit#frag",
                &mut Vec::new(),
                |_| {},
            )
            .expect_err("http");
        check(&err, "http://h.invalid/x");
        assert_eq!(
            err.downcast_ref::<NotHttps>().map(|e| e.url.as_str()),
            Some("http://h.invalid/x")
        );
    }

    /// Serve `response` verbatim to one request, then close. Returns the
    /// address.
    fn serve_raw(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "empty request");
            // The client may hang up as soon as the cap is passed.
            if let Err(e) = s.write_all(&response) {
                assert!(
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ),
                    "serve: {e}"
                );
            }
        });
        addr
    }

    /// A capped fetcher over plain http, which only a test can reach: the
    /// cap is what is under test.
    fn capped(max_bytes: u64) -> Fetcher {
        Fetcher {
            https_only: false,
            ..Fetcher::https_capped(max_bytes)
        }
    }

    /// `https_only` refuses `http://` (any case) before connecting: the
    /// port here has no listener, so a connect would fail differently.
    #[test]
    fn https_only_refuses_http_before_sending() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        for url in [
            format!("http://{addr}/x.hephsession"),
            format!("HTTP://{addr}/x.hephsession"),
            format!("ftp://{addr}/x.hephsession"),
        ] {
            let mut out = Vec::new();
            let err = Fetcher::https_capped(1 << 30)
                .get(&url, &mut out, |_| {})
                .expect_err("http must be refused");
            assert_eq!(
                err.downcast_ref::<NotHttps>(),
                Some(&NotHttps {
                    url: redact_url(&url)
                }),
                "{err:#}"
            );
            assert!(format!("{err:#}").contains("only https://"), "{err:#}");
        }
        // Not a URL at all: a parse error, not a request.
        let err = Fetcher::https_capped(1)
            .get("not a url", &mut Vec::new(), |_| {})
            .expect_err("garbage must fail");
        assert!(format!("{err:#}").contains("parse URL"), "{err:#}");
    }

    /// An announced length over the cap fails before any byte is written.
    #[test]
    fn cap_refuses_an_announced_length_before_writing() {
        let addr = serve_raw(
            b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\nconnection: close\r\n\r\n01234567890"
                .to_vec(),
        );
        let mut out = Vec::new();
        let err = capped(10)
            .get(&format!("http://{addr}/x"), &mut out, |_| {
                panic!("the length callback runs only within the cap")
            })
            .expect_err("over the cap");
        assert_eq!(
            err.downcast_ref::<TooLarge>(),
            Some(&TooLarge { max_bytes: 10 })
        );
        assert!(out.is_empty());
    }

    /// A body with no length that grows past the cap fails as it does; one
    /// exactly at the cap is accepted.
    #[test]
    fn cap_stops_an_unannounced_body() {
        let body = |n: usize| {
            let mut r = b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
            r.extend(std::iter::repeat_n(b'x', n));
            r
        };
        let addr = serve_raw(body(64 * 1024));
        let mut out = Vec::new();
        let err = capped(1000)
            .get(&format!("http://{addr}/x"), &mut out, |_| {})
            .expect_err("over the cap");
        assert_eq!(
            err.downcast_ref::<TooLarge>(),
            Some(&TooLarge { max_bytes: 1000 })
        );
        assert!(out.len() <= 1000, "wrote {} bytes past the cap", out.len());

        let addr = serve_raw(body(1000));
        let mut out = Vec::new();
        let n = capped(1000)
            .get(&format!("http://{addr}/x"), &mut out, |_| {})
            .expect("exactly at the cap");
        assert_eq!(n, 1000);
    }

    /// `get_to_temp` leaves the body in a file in `dir`; a failed download
    /// leaves nothing behind.
    #[test]
    fn get_to_temp_keeps_only_a_complete_body() {
        let dir = tempfile::tempdir().unwrap();
        let addr = serve_raw(
            b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\ntest".to_vec(),
        );
        let file = capped(4)
            .get_to_temp(&format!("http://{addr}/x"), dir.path())
            .unwrap();
        assert_eq!(file.path().parent(), Some(dir.path()));
        assert_eq!(std::fs::read(file.path()).unwrap(), b"test");
        drop(file);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let addr = serve_raw(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\ntests".to_vec(),
        );
        capped(4)
            .get_to_temp(&format!("http://{addr}/x"), dir.path())
            .expect_err("over the cap");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a failed download left a file"
        );
    }
}
