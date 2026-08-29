//! [`RemoteCacheBackend`] backed by the `object_store` crate. A single backend
//! type serves every supported URI scheme — `s3://`, `gs://`, `az://`,
//! `http(s)://`, `memory://`, `file://`. GCS, S3, Azure, and HTTP each go
//! through their own `object_store` builder (so every networked scheme can
//! carry [`retry_config()`] directly); `file`/`memory` fall back to
//! `object_store::parse_url_opts`, which dispatches on the scheme and returns
//! the right store. Credentials are read from the process environment (e.g.
//! `AWS_ACCESS_KEY_ID`, `GOOGLE_SERVICE_ACCOUNT`) by feeding `std::env::vars()`
//! to the builder, mirroring each builder's `from_env`. An `s3://` cache can
//! additionally be pointed at a non-AWS S3-compatible service from the config
//! file — see [`StoreOptions`].
//!
//! All transfers are streamed: reads expose the object's byte stream as an
//! [`AsyncRead`], and writes go through object_store's multipart [`BufWriter`],
//! so a blob is never held whole in memory.
//!
//! One credential type needs special handling. object_store's GCS builder reads
//! Application Default Credentials itself and only decodes `service_account` and
//! `authorized_user` files — an `external_account` ADC (GCP workload identity
//! federation, e.g. what `google-github-actions/auth` writes in CI) fails to
//! parse before any request is made. For that case only, we mint a bearer token
//! out-of-band with `google-cloud-auth` (which does the STS token exchange and
//! service-account impersonation in-process) and hand it to the GCS builder via
//! [`object_store::gcp::GoogleCloudStorageBuilder::with_credentials`], which then
//! skips ADC parsing entirely. Every other scheme and credential type is left on
//! object_store's native path.

use crate::engine::remote_cache::RemoteCacheBackend;
use anyhow::Context;
use async_trait::async_trait;
use enclose::enclose;
use futures::TryStreamExt;
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder as AdcBuilder};
use object_store::aws::AmazonS3Builder;
use object_store::azure::MicrosoftAzureBuilder;
use object_store::buffered::BufWriter;
use object_store::client::{HttpClient, HttpConnector};
use object_store::gcp::{GcpCredential, GcpCredentialProvider, GoogleCloudStorageBuilder};
use object_store::http::HttpBuilder;
use object_store::limit::LimitStore;
use object_store::{
    ClientOptions, CredentialProvider, ObjectStore, ObjectStoreExt, ObjectStoreScheme, RetryConfig,
    parse_url_opts, path::Path as ObjPath,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::OnceCell;
use tokio::time::{Instant, Sleep, sleep};
use tokio_util::io::StreamReader;
use url::Url;

/// OAuth scope for read/write access to GCS objects — what a remote cache needs.
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

// Liveness for a cache transfer.
//
// object_store's GCS/S3 client is HTTP/1.1 (its `ClientOptions` default is
// `http1_only`), so there is no HTTP/2 keep-alive ping to detect a dead or
// trickling peer — the only liveness knob that actually applies is the
// per-request timeout. A *disabled* timeout (a previous attempt at supporting
// multi-GiB blobs) is therefore unsafe: a stalled or pathologically slow
// connection is never reset and the transfer hangs indefinitely.
//
// So we keep the timeout FINITE but generous — enough for a large blob on a
// healthy link, while a genuinely stuck attempt is aborted and object_store
// resumes from the current offset via a `Range` request on a fresh connection
// (usually fast). During a transfer, [`InactivityReader`] adds a tighter bound:
// if no bytes arrive for `INACTIVITY_TIMEOUT`, the read fails fast rather than
// waiting out the whole request timeout.

/// Per-request (per-attempt) timeout. Lifted well above object_store's 30s
/// default so a large blob isn't chopped mid-body, but finite so a stalled or
/// trickling connection is eventually reset and resumed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Abort a transfer that delivers no bytes for this long — catches a stall
/// without waiting out the full [`REQUEST_TIMEOUT`].
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP connector that replaces object_store's HTTP/1.1-only client with a
/// reqwest client that negotiates HTTP/2 via ALPN and gracefully falls back to
/// HTTP/1.1 — matching the Go client, and honoring `HTTP(S)_PROXY`/`NO_PROXY`
/// from the environment.
///
/// Why: object_store's `ClientOptions` defaults to `http1_only` and offers no
/// ALPN-negotiate mode (only force-h1 or force-h2-prior-knowledge). Under a wide
/// `//...` fan-out that opens a request per cache artifact, HTTP/1.1 needs a
/// fresh TCP+TLS connection per concurrent request; they pile up (TIME_WAIT /
/// ephemeral-port / conntrack exhaustion) until new connects time out ("error
/// sending request"). HTTP/2 multiplexes every request onto a handful of
/// connections. object_store lets us swap the client via `with_http_connector`.
#[derive(Debug)]
struct NegotiatingConnector;

impl HttpConnector for NegotiatingConnector {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        // reqwest's rustls backend uses the process-default crypto provider.
        ensure_rustls_provider_installed();
        // Default builder = ALPN offering h2 + http/1.1 (server picks), env
        // proxies honored, no gzip/brotli features so blobs aren't decompressed.
        //
        // `http2_adaptive_window` grows the HTTP/2 flow-control window under load
        // (like Go's client). Without it, multiplexing every transfer over one
        // connection would throttle aggregate throughput on a fixed 64 KiB
        // window — the very reason object_store defaults to HTTP/1.1. With it, we
        // get h2's few-connections benefit *and* full throughput.
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .http2_adaptive_window(true)
            .build()
            .map_err(|e| object_store::Error::Generic {
                store: "remote-cache",
                source: Box::new(e),
            })?;
        Ok(HttpClient::new(client))
    }
}

/// Retry budget for the explicit store builders. The default (10 retries /
/// 180s elapsed) is sized for small objects; a genuine reset deep into a
/// multi-GiB transfer would hit the 180s elapsed cap and fail despite using a
/// single retry. Widen it so resume-via-`Range` has room.
///
/// object_store's retry clock (`retry.rs`) starts once per logical GET and
/// keeps running across every mid-stream `Range`-resume attempt — it is not
/// reset per attempt. `retry_timeout` therefore has to cover a *whole*
/// transfer's retry chain, not one [`REQUEST_TIMEOUT`] attempt: setting it
/// equal to (or below) `REQUEST_TIMEOUT` means the very first attempt on a
/// large/slow blob exhausts the retry clock at the same moment it hits the
/// per-request timeout, leaving zero budget for the resume retries
/// `max_retries` promises. Keep this several multiples of `REQUEST_TIMEOUT`
/// (a guard test enforces the margin) — still comfortably under GCS's ~1h
/// token lifetime.
fn retry_config() -> RetryConfig {
    RetryConfig {
        max_retries: 20,
        retry_timeout: Duration::from_secs(30 * 60),
        ..Default::default()
    }
}

/// Fold `opts` onto `$builder::new().with_url($url)` exactly as
/// `object_store::parse_url_opts`'s internal `builder_opts!` macro does: each
/// `(key, value)` that parses as the builder's own `ConfigKey` is applied via
/// `with_config`, everything else (most of the process environment) is
/// silently skipped. A macro rather than a generic fn because
/// `AmazonS3Builder`/`MicrosoftAzureBuilder`/`HttpBuilder` share no common
/// trait for `with_config`/`ConfigKey` — `parse_url_opts` hits the same wall
/// and works around it the same way.
macro_rules! build_with_opts {
    ($builder:ty, $url:expr, $opts:expr) => {
        $opts.into_iter().fold(
            <$builder>::new().with_url($url.to_string()),
            |builder, (key, value): (String, String)| match key.to_ascii_lowercase().parse() {
                Ok(k) => builder.with_config(k, value),
                Err(_) => builder,
            },
        )
    };
}

/// Extra options fed to [`parse_url_opts`] for networked schemes (s3/azure/http):
/// a finite request timeout above object_store's 30s default. GCS does not use
/// this path — it goes through [`NegotiatingConnector`]. Local schemes (`file`,
/// `memory`) have no HTTP client and reject client config keys, so they get
/// nothing.
fn transfer_opts(scheme: &str) -> Vec<(String, String)> {
    match scheme {
        "file" | "memory" => Vec::new(),
        _ => vec![(
            "timeout".to_string(),
            format!("{}s", REQUEST_TIMEOUT.as_secs()),
        )],
    }
}

/// Per-cache connection settings that have no place in the cache URI, from
/// `caches: { <name>: { endpoint, region } }`.
///
/// Both are S3-only: they exist to point an `s3://bucket/prefix` cache at an
/// S3-compatible service that is not AWS (Cloudflare R2, MinIO, Ceph, …), where
/// the URI still names the bucket and the prefix and the endpoint names the host
/// to talk to. Setting either on any other scheme is rejected by
/// [`ObjStoreBackend::from_uri`] rather than silently ignored.
///
/// Both override the corresponding environment variable (`AWS_ENDPOINT_URL`,
/// `AWS_REGION`), which stays supported for the case where the endpoint is a
/// property of the machine rather than of the repo.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreOptions<'a> {
    /// Base URL of the service, e.g. `https://<account>.r2.cloudflarestorage.com`
    /// or `http://localhost:9000`. An `http://` endpoint also lifts
    /// object_store's plaintext-HTTP block, which otherwise rejects every
    /// request to it — writing `http://` is the opt-in.
    pub endpoint: Option<&'a str>,
    /// Region to sign requests for. Unset leaves object_store's own resolution
    /// (`AWS_REGION`, else `us-east-1`) in place.
    pub region: Option<&'a str>,
}

impl StoreOptions<'_> {
    /// Reject endpoint/region on a store that has no notion of them. Silently
    /// dropping them would leave a cache pointed at the wrong host with nothing
    /// in the output saying so; failing at startup names the field and the URI.
    fn reject_non_s3(&self, uri: &str) -> anyhow::Result<()> {
        let field = match (self.endpoint, self.region) {
            (Some(_), _) => "endpoint",
            (_, Some(_)) => "region",
            _ => return Ok(()),
        };
        anyhow::bail!("`{field}` is only supported for s3:// remote caches, not {uri}")
    }
}

/// One remote object store plus the path prefix carved out of its URI. Object
/// keys handed to [`RemoteCacheBackend`] are joined under `prefix`, so two
/// repos can share a bucket by pointing at distinct prefixes (`s3://b/repo-a`,
/// `s3://b/repo-b`).
pub struct ObjStoreBackend {
    store: Arc<dyn ObjectStore>,
    prefix: ObjPath,
}

impl ObjStoreBackend {
    /// Build a backend from a cache URI. Pure/synchronous — constructs the
    /// client only; no network or credential validation happens here (the first
    /// real request surfaces auth errors). Env vars supply credentials.
    ///
    /// `max_concurrency` caps in-flight requests to this store via
    /// [`LimitStore`], so a wide build fan-out can't open thousands of
    /// simultaneous connections. `opts` carries the S3-only endpoint/region
    /// overrides; see [`StoreOptions`].
    pub fn from_uri(
        uri: &str,
        max_concurrency: usize,
        opts_override: &StoreOptions<'_>,
    ) -> anyhow::Result<Self> {
        let url = Url::parse(uri).with_context(|| format!("parse remote cache uri {uri}"))?;
        let (store, prefix): (Box<dyn ObjectStore>, ObjPath) = if url.scheme() == "gs" {
            opts_override.reject_non_s3(uri)?;
            // Always drive GCS through the NegotiatingConnector so transfers use
            // HTTP/2 (falling back to HTTP/1.1) instead of object_store's
            // connection-storming HTTP/1.1-only default.
            let mut builder = GoogleCloudStorageBuilder::from_env()
                .with_url(uri)
                .with_retry(retry_config())
                .with_http_connector(NegotiatingConnector);
            if adc_is_external_account() {
                // object_store can't decode an external_account ADC. Inject a
                // `google-cloud-auth`-backed bearer provider so the builder skips
                // ADC parsing; the federation handshake happens lazily on the
                // first request inside the provider.
                let provider: GcpCredentialProvider =
                    Arc::new(ExternalAccountCredentialProvider::new());
                builder = builder.with_credentials(provider);
            }
            let store = builder
                .build()
                .with_context(|| format!("build GCS store for {uri}"))?;
            // The builder consumes only the bucket from the URL; derive the key
            // prefix exactly as `parse_url_opts` would (path minus the leading
            // slash, percent-decoded).
            let prefix = ObjPath::from_url_path(url.path())
                .with_context(|| format!("parse object prefix from {uri}"))?;
            (Box::new(store), prefix)
        } else {
            // s3/azure/http: dedicated per-scheme builders so each carries
            // `retry_config()` directly, the same policy GCS gets above.
            // `parse_url_opts`'s generic dispatch has no string `ConfigKey`
            // for retry_timeout/max_retries, so these three schemes used to
            // silently fall back to object_store's `RetryConfig::default()`
            // (180s) — already below `REQUEST_TIMEOUT`, and the same
            // resume-via-`Range`-defeating bug class fixed for GCS above,
            // just never wired for the other three schemes at all.
            //
            // `ObjectStoreScheme::parse` gives the same host-aware scheme
            // detection `parse_url_opts` uses internally (e.g.
            // `https://bucket.s3.<region>.amazonaws.com` still routes to S3,
            // not a generic HTTP store) — this is a mechanical swap of
            // builder, not a change to which store a URI resolves to.
            let (scheme, path) = ObjectStoreScheme::parse(&url)
                .with_context(|| format!("recognize remote cache uri scheme {uri}"))?;
            // Environment pass-through, same as the GCS `from_env()` builder
            // above: each builder's `ConfigKey::from_str` accepts only its
            // own known aliases, so unrelated vars are silently skipped.
            // Also carries a lifted-but-finite request timeout — chained
            // after the env vars, so `transfer_opts`'s fixed value always
            // wins over an env-supplied `timeout` (`fold` applies entries in
            // order and `with_config` overwrites), not the other way round.
            let opts: Vec<(String, String)> = std::env::vars()
                .chain(transfer_opts(url.scheme()))
                .collect();
            let store: Box<dyn ObjectStore> = match scheme {
                ObjectStoreScheme::AmazonS3 => {
                    let mut builder =
                        build_with_opts!(AmazonS3Builder, uri, opts).with_retry(retry_config());
                    // Applied *after* the env fold so an endpoint/region written
                    // in `.hephconfig` wins over an ambient `AWS_ENDPOINT_URL` /
                    // `AWS_REGION` — the repo's own config is the more specific
                    // statement of where its cache lives.
                    if let Some(endpoint) = opts_override.endpoint {
                        builder = builder.with_endpoint(endpoint);
                        if endpoint.starts_with("http://") {
                            // object_store refuses plaintext HTTP unless asked;
                            // spelling out `http://` in the config is the ask.
                            builder = builder.with_allow_http(true);
                        }
                    }
                    if let Some(region) = opts_override.region {
                        builder = builder.with_region(region);
                    }
                    Box::new(
                        builder
                            .build()
                            .with_context(|| format!("build S3 store for {uri}"))?,
                    )
                }
                ObjectStoreScheme::MicrosoftAzure => {
                    opts_override.reject_non_s3(uri)?;
                    Box::new(
                        build_with_opts!(MicrosoftAzureBuilder, uri, opts)
                            .with_retry(retry_config())
                            .build()
                            .with_context(|| format!("build Azure store for {uri}"))?,
                    )
                }
                ObjectStoreScheme::Http => {
                    opts_override.reject_non_s3(uri)?;
                    let base = &url[..url::Position::BeforePath];
                    Box::new(
                        build_with_opts!(HttpBuilder, base, opts)
                            .with_retry(retry_config())
                            .build()
                            .with_context(|| format!("build HTTP store for {uri}"))?,
                    )
                }
                // `file`/`memory`: no network client, no retry semantics.
                _ => {
                    opts_override.reject_non_s3(uri)?;
                    parse_url_opts(&url, opts)
                        .with_context(|| format!("open remote cache store for {uri}"))?
                        .0
                }
            };
            (store, path)
        };
        let store: Arc<dyn ObjectStore> = Arc::new(LimitStore::new(store, max_concurrency));
        Ok(Self { store, prefix })
    }

    /// Join a logical cache key under the configured prefix. `Path::from`
    /// normalizes the segments, so a leading slash from an empty prefix (the
    /// `memory://` / bucket-root case) is dropped.
    fn object_path(&self, key: &str) -> ObjPath {
        ObjPath::from(format!("{}/{}", self.prefix, key))
    }
}

/// `AsyncRead` adapter that fails a transfer stalled for `timeout` — no bytes
/// delivered within the window. The object_store GCS client is HTTP/1.1 with no
/// keep-alive ping, so without this a dead-but-not-closed connection would hang
/// until the (generous) per-request timeout; this bounds a stall tightly. The
/// deadline is extended on every read that yields bytes.
/// Flush interval for the diag byte counter.
///
/// Bumping a shared atomic per 8 KiB chunk would put ~100k contended RMWs on one
/// cache line during a cold multi-GB pull. Accumulating locally and flushing per
/// MiB keeps the counter useful — the reporting window is 60s, far longer than
/// the time to move a MiB on any link worth diagnosing — at a fraction of the
/// traffic.
const BYTES_FLUSH: u64 = 1 << 20;

struct InactivityReader<R> {
    inner: R,
    timeout: Duration,
    deadline: Pin<Box<Sleep>>,
    /// Bytes read since the last flush to the diag table.
    pending: u64,
}

impl<R> InactivityReader<R> {
    fn new(inner: R, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: Box::pin(sleep(timeout)),
            pending: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for InactivityReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                // Any forward progress (bytes read) resets the inactivity clock.
                // EOF (a ready read with no new bytes) passes through untouched.
                let read = buf.filled().len().saturating_sub(before);
                if read != 0 {
                    let next = Instant::now() + this.timeout;
                    this.deadline.as_mut().reset(next);
                    // Bytes moving is what separates "stalled" from "slow" — with
                    // only counts and ages, a wedged socket and a slow link look
                    // identical.
                    this.pending += read as u64;
                    if this.pending >= BYTES_FLUSH {
                        crate::engine::diag::global()
                            .add_bytes(crate::engine::diag::Op::RemoteCacheRead, this.pending);
                        this.pending = 0;
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("remote read stalled: no data for {:?}", this.timeout),
                ))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl<R> Drop for InactivityReader<R> {
    fn drop(&mut self) {
        // Flush the tail, including on a cancelled transfer — otherwise the last
        // partial MiB of every pull is lost and a run that moved real bytes can
        // report zero.
        if self.pending != 0 {
            crate::engine::diag::global()
                .add_bytes(crate::engine::diag::Op::RemoteCacheRead, self.pending);
        }
    }
}

/// Convert a mid-stream object_store error into the `io::Error` that
/// [`StreamReader`] requires, folding the object path into the message so a
/// body-read failure names the artifact instead of a bare HTTP error.
fn stream_read_err(path: &ObjPath, e: object_store::Error) -> std::io::Error {
    std::io::Error::other(format!("remote object {path} stream error: {e}"))
}

#[async_trait]
impl RemoteCacheBackend for ObjStoreBackend {
    async fn open_read(&self, key: &str) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
        let path = self.object_path(key);
        match self.store.get(&path).await {
            Ok(res) => {
                let stream = res
                    .into_stream()
                    .map_err(enclose!((path) move |e| stream_read_err(&path, e)));
                let reader = InactivityReader::new(StreamReader::new(stream), INACTIVITY_TIMEOUT);
                Ok(Some(Box::pin(reader)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("open remote object {path}")),
        }
    }

    async fn open_write(&self, key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
        let path = self.object_path(key);
        // `BufWriter` performs a multipart upload under the hood, finalized on
        // `poll_shutdown`.
        //
        // `with_max_concurrency(1)` is load-bearing, not tuning. At its default
        // of 8 the writer spawns up to eight `put_part` tasks, and **each one
        // takes its own permit from the store-wide `LimitStore`** — so one heph
        // blob slot expands to eight store requests. That breaks the 1:1
        // slot-to-request invariant `split_request_budget` is built on: the
        // metadata reserve's real store headroom is only the metadata share, so
        // a few dozen concurrent uploads exhaust the whole store budget, a
        // manifest read then starts its `METADATA_TIMEOUT` clock while queued
        // behind blob parts whose own stall bound is ten minutes, and three such
        // timeouts trip the breaker — dropping a *healthy* cache for the rest of
        // the run. That is the failure PR #178 fixed one layer out.
        //
        // Raising the `LimitStore` ceiling instead would defeat the connection
        // bound it exists to impose. The cost is losing part pipelining on a
        // high-latency link for multi-GiB blobs; `REVISION_BLOB_CONCURRENCY`
        // still fans out across blobs, and can be raised if throughput regresses.
        Ok(Box::pin(
            BufWriter::new(self.store.clone(), path).with_max_concurrency(1),
        ))
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = self.object_path(key);
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("head remote object {path}")),
        }
    }

    async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let path = self.object_path(prefix);
        let mut names = Vec::new();
        let mut objects = self.store.list(Some(&path));
        while let Some(meta) = objects
            .try_next()
            .await
            .with_context(|| format!("list remote objects under {path}"))?
        {
            // Keys never nest below a revision, so the filename is the artifact
            // name as written by `RemoteCacheSet::key`.
            if let Some(name) = meta.location.filename() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }
}

/// Mints GCS bearer tokens from an `external_account` ADC via `google-cloud-auth`.
///
/// object_store calls [`get_credential`](CredentialProvider::get_credential) on
/// every request; the underlying [`AccessTokenCredentials`] caches the token and
/// refreshes it near expiry, so the expensive STS exchange happens once per token
/// lifetime, not once per request. Construction is deferred to the first call
/// (and memoized via [`OnceCell`]) so `from_uri` stays synchronous and a
/// misconfigured cache never blocks engine startup on a network handshake.
struct ExternalAccountCredentialProvider {
    creds: OnceCell<AccessTokenCredentials>,
}

impl ExternalAccountCredentialProvider {
    fn new() -> Self {
        Self {
            creds: OnceCell::new(),
        }
    }
}

impl std::fmt::Debug for ExternalAccountCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccountCredentialProvider")
            .field("initialized", &self.creds.initialized())
            .finish()
    }
}

#[async_trait]
impl CredentialProvider for ExternalAccountCredentialProvider {
    type Credential = GcpCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<GcpCredential>> {
        let creds = self
            .creds
            .get_or_try_init(|| async {
                ensure_rustls_provider_installed();
                AdcBuilder::default()
                    .with_scopes([GCS_SCOPE])
                    .build_access_token_credentials()
            })
            .await
            .map_err(|e| object_store::Error::Generic {
                store: "GCS",
                source: Box::new(e),
            })?;
        let token = creds
            .access_token()
            .await
            .map_err(|e| object_store::Error::Generic {
                store: "GCS",
                source: Box::new(e),
            })?;
        Ok(Arc::new(GcpCredential {
            bearer: token.token,
        }))
    }
}

/// True when the active GCP Application Default Credentials are an
/// `external_account` file (workload identity federation) — the one ADC shape
/// object_store's GCS builder refuses to decode.
fn adc_is_external_account() -> bool {
    adc_credential_path()
        .as_deref()
        .and_then(read_adc_type)
        .as_deref()
        == Some("external_account")
}

/// Resolve the ADC file the GCS builder would read: `GOOGLE_APPLICATION_CREDENTIALS`
/// if set, else the gcloud well-known location under `$HOME`. Mirrors
/// object_store's own resolution so detection matches what would actually load.
fn adc_credential_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home).join(".config/gcloud/application_default_credentials.json");
    path.exists().then_some(path)
}

/// Read the `type` field of an ADC JSON file. Returns `None` if the file is
/// missing or unparseable — detection then falls through to object_store, which
/// surfaces its own error.
fn read_adc_type(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("type")?.as_str().map(str::to_owned)
}

/// Install the ring rustls crypto provider as the process-wide default, once.
/// `google-cloud-auth`'s reqwest is built with `rustls-no-provider`, so the
/// first TLS handshake panics unless a default provider is installed. Idempotent:
/// `install_default` errors if one is already set, which we ignore.
fn ensure_rustls_provider_installed() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Err means another provider was already installed — equally fine.
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn put(backend: &ObjStoreBackend, key: &str, data: &[u8]) {
        let mut w = backend.open_write(key).await.expect("open_write");
        w.write_all(data).await.expect("write");
        w.shutdown().await.expect("shutdown");
    }

    async fn get(backend: &ObjStoreBackend, key: &str) -> Option<Vec<u8>> {
        let r = backend.open_read(key).await.expect("open_read")?;
        let mut r = r;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.expect("read");
        Some(buf)
    }

    /// One heph blob slot must mean one store request.
    ///
    /// `split_request_budget` divides a cache's `concurrency` into a metadata
    /// reserve and a bulk pool on exactly that basis, so the store's own
    /// `LimitStore` is never the binding constraint. `BufWriter` at its default
    /// concurrency breaks it: it spawns up to eight `put_part` tasks per writer,
    /// each taking its own store permit, and the metadata reserve's real store
    /// headroom collapses. A manifest read then starts its `METADATA_TIMEOUT`
    /// clock while queued behind blob parts bounded at ten minutes, and three of
    /// those trip the breaker on a cache that is perfectly healthy.
    #[tokio::test]
    async fn a_multipart_upload_takes_one_store_request_at_a_time() {
        use object_store::{
            GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, PutMultipartOptions,
            PutOptions, PutPayload, PutResult, Result as OsResult, UploadPart,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Delegates to an in-memory store, recording peak concurrent parts.
        #[derive(Debug)]
        struct PartCountingStore {
            inner: Arc<dyn ObjectStore>,
            inflight: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }

        impl std::fmt::Display for PartCountingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("PartCountingStore")
            }
        }

        struct CountingUpload {
            inner: Box<dyn MultipartUpload>,
            inflight: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }

        impl std::fmt::Debug for CountingUpload {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("CountingUpload")
            }
        }

        #[async_trait::async_trait]
        impl MultipartUpload for CountingUpload {
            fn put_part(&mut self, data: PutPayload) -> UploadPart {
                let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                let part = self.inner.put_part(data);
                let inflight = Arc::clone(&self.inflight);
                Box::pin(async move {
                    // Slow enough that overlapping parts overlap observably.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let r = part.await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    r
                })
            }
            async fn complete(&mut self) -> OsResult<PutResult> {
                self.inner.complete().await
            }
            async fn abort(&mut self) -> OsResult<()> {
                self.inner.abort().await
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for PartCountingStore {
            async fn put_opts(
                &self,
                location: &ObjPath,
                payload: PutPayload,
                opts: PutOptions,
            ) -> OsResult<PutResult> {
                self.inner.put_opts(location, payload, opts).await
            }
            async fn put_multipart_opts(
                &self,
                location: &ObjPath,
                opts: PutMultipartOptions,
            ) -> OsResult<Box<dyn MultipartUpload>> {
                let inner = self.inner.put_multipart_opts(location, opts).await?;
                Ok(Box::new(CountingUpload {
                    inner,
                    inflight: Arc::clone(&self.inflight),
                    peak: Arc::clone(&self.peak),
                }))
            }
            async fn get_opts(&self, location: &ObjPath, opts: GetOptions) -> OsResult<GetResult> {
                self.inner.get_opts(location, opts).await
            }
            fn delete_stream(
                &self,
                locations: futures::stream::BoxStream<'static, OsResult<ObjPath>>,
            ) -> futures::stream::BoxStream<'static, OsResult<ObjPath>> {
                self.inner.delete_stream(locations)
            }
            fn list(
                &self,
                prefix: Option<&ObjPath>,
            ) -> futures::stream::BoxStream<'static, OsResult<ObjectMeta>> {
                self.inner.list(prefix)
            }
            async fn list_with_delimiter(&self, prefix: Option<&ObjPath>) -> OsResult<ListResult> {
                self.inner.list_with_delimiter(prefix).await
            }
            async fn copy_opts(
                &self,
                from: &ObjPath,
                to: &ObjPath,
                opts: object_store::CopyOptions,
            ) -> OsResult<()> {
                self.inner.copy_opts(from, to, opts).await
            }
        }

        let (inflight, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let backend = ObjStoreBackend {
            store: Arc::new(PartCountingStore {
                inner: Arc::new(object_store::memory::InMemory::new()),
                inflight: Arc::clone(&inflight),
                peak: Arc::clone(&peak),
            }),
            prefix: ObjPath::from("repo"),
        };

        // Several parts' worth: `BufWriter`'s default part size is 10 MiB, and
        // at its default concurrency of 8 these would overlap.
        //
        // Written in chunks rather than one 45 MiB call, because that is what
        // the upload path does (`stream_file_to_backend` copies chunk by chunk)
        // and because the bound applies *between* writes: a single `write` of
        // several parts' worth splits them internally with no capacity check.
        let blob = vec![0u8; 45 * 1024 * 1024];
        let mut w = backend.open_write("big.blob").await.expect("open_write");
        for chunk in blob.chunks(256 * 1024) {
            w.write_all(chunk).await.expect("write");
        }
        w.shutdown().await.expect("shutdown");
        assert_eq!(get(&backend, "big.blob").await.expect("present"), blob);

        assert!(
            peak.load(Ordering::SeqCst) > 0,
            "the upload must actually have gone multipart"
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "one blob slot must never expand into several store requests",
        );
    }

    #[tokio::test]
    async fn memory_backend_streams_under_prefix() {
        let backend = ObjStoreBackend::from_uri("memory:///repo-a", 10, &StoreOptions::default())
            .expect("backend");
        assert!(!backend.exists("k/v").await.expect("exists"));
        assert!(get(&backend, "k/v").await.is_none());

        put(&backend, "k/v", b"hello").await;
        assert!(backend.exists("k/v").await.expect("exists"));
        assert_eq!(get(&backend, "k/v").await.expect("present"), b"hello");
    }

    #[test]
    fn stream_read_err_names_object_path_and_keeps_cause() {
        let path = ObjPath::from("repo-a/cas/deadbeef");
        let e = object_store::Error::Generic {
            store: "S3",
            source: "connection reset".into(),
        };
        let msg = stream_read_err(&path, e).to_string();
        // Path identifies the artifact; original error text is folded in so a
        // mid-stream body failure stays diagnosable.
        assert!(msg.contains("repo-a/cas/deadbeef"), "msg: {msg}");
        assert!(msg.contains("connection reset"), "msg: {msg}");
    }

    #[tokio::test]
    async fn file_backend_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uri = format!("file://{}", dir.path().display());
        let backend =
            ObjStoreBackend::from_uri(&uri, 10, &StoreOptions::default()).expect("backend");
        put(&backend, "a/b/c", b"payload").await;
        assert_eq!(get(&backend, "a/b/c").await.expect("present"), b"payload");
    }

    fn write_adc(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("adc.json");
        std::fs::write(&path, body).expect("write adc");
        path
    }

    #[test]
    fn read_adc_type_classifies_external_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Minified, single-line — the exact shape google-github-actions/auth
        // writes and that object_store fails to decode.
        let path = write_adc(
            dir.path(),
            r#"{"type":"external_account","audience":"//iam.googleapis.com/x","token_url":"https://sts.googleapis.com/v1/token"}"#,
        );
        assert_eq!(read_adc_type(&path).as_deref(), Some("external_account"));
    }

    #[test]
    fn read_adc_type_classifies_authorized_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_adc(
            dir.path(),
            r#"{"type": "authorized_user", "client_id": "x", "refresh_token": "y"}"#,
        );
        assert_eq!(read_adc_type(&path).as_deref(), Some("authorized_user"));
    }

    #[test]
    fn transfer_opts_sets_finite_timeout_on_networked_schemes_only() {
        // s3/azure/http lift the total timeout above object_store's 30s default
        // while keeping it finite. GCS uses the connector, not this path.
        for scheme in ["s3", "az", "http", "https"] {
            let opts = transfer_opts(scheme);
            assert_eq!(
                opts.iter()
                    .find(|(k, _)| k == "timeout")
                    .map(|(_, v)| v.as_str()),
                Some(format!("{}s", REQUEST_TIMEOUT.as_secs()).as_str()),
                "scheme {scheme} should carry the finite request timeout"
            );
        }
        // Local schemes have no HTTP client and reject client config keys.
        assert!(transfer_opts("file").is_empty());
        assert!(transfer_opts("memory").is_empty());
    }

    /// object_store's retry clock runs for the whole GET, including
    /// mid-stream `Range`-resume attempts — it does not reset per attempt.
    /// If `retry_timeout` were <= `REQUEST_TIMEOUT`, the first attempt on a
    /// large/slow blob would exhaust the retry clock the instant it hit the
    /// per-request timeout, leaving zero budget for any resume retry despite
    /// `max_retries` promising 20. Guards against the regression where both
    /// were narrowed to the same 5 minutes.
    #[test]
    fn retry_timeout_stays_well_above_request_timeout() {
        let retry_timeout = retry_config().retry_timeout;
        assert!(
            retry_timeout >= REQUEST_TIMEOUT * 3,
            "retry_timeout ({retry_timeout:?}) must stay several multiples of \
             REQUEST_TIMEOUT ({REQUEST_TIMEOUT:?}) so resume-via-Range retries \
             have room after the first attempt hits its per-request timeout"
        );
    }

    #[test]
    fn negotiating_connector_builds_a_client() {
        // Smoke test: the connector builds an ALPN-negotiating reqwest client
        // (installs the rustls provider, honors env proxies) without error.
        HttpConnector::connect(&NegotiatingConnector, &ClientOptions::default())
            .expect("connector builds a client");
    }

    #[tokio::test]
    async fn inactivity_reader_passes_data_through() {
        use tokio::io::AsyncReadExt;
        let data: &[u8] = b"hello remote cache world";
        let mut r = InactivityReader::new(data, Duration::from_secs(30));
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.expect("read");
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn inactivity_reader_times_out_on_stall() {
        use tokio::io::AsyncReadExt;

        // A reader that never yields bytes and never wakes — only the inactivity
        // deadline can make progress.
        struct Never;
        impl AsyncRead for Never {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut TaskContext<'_>,
                _buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }

        let mut r = InactivityReader::new(Never, Duration::from_millis(50));
        let mut buf = [0u8; 16];
        let err = r.read(&mut buf).await.expect_err("must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn retry_config_widens_budget_within_token_lifetime() {
        let r = retry_config();
        // Bigger than the 10-retry / 180s default so resume-via-Range survives a
        // reset deep into a long transfer...
        assert!(r.max_retries > 10);
        assert!(r.retry_timeout > Duration::from_secs(180));
        // ...but under the ~1h GCS bearer lifetime.
        assert!(r.retry_timeout < Duration::from_secs(60 * 60));
    }

    /// Regression guard for the bug this fix closes: s3/azure/http used to
    /// build via the generic `parse_url_opts` path, which has no string
    /// `ConfigKey` for retry_timeout/max_retries, so all three silently fell
    /// back to `RetryConfig::default()` (10 retries / 180s) instead of
    /// [`retry_config()`] — the same policy GCS gets via its dedicated
    /// builder. Every layer down to `RetryConfig` derives `Debug`, so
    /// Debug-formatting the built store (through the `dyn ObjectStore` trait
    /// object, itself `Debug`) surfaces the value without a public getter.
    ///
    /// Deliberately routes through [`ObjStoreBackend::from_uri`] itself,
    /// not a hand-built store — a hand-built one would still pass if a
    /// future edit dropped `.with_retry(retry_config())` from the actual
    /// match arm. And deliberately never embeds the Debug string in an
    /// assertion message: `from_uri` folds the live process environment into
    /// these builders, so any real `AWS_*` credentials in scope end up inside
    /// the store — printing its Debug output on failure would risk leaking
    /// them into CI logs, even though `AwsCredential`'s `Debug` impl currently
    /// redacts secrets. This repo's CI no longer supplies any: the build cache
    /// moved from sccache, which needed `AWS_ACCESS_KEY_ID` /
    /// `AWS_SECRET_ACCESS_KEY`, to kache, which reads its own
    /// `KACHE_S3_ACCESS_KEY` / `KACHE_S3_SECRET_KEY` that these builders do not
    /// look at. Keep the practice regardless — it costs nothing, and it is one
    /// `AWS_PROFILE` in a developer's shell away from mattering again.
    fn assert_wires_retry_config(store: &dyn ObjectStore, scheme: &str) {
        let debug = format!("{store:?}");
        let want = retry_config();
        assert!(
            debug.contains(&format!("max_retries: {}", want.max_retries)),
            "{scheme} store missing retry_config()'s max_retries wiring"
        );
        assert!(
            !debug.contains("max_retries: 10"),
            "{scheme} store still carries object_store's RetryConfig::default() (max_retries: 10)"
        );
    }

    #[test]
    fn s3_scheme_wires_retry_config_not_object_store_default() {
        let backend =
            ObjStoreBackend::from_uri("s3://some-bucket/prefix", 10, &StoreOptions::default())
                .expect("backend");
        assert_wires_retry_config(&*backend.store, "S3");
    }

    #[test]
    fn azure_scheme_wires_retry_config_not_object_store_default() {
        let backend = ObjStoreBackend::from_uri(
            "abfss://some-container@some-account.dfs.core.windows.net/prefix",
            10,
            &StoreOptions::default(),
        )
        .expect("backend");
        assert_wires_retry_config(&*backend.store, "Azure");
    }

    /// A custom endpoint has to reach the built client, not just the builder:
    /// object_store folds it into `S3Config::bucket_endpoint`, which is what
    /// every request URL is built from. Same Debug-introspection trick (and the
    /// same never-print-the-string rule) as [`assert_wires_retry_config`].
    #[test]
    fn s3_endpoint_and_region_reach_the_built_store() {
        let backend = ObjStoreBackend::from_uri(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions {
                endpoint: Some("https://accountid.r2.cloudflarestorage.com"),
                region: Some("auto"),
            },
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("https://accountid.r2.cloudflarestorage.com/some-bucket"),
            "custom endpoint never reached the S3 client's request URL"
        );
        assert!(
            !debug.contains("amazonaws.com"),
            "S3 client still points at AWS despite a configured endpoint"
        );
        assert!(
            debug.contains("auto"),
            "configured region never reached the S3 client"
        );
    }

    /// A plaintext endpoint (a local MinIO, a test double) is otherwise refused
    /// by object_store's `allow_http` guard on the first request — writing
    /// `http://` in the config is the opt-in, so nothing else has to be set.
    #[test]
    fn s3_http_endpoint_opts_into_plaintext() {
        let backend = ObjStoreBackend::from_uri(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions {
                endpoint: Some("http://localhost:9000"),
                region: None,
            },
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("http://localhost:9000/some-bucket"),
            "plaintext endpoint never reached the S3 client's request URL"
        );
        assert!(
            debug.contains("allow_http: Parsed(true)"),
            "an http:// endpoint must lift object_store's plaintext block"
        );
    }

    /// An https endpoint must NOT lift the guard — the opt-in is scoped to the
    /// scheme actually written, so a typo elsewhere can't silently downgrade a
    /// TLS connection.
    #[test]
    fn s3_https_endpoint_leaves_plaintext_blocked() {
        let backend = ObjStoreBackend::from_uri(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions {
                endpoint: Some("https://accountid.r2.cloudflarestorage.com"),
                region: None,
            },
        )
        .expect("backend");
        assert!(
            !format!("{:?}", backend.store).contains("allow_http: Parsed(true)"),
            "an https:// endpoint must leave plaintext HTTP blocked"
        );
    }

    /// Endpoint/region are S3-only. On any other scheme they are a config
    /// mistake, and silently dropping them would leave the cache pointed
    /// somewhere the config does not say — fail at startup, naming the field.
    #[test]
    fn endpoint_or_region_on_a_non_s3_scheme_is_rejected() {
        for uri in [
            "gs://some-bucket/prefix",
            "abfss://some-container@some-account.dfs.core.windows.net/prefix",
            "https://example.com/prefix",
            "memory:///repo-a",
        ] {
            let err = ObjStoreBackend::from_uri(
                uri,
                10,
                &StoreOptions {
                    endpoint: Some("https://example.invalid"),
                    region: None,
                },
            )
            .err()
            .expect("endpoint on a non-s3 scheme must not be accepted");
            let msg = format!("{err:#}");
            assert!(msg.contains("endpoint") && msg.contains(uri), "{msg}");

            let err = ObjStoreBackend::from_uri(
                uri,
                10,
                &StoreOptions {
                    endpoint: None,
                    region: Some("auto"),
                },
            )
            .err()
            .expect("region on a non-s3 scheme must not be accepted");
            assert!(format!("{err:#}").contains("region"), "{err:#}");
        }
    }

    #[test]
    fn http_scheme_wires_retry_config_not_object_store_default() {
        let backend =
            ObjStoreBackend::from_uri("http://example.com/prefix", 10, &StoreOptions::default())
                .expect("backend");
        assert_wires_retry_config(&*backend.store, "HTTP");
    }

    #[test]
    fn read_adc_type_none_on_missing_or_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_adc_type(&dir.path().join("nope.json")), None);
        let garbage = write_adc(dir.path(), "not json at all");
        assert_eq!(read_adc_type(&garbage), None);
        let no_type = write_adc(dir.path(), r#"{"audience": "x"}"#);
        assert_eq!(read_adc_type(&no_type), None);
    }
}
