//! [`RemoteCacheBackend`] backed by the `object_store` crate. A single backend
//! type serves every supported URI scheme — `s3://`, `gs://`, `memory://`,
//! `file://` — because `object_store::parse_url_opts` dispatches on the scheme
//! and returns the right store. Credentials are read from the process
//! environment (e.g. `AWS_ACCESS_KEY_ID`, `GOOGLE_SERVICE_ACCOUNT`) by feeding
//! `std::env::vars()` to the builder, mirroring each builder's `from_env`.
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
use futures::TryStreamExt;
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder as AdcBuilder};
use object_store::buffered::BufWriter;
use object_store::gcp::{GcpCredential, GcpCredentialProvider, GoogleCloudStorageBuilder};
use object_store::limit::LimitStore;
use object_store::{
    CredentialProvider, ObjectStore, ObjectStoreExt, parse_url_opts, path::Path as ObjPath,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::OnceCell;
use tokio_util::io::StreamReader;
use url::Url;

/// OAuth scope for read/write access to GCS objects — what a remote cache needs.
const GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

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
    /// simultaneous connections.
    pub fn from_uri(uri: &str, max_concurrency: usize) -> anyhow::Result<Self> {
        let url = Url::parse(uri).with_context(|| format!("parse remote cache uri {uri}"))?;
        let (store, prefix): (Box<dyn ObjectStore>, ObjPath) =
            if url.scheme() == "gs" && adc_is_external_account() {
                // object_store can't decode an external_account ADC. Inject a
                // `google-cloud-auth`-backed bearer provider so the GCS builder
                // skips ADC parsing; the federation handshake happens lazily on
                // the first request inside the provider.
                let provider: GcpCredentialProvider =
                    Arc::new(ExternalAccountCredentialProvider::new());
                let store = GoogleCloudStorageBuilder::from_env()
                    .with_url(uri)
                    .with_credentials(provider)
                    .build()
                    .with_context(|| {
                        format!(
                            "build GCS store for {uri} (external_account ADC via google-cloud-auth)"
                        )
                    })?;
                // The builder consumes only the bucket from the URL; derive the
                // key prefix exactly as `parse_url_opts` would (path minus the
                // leading slash, percent-decoded).
                let prefix = ObjPath::from_url_path(url.path())
                    .with_context(|| format!("parse object prefix from {uri}"))?;
                (Box::new(store), prefix)
            } else {
                // Pass the environment through so s3/gcs/azure builders pick up
                // credentials exactly as their `from_env` constructors would.
                parse_url_opts(&url, std::env::vars())
                    .with_context(|| format!("open remote cache store for {uri}"))?
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

#[async_trait]
impl RemoteCacheBackend for ObjStoreBackend {
    async fn open_read(&self, key: &str) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
        let path = self.object_path(key);
        match self.store.get(&path).await {
            Ok(res) => {
                // Adapt the object byte stream into an `AsyncRead`. `io::Error`
                // is what `StreamReader` requires; the object_store error is
                // preserved as its source.
                let stream = res.into_stream().map_err(std::io::Error::other);
                Ok(Some(Box::pin(StreamReader::new(stream))))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("open remote object {path}")),
        }
    }

    async fn open_write(&self, key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
        let path = self.object_path(key);
        // `BufWriter` performs a multipart upload under the hood, buffering at
        // most one part (default 10 MiB) before flushing — bounded memory.
        // Finalized on `poll_shutdown`.
        Ok(Box::pin(BufWriter::new(self.store.clone(), path)))
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = self.object_path(key);
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e).with_context(|| format!("head remote object {path}")),
        }
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

    #[tokio::test]
    async fn memory_backend_streams_under_prefix() {
        let backend = ObjStoreBackend::from_uri("memory:///repo-a", 10).expect("backend");
        assert!(!backend.exists("k/v").await.expect("exists"));
        assert!(get(&backend, "k/v").await.is_none());

        put(&backend, "k/v", b"hello").await;
        assert!(backend.exists("k/v").await.expect("exists"));
        assert_eq!(get(&backend, "k/v").await.expect("present"), b"hello");
    }

    #[tokio::test]
    async fn file_backend_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uri = format!("file://{}", dir.path().display());
        let backend = ObjStoreBackend::from_uri(&uri, 10).expect("backend");
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
    fn read_adc_type_none_on_missing_or_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_adc_type(&dir.path().join("nope.json")), None);
        let garbage = write_adc(dir.path(), "not json at all");
        assert_eq!(read_adc_type(&garbage), None);
        let no_type = write_adc(dir.path(), r#"{"audience": "x"}"#);
        assert_eq!(read_adc_type(&no_type), None);
    }
}
