//! [`RemoteCacheBackend`] backed by the `object_store` crate. A single backend
//! type serves every supported URI scheme — `s3://`, `gs://`, `az://`,
//! `http(s)://`, `memory://`, `file://`. GCS, S3, Azure, and HTTP each go
//! through their own `object_store` builder (so every networked scheme can
//! carry [`retry_config()`] directly); `file`/`memory` fall back to
//! `object_store::parse_url_opts`, which dispatches on the scheme and returns
//! the right store. Credentials are read from the process environment (e.g.
//! `AWS_ACCESS_KEY_ID`, `GOOGLE_SERVICE_ACCOUNT`) by feeding `std::env::vars()`
//! to the builder, mirroring each builder's `from_env` — or, when the ambient
//! cloud environment belongs to something other than the cache, from heph's own
//! `HEPH_S3_*` / `HEPH_GCS_*` / `HEPH_AZURE_*` / `HEPH_HTTP_*` namespace; see
//! [`SchemeEnv`]. An `s3://` cache can additionally be pointed at a non-AWS
//! S3-compatible service from the config file — see [`StoreOptions`].
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
use google_cloud_auth::credentials::external_account::Builder as ExternalAccountBuilder;
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder as AdcBuilder};
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::buffered::BufWriter;
use object_store::client::{HttpClient, HttpConnector};
use object_store::gcp::{
    GcpCredential, GcpCredentialProvider, GoogleCloudStorageBuilder, GoogleConfigKey,
};
use object_store::http::HttpBuilder;
use object_store::limit::LimitStore;
use object_store::{
    ClientConfigKey, ClientOptions, CredentialProvider, ObjectStore, ObjectStoreExt,
    ObjectStoreScheme, RetryConfig, parse_url_opts, path::Path as ObjPath,
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

/// Env-var prefix that scopes a remote-cache setting to heph, one per store kind.
///
/// The ambient cloud environment is not heph's to claim: it is shared with every
/// other tool on the machine *and* with the targets heph builds. A repo whose
/// cache lives in Cloudflare R2 needs `AWS_ACCESS_KEY_ID` to hold an R2 key —
/// which is then the wrong key for every rule that talks to real AWS, and there
/// is only one of that name. These prefixes give the cache credentials of its
/// own instead of fighting over the shared names. The suffix is the store's own
/// setting name, so the translation is mechanical: `HEPH_S3_ACCESS_KEY_ID` for
/// `AWS_ACCESS_KEY_ID`, `HEPH_S3_ENDPOINT_URL` for `AWS_ENDPOINT_URL`,
/// `HEPH_GCS_SERVICE_ACCOUNT` for `GOOGLE_SERVICE_ACCOUNT`,
/// `HEPH_AZURE_ACCOUNT_NAME` for `AZURE_STORAGE_ACCOUNT_NAME`. The vendor half
/// of the name is optional and equivalent (`HEPH_S3_AWS_ACCESS_KEY_ID` names the
/// same setting), because the builders accept both spellings themselves.
const S3_ENV_PREFIX: &str = "HEPH_S3_";
/// `gs://` counterpart of [`S3_ENV_PREFIX`].
const GCS_ENV_PREFIX: &str = "HEPH_GCS_";
/// `az://`/`abfss://` counterpart of [`S3_ENV_PREFIX`].
const AZURE_ENV_PREFIX: &str = "HEPH_AZURE_";
/// `http(s)://` counterpart of [`S3_ENV_PREFIX`].
const HTTP_ENV_PREFIX: &str = "HEPH_HTTP_";

/// The environment one store builder should see, split by whether it was aimed
/// at heph.
///
/// **A scoped variable takes the whole environment, not just its own key.** A
/// credential set is atomic. Merging per-key would let an `AWS_SESSION_TOKEN`
/// left over from an SSO login ride along with a `HEPH_S3_ACCESS_KEY_ID` /
/// `HEPH_S3_SECRET_ACCESS_KEY` pair meant for R2, signing every request with a
/// token that key never issued — a 403 with no trace of its cause in either the
/// config or the variables the user set. So the moment a single
/// `HEPH_<KIND>_*` variable is present, the ambient environment is dropped for
/// that store and the heph namespace describes it alone. Setting one means
/// setting all of them, which is the point: the cache's credentials stop being
/// a function of whatever else the shell happens to carry.
///
/// The config file's `endpoint`/`region` (see [`StoreOptions`]) still win over
/// both — they are the repo's own statement of where its cache lives.
#[derive(Debug, Default)]
struct SchemeEnv {
    /// `HEPH_<KIND>_*` entries: prefix stripped, name lowercased, each already
    /// validated against the builder's own `ConfigKey`.
    scoped: Vec<(String, String)>,
    /// Everything else, verbatim — the ambient environment the builder's own
    /// `from_env` would read. Empty whenever `scoped` is not.
    ambient: Vec<(String, String)>,
}

impl SchemeEnv {
    /// Split `env` around `prefix`, rejecting a scoped name the builder does not
    /// know.
    ///
    /// `K` is the builder's own config-key type, whose `FromStr` decides what is
    /// a real setting. Unlike the ambient environment — where most variables
    /// have nothing to do with object storage and silently skipping them is the
    /// only option — `prefix` is heph's namespace, so a name that does not parse
    /// is a typo. Failing here names it at engine startup instead of surfacing
    /// later as an unexplained missing credential.
    fn split<K: std::str::FromStr>(
        prefix: &str,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> anyhow::Result<Self> {
        let mut this = Self::default();
        for (name, value) in env {
            let Some(rest) = strip_prefix_ascii_case(&name, prefix) else {
                this.ambient.push((name, value));
                continue;
            };
            let key = rest.to_ascii_lowercase();
            if key.parse::<K>().is_err() {
                anyhow::bail!(
                    "unknown remote cache setting `{name}`: `{prefix}` is heph's own \
                     namespace, and `{key}` is not a setting this store understands"
                );
            }
            this.scoped.push((key, value));
        }
        if !this.scoped.is_empty() {
            this.ambient.clear();
        }
        Ok(this)
    }

    /// The `(key, value)` list to fold onto a builder. Order *is* precedence,
    /// since `with_config` overwrites: the ambient environment first, then
    /// heph's fixed transfer settings — so a stray `TIMEOUT` in the environment,
    /// aimed at nothing in particular, cannot chop a multi-GiB transfer — then
    /// the scoped namespace last, because `HEPH_<KIND>_TIMEOUT` *is* aimed at
    /// this cache and gets the last word.
    fn opts(self, scheme: &str) -> Vec<(String, String)> {
        self.ambient
            .into_iter()
            .chain(transfer_opts(scheme))
            .chain(self.scoped)
            .collect()
    }
}

/// `str::strip_prefix`, case-insensitive over ASCII. Environment variable names
/// are conventionally uppercase, but nothing enforces it and a lowercase
/// `heph_s3_access_key_id` should not silently become an ambient variable.
fn strip_prefix_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `split_at_checked` rather than indexing: a name shorter than the prefix,
    // or one whose first character is multi-byte, has no boundary there.
    let (head, rest) = s.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
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
        Self::from_uri_with_env(uri, max_concurrency, opts_override, std::env::vars())
    }

    /// [`from_uri`](Self::from_uri) with the environment passed in, so the
    /// credential-namespace rules of [`SchemeEnv`] are testable without mutating
    /// — and racing on — the process environment.
    ///
    /// One seam is not covered by `env`: the `gs://` *ambient* path delegates to
    /// object_store's own `from_env`, which reads the process environment
    /// directly (and reads it more narrowly than folding everything would — see
    /// there). `env` still decides whether that path is taken at all.
    fn from_uri_with_env(
        uri: &str,
        max_concurrency: usize,
        opts_override: &StoreOptions<'_>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> anyhow::Result<Self> {
        let url = Url::parse(uri).with_context(|| format!("parse remote cache uri {uri}"))?;
        let (store, prefix): (Box<dyn ObjectStore>, ObjPath) = if url.scheme() == "gs" {
            opts_override.reject_non_s3(uri)?;
            let genv = SchemeEnv::split::<GoogleConfigKey>(GCS_ENV_PREFIX, env)?;
            let external = external_account_source(&genv);
            // Ambient: object_store's own `from_env`, which also honors the bare
            // `SERVICE_ACCOUNT` variable and restricts itself to `GOOGLE_*` —
            // folding the whole environment here instead would let a bare
            // `BUCKET` or `BASE_URL` redirect the cache. Scoped: only what the
            // `HEPH_GCS_*` namespace says, nothing ambient.
            let mut builder = if genv.scoped.is_empty() {
                GoogleCloudStorageBuilder::from_env()
            } else {
                genv.scoped.iter().fold(
                    GoogleCloudStorageBuilder::new(),
                    |builder, (key, value)| {
                        match key.parse() {
                            Ok(k) => builder.with_config(k, value),
                            // Unreachable: `split` already parsed every key.
                            Err(_) => builder,
                        }
                    },
                )
            };
            // Always drive GCS through the NegotiatingConnector so transfers use
            // HTTP/2 (falling back to HTTP/1.1) instead of object_store's
            // connection-storming HTTP/1.1-only default.
            builder = builder
                .with_url(uri)
                .with_retry(retry_config())
                .with_http_connector(NegotiatingConnector);
            if let Some(source) = external {
                // object_store can't decode an external_account credential.
                // Inject a `google-cloud-auth`-backed bearer provider so the
                // builder skips ADC parsing; the federation handshake happens
                // lazily on the first request inside the provider.
                let provider: GcpCredentialProvider =
                    Arc::new(ExternalAccountCredentialProvider::new(source));
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
            // Environment pass-through, same as the GCS builder above: each
            // builder's `ConfigKey::from_str` accepts only its own known
            // aliases, so unrelated vars are silently skipped — unless they
            // carry the builder's `HEPH_<KIND>_*` prefix, which replaces the
            // ambient environment outright ([`SchemeEnv`]). The list also
            // carries a lifted-but-finite request timeout; see
            // [`SchemeEnv::opts`] for why it sits where it does.
            let store: Box<dyn ObjectStore> = match scheme {
                ObjectStoreScheme::AmazonS3 => {
                    let opts = SchemeEnv::split::<AmazonS3ConfigKey>(S3_ENV_PREFIX, env)?
                        .opts(url.scheme());
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
                    let opts = SchemeEnv::split::<AzureConfigKey>(AZURE_ENV_PREFIX, env)?
                        .opts(url.scheme());
                    Box::new(
                        build_with_opts!(MicrosoftAzureBuilder, uri, opts)
                            .with_retry(retry_config())
                            .build()
                            .with_context(|| format!("build Azure store for {uri}"))?,
                    )
                }
                ObjectStoreScheme::Http => {
                    opts_override.reject_non_s3(uri)?;
                    let opts = SchemeEnv::split::<ClientConfigKey>(HTTP_ENV_PREFIX, env)?
                        .opts(url.scheme());
                    let base = &url[..url::Position::BeforePath];
                    Box::new(
                        build_with_opts!(HttpBuilder, base, opts)
                            .with_retry(retry_config())
                            .build()
                            .with_context(|| format!("build HTTP store for {uri}"))?,
                    )
                }
                // `file`/`memory`: no network client, no retry semantics — and
                // no credentials, so no `HEPH_*` namespace to carve out either.
                _ => {
                    opts_override.reject_non_s3(uri)?;
                    parse_url_opts(&url, env)
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

/// Where an `external_account` credential is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExternalAccountSource {
    /// The ambient Application Default Credentials, resolved by
    /// `google-cloud-auth`'s own environment lookup — the same file
    /// object_store would have loaded.
    Adc,
    /// The file named by `HEPH_GCS_APPLICATION_CREDENTIALS`. In scoped mode the
    /// ambient ADC is deliberately not consulted, so the file is read here and
    /// handed to `google-cloud-auth` directly rather than through its env
    /// resolution, which would find the ambient one instead.
    File(PathBuf),
}

/// Mints GCS bearer tokens from an `external_account` credential via
/// `google-cloud-auth`.
///
/// object_store calls [`get_credential`](CredentialProvider::get_credential) on
/// every request; the underlying [`AccessTokenCredentials`] caches the token and
/// refreshes it near expiry, so the expensive STS exchange happens once per token
/// lifetime, not once per request. Construction is deferred to the first call
/// (and memoized via [`OnceCell`]) so `from_uri` stays synchronous and a
/// misconfigured cache never blocks engine startup on a network handshake.
struct ExternalAccountCredentialProvider {
    source: ExternalAccountSource,
    creds: OnceCell<AccessTokenCredentials>,
}

impl ExternalAccountCredentialProvider {
    fn new(source: ExternalAccountSource) -> Self {
        Self {
            source,
            creds: OnceCell::new(),
        }
    }
}

impl std::fmt::Debug for ExternalAccountCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccountCredentialProvider")
            .field("source", &self.source)
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
                match &self.source {
                    ExternalAccountSource::Adc => AdcBuilder::default()
                        .with_scopes([GCS_SCOPE])
                        .build_access_token_credentials()
                        .map_err(gcs_error),
                    ExternalAccountSource::File(path) => {
                        let bytes = std::fs::read(path).map_err(|e| {
                            gcs_error(anyhow::anyhow!(
                                "read {GCS_ENV_PREFIX}APPLICATION_CREDENTIALS ({}): {e}",
                                path.display()
                            ))
                        })?;
                        let json = serde_json::from_slice(&bytes).map_err(|e| {
                            gcs_error(anyhow::anyhow!("parse {}: {e}", path.display()))
                        })?;
                        ExternalAccountBuilder::new(json)
                            .with_scopes([GCS_SCOPE])
                            .build_access_token_credentials()
                            .map_err(gcs_error)
                    }
                }
            })
            .await?;
        let token = creds.access_token().await.map_err(gcs_error)?;
        Ok(Arc::new(GcpCredential {
            bearer: token.token,
        }))
    }
}

/// Wrap any error as an object_store GCS error.
fn gcs_error(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> object_store::Error {
    object_store::Error::Generic {
        store: "GCS",
        source: source.into(),
    }
}

/// Which `external_account` credential — if any — object_store's GCS builder
/// would choke on, and where to read it from.
///
/// `external_account` (workload identity federation, e.g. what
/// `google-github-actions/auth` writes in CI) is the one credential shape
/// object_store refuses to decode, so it has to be minted out-of-band. Which
/// file that is follows the same rule as every other setting: ambient mode looks
/// at the ADC object_store itself would load, scoped mode looks only at
/// `HEPH_GCS_APPLICATION_CREDENTIALS`. Without the second half, a scoped cache
/// pointed at a federated identity would fail inside object_store's parser with
/// no mention of the variable that selected the file.
fn external_account_source(env: &SchemeEnv) -> Option<ExternalAccountSource> {
    if env.scoped.is_empty() {
        let path = adc_credential_path()?;
        return is_external_account(&path).then_some(ExternalAccountSource::Adc);
    }
    let path = env.scoped.iter().find_map(|(key, value)| {
        // `split` lowercases and strips the prefix; object_store accepts the
        // name with or without its `google_` half, so both spellings arrive.
        matches!(
            key.as_str(),
            "application_credentials" | "google_application_credentials"
        )
        .then(|| PathBuf::from(value))
    })?;
    is_external_account(&path).then_some(ExternalAccountSource::File(path))
}

/// True when `path` holds an `external_account` credential file.
fn is_external_account(path: &Path) -> bool {
    read_adc_type(path).as_deref() == Some("external_account")
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

    /// Env pairs, spelled the way a shell would.
    fn env(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The whole point of the namespace: one `HEPH_S3_*` variable takes the
    /// environment, it does not merge into it. Merging per-key is what leaks a
    /// stale `AWS_SESSION_TOKEN` into a static R2 key pair.
    #[test]
    fn one_scoped_var_replaces_the_whole_ambient_environment() {
        let split = SchemeEnv::split::<AmazonS3ConfigKey>(
            S3_ENV_PREFIX,
            env(&[
                ("AWS_ACCESS_KEY_ID", "ambient"),
                ("AWS_SECRET_ACCESS_KEY", "ambient-secret"),
                ("AWS_SESSION_TOKEN", "ambient-token"),
                ("PATH", "/usr/bin"),
                ("HEPH_S3_ACCESS_KEY_ID", "scoped"),
            ]),
        )
        .expect("split");
        assert_eq!(
            split.scoped,
            env(&[("access_key_id", "scoped")]),
            "prefix must be stripped and the name lowercased"
        );
        assert!(
            split.ambient.is_empty(),
            "a scoped variable must drop the ambient environment, not merge with it"
        );
    }

    /// With nothing scoped, the ambient environment passes through untouched —
    /// the behaviour every existing cache relies on.
    #[test]
    fn without_a_scoped_var_the_ambient_environment_passes_through() {
        let split = SchemeEnv::split::<AmazonS3ConfigKey>(
            S3_ENV_PREFIX,
            env(&[("AWS_ACCESS_KEY_ID", "ambient"), ("PATH", "/usr/bin")]),
        )
        .expect("split");
        assert!(split.scoped.is_empty());
        assert_eq!(
            split.ambient,
            env(&[("AWS_ACCESS_KEY_ID", "ambient"), ("PATH", "/usr/bin")])
        );
    }

    /// `HEPH_S3_` is heph's own namespace, so a name the store does not know is
    /// a typo, not an unrelated variable. Silently skipping it — the only
    /// option for the ambient environment — would surface much later as a
    /// missing credential with nothing pointing at the misspelling.
    #[test]
    fn an_unknown_scoped_setting_is_rejected_by_name() {
        let err = SchemeEnv::split::<AmazonS3ConfigKey>(
            S3_ENV_PREFIX,
            env(&[("HEPH_S3_ACCES_KEY_ID", "typo")]),
        )
        .expect_err("an unknown scoped setting must not be ignored");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HEPH_S3_ACCES_KEY_ID") && msg.contains("acces_key_id"),
            "{msg}"
        );

        // Every scheme's namespace validates against its own store's keys: an
        // Azure setting is not an S3 setting.
        assert!(
            SchemeEnv::split::<AmazonS3ConfigKey>(
                S3_ENV_PREFIX,
                env(&[("HEPH_S3_ACCOUNT_NAME", "not-an-s3-setting")])
            )
            .is_err()
        );
        for (prefix, name) in [
            (GCS_ENV_PREFIX, "HEPH_GCS_NOPE"),
            (AZURE_ENV_PREFIX, "HEPH_AZURE_NOPE"),
            (HTTP_ENV_PREFIX, "HEPH_HTTP_NOPE"),
        ] {
            let one = env(&[(name, "x")]);
            let rejected = match prefix {
                GCS_ENV_PREFIX => SchemeEnv::split::<GoogleConfigKey>(prefix, one).is_err(),
                AZURE_ENV_PREFIX => SchemeEnv::split::<AzureConfigKey>(prefix, one).is_err(),
                _ => SchemeEnv::split::<ClientConfigKey>(prefix, one).is_err(),
            };
            assert!(rejected, "{name} must be rejected");
        }
    }

    /// A bare `HEPH_S3_` with nothing after it names no setting at all.
    #[test]
    fn a_bare_prefix_is_rejected() {
        assert!(
            SchemeEnv::split::<AmazonS3ConfigKey>(S3_ENV_PREFIX, env(&[("HEPH_S3_", "x")]))
                .is_err()
        );
    }

    /// Environment variable names are conventionally uppercase but nothing
    /// enforces it; a lowercase spelling must not fall through to the ambient
    /// side, where it would be silently ignored *and* leave the real
    /// credentials in play.
    #[test]
    fn the_prefix_matches_case_insensitively() {
        let split = SchemeEnv::split::<AmazonS3ConfigKey>(
            S3_ENV_PREFIX,
            env(&[("heph_s3_access_key_id", "scoped")]),
        )
        .expect("split");
        assert_eq!(split.scoped, env(&[("access_key_id", "scoped")]));
    }

    /// A short name cannot be indexed at the prefix length, and a multi-byte
    /// first character cannot be sliced there at all.
    #[test]
    fn prefix_matching_survives_short_and_non_ascii_names() {
        let split = SchemeEnv::split::<AmazonS3ConfigKey>(
            S3_ENV_PREFIX,
            env(&[("H", "short"), ("ÉCOLE", "non-ascii")]),
        )
        .expect("split");
        assert!(split.scoped.is_empty());
        assert_eq!(split.ambient.len(), 2);
    }

    /// Order is precedence. heph's fixed transfer settings outrank the ambient
    /// environment (an untargeted `TIMEOUT` must not chop a multi-GiB
    /// transfer), and the scoped namespace outranks both — it is an explicit
    /// statement about this cache.
    #[test]
    fn opts_rank_scoped_over_transfer_defaults_over_ambient() {
        let opts = SchemeEnv {
            ambient: Vec::new(),
            scoped: env(&[("timeout", "42s")]),
        }
        .opts("s3");
        let last = opts
            .iter()
            .rposition(|(k, _)| k == "timeout")
            .expect("timeout present");
        assert_eq!(
            opts[last].1, "42s",
            "a scoped setting must have the last word"
        );

        let opts = SchemeEnv {
            ambient: env(&[("TIMEOUT", "1s")]),
            scoped: Vec::new(),
        }
        .opts("s3");
        assert_eq!(
            opts.last().map(|(_, v)| v.as_str()),
            Some(format!("{}s", REQUEST_TIMEOUT.as_secs()).as_str()),
            "heph's transfer timeout must outrank an ambient one"
        );
    }

    /// The reported bug, end to end: the shell holds real AWS credentials for
    /// the rules being built, and the cache lives in R2. The store must sign
    /// with the heph key — and must *not* carry the ambient session token,
    /// which would make every request fail a signature it never issued.
    #[test]
    fn s3_scoped_env_replaces_ambient_aws_credentials() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions::default(),
            env(&[
                ("AWS_ACCESS_KEY_ID", "AMBIENTKEYID"),
                ("AWS_SECRET_ACCESS_KEY", "ambient-secret"),
                ("AWS_SESSION_TOKEN", "ambient-session-token"),
                ("HEPH_S3_ACCESS_KEY_ID", "SCOPEDKEYID"),
                ("HEPH_S3_SECRET_ACCESS_KEY", "scoped-secret"),
            ]),
        )
        .expect("backend");
        // Same never-print-the-Debug-string rule as `assert_wires_retry_config`:
        // `AwsCredential`'s Debug redacts the secret but not the key id.
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("SCOPEDKEYID"),
            "the HEPH_S3_ key id never reached the S3 client"
        );
        assert!(
            !debug.contains("AMBIENTKEYID"),
            "the ambient AWS key id must not survive a HEPH_S3_ override"
        );
        assert!(
            debug.contains("token: None"),
            "an ambient session token must not ride along with scoped static keys"
        );
    }

    /// Control for the above: with no `HEPH_S3_*` set, the ambient AWS
    /// environment is still exactly what configures the store.
    #[test]
    fn s3_ambient_aws_credentials_still_configure_the_store() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions::default(),
            env(&[
                ("AWS_ACCESS_KEY_ID", "AMBIENTKEYID"),
                ("AWS_SECRET_ACCESS_KEY", "ambient-secret"),
                ("AWS_SESSION_TOKEN", "ambient-session-token"),
            ]),
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(debug.contains("AMBIENTKEYID"));
        assert!(
            debug.contains("token: Some"),
            "an ambient session token belongs to the ambient credential set"
        );
    }

    /// The endpoint is how an `s3://` URI reaches a non-AWS service, so it has
    /// to be settable from the namespace too — a machine whose cache endpoint
    /// is not the repo's business.
    #[test]
    fn s3_scoped_env_can_redirect_the_endpoint() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions::default(),
            env(&[
                ("AWS_ENDPOINT_URL", "https://ambient.invalid"),
                ("HEPH_S3_ENDPOINT_URL", "https://scoped.invalid"),
                ("HEPH_S3_REGION", "auto"),
            ]),
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("https://scoped.invalid/some-bucket"),
            "the scoped endpoint never reached the S3 client's request URL"
        );
        assert!(!debug.contains("ambient.invalid"));
        assert!(debug.contains("auto"));
    }

    /// The config file is the repo's own statement of where its cache lives, so
    /// it outranks the environment — the scoped namespace included.
    #[test]
    fn config_endpoint_outranks_the_scoped_env() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "s3://some-bucket/prefix",
            10,
            &StoreOptions {
                endpoint: Some("https://from-config.invalid"),
                region: None,
            },
            env(&[("HEPH_S3_ENDPOINT_URL", "https://scoped.invalid")]),
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("https://from-config.invalid/some-bucket"),
            "the config endpoint must outrank a scoped one"
        );
        assert!(!debug.contains("scoped.invalid"));
    }

    #[test]
    fn azure_scoped_env_replaces_the_ambient_endpoint() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "abfss://some-container@some-account.dfs.core.windows.net/prefix",
            10,
            &StoreOptions::default(),
            env(&[
                ("AZURE_STORAGE_ENDPOINT", "https://ambient.invalid"),
                ("HEPH_AZURE_ENDPOINT", "https://scoped.invalid"),
            ]),
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("scoped.invalid"),
            "the HEPH_AZURE_ endpoint never reached the Azure client"
        );
        assert!(!debug.contains("ambient.invalid"));
    }

    #[test]
    fn http_scoped_env_reaches_the_built_store() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "https://example.com/prefix",
            10,
            &StoreOptions::default(),
            env(&[
                ("PROXY_URL", "http://ambient.invalid:3128"),
                ("HEPH_HTTP_PROXY_URL", "http://scoped.invalid:3128"),
            ]),
        )
        .expect("backend");
        let debug = format!("{:?}", backend.store);
        assert!(
            debug.contains("scoped.invalid:3128"),
            "the HEPH_HTTP_ proxy never reached the HTTP client"
        );
        assert!(!debug.contains("ambient.invalid"));
    }

    /// GCS's ambient path is object_store's own `from_env`, which reads the
    /// process environment; the scoped path must not, so a `HEPH_GCS_*`
    /// credential has to configure the store on its own.
    #[test]
    fn gcs_scoped_env_configures_the_store() {
        let backend = ObjStoreBackend::from_uri_with_env(
            "gs://some-bucket/prefix",
            10,
            &StoreOptions::default(),
            env(&[("HEPH_GCS_BEARER_TOKEN", "scoped-bearer-token")]),
        )
        .expect("backend");
        assert!(
            format!("{:?}", backend.store).contains("scoped-bearer-token"),
            "the HEPH_GCS_ bearer token never reached the GCS client"
        );
        assert_eq!(backend.prefix.as_ref(), "prefix");
    }

    /// `external_account` is the one credential shape object_store cannot
    /// decode, so it is minted out-of-band — and in scoped mode the file to
    /// mint it from is the scoped one, never the ambient ADC.
    #[test]
    fn external_account_source_follows_the_scoped_namespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let federated = write_adc(dir.path(), r#"{"type": "external_account"}"#);
        let scoped = SchemeEnv::split::<GoogleConfigKey>(
            GCS_ENV_PREFIX,
            env(&[(
                "HEPH_GCS_APPLICATION_CREDENTIALS",
                &federated.to_string_lossy(),
            )]),
        )
        .expect("split");
        assert_eq!(
            external_account_source(&scoped),
            Some(ExternalAccountSource::File(federated))
        );

        // A service-account key is decodable by object_store itself — no
        // out-of-band provider, or the native path would be bypassed for a
        // credential type that works.
        let sa = dir.path().join("sa.json");
        std::fs::write(&sa, r#"{"type": "service_account"}"#).expect("write");
        let scoped = SchemeEnv::split::<GoogleConfigKey>(
            GCS_ENV_PREFIX,
            env(&[("HEPH_GCS_APPLICATION_CREDENTIALS", &sa.to_string_lossy())]),
        )
        .expect("split");
        assert_eq!(external_account_source(&scoped), None);

        // Scoped, but saying nothing about credentials: the ambient ADC is out
        // of scope, so there is nothing to mint from.
        let scoped =
            SchemeEnv::split::<GoogleConfigKey>(GCS_ENV_PREFIX, env(&[("HEPH_GCS_BUCKET", "b")]))
                .expect("split");
        assert_eq!(external_account_source(&scoped), None);
    }
}
