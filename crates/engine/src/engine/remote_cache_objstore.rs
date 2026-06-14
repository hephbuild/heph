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

use crate::engine::remote_cache::RemoteCacheBackend;
use anyhow::Context;
use async_trait::async_trait;
use futures::TryStreamExt;
use object_store::buffered::BufWriter;
use object_store::limit::LimitStore;
use object_store::{ObjectStore, ObjectStoreExt, parse_url_opts, path::Path as ObjPath};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::StreamReader;
use url::Url;

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
        // Pass the environment through so s3/gcs/azure builders pick up
        // credentials exactly as their `from_env` constructors would.
        let (store, prefix) = parse_url_opts(&url, std::env::vars())
            .with_context(|| format!("open remote cache store for {uri}"))?;
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
}
