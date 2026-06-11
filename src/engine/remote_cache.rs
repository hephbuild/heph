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
//! **Streaming.** No blob is ever held whole in memory. Each blob moves through
//! a temp file: on upload the engine gzip-compresses the local blob into a temp
//! file (synchronous, on the cache thread) and the set streams that file to the
//! backend via object_store multipart; on download the set streams the backend
//! object into a temp file and the engine gunzips it into the local cache. The
//! async path only ever touches `Send` temp files and backend streams, so the
//! synchronous (and partly non-`Send`) local-cache I/O never crosses an `await`.
//!
//! **Background upload.** The engine pushes to the remote on a detached task,
//! tracked by the request's `bg_pending` counter (the same one sandbox cleanup
//! uses), so the CLI/TUI stays open until every upload drains — but the build's
//! critical path doesn't wait on the network.

use crate::engine::Engine;
use crate::engine::local_cache::{MANIFEST_V1, Manifest};
use crate::engine::remote_cache_latency::{UNREACHABLE, load_order, store_order};
use crate::engine::remote_cache_objstore::ObjStoreBackend;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use anyhow::Context;
use async_trait::async_trait;
use futures::future::join_all;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::OnceCell;
use tracing::warn;

/// Default per-cache request-concurrency cap (object_store [`LimitStore`]).
pub const DEFAULT_CACHE_CONCURRENCY: usize = 10;

/// A streaming object store. The set layers cache semantics (manifest affinity,
/// ordering, parallel fan-out) on top; a backend only moves bytes.
#[async_trait]
pub trait RemoteCacheBackend: Send + Sync {
    /// Open a streaming reader for an object, or `None` if it does not exist.
    async fn open_read(&self, key: &str) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>>;
    /// Open a streaming (multipart) writer; finalized on `shutdown`.
    async fn open_write(&self, key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>>;
    /// Whether an object exists, without fetching it.
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;
}

/// One cache entry from `caches:` — name plus URI, permissions, and request cap.
/// Plain data so it can live in [`crate::engine::Config`] (Clone/Debug/PartialEq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCacheDef {
    pub name: String,
    pub uri: String,
    pub read: bool,
    pub write: bool,
    /// Max in-flight requests to this cache (object_store `LimitStore`).
    pub concurrency: usize,
}

/// A configured cache: its definition plus the constructed backend.
struct ConfiguredCache {
    def: RemoteCacheDef,
    backend: Arc<dyn RemoteCacheBackend>,
}

/// Per-cache latency probe result, surfaced by `heph tool cache measure-latency`.
#[derive(Debug, Clone)]
pub struct CacheLatency {
    pub name: String,
    pub uri: String,
    pub readable: bool,
    pub writable: bool,
    /// Round-trip of a single probe request; `None` if the cache was unreachable.
    pub latency: Option<Duration>,
}

/// A revision fetched from one remote cache: the manifest plus the local temp
/// files (still gzip-compressed) each blob was streamed into.
pub(crate) struct FetchedRevision {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    /// `(artifact name, temp file holding its compressed bytes)`.
    pub blobs: Vec<(String, PathBuf)>,
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
            let backend = ObjStoreBackend::from_uri(&def.uri, def.concurrency)
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

    /// Probe every cache's round-trip latency once, concurrently, and report it.
    /// Drives `heph tool cache measure-latency`; also persists the resulting read
    /// order so subsequent runs skip the probe.
    pub async fn measure_latency(&self) -> Vec<CacheLatency> {
        let probes = self.caches.iter().map(|c| async move {
            let started = Instant::now();
            let latency = match c.backend.exists("__heph_latency_probe__").await {
                Ok(_) => Some(started.elapsed()),
                Err(e) => {
                    warn!(cache = %c.def.name, error = ?e, "remote cache latency probe failed");
                    None
                }
            };
            CacheLatency {
                name: c.def.name.clone(),
                uri: c.def.uri.clone(),
                readable: c.def.read,
                writable: c.def.write,
                latency,
            }
        });
        let mut results: Vec<CacheLatency> = join_all(probes).await;
        results.sort_by_key(|r| r.latency.unwrap_or(UNREACHABLE));

        // Persist the read order (readable caches, fastest first) so the next run
        // doesn't have to re-probe.
        let order: Vec<String> = results
            .iter()
            .filter(|r| r.readable)
            .map(|r| r.name.clone())
            .collect();
        if let Err(e) = store_order(&self.home, &self.config_hash, &order) {
            warn!(error = ?e, "persist remote cache latency order");
        }
        results
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
            for &i in &readable {
                if !ordered.contains(&i) {
                    ordered.push(i);
                }
            }
            return ordered;
        }

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

    /// Drain a small object (the manifest) fully into memory. Manifests are tiny
    /// by design, so buffering one is fine; blobs never take this path.
    async fn read_small(&self, cache_idx: usize, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        use tokio::io::AsyncReadExt;
        let cache = self
            .caches
            .get(cache_idx)
            .context("remote cache index out of range")?;
        match cache.backend.open_read(key).await? {
            Some(mut r) => {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)
                    .await
                    .with_context(|| format!("read remote object {key}"))?;
                Ok(Some(buf))
            }
            None => Ok(None),
        }
    }

    /// Stream a full revision to every writable cache. `blobs` gives each
    /// artifact's name and the temp file holding its (already gzip-compressed)
    /// bytes. Within a cache all blobs upload concurrently and the manifest is
    /// written *last*; across caches the work runs in parallel. Best-effort: a
    /// failing cache logs a warning and does not fail the build.
    pub(crate) async fn put_revision(
        &self,
        addr: &Addr,
        hashin: &str,
        manifest_bytes: &[u8],
        blobs: &[(String, PathBuf)],
    ) {
        let writers = self.caches.iter().filter(|c| c.def.write);
        let per_cache = writers.map(|cache| async move {
            let blob_puts = blobs.iter().map(|(name, path)| {
                let key = Self::key(addr, hashin, name);
                async move { stream_file_to_backend(cache.backend.as_ref(), &key, path).await }
            });
            for res in join_all(blob_puts).await {
                if let Err(e) = res {
                    warn!(cache = %cache.def.name, error = ?e, "remote cache blob upload failed; skipping manifest");
                    return;
                }
            }
            // Manifest last: its presence implies every blob is already stored.
            let manifest_key = Self::key(addr, hashin, MANIFEST_V1);
            if let Err(e) = write_bytes_to_backend(cache.backend.as_ref(), &manifest_key, manifest_bytes).await {
                warn!(cache = %cache.def.name, error = ?e, "remote cache manifest upload failed");
            }
        });
        join_all(per_cache).await;
    }

    /// Find the first readable cache (latency order) holding the manifest for
    /// `(addr, hashin)`, then stream every blob it names from that same cache
    /// into temp files under `dest_dir`. Returns `None` on a miss, or if any
    /// named blob is absent (an incomplete revision is treated as a miss).
    pub(crate) async fn fetch_revision(
        &self,
        addr: &Addr,
        hashin: &str,
        dest_dir: &Path,
    ) -> anyhow::Result<Option<FetchedRevision>> {
        if self.caches.is_empty() {
            return Ok(None);
        }
        let manifest_key = Self::key(addr, hashin, MANIFEST_V1);

        // Locate the manifest in the fastest cache that has it.
        let mut found: Option<(usize, Vec<u8>)> = None;
        for &i in self.read_order().await {
            match self.read_small(i, &manifest_key).await {
                Ok(Some(bytes)) => {
                    found = Some((i, bytes));
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    warn!(cache = %self.caches.get(i).map(|c| c.def.name.as_str()).unwrap_or(""), error = ?e, "remote cache manifest read failed");
                    continue;
                }
            }
        }
        let Some((cache_idx, manifest_bytes)) = found else {
            return Ok(None);
        };
        let manifest = match borsh::from_slice::<Manifest>(&manifest_bytes) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, %addr, "deserialize remote manifest; treating as miss");
                return Ok(None);
            }
        };

        let cache = self
            .caches
            .get(cache_idx)
            .context("remote cache index out of range")?;
        let mut blobs = Vec::with_capacity(manifest.artifacts.len());
        for artifact in &manifest.artifacts {
            let key = Self::key(addr, hashin, &artifact.name);
            let Some(mut reader) = cache.backend.open_read(&key).await? else {
                // Manifest names a blob the cache no longer has → incomplete.
                warn!(%addr, name = %artifact.name, "remote manifest blob missing; treating as miss");
                return Ok(None);
            };
            let temp = dest_dir.join(format!("{}.gz", uuid::Uuid::new_v4()));
            let mut file = tokio::fs::File::create(&temp)
                .await
                .with_context(|| format!("create temp for remote blob {}", artifact.name))?;
            tokio::io::copy(&mut reader, &mut file)
                .await
                .with_context(|| format!("stream remote blob {} to temp", artifact.name))?;
            file.shutdown()
                .await
                .with_context(|| format!("flush temp for remote blob {}", artifact.name))?;
            blobs.push((artifact.name.clone(), temp));
        }

        Ok(Some(FetchedRevision {
            manifest,
            manifest_bytes,
            blobs,
        }))
    }
}

/// Stream a local file's bytes to a backend object via the multipart writer.
async fn stream_file_to_backend(
    backend: &dyn RemoteCacheBackend,
    key: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let mut src = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open temp blob {}", path.display()))?;
    let mut w = backend.open_write(key).await?;
    tokio::io::copy(&mut src, &mut w)
        .await
        .with_context(|| format!("stream blob to remote {key}"))?;
    w.shutdown()
        .await
        .with_context(|| format!("finalize remote object {key}"))?;
    Ok(())
}

/// Write a small in-memory buffer (the manifest) to a backend object.
async fn write_bytes_to_backend(
    backend: &dyn RemoteCacheBackend,
    key: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut w = backend.open_write(key).await?;
    w.write_all(bytes)
        .await
        .with_context(|| format!("write remote object {key}"))?;
    w.shutdown()
        .await
        .with_context(|| format!("finalize remote object {key}"))?;
    Ok(())
}

/// Gzip-compress `reader` into a new file at `dest`. Pure-Rust backend
/// (miniz_oxide), so it stays cross-compile clean.
fn gzip_to_file(mut reader: impl std::io::Read, dest: &Path) -> anyhow::Result<()> {
    let file =
        std::fs::File::create(dest).with_context(|| format!("create temp {}", dest.display()))?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    std::io::copy(&mut reader, &mut enc).context("gzip copy")?;
    enc.finish().context("gzip finish")?;
    Ok(())
}

/// Gunzip the file at `src` into `writer`.
fn gunzip_from_file(src: &Path, mut writer: impl std::io::Write) -> anyhow::Result<()> {
    let file = std::fs::File::open(src).with_context(|| format!("open temp {}", src.display()))?;
    let mut dec = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    std::io::copy(&mut dec, &mut writer).context("gunzip copy")?;
    Ok(())
}

/// Stable hash of the definition set (order-independent) used to invalidate the
/// persisted latency order when caches are added, removed, or re-pointed.
/// `concurrency` is excluded — it does not affect which caches exist or how fast
/// they are, so changing it must not force a re-measure.
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
    pub(crate) fn remote_caches(&self) -> &Arc<RemoteCacheSet> {
        &self.remote_caches
    }

    /// Push a just-cached revision to the writable remote caches on a detached
    /// background task. The request's `bg_pending` counter (the same one sandbox
    /// cleanup uses) is bumped for the lifetime of the task, so the CLI/TUI stays
    /// open until the upload drains — but the build's critical path never waits
    /// on the network. The upload is bracketed by `RemoteCacheWrite{Start,End}`
    /// events so a long push surfaces in the TUI's slow-target breakdown.
    ///
    /// No-op when no caches are configured or none are writable.
    pub(crate) fn spawn_remote_upload(
        self: &Arc<Self>,
        rs: &Arc<crate::engine::request_state::RequestState>,
        addr: Addr,
        hashin: String,
    ) {
        use std::sync::atomic::Ordering;
        if self.remote_caches.is_empty() || !self.remote_caches.has_writable() {
            return;
        }
        let bg_pending = rs.bg_pending();
        // Count before spawning so shutdown can never observe the task as already
        // drained; the guard below drops it back once the upload finishes (or the
        // task panics).
        bg_pending.fetch_add(1, Ordering::AcqRel);
        let engine = Arc::clone(self);
        let rs = Arc::clone(rs);
        tokio::spawn(async move {
            struct Decrement(crate::engine::sandbox_cleaner::PendingCounter);
            impl Drop for Decrement {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _dec = Decrement(bg_pending);

            rs.emit(
                crate::engine::event::BuildEventKind::RemoteCacheWriteStart {
                    addr: addr.format(),
                },
            );
            engine.upload_to_remote(&addr, &hashin).await;
            rs.emit(crate::engine::event::BuildEventKind::RemoteCacheWriteEnd {
                addr: addr.format(),
                error: None,
            });
        });
    }

    /// Directory for transient gzip temp files, alongside the cache so temp and
    /// final live on the same filesystem.
    fn remote_tmp_dir(&self) -> PathBuf {
        self.home.join("cache").join("remote-tmp")
    }

    /// Push a just-cached revision to the writable remote caches. Reads the
    /// local manifest + blobs, gzip-compresses each blob into a temp file, and
    /// streams them up. Best-effort — never fails the build.
    pub(crate) async fn upload_to_remote(self: &Arc<Self>, addr: &Addr, hashin: &str) {
        if self.remote_caches.is_empty() || !self.remote_caches.has_writable() {
            return;
        }
        if let Err(e) = self.upload_to_remote_inner(addr, hashin).await {
            warn!(error = ?e, %addr, "remote cache upload failed");
        }
    }

    async fn upload_to_remote_inner(&self, addr: &Addr, hashin: &str) -> anyhow::Result<()> {
        let Some(manifest) = self.read_manifest(addr, hashin)? else {
            return Ok(());
        };
        let manifest_bytes = borsh::to_vec(&manifest).context("serialize manifest")?;

        let tmp_dir = self.remote_tmp_dir();
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create remote temp dir {}", tmp_dir.display()))?;

        // Compress every blob to a temp file (synchronous local I/O, off the
        // runtime worker via block_or_inline; the non-`Send` local reader stays
        // on this thread and never crosses an await).
        let names: Vec<String> = manifest.artifacts.iter().map(|a| a.name.clone()).collect();
        let local_cache = self.local_cache.clone();
        let temps: Vec<(String, PathBuf)> = {
            let addr = addr.clone();
            let hashin = hashin.to_string();
            let tmp_dir = tmp_dir.clone();
            crate::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                use std::io::Read;
                let mut out = Vec::with_capacity(names.len());
                for name in &names {
                    let sized = local_cache
                        .reader(&addr, &hashin, name)
                        .with_context(|| format!("open local blob {name}"))?;
                    let temp = tmp_dir.join(format!("{}.gz", uuid::Uuid::new_v4()));
                    gzip_to_file(sized.reader.take(sized.size), &temp)
                        .with_context(|| format!("compress local blob {name}"))?;
                    out.push((name.clone(), temp));
                }
                Ok(out)
            })?
        };

        self.remote_caches
            .put_revision(addr, hashin, &manifest_bytes, &temps)
            .await;

        for (_, path) in &temps {
            drop(std::fs::remove_file(path));
        }
        Ok(())
    }

    /// On a local cache miss, pull a complete revision from the remote caches
    /// into the local cache. Returns the manifest on success (the entry is now
    /// fully present locally), or `None` to fall through to execution.
    ///
    /// Must be called under the per-addr **write** lock: it writes blobs into
    /// the local cache, and the write lock excludes GC and other writers. The
    /// whole revision comes from a single cache (manifest affinity); blobs land
    /// first and the manifest is written *last*.
    pub(crate) async fn download_from_remote(
        &self,
        _ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        if self.remote_caches.is_empty() {
            return Ok(None);
        }

        let tmp_dir = self.remote_tmp_dir();
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create remote temp dir {}", tmp_dir.display()))?;

        let Some(fetched) = self
            .remote_caches
            .fetch_revision(addr, hashin, &tmp_dir)
            .await?
        else {
            return Ok(None);
        };

        // Gunzip each temp file into the local cache (synchronous; non-`Send`
        // local writer stays on this thread), blobs first then manifest last.
        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let blobs = fetched.blobs.clone();
        let manifest_bytes = fetched.manifest_bytes.clone();
        let res = crate::process_supervisor::block_or_inline(move || -> anyhow::Result<()> {
            use std::io::Write;
            for (name, path) in &blobs {
                let mut w = local_cache
                    .writer(&addr_owned, &hashin_owned, name)
                    .with_context(|| format!("open local writer for downloaded blob {name}"))?;
                gunzip_from_file(path, &mut w)
                    .with_context(|| format!("decompress downloaded blob {name}"))?;
            }
            let mut mw = local_cache
                .writer(&addr_owned, &hashin_owned, MANIFEST_V1)
                .context("open local writer for downloaded manifest")?;
            mw.write_all(&manifest_bytes)
                .context("write downloaded manifest")?;
            Ok(())
        });

        // Always drop the temp files, success or failure.
        for (_, path) in &fetched.blobs {
            drop(std::fs::remove_file(path));
        }
        res?;

        Ok(Some(fetched.manifest))
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
            concurrency: DEFAULT_CACHE_CONCURRENCY,
        }
    }

    fn addr() -> Addr {
        Addr::new(
            crate::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        )
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
        assert!(
            set.fetch_revision(&addr(), "h", Path::new("/tmp"))
                .await
                .expect("fetch")
                .is_none()
        );
    }

    /// Round-trip a revision through two memory caches: blobs are gzip temp
    /// files, manifest is found, and the blob streams back byte-identical.
    #[tokio::test]
    async fn put_then_fetch_revision_roundtrips() {
        let defs = vec![
            def("a", "memory:///a", true, true),
            def("b", "memory:///b", true, true),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = addr();

        // Build a gzip temp file for one blob (compressible payload).
        let raw = vec![b'x'; 5000];
        let blob_tmp = dir.path().join("blob.gz");
        gzip_to_file(&raw[..], &blob_tmp).expect("gzip");
        assert!(
            std::fs::metadata(&blob_tmp).expect("stat").len() < raw.len() as u64 / 2,
            "stored blob must be compressed"
        );

        // A minimal manifest naming that one blob.
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: 0,
            hashin: "h1".to_string(),
            artifacts: vec![crate::engine::local_cache::ManifestArtifact {
                hashout: "ho".to_string(),
                group: "out".to_string(),
                name: "o.tar".to_string(),
                size: raw.len() as u64,
                r#type: crate::engine::local_cache::ManifestArtifactType::Output,
                content_type: crate::engine::local_cache::ManifestArtifactContentType::Tar,
                encoding: crate::engine::local_cache::ManifestArtifactEncoding::None,
            }],
        };
        let manifest_bytes = borsh::to_vec(&manifest).expect("borsh");

        set.put_revision(
            &addr,
            "h1",
            &manifest_bytes,
            &[("o.tar".to_string(), blob_tmp)],
        )
        .await;

        // Fetch back; manifest parses and the blob temp gunzips to the original.
        let fetch_dir = dir.path().join("fetched");
        std::fs::create_dir_all(&fetch_dir).expect("mkdir");
        let fetched = set
            .fetch_revision(&addr, "h1", &fetch_dir)
            .await
            .expect("fetch")
            .expect("present");
        assert_eq!(fetched.manifest.artifacts.len(), 1);
        assert_eq!(fetched.blobs.len(), 1);
        let mut restored = Vec::new();
        gunzip_from_file(&fetched.blobs[0].1, &mut restored).expect("gunzip");
        assert_eq!(restored, raw);
    }

    #[tokio::test]
    async fn read_only_cache_is_not_written() {
        let defs = vec![def("ro", "memory:///ro", true, false)];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = addr();

        let blob_tmp = dir.path().join("b.gz");
        gzip_to_file(&b"x"[..], &blob_tmp).expect("gzip");
        set.put_revision(&addr, "h1", b"m", &[("o.tar".to_string(), blob_tmp)])
            .await;
        assert!(
            set.fetch_revision(&addr, "h1", dir.path())
                .await
                .expect("fetch")
                .is_none()
        );
    }
}
