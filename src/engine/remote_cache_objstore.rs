//! [`RemoteCacheBackend`] backed by the `object_store` crate. A single backend
//! type serves every supported URI scheme — `s3://`, `gs://`, `memory://`,
//! `file://` — because `object_store::parse_url_opts` dispatches on the scheme
//! and returns the right store. Credentials are read from the process
//! environment (e.g. `AWS_ACCESS_KEY_ID`, `GOOGLE_SERVICE_ACCOUNT`) by feeding
//! `std::env::vars()` to the builder, mirroring each builder's `from_env`.

use crate::engine::remote_cache::RemoteCacheBackend;
use anyhow::Context;
use async_trait::async_trait;
use object_store::{ObjectStore, ObjectStoreExt, parse_url_opts, path::Path as ObjPath};
use url::Url;

/// One remote object store plus the path prefix carved out of its URI. Object
/// keys handed to [`RemoteCacheBackend`] are joined under `prefix`, so two
/// repos can share a bucket by pointing at distinct prefixes (`s3://b/repo-a`,
/// `s3://b/repo-b`).
pub struct ObjStoreBackend {
    store: Box<dyn ObjectStore>,
    prefix: ObjPath,
}

impl ObjStoreBackend {
    /// Build a backend from a cache URI. Pure/synchronous — constructs the
    /// client only; no network or credential validation happens here (the first
    /// real request surfaces auth errors). Env vars supply credentials.
    pub fn from_uri(uri: &str) -> anyhow::Result<Self> {
        let url = Url::parse(uri).with_context(|| format!("parse remote cache uri {uri}"))?;
        // Pass the environment through so s3/gcs builders pick up credentials
        // exactly as their `from_env` constructors would.
        let (store, prefix) = parse_url_opts(&url, std::env::vars())
            .with_context(|| format!("open remote cache store for {uri}"))?;
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
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.object_path(key);
        match self.store.get(&path).await {
            Ok(res) => {
                let bytes = res
                    .bytes()
                    .await
                    .with_context(|| format!("read remote object {path}"))?;
                Ok(Some(bytes.to_vec()))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("get remote object {path}")),
        }
    }

    async fn put(&self, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.object_path(key);
        self.store
            .put(&path, data.into())
            .await
            .with_context(|| format!("put remote object {path}"))?;
        Ok(())
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

    #[tokio::test]
    async fn memory_backend_roundtrips_under_prefix() {
        let backend = ObjStoreBackend::from_uri("memory:///repo-a").expect("backend");
        assert!(!backend.exists("k/v").await.expect("exists"));
        assert!(backend.get("k/v").await.expect("get").is_none());

        backend.put("k/v", b"hello".to_vec()).await.expect("put");
        assert!(backend.exists("k/v").await.expect("exists"));
        assert_eq!(
            backend.get("k/v").await.expect("get").expect("present"),
            b"hello"
        );
    }

    #[tokio::test]
    async fn file_backend_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let uri = format!("file://{}", dir.path().display());
        let backend = ObjStoreBackend::from_uri(&uri).expect("backend");
        backend
            .put("a/b/c", b"payload".to_vec())
            .await
            .expect("put");
        assert_eq!(
            backend.get("a/b/c").await.expect("get").expect("present"),
            b"payload"
        );
    }
}
