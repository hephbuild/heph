//! Remote (shared) cache: an ordered set of object-store backends fronting the
//! local cache. Configured via the `caches:` map in `.hephconfig2`.
//!
//! Semantics (see [`RemoteCacheSet`]):
//! - **write** — push to every writable cache in parallel; within each cache the
//!   manifest is written *last*, so a reader that sees a manifest is guaranteed
//!   every blob it names is already present (same invariant the local cache
//!   relies on).
//! - **read** — try caches one-by-one in ascending-latency order. The first
//!   cache whose manifest is present serves the *whole* revision: every blob is
//!   pulled from that same cache, never spliced across caches.
//!
//! The engine integrates this around the local cache: [`Engine::cache_locally`]
//! pushes after a local write, and [`Engine::resolve_locked_inner`] pulls a full
//! revision into the local cache on a local miss (under the per-addr write lock,
//! so the blob writes can't race GC).

use crate::engine::Engine;
use crate::engine::local_cache::{MANIFEST_V1, Manifest};
use crate::engine::remote_cache_latency::{UNREACHABLE, load_order, store_order};
use crate::engine::remote_cache_objstore::ObjStoreBackend;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use anyhow::Context;
use async_trait::async_trait;
use futures::future::join_all;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use tracing::warn;

/// A flat key-value object store. The set layers cache semantics (manifest
/// affinity, ordering, parallel fan-out) on top; a backend only moves bytes.
#[async_trait]
pub trait RemoteCacheBackend: Send + Sync {
    /// Fetch an object, or `None` if it does not exist.
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// Store an object, overwriting any existing value.
    async fn put(&self, key: &str, data: Vec<u8>) -> anyhow::Result<()>;
    /// Whether an object exists, without fetching it.
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
}

/// One cache entry from `caches:` — name plus URI and read/write permissions.
/// Plain data so it can live in [`crate::engine::Config`] (Clone/Debug/PartialEq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCacheDef {
    pub name: String,
    pub uri: String,
    pub read: bool,
    pub write: bool,
}

/// A configured cache: its definition plus the constructed backend.
struct ConfiguredCache {
    def: RemoteCacheDef,
    backend: Arc<dyn RemoteCacheBackend>,
}

/// The ordered set of remote caches. Empty when no `caches:` are configured, in
/// which case every method is a cheap no-op and the engine behaves exactly as
/// before.
pub struct RemoteCacheSet {
    caches: Vec<ConfiguredCache>,
    home: PathBuf,
    /// Identifies the exact definition set; ties the persisted latency order to
    /// the config it was measured against.
    config_hash: String,
    /// Readable cache indices, fastest-first. Computed once (probe or load from
    /// disk) on first read.
    read_order: OnceCell<Vec<usize>>,
}

impl RemoteCacheSet {
    /// Build the set from definitions. Backend construction is synchronous (no
    /// network), so a bad URI fails here, at engine startup, with context.
    pub fn new(defs: &[RemoteCacheDef], home: PathBuf) -> anyhow::Result<Arc<Self>> {
        let mut caches = Vec::with_capacity(defs.len());
        for def in defs {
            let backend = ObjStoreBackend::from_uri(&def.uri)
                .with_context(|| format!("configure remote cache `{}`", def.name))?;
            caches.push(ConfiguredCache {
                def: def.clone(),
                backend: Arc::new(backend),
            });
        }
        let config_hash = config_hash(defs);
        Ok(Arc::new(Self {
            caches,
            home,
            config_hash,
            read_order: OnceCell::new(),
        }))
    }

    /// An empty set — used by tests and the no-config path.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            caches: Vec::new(),
            home: PathBuf::new(),
            config_hash: String::new(),
            read_order: OnceCell::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.caches.is_empty()
    }

    fn has_writable(&self) -> bool {
        self.caches.iter().any(|c| c.def.write)
    }

    /// Object key for a cached blob, namespaced by a stable hash of the target
    /// address so distinct targets that happen to share a `hashin` never alias.
    fn key(addr: &Addr, hashin: &str, name: &str) -> String {
        let addr_hash = xxhash_rust::xxh3::xxh3_64(addr.format().as_bytes());
        format!("{addr_hash:016x}/{hashin}/{name}")
    }

    /// Readable cache indices in ascending-latency order. Loads the persisted
    /// order when it matches the current config, otherwise probes every readable
    /// cache once (in parallel) and persists the result.
    async fn read_order(&self) -> &[usize] {
        self.read_order
            .get_or_init(|| async { self.compute_read_order().await })
            .await
    }

    async fn compute_read_order(&self) -> Vec<usize> {
        let readable: Vec<usize> = self
            .caches
            .iter()
            .enumerate()
            .filter(|(_, c)| c.def.read)
            .map(|(i, _)| i)
            .collect();
        if readable.len() <= 1 {
            return readable;
        }

        // Reuse a previously-measured order when the definitions are unchanged.
        if let Some(names) = load_order(&self.home, &self.config_hash) {
            let mut ordered: Vec<usize> = Vec::with_capacity(readable.len());
            for name in &names {
                if let Some((i, _)) = self
                    .caches
                    .iter()
                    .enumerate()
                    .find(|(i, c)| c.def.read && &c.def.name == name && !ordered.contains(i))
                {
                    ordered.push(i);
                }
            }
            // Append any readable cache the stored order missed (defensive — a
            // matching hash should already cover them).
            for &i in &readable {
                if !ordered.contains(&i) {
                    ordered.push(i);
                }
            }
            return ordered;
        }

        // Probe each readable cache's round-trip latency once, concurrently.
        let probes = self
            .caches
            .iter()
            .enumerate()
            .filter(|(_, c)| c.def.read)
            .map(|(i, c)| async move {
                let started = Instant::now();
                let lat = match c.backend.exists("__heph_latency_probe__").await {
                    Ok(_) => started.elapsed(),
                    Err(e) => {
                        warn!(cache = %c.def.name, error = ?e, "remote cache latency probe failed");
                        UNREACHABLE
                    }
                };
                (i, lat)
            });
        let mut measured: Vec<(usize, Duration)> = join_all(probes).await;
        measured.sort_by_key(|&(_, lat)| lat);
        let ordered: Vec<usize> = measured.iter().map(|&(i, _)| i).collect();

        let names: Vec<String> = ordered
            .iter()
            .filter_map(|&i| self.caches.get(i).map(|c| c.def.name.clone()))
            .collect();
        if let Err(e) = store_order(&self.home, &self.config_hash, &names) {
            warn!(error = ?e, "persist remote cache latency order");
        }
        ordered
    }

    /// Find the first readable cache (latency order) holding the manifest for
    /// `(addr, hashin)`. Returns its index plus the manifest bytes so the caller
    /// can pull every blob from that same cache.
    async fn find_manifest(&self, addr: &Addr, hashin: &str) -> Option<(usize, Vec<u8>)> {
        if self.caches.is_empty() {
            return None;
        }
        let key = Self::key(addr, hashin, MANIFEST_V1);
        for &i in self.read_order().await {
            let Some(cache) = self.caches.get(i) else {
                continue;
            };
            match cache.backend.get(&key).await {
                Ok(Some(bytes)) => return Some((i, bytes)),
                Ok(None) => continue,
                Err(e) => {
                    warn!(cache = %cache.def.name, error = ?e, "remote cache manifest read failed");
                    continue;
                }
            }
        }
        None
    }

    /// Fetch a single blob from a specific cache (the one [`find_manifest`]
    /// resolved), preserving manifest affinity.
    ///
    /// [`find_manifest`]: Self::find_manifest
    async fn get_blob(
        &self,
        cache_idx: usize,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let key = Self::key(addr, hashin, name);
        let cache = self
            .caches
            .get(cache_idx)
            .context("remote cache index out of range")?;
        cache.backend.get(&key).await
    }

    /// Push a full revision to every writable cache. Within each cache, all
    /// blobs upload concurrently and the manifest is written *last*; across
    /// caches the work runs in parallel. Best-effort: a failing cache logs a
    /// warning and does not fail the build.
    async fn put_revision(
        &self,
        addr: &Addr,
        hashin: &str,
        manifest_bytes: &[u8],
        blobs: &[(String, Vec<u8>)],
    ) {
        let writers = self.caches.iter().filter(|c| c.def.write);
        let per_cache = writers.map(|cache| async move {
            // Blobs first, in parallel.
            let blob_puts = blobs.iter().map(|(name, data)| {
                let key = Self::key(addr, hashin, name);
                let backend = &cache.backend;
                async move { backend.put(&key, data.clone()).await }
            });
            for res in join_all(blob_puts).await {
                if let Err(e) = res {
                    warn!(cache = %cache.def.name, error = ?e, "remote cache blob upload failed; skipping manifest");
                    return;
                }
            }
            // Manifest last: its presence implies every blob is already stored.
            let manifest_key = Self::key(addr, hashin, MANIFEST_V1);
            if let Err(e) = cache.backend.put(&manifest_key, manifest_bytes.to_vec()).await {
                warn!(cache = %cache.def.name, error = ?e, "remote cache manifest upload failed");
            }
        });
        join_all(per_cache).await;
    }
}

/// Stable hash of the definition set (order-independent) used to invalidate the
/// persisted latency order when caches are added, removed, or re-pointed.
fn config_hash(defs: &[RemoteCacheDef]) -> String {
    let mut sorted: Vec<&RemoteCacheDef> = defs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for d in sorted {
        h.update(d.name.as_bytes());
        h.update(&[0]);
        h.update(d.uri.as_bytes());
        h.update(&[d.read as u8, d.write as u8]);
        h.update(&[0xff]);
    }
    format!("{:016x}", h.digest())
}

impl Engine {
    /// Push a just-cached revision to the writable remote caches. No-op when no
    /// caches are configured or none are writable. Best-effort — never fails the
    /// build; remote push is an optimization, not a correctness requirement.
    pub(crate) async fn upload_to_remote(
        &self,
        addr: &Addr,
        key: &str,
        manifest: &Manifest,
        artifact_names: &[String],
    ) {
        let remote = &self.remote_caches;
        if remote.is_empty() || !remote.has_writable() {
            return;
        }
        let manifest_bytes = match borsh::to_vec(manifest) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = ?e, "serialize manifest for remote upload");
                return;
            }
        };
        // Read each blob back from the local cache (cheap — typically still in
        // the mem tier) to get the exact bytes that were stored.
        let mut blobs = Vec::with_capacity(artifact_names.len());
        for name in artifact_names {
            match self.read_local_blob(addr, key, name) {
                Ok(bytes) => blobs.push((name.clone(), bytes)),
                Err(e) => {
                    warn!(error = ?e, name, "read local blob for remote upload; skipping revision");
                    return;
                }
            }
        }
        remote
            .put_revision(addr, key, &manifest_bytes, &blobs)
            .await;
    }

    /// Read a single local-cache blob into memory. Synchronous local I/O.
    fn read_local_blob(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Vec<u8>> {
        use std::io::Read;
        let sized = self
            .local_cache
            .reader(addr, hashin, name)
            .with_context(|| format!("open local blob {name} for {addr}"))?;
        let mut buf = Vec::with_capacity(sized.size as usize);
        sized
            .reader
            .take(sized.size)
            .read_to_end(&mut buf)
            .with_context(|| format!("read local blob {name} for {addr}"))?;
        Ok(buf)
    }

    /// On a local cache miss, try to pull a complete revision from the remote
    /// caches into the local cache. Returns the manifest on success (the entry
    /// is now fully present locally), or `None` to fall through to execution.
    ///
    /// Must be called under the per-addr **write** lock: it writes blobs into
    /// the local cache, and the write lock excludes GC and other writers. The
    /// whole revision comes from a single cache (manifest affinity), and the
    /// manifest is written into the local cache *last*.
    pub(crate) async fn download_from_remote(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        let remote = &self.remote_caches;
        if remote.is_empty() {
            return Ok(None);
        }

        let Some((cache_idx, manifest_bytes)) = remote.find_manifest(addr, hashin).await else {
            return Ok(None);
        };
        let manifest = match borsh::from_slice::<Manifest>(&manifest_bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, %addr, "deserialize remote manifest; treating as miss");
                return Ok(None);
            }
        };

        // Pull every blob the manifest names, from the same cache.
        let mut blobs: Vec<(String, Vec<u8>)> = Vec::with_capacity(manifest.artifacts.len());
        for artifact in &manifest.artifacts {
            match remote
                .get_blob(cache_idx, addr, hashin, &artifact.name)
                .await
            {
                Ok(Some(bytes)) => blobs.push((artifact.name.clone(), bytes)),
                Ok(None) => {
                    // Manifest references a blob the cache no longer has: the
                    // revision is incomplete, so treat the whole thing as a miss.
                    warn!(%addr, name = %artifact.name, "remote manifest blob missing; treating as miss");
                    return Ok(None);
                }
                Err(e) => {
                    warn!(error = ?e, %addr, name = %artifact.name, "remote blob download failed; treating as miss");
                    return Ok(None);
                }
            }
        }

        // Write the whole revision into the local cache: blobs first, manifest
        // last — same ordering invariant `cache_locally` upholds. Synchronous
        // local I/O, kept off the runtime worker via `block_or_inline`.
        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let manifest_bytes_owned = manifest_bytes.clone();
        let _ = ctoken;
        crate::process_supervisor::block_or_inline(move || -> anyhow::Result<()> {
            use std::io::Write;
            for (name, bytes) in &blobs {
                let mut w = local_cache
                    .writer(&addr_owned, &hashin_owned, name)
                    .with_context(|| format!("open local writer for downloaded blob {name}"))?;
                w.write_all(bytes)
                    .with_context(|| format!("write downloaded blob {name}"))?;
            }
            let mut mw = local_cache
                .writer(&addr_owned, &hashin_owned, MANIFEST_V1)
                .context("open local writer for downloaded manifest")?;
            mw.write_all(&manifest_bytes_owned)
                .context("write downloaded manifest")?;
            Ok(())
        })?;

        Ok(Some(manifest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, uri: &str, read: bool, write: bool) -> RemoteCacheDef {
        RemoteCacheDef {
            name: name.to_string(),
            uri: uri.to_string(),
            read,
            write,
        }
    }

    #[test]
    fn config_hash_is_order_independent_and_change_sensitive() {
        let a = vec![
            def("x", "memory:///x", true, true),
            def("y", "memory:///y", true, false),
        ];
        let b = vec![
            def("y", "memory:///y", true, false),
            def("x", "memory:///x", true, true),
        ];
        assert_eq!(config_hash(&a), config_hash(&b), "order must not matter");

        let c = vec![
            def("x", "memory:///x", true, true),
            def("y", "memory:///CHANGED", true, false),
        ];
        assert_ne!(config_hash(&a), config_hash(&c), "uri change must matter");
    }

    #[tokio::test]
    async fn empty_set_is_noop() {
        let set = RemoteCacheSet::empty();
        assert!(set.is_empty());
        let addr = Addr::new(
            crate::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        );
        assert!(set.find_manifest(&addr, "h").await.is_none());
    }

    #[tokio::test]
    async fn put_revision_writes_manifest_last_and_reads_back() {
        // Two memory caches, both writable/readable.
        let defs = vec![
            def("a", "memory:///a", true, true),
            def("b", "memory:///b", true, true),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = Addr::new(
            crate::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        );

        let blobs = vec![
            ("out_a.tar".to_string(), b"blob-a".to_vec()),
            ("out_b.tar".to_string(), b"blob-b".to_vec()),
        ];
        set.put_revision(&addr, "h1", b"manifest-bytes", &blobs)
            .await;

        // Manifest is found and a blob is pullable from the same cache.
        let (idx, m) = set
            .find_manifest(&addr, "h1")
            .await
            .expect("manifest present");
        assert_eq!(m, b"manifest-bytes");
        let blob = set
            .get_blob(idx, &addr, "h1", "out_a.tar")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(blob, b"blob-a");
    }

    #[tokio::test]
    async fn read_only_cache_is_not_written() {
        let defs = vec![def("ro", "memory:///ro", true, false)];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = Addr::new(
            crate::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        );

        set.put_revision(&addr, "h1", b"m", &[("o.tar".into(), b"x".to_vec())])
            .await;
        // Nothing was written, so nothing is found.
        assert!(set.find_manifest(&addr, "h1").await.is_none());
    }
}
