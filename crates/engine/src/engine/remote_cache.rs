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
//! **Separate manifest.** The remote uses its own [`RemoteManifest`], distinct
//! from the local [`Manifest`], so the two layers can store artifacts
//! differently. Each remote artifact records its `encoding` (`Gzip` or `None`):
//! artifacts worth compressing are gzipped, small ones are stored verbatim (see
//! [`compression_for`]). The engine converts remote↔local on upload/download;
//! the local manifest always describes decoded bytes.
//!
//! **Streaming.** No blob is ever held whole in memory. Each blob moves through a
//! temp file: on upload the engine encodes the local blob into a temp file
//! (synchronous, on the cache thread) and the set streams that file to the
//! backend via object_store multipart; on download the set streams the backend
//! object into a temp file and the engine decodes it into the local cache. The
//! async path only ever touches `Send` temp files and backend streams, so the
//! synchronous (and partly non-`Send`) local-cache I/O never crosses an `await`.
//!
//! **Background upload.** The engine pushes to the remote on a detached task,
//! tracked by the request's `bg_pending` counter (the same one sandbox cleanup
//! uses), so the CLI/TUI stays open until every upload drains — but the build's
//! critical path doesn't wait on the network.

use crate::engine::Engine;
use crate::engine::local_cache::{
    MANIFEST_V1, Manifest, ManifestArtifact, ManifestArtifactContentType, ManifestArtifactEncoding,
    ManifestArtifactType,
};
use crate::engine::remote_cache_latency::{UNREACHABLE, load_order, store_order};
use crate::engine::remote_cache_objstore::ObjStoreBackend;
use anyhow::Context;
use async_trait::async_trait;
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use futures::future::join_all;
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

impl RemoteCacheDef {
    /// Coarse backend kind from the URI scheme — `s3`, `gcs`, `azure`, `http`,
    /// `file`, `memory`, or `other`. The bucket/host/path is dropped, so this is
    /// non-PII and safe to report in telemetry.
    pub fn backend_kind(&self) -> &'static str {
        let scheme = self
            .uri
            .split("://")
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match scheme.as_str() {
            "s3" | "s3a" => "s3",
            "gs" => "gcs",
            "az" | "adl" | "azure" | "abfs" | "abfss" => "azure",
            "http" | "https" => "http",
            "file" => "file",
            "memory" => "memory",
            _ => "other",
        }
    }
}

/// After this many consecutive failures a cache is circuit-broken for the rest
/// of the process: skipped without further network calls or log lines. Stops a
/// down or misconfigured (e.g. auth-failing) cache from slowing every target and
/// flooding the logs on a wide build.
const FAILURE_THRESHOLD: usize = 3;

/// Per-cache failure tracking. The first error for a cache is logged once, then
/// every later error is suppressed; after [`FAILURE_THRESHOLD`] consecutive
/// failures the cache is disabled for the rest of the process so we stop hitting
/// it at all. A success resets the consecutive-failure run.
#[derive(Default)]
struct CacheHealth {
    warned: AtomicBool,
    consecutive_failures: AtomicUsize,
    disabled: AtomicBool,
}

/// A configured cache: its definition, the constructed backend, and its health.
struct ConfiguredCache {
    def: RemoteCacheDef,
    backend: Arc<dyn RemoteCacheBackend>,
    health: CacheHealth,
}

impl ConfiguredCache {
    /// Whether the cache has been circuit-broken and should be skipped.
    fn broken(&self) -> bool {
        self.health.disabled.load(Ordering::Relaxed)
    }

    /// A successful op clears the consecutive-failure run (the breaker stays
    /// tripped if it already fired — we don't probe a disabled cache anyway).
    fn note_ok(&self) {
        self.health.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed op: warn exactly once per cache, suppress the rest, and
    /// trip the breaker after [`FAILURE_THRESHOLD`] consecutive failures.
    fn note_err(&self, op: &str, e: &anyhow::Error) {
        if !self.health.warned.swap(true, Ordering::Relaxed) {
            warn!(
                cache = %self.def.name, op, error = ?e,
                "remote cache error; further warnings for this cache are suppressed",
            );
        }
        let n = self
            .health
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if n >= FAILURE_THRESHOLD && !self.health.disabled.swap(true, Ordering::Relaxed) {
            warn!(
                cache = %self.def.name,
                "remote cache disabled for the rest of this run after {n} consecutive failures",
            );
        }
    }
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

/// The remote cache's own manifest — deliberately distinct from the local
/// [`Manifest`]. The remote layer may store an artifact's bytes differently from
/// the local cache (gzip-compressed, or not, decided per artifact), so each
/// entry records its on-remote [`encoding`](RemoteManifestArtifact::encoding).
/// The local manifest always describes *decoded* bytes; the engine converts
/// between the two on upload/download.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(crate) struct RemoteManifest {
    pub version: String,
    pub target: String,
    pub hashin: String,
    pub artifacts: Vec<RemoteManifestArtifact>,
}

/// One artifact as stored on the remote, including how its bytes are encoded.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(crate) struct RemoteManifestArtifact {
    pub hashout: String,
    pub group: String,
    pub name: String,
    /// Decoded (local) byte size — what the artifact unpacks to.
    pub size: u64,
    pub r#type: ManifestArtifactType,
    pub content_type: ManifestArtifactContentType,
    /// How this artifact's object is stored on the remote (`None` or `Gzip`).
    pub encoding: ManifestArtifactEncoding,
}

/// Remote manifest format version — independent of the local manifest's, so the
/// two can evolve separately.
const REMOTE_MANIFEST_VERSION: &str = "1.0.0";

/// Below this size, gzip overhead isn't worth it (tiny artifacts barely shrink,
/// and the header/footer can make them *grow*), so they're stored uncompressed.
/// The decision is recorded per artifact in the remote manifest, so this policy
/// can change without invalidating existing entries.
const MIN_COMPRESS_BYTES: u64 = 1024;

/// Whether an artifact of `size` decoded bytes is worth compressing for the
/// remote. Per-artifact so "some artifacts aren't worth compressing" is a policy
/// knob, not a global on/off.
fn compression_for(size: u64) -> ManifestArtifactEncoding {
    if size >= MIN_COMPRESS_BYTES {
        ManifestArtifactEncoding::Gzip
    } else {
        ManifestArtifactEncoding::None
    }
}

/// A revision fetched from one remote cache: the remote manifest plus the local
/// temp files each blob was streamed into (still in their on-remote encoding —
/// the engine decodes them per [`RemoteManifestArtifact::encoding`]).
pub(crate) struct FetchedRevision {
    pub manifest: RemoteManifest,
    /// `(artifact name, temp file holding its on-remote bytes)`.
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
                health: CacheHealth::default(),
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

    pub(crate) fn has_writable(&self) -> bool {
        self.caches.iter().any(|c| c.def.write)
    }

    /// Whether any cache is readable — the gate for the download/read path.
    pub(crate) fn has_readable(&self) -> bool {
        self.caches.iter().any(|c| c.def.read)
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
                        // Log-once + breaker via shared health; a probe that fails
                        // sorts the cache last (and may trip the breaker).
                        c.note_err("latency probe", &e);
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
        // Skip caches the breaker has already tripped — no point hitting (or
        // re-logging) a cache that's down.
        let writers = self.caches.iter().filter(|c| c.def.write && !c.broken());
        let per_cache = writers.map(|cache| async move {
            let blob_puts = blobs.iter().map(|(name, path)| {
                let key = Self::key(addr, hashin, name);
                async move { stream_file_to_backend(cache.backend.as_ref(), &key, path).await }
            });
            for res in join_all(blob_puts).await {
                if let Err(e) = res {
                    cache.note_err("blob upload", &e);
                    return;
                }
            }
            // Manifest last: its presence implies every blob is already stored.
            let manifest_key = Self::key(addr, hashin, MANIFEST_V1);
            match write_bytes_to_backend(cache.backend.as_ref(), &manifest_key, manifest_bytes)
                .await
            {
                Ok(()) => cache.note_ok(),
                Err(e) => cache.note_err("manifest upload", &e),
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
        if !self.has_readable() {
            return Ok(None);
        }
        let manifest_key = Self::key(addr, hashin, MANIFEST_V1);

        // Locate the manifest in the fastest cache that has it. Skip caches the
        // breaker has tripped; record success/failure so a flaky cache trips.
        let mut found: Option<(usize, Vec<u8>)> = None;
        for &i in self.read_order().await {
            let Some(cache) = self.caches.get(i) else {
                continue;
            };
            if cache.broken() {
                continue;
            }
            match self.read_small(i, &manifest_key).await {
                Ok(Some(bytes)) => {
                    cache.note_ok();
                    found = Some((i, bytes));
                    break;
                }
                Ok(None) => {
                    cache.note_ok();
                    continue;
                }
                Err(e) => {
                    cache.note_err("manifest read", &e);
                    continue;
                }
            }
        }
        let Some((cache_idx, manifest_bytes)) = found else {
            return Ok(None);
        };
        let manifest = match borsh::from_slice::<RemoteManifest>(&manifest_bytes) {
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
        let mut blobs: Vec<(String, PathBuf)> = Vec::with_capacity(manifest.artifacts.len());
        for artifact in &manifest.artifacts {
            let key = Self::key(addr, hashin, &artifact.name);
            // A backend error here is best-effort: log once (per cache), drop any
            // partial temps, and treat the whole revision as a miss so the build
            // executes — never propagate it up as a hard failure.
            let reader = match cache.backend.open_read(&key).await {
                Ok(Some(reader)) => reader,
                Ok(None) => {
                    // Manifest names a blob the cache no longer has → incomplete.
                    cleanup_temps(&blobs);
                    return Ok(None);
                }
                Err(e) => {
                    cache.note_err("blob download", &e);
                    cleanup_temps(&blobs);
                    return Ok(None);
                }
            };

            let temp = dest_dir.join(format!("{}.gz", uuid::Uuid::new_v4()));
            // Temp-file I/O is local and genuinely fatal — propagate.
            let mut file = tokio::fs::File::create(&temp)
                .await
                .with_context(|| format!("create temp for remote blob {}", artifact.name))?;
            let mut reader = reader;
            if let Err(e) = tokio::io::copy(&mut reader, &mut file).await {
                // Mid-stream network error from the cache → best-effort miss.
                cache.note_err(
                    "blob download",
                    &anyhow::Error::new(e).context(format!("stream remote blob {}", artifact.name)),
                );
                drop(file);
                drop(std::fs::remove_file(&temp));
                cleanup_temps(&blobs);
                return Ok(None);
            }
            file.shutdown()
                .await
                .with_context(|| format!("flush temp for remote blob {}", artifact.name))?;
            blobs.push((artifact.name.clone(), temp));
        }

        cache.note_ok();
        Ok(Some(FetchedRevision { manifest, blobs }))
    }
}

/// Best-effort removal of temp files collected during a fetch that ended up a
/// miss, so a remote failure mid-download doesn't leak.
fn cleanup_temps(temps: &[(String, PathBuf)]) {
    for (_, path) in temps {
        drop(std::fs::remove_file(path));
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

/// Copy `reader` verbatim into a new file at `dest` (the uncompressed path).
fn copy_to_file(mut reader: impl std::io::Read, dest: &Path) -> anyhow::Result<()> {
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("create temp {}", dest.display()))?;
    std::io::copy(&mut reader, &mut file).context("copy")?;
    Ok(())
}

/// Copy the file at `src` verbatim into `writer` (the uncompressed path).
fn copy_file_to(src: &Path, mut writer: impl std::io::Write) -> anyhow::Result<()> {
    let mut file =
        std::fs::File::open(src).with_context(|| format!("open temp {}", src.display()))?;
    std::io::copy(&mut file, &mut writer).context("copy")?;
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
    pub fn remote_caches(&self) -> &Arc<RemoteCacheSet> {
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

        let tmp_dir = self.remote_tmp_dir();
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create remote temp dir {}", tmp_dir.display()))?;

        // Encode every blob to a temp file (synchronous local I/O, off the
        // runtime worker via block_or_inline; the non-`Send` local reader stays
        // on this thread and never crosses an await). Each artifact is gzipped or
        // copied verbatim per `compression_for`, and the chosen encoding is
        // recorded so the remote manifest is self-describing.
        let local_cache = self.local_cache.clone();
        let artifacts = manifest.artifacts.clone();
        let prepared: Vec<(String, PathBuf, ManifestArtifactEncoding)> = {
            let addr = addr.clone();
            let hashin = hashin.to_string();
            let tmp_dir = tmp_dir.clone();
            hproc::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                use std::io::Read;
                let mut out = Vec::with_capacity(artifacts.len());
                for a in &artifacts {
                    let sized = local_cache
                        .reader(&addr, &hashin, &a.name)
                        .with_context(|| format!("open local blob {}", a.name))?;
                    let encoding = compression_for(a.size);
                    let temp = tmp_dir.join(format!("{}.blob", uuid::Uuid::new_v4()));
                    let reader = sized.reader.take(sized.size);
                    match encoding {
                        ManifestArtifactEncoding::Gzip => gzip_to_file(reader, &temp)
                            .with_context(|| format!("compress local blob {}", a.name))?,
                        _ => copy_to_file(reader, &temp)
                            .with_context(|| format!("copy local blob {}", a.name))?,
                    }
                    out.push((a.name.clone(), temp, encoding));
                }
                Ok(out)
            })?
        };

        // Build the remote manifest from the local one plus the per-artifact
        // encodings just chosen.
        let remote_manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: manifest.target.clone(),
            hashin: hashin.to_string(),
            artifacts: manifest
                .artifacts
                .iter()
                .zip(prepared.iter())
                .map(|(a, (_, _, encoding))| RemoteManifestArtifact {
                    hashout: a.hashout.clone(),
                    group: a.group.clone(),
                    name: a.name.clone(),
                    size: a.size,
                    r#type: a.r#type.clone(),
                    content_type: a.content_type.clone(),
                    encoding: encoding.clone(),
                })
                .collect(),
        };
        let manifest_bytes =
            borsh::to_vec(&remote_manifest).context("serialize remote manifest")?;

        let temps: Vec<(String, PathBuf)> = prepared
            .iter()
            .map(|(name, path, _)| (name.clone(), path.clone()))
            .collect();
        self.remote_caches
            .put_revision(addr, hashin, &manifest_bytes, &temps)
            .await;

        for (_, path, _) in &prepared {
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
        if !self.remote_caches.has_readable() {
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

        // Decode each temp file into the local cache per its on-remote encoding
        // (synchronous; the non-`Send` local writer stays on this thread). Then
        // build the *local* manifest — which always describes decoded bytes
        // (`encoding = None`) — and write it last, mirroring `cache_locally`.
        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let temp_paths: Vec<PathBuf> = fetched.blobs.iter().map(|(_, p)| p.clone()).collect();
        let RemoteManifest {
            target, artifacts, ..
        } = fetched.manifest;
        let blobs = fetched.blobs;
        let res =
            hproc::process_supervisor::block_or_inline(move || -> anyhow::Result<Manifest> {
                use std::io::Write;
                for (artifact, (name, path)) in artifacts.iter().zip(blobs.iter()) {
                    let mut w = local_cache
                        .writer(&addr_owned, &hashin_owned, name)
                        .with_context(|| format!("open local writer for downloaded blob {name}"))?;
                    match artifact.encoding {
                        ManifestArtifactEncoding::Gzip => gunzip_from_file(path, &mut w)
                            .with_context(|| format!("decompress downloaded blob {name}"))?,
                        _ => copy_file_to(path, &mut w)
                            .with_context(|| format!("write downloaded blob {name}"))?,
                    }
                }

                let local_manifest = Manifest {
                    version: "1.0.0".to_string(),
                    target,
                    created_at_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0),
                    hashin: hashin_owned.clone(),
                    artifacts: artifacts
                        .iter()
                        .map(|a| ManifestArtifact {
                            hashout: a.hashout.clone(),
                            group: a.group.clone(),
                            name: a.name.clone(),
                            size: a.size,
                            r#type: a.r#type.clone(),
                            content_type: a.content_type.clone(),
                            // Local cache stores decoded bytes.
                            encoding: ManifestArtifactEncoding::None,
                        })
                        .collect(),
                };
                let bytes = borsh::to_vec(&local_manifest).context("serialize local manifest")?;
                let mut mw = local_cache
                    .writer(&addr_owned, &hashin_owned, MANIFEST_V1)
                    .context("open local writer for downloaded manifest")?;
                mw.write_all(&bytes).context("write downloaded manifest")?;
                Ok(local_manifest)
            });

        // Always drop the temp files, success or failure.
        for path in &temp_paths {
            drop(std::fs::remove_file(path));
        }

        Ok(Some(res?))
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
            hmodel::htpkg::PkgBuf::from("p"),
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

    #[test]
    fn backend_kind_maps_scheme() {
        let k = |uri: &str| def("c", uri, true, true).backend_kind();
        assert_eq!(k("s3://bucket/p"), "s3");
        assert_eq!(k("s3a://bucket/p"), "s3");
        assert_eq!(k("gs://bucket/p"), "gcs");
        assert_eq!(k("abfs://c@acct.dfs.core.windows.net/p"), "azure");
        assert_eq!(k("https://example.com/cache"), "http");
        assert_eq!(k("file:///tmp/c"), "file");
        assert_eq!(k("memory:///x"), "memory");
        assert_eq!(k("weird:///x"), "other");
    }

    /// Backend whose every operation fails — stands in for an auth/credential
    /// failure or an unreachable endpoint.
    struct FailBackend;

    #[async_trait]
    impl RemoteCacheBackend for FailBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            anyhow::bail!("auth failed")
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("auth failed")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            anyhow::bail!("auth failed")
        }
    }

    fn failing_set(home: PathBuf) -> Arc<RemoteCacheSet> {
        Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache {
                def: def("broken", "memory:///broken", true, true),
                backend: Arc::new(FailBackend),
                health: CacheHealth::default(),
            }],
            home,
            config_hash: String::new(),
            read_order: OnceCell::new(),
        })
    }

    /// A failing remote is best-effort: reads return a miss (never a hard error
    /// that would fail the build), and after repeated failures the cache trips
    /// its breaker and is skipped.
    #[tokio::test]
    async fn failing_backend_is_best_effort_and_trips_breaker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let set = failing_set(dir.path().to_path_buf());
        let addr = addr();

        for _ in 0..FAILURE_THRESHOLD {
            assert!(
                !set.caches.first().expect("cache").broken(),
                "should not break before the threshold"
            );
            let res = set
                .fetch_revision(&addr, "h", dir.path())
                .await
                .expect("fetch must be best-effort (Ok), never a hard error");
            assert!(res.is_none(), "a failing remote read is a miss");
        }
        assert!(
            set.caches.first().expect("cache").broken(),
            "cache must be circuit-broken after {FAILURE_THRESHOLD} consecutive failures"
        );
    }

    #[test]
    fn note_ok_resets_consecutive_failures() {
        let cache = ConfiguredCache {
            def: def("x", "memory:///x", true, true),
            backend: Arc::new(FailBackend),
            health: CacheHealth::default(),
        };
        let e = anyhow::anyhow!("boom");
        // One short of the threshold, then a success → the run resets.
        for _ in 0..FAILURE_THRESHOLD - 1 {
            cache.note_err("op", &e);
        }
        assert!(!cache.broken());
        cache.note_ok();
        // The counter restarts, so it takes a full threshold run again to trip.
        for _ in 0..FAILURE_THRESHOLD - 1 {
            cache.note_err("op", &e);
        }
        assert!(
            !cache.broken(),
            "note_ok must reset the consecutive-failure run"
        );
        cache.note_err("op", &e);
        assert!(cache.broken());
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

        // One gzip-encoded artifact (compressible) and one stored verbatim — the
        // remote manifest records the per-artifact encoding.
        let raw_gz = vec![b'x'; 5000];
        let gz_tmp = dir.path().join("gz.blob");
        gzip_to_file(&raw_gz[..], &gz_tmp).expect("gzip");
        assert!(
            std::fs::metadata(&gz_tmp).expect("stat").len() < raw_gz.len() as u64 / 2,
            "gzip artifact must be compressed"
        );

        let raw_plain = b"tiny".to_vec();
        let plain_tmp = dir.path().join("plain.blob");
        copy_to_file(&raw_plain[..], &plain_tmp).expect("copy");

        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h1".to_string(),
            artifacts: vec![
                RemoteManifestArtifact {
                    hashout: "ho-gz".to_string(),
                    group: "out".to_string(),
                    name: "gz.tar".to_string(),
                    size: raw_gz.len() as u64,
                    r#type: ManifestArtifactType::Output,
                    content_type: ManifestArtifactContentType::Tar,
                    encoding: ManifestArtifactEncoding::Gzip,
                },
                RemoteManifestArtifact {
                    hashout: "ho-plain".to_string(),
                    group: "out".to_string(),
                    name: "plain.tar".to_string(),
                    size: raw_plain.len() as u64,
                    r#type: ManifestArtifactType::Output,
                    content_type: ManifestArtifactContentType::Tar,
                    encoding: ManifestArtifactEncoding::None,
                },
            ],
        };
        let manifest_bytes = borsh::to_vec(&manifest).expect("borsh");

        set.put_revision(
            &addr,
            "h1",
            &manifest_bytes,
            &[
                ("gz.tar".to_string(), gz_tmp),
                ("plain.tar".to_string(), plain_tmp),
            ],
        )
        .await;

        // Fetch back; the manifest parses with both encodings, and each blob temp
        // decodes per its recorded encoding.
        let fetch_dir = dir.path().join("fetched");
        std::fs::create_dir_all(&fetch_dir).expect("mkdir");
        let fetched = set
            .fetch_revision(&addr, "h1", &fetch_dir)
            .await
            .expect("fetch")
            .expect("present");
        assert_eq!(fetched.manifest.artifacts.len(), 2);
        assert_eq!(
            fetched.manifest.artifacts[0].encoding,
            ManifestArtifactEncoding::Gzip
        );
        assert_eq!(
            fetched.manifest.artifacts[1].encoding,
            ManifestArtifactEncoding::None
        );

        let mut restored_gz = Vec::new();
        gunzip_from_file(&fetched.blobs[0].1, &mut restored_gz).expect("gunzip");
        assert_eq!(restored_gz, raw_gz);

        let restored_plain = std::fs::read(&fetched.blobs[1].1).expect("read plain");
        assert_eq!(
            restored_plain, raw_plain,
            "None-encoded artifact is stored verbatim"
        );
    }

    #[test]
    fn compression_for_respects_threshold() {
        assert_eq!(compression_for(0), ManifestArtifactEncoding::None);
        assert_eq!(
            compression_for(MIN_COMPRESS_BYTES - 1),
            ManifestArtifactEncoding::None
        );
        assert_eq!(
            compression_for(MIN_COMPRESS_BYTES),
            ManifestArtifactEncoding::Gzip
        );
    }

    /// Backend whose blob writes block on a shared barrier, so the upload only
    /// completes if every blob across every cache is in flight at the same time.
    struct BarrierBackend {
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl RemoteCacheBackend for BarrierBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            Ok(None)
        }
        async fn open_write(&self, key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            // Manifests are written after their cache's blobs, so they must not
            // join the blob rendezvous — only blob writes do.
            if !key.ends_with(MANIFEST_V1) {
                self.barrier.wait().await;
            }
            Ok(Box::pin(tokio::io::sink()))
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// Proves every artifact is uploaded to every cache in parallel: the barrier
    /// only releases when all `caches × blobs` blob writes are concurrently in
    /// flight. Any serialization (blobs within a cache, or caches between each
    /// other) would leave fewer than that at the barrier, so it never releases
    /// and the bounded wait fails the test.
    #[tokio::test]
    async fn uploads_every_blob_to_every_cache_in_parallel() {
        const CACHES: usize = 3;
        const BLOBS: usize = 4;

        let dir = tempfile::tempdir().expect("tempdir");
        let barrier = Arc::new(tokio::sync::Barrier::new(CACHES * BLOBS));
        let caches = (0..CACHES)
            .map(|i| ConfiguredCache {
                def: def(&format!("c{i}"), &format!("memory:///c{i}"), true, true),
                backend: Arc::new(BarrierBackend {
                    barrier: barrier.clone(),
                }),
                health: CacheHealth::default(),
            })
            .collect();
        let set = RemoteCacheSet {
            caches,
            home: dir.path().to_path_buf(),
            config_hash: String::new(),
            read_order: OnceCell::new(),
        };

        let blobs: Vec<(String, PathBuf)> = (0..BLOBS)
            .map(|i| {
                let path = dir.path().join(format!("b{i}.gz"));
                gzip_to_file(&b"x"[..], &path).expect("gzip");
                (format!("o{i}.tar"), path)
            })
            .collect();

        tokio::time::timeout(
            Duration::from_secs(5),
            set.put_revision(&addr(), "h1", b"m", &blobs),
        )
        .await
        .expect("all blob uploads across all caches must be in flight together");
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
