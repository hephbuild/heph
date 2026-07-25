//! Remote (shared) cache: an ordered set of object-store backends fronting the
//! local cache. Configured via the `caches:` map in `.hephconfig2`.
//!
//! Semantics (see [`RemoteCacheSet`]):
//! - **write** — push to every writable cache in parallel; within each cache the
//!   manifest is written *last*, so a manifest that appears was complete at
//!   upload time. Blobs are independent objects, though, and a bucket lifecycle
//!   rule can expire them out from under a surviving manifest — hence the
//!   presence check in [`RemoteCacheSet::blobs_exist`].
//! - **read** — try caches one-by-one in ascending-latency order. The first
//!   cache whose manifest is present serves the revision: every blob comes from
//!   that same cache, never spliced across caches.
//!
//! **Reads are lazy.** A remote hit is decided from the *manifest alone* — it
//! carries every artifact's `hashout`, which is all a dependent needs to compute
//! its own `hashin`. Output blobs transfer only when a caller actually reads
//! them, and only the groups it asked for (see [`Engine::pull_remote_blobs`]).
//! A target resolved purely to feed a dependent's hash — the common case in a
//! fully-cached build — therefore moves a few hundred manifest bytes, not its
//! outputs.
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
//! (synchronous, on the blocking pool — see [`run_codec`]) and the set streams
//! that file to the backend via object_store multipart; on download the set
//! streams the backend object into a temp file and the engine decodes it into the
//! local cache. The async path only ever touches `Send` temp files and backend
//! streams, so the synchronous (and partly non-`Send`) local-cache I/O never
//! crosses an `await`.
//!
//! **Background upload.** The engine pushes to the remote on a detached task,
//! tracked by the request's `bg_pending` counter (the same one sandbox cleanup
//! uses), so the CLI/TUI stays open until every upload drains — but the build's
//! critical path doesn't wait on the network.
//!
//! **Scale.** Everything here is sized for a wide fan-out — hundreds of targets
//! pulling and pushing at once — and every unbounded queue in that path is an
//! opportunity to look hung. Three bounds keep it honest:
//! [`REVISION_BLOB_CONCURRENCY`] (blobs in flight per revision),
//! [`MAX_CONCURRENT_UPLOADS`] (background push tasks process-wide), and
//! [`CODEC_SLOTS`] (concurrent compress/decompress, off the runtime workers).
//! Above them sits the per-cache request ceiling
//! ([`RemoteCacheDef::concurrency`]), whose semaphore is FIFO-fair, so a queued
//! transfer always makes progress rather than starving.

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
use futures::stream::{self, StreamExt, TryStreamExt};
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OnceCell, Semaphore};
use tracing::warn;

/// Max blobs of a *single* revision transferred concurrently, on both the pull
/// and the push side.
///
/// Fanning a revision out is what makes a multi-output target fast, but it must
/// stay bounded: each in-flight download holds an open temp file plus a live
/// response stream, and each in-flight upload holds an `object_store` multipart
/// buffer (10 MiB). A target with thousands of artifacts, multiplied by the
/// engine's own target-level parallelism, would otherwise run the process out of
/// file descriptors or memory. Requests past this bound simply queue.
pub(crate) const REVISION_BLOB_CONCURRENCY: usize = 32;

/// Max background upload *tasks* in flight process-wide.
///
/// [`Engine::spawn_remote_upload`] detaches one task per newly-cached target, so
/// without a bound a wide build spawns hundreds at once. Each holds every one of
/// its gzipped blobs as a temp file for its whole lifetime, and competes with the
/// critical-path *pulls* for the same per-cache request budget
/// ([`RemoteCacheDef::concurrency`]). Capping the task count bounds temp-disk use
/// and keeps pushes — which are pure background work — from crowding out the
/// pulls a blocked target is actually waiting on. Uploads are network-bound and
/// each fans out to [`REVISION_BLOB_CONCURRENCY`] blobs, so a modest cap still
/// saturates the link.
const MAX_CONCURRENT_UPLOADS: usize = 16;

static UPLOAD_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_UPLOADS));

/// Backstop on one background push, queue wait included.
///
/// Generous — a legitimately large revision over a slow link should finish, not
/// get cut off. It exists only so a wedged push can't hold the process open
/// forever (see [`Engine::spawn_remote_upload`]); abandoning one costs nothing
/// but a remote cache miss next time, since the revision is already local.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Permits for the synchronous compress/decompress + local-cache I/O that
/// brackets every transfer.
///
/// That work is CPU-bound and runs on the blocking pool (see
/// [`run_codec`]), so it is capped at the core count: enough to keep every core
/// busy, few enough that it can't drain the blocking pool that `tokio::fs` also
/// draws from.
static CODEC_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| {
    Semaphore::new(
        std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8),
    )
});

/// Run the synchronous codec step (gzip/gunzip plus local-cache reads/writes) on
/// the blocking pool, bounded by [`CODEC_SLOTS`].
///
/// It must **not** run on a runtime worker. Compressing or decompressing a
/// revision takes hundreds of milliseconds to seconds of straight CPU; with
/// hundreds of targets pulling and pushing at once, doing it inline occupies
/// every worker thread and the runtime stops polling *everything* — in-flight
/// transfers, their inactivity deadlines, the TUI. The build looks hung even
/// though no lock is actually deadlocked. (The previous `block_or_inline` did
/// exactly that: inline on Linux, `block_in_place` on macOS.)
///
/// The closure is `Send`, but the values it builds are not required to be — the
/// non-`Send` local-cache reader/writer is created and dropped entirely inside
/// it and never crosses an `await`.
async fn run_codec<F, R>(what: &'static str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    let _permit = CODEC_SLOTS
        .acquire()
        .await
        .with_context(|| format!("acquire remote cache codec slot for {what}"))?;
    tokio::task::spawn_blocking(f)
        .await
        .with_context(|| format!("join remote cache {what} task"))?
}

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
            // `{e:#}` is anyhow's alternate Display: the cause chain on one line,
            // no backtrace. (`{e:?}` would dump the full `Caused by:` ladder plus
            // a process backtrace — useless noise for the user.)
            warn!(
                cache = %self.def.name,
                op,
                "remote cache unavailable, skipping it for {op}: {e:#}. Further errors for this cache are suppressed.",
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

    /// Probe round-trip latency: one warmup call (discarded — it pays the
    /// connection/TLS setup cost that would otherwise skew the first sample),
    /// then [`LATENCY_SAMPLES`] consecutive measured calls reduced to their
    /// median (resists a one-off spike). Any probe failing aborts the whole
    /// measurement with that error.
    async fn probe_latency(&self) -> anyhow::Result<Duration> {
        self.probe_once().await.context("latency warmup probe")?;
        let mut samples = Vec::with_capacity(LATENCY_SAMPLES);
        for _ in 0..LATENCY_SAMPLES {
            let started = Instant::now();
            self.probe_once().await.context("latency probe")?;
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        samples
            .into_iter()
            .nth(LATENCY_SAMPLES / 2)
            .context("latency probe collected no samples")
    }

    /// One probe request, hard-bounded by [`LATENCY_PROBE_TIMEOUT`].
    ///
    /// The bound is the point: `read_order` is a one-shot init that *every*
    /// target's first remote read waits on, each already holding its per-addr
    /// write lock. Left to the backend's own budget a probe against a hung
    /// endpoint rides 20 retries over 30 minutes, and the whole build sits behind
    /// it. Timing out just sorts that cache last (or, from `measure_latency`,
    /// reports it unreachable).
    async fn probe_once(&self) -> anyhow::Result<()> {
        tokio::time::timeout(
            LATENCY_PROBE_TIMEOUT,
            self.backend.exists(LATENCY_PROBE_KEY),
        )
        .await
        .with_context(|| {
            format!("remote cache probe timed out after {LATENCY_PROBE_TIMEOUT:?}")
        })??;
        Ok(())
    }
}

/// Object key probed to measure cache latency. Never expected to exist; the
/// backend round-trip is what we time.
const LATENCY_PROBE_KEY: &str = "__heph_latency_probe__";

/// Measured (non-warmup) latency calls per cache. Odd so the median is a single
/// sample, not an average of two.
const LATENCY_SAMPLES: usize = 3;

/// Hard cap on one latency probe. A probe is a `HEAD` of a key that never
/// exists — sub-second on any healthy cache, so a generous bound still fires
/// only on a genuinely stuck endpoint.
const LATENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on reading one manifest.
///
/// Manifests are tiny by design, so one that hasn't arrived in a minute is stuck
/// rather than slow. The bound matters because the read happens under the
/// target's per-addr **write** lock: left to the backend's own budget (up to 20
/// retries across 30 minutes, sized for multi-GiB blobs) a single wedged manifest
/// GET parks that target — and everything waiting on it — for half an hour.
const MANIFEST_READ_TIMEOUT: Duration = Duration::from_secs(60);

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

/// A revision *located* on one remote cache — the decision half of a remote hit.
///
/// Holds no blob bytes. The manifest names the revision's artifacts, records
/// each one's `hashout` (enough to answer "already built" and to feed a
/// dependent's `hashin`) and its on-remote [`encoding`](RemoteManifestArtifact::encoding),
/// so individual blobs can be pulled later, on demand, from the same cache the
/// manifest came from (manifest affinity).
pub(crate) struct RemoteRevision {
    /// The cache that served the manifest. Every blob of this revision is pulled
    /// from it, so a revision is never spliced across caches.
    cache_idx: usize,
    pub manifest: RemoteManifest,
}

impl RemoteRevision {
    /// The artifact entry for `name`, or an error if the manifest never named it
    /// — a caller asking for a blob outside the revision is a bug, not a miss.
    fn artifact(&self, name: &str) -> anyhow::Result<&RemoteManifestArtifact> {
        self.manifest
            .artifacts
            .iter()
            .find(|a| a.name == name)
            .with_context(|| {
                format!(
                    "remote manifest for {} does not name blob {name}",
                    self.manifest.target
                )
            })
    }
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

    /// Object key for a cached blob: the target address rendered as a path, so a
    /// bucket browses like the source tree — `//pkg/path:name@v=x` with `hashin`
    /// `abc` and artifact `out.tar` becomes `pkg/path/name@v=x/abc/out.tar`.
    ///
    /// Readability never costs uniqueness: [`key_segment`] keeps a segment
    /// verbatim only when it is unambiguously safe, otherwise it appends a hash
    /// of the original (see there). Two distinct addresses therefore always
    /// produce distinct key prefixes, so targets sharing a `hashin` never alias.
    fn key(addr: &Addr, hashin: &str, name: &str) -> String {
        let mut key = String::new();
        for c in addr.package.components() {
            key.push_str(&key_segment(c));
            key.push('/');
        }
        key.push_str(&key_segment(&addr_name_segment(addr)));
        key.push('/');
        key.push_str(&key_segment(hashin));
        key.push('/');
        key.push_str(&key_segment(name));
        key
    }

    /// Probe every cache's round-trip latency once, concurrently, and report it.
    /// Drives `heph tool cache measure-latency`; also persists the resulting read
    /// order so subsequent runs skip the probe.
    pub async fn measure_latency(&self) -> Vec<CacheLatency> {
        let probes = self.caches.iter().map(|c| async move {
            let latency = match c.probe_latency().await {
                Ok(d) => Some(d),
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
                let lat = match c.probe_latency().await {
                    Ok(d) => d,
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
    ///
    /// Bounded by [`MANIFEST_READ_TIMEOUT`] — a timeout surfaces as an ordinary
    /// error, which the caller already treats as "this cache didn't serve it",
    /// so a wedged cache falls through to the next one instead of parking the
    /// target.
    async fn read_small(&self, cache_idx: usize, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let cache = self
            .caches
            .get(cache_idx)
            .context("remote cache index out of range")?;
        tokio::time::timeout(MANIFEST_READ_TIMEOUT, read_small_inner(cache, key))
            .await
            .with_context(|| {
                format!("read remote object {key} timed out after {MANIFEST_READ_TIMEOUT:?}")
            })?
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
            // Collected eagerly rather than left as a lazy `Map`: feeding a
            // closure-returning-async-block straight into `stream::iter` makes
            // rustc demand a higher-ranked `FnOnce` impl it can't prove.
            let blob_puts: Vec<_> = blobs
                .iter()
                .map(|(name, path)| {
                    let key = Self::key(addr, hashin, name);
                    async move { stream_file_to_backend(cache.backend.as_ref(), &key, path).await }
                })
                .collect();
            // Bounded fan-out: each in-flight put holds a multipart buffer, so a
            // revision with thousands of artifacts must not open them all at
            // once. Returning on the first error drops the stream, abandoning the
            // rest — a cache that just failed isn't worth finishing.
            let results = stream::iter(blob_puts)
                .buffered(REVISION_BLOB_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            for res in results {
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

    /// Locate `(addr, hashin)` on the first readable cache (latency order) that
    /// holds its manifest — **manifest only, no blob transfer**.
    ///
    /// This is the whole remote hit/miss decision: the manifest carries every
    /// artifact's `hashout`, so "already built" is answerable, and a dependent's
    /// `hashin` computable, without moving a single output byte. Blobs follow
    /// later, per caller, via [`Self::fetch_blob`].
    ///
    /// `None` on a miss (no readable cache has it) or on an unparseable manifest.
    pub(crate) async fn fetch_manifest(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<RemoteRevision>> {
        if !self.has_readable() {
            return Ok(None);
        }
        let manifest_key = Self::key(addr, hashin, MANIFEST_V1);

        // Locate the manifest in the fastest cache that has it. Skip caches the
        // breaker has tripped; record success/failure so a flaky cache trips.
        let mut found: Option<(usize, Vec<u8>)> = None;
        for &i in self.read_order().await {
            if ctoken.is_cancelled() {
                return Ok(None);
            }
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
        Ok(Some(RemoteRevision {
            cache_idx,
            manifest,
        }))
    }

    /// Whether `rev`'s cache still holds every one of `names` — presence only, no
    /// bytes transferred.
    ///
    /// This is what keeps a lazy pull fail-*soft*. The hit is decided from the
    /// manifest, but the blobs it names are separate objects that a bucket
    /// lifecycle rule can expire independently, so "manifest present" alone does
    /// not prove the revision is still servable. Checking presence up front, at
    /// decision time, turns an evicted revision back into an ordinary cache miss
    /// (the target executes) instead of a failure discovered later, mid-read,
    /// after the engine already committed to "already built".
    ///
    /// Any error (or cancellation) answers `false`: unproven presence must never
    /// be reported as a hit.
    pub(crate) async fn blobs_exist(
        &self,
        ctoken: &dyn Cancellable,
        rev: &RemoteRevision,
        addr: &Addr,
        hashin: &str,
        names: &[String],
    ) -> bool {
        let Some(cache) = self.caches.get(rev.cache_idx) else {
            return false;
        };
        // Collected eagerly — see the note in `put_revision`.
        let checks: Vec<_> = names
            .iter()
            .map(|name| {
                let key = Self::key(addr, hashin, name);
                async move {
                    if ctoken.is_cancelled() {
                        return false;
                    }
                    match cache.backend.exists(&key).await {
                        Ok(present) => present,
                        Err(e) => {
                            cache.note_err("blob presence check", &e);
                            false
                        }
                    }
                }
            })
            .collect();
        stream::iter(checks)
            .buffered(REVISION_BLOB_CONCURRENCY)
            .all(|present| async move { present })
            .await
    }

    /// Stream one blob of `rev` from the cache that served its manifest into a
    /// temp file under `dest_dir`, still in its on-remote encoding (the engine
    /// decodes it into the local cache — see [`Engine::pull_remote_blobs`]).
    ///
    /// `Ok(None)` when the cache no longer has the object, the transfer failed
    /// (already noted on the cache), or the request was cancelled — all of which
    /// mean "this cache cannot serve the blob". Local temp-file I/O failures are
    /// genuinely fatal and propagate.
    pub(crate) async fn fetch_blob(
        &self,
        ctoken: &dyn Cancellable,
        rev: &RemoteRevision,
        addr: &Addr,
        hashin: &str,
        name: &str,
        dest_dir: &Path,
    ) -> anyhow::Result<Option<PathBuf>> {
        let cache = self
            .caches
            .get(rev.cache_idx)
            .context("remote cache index out of range")?;
        let key = Self::key(addr, hashin, name);
        let reader = match cache.backend.open_read(&key).await {
            Ok(Some(reader)) => reader,
            // Manifest names a blob the cache no longer has → incomplete.
            Ok(None) => return Ok(None),
            Err(e) => {
                cache.note_err("blob download", &e);
                return Ok(None);
            }
        };
        let temp = dest_dir.join(format!("{}.blob", uuid::Uuid::new_v4()));
        // Temp-file I/O is local and genuinely fatal — propagate.
        let mut file = tokio::fs::File::create(&temp)
            .await
            .with_context(|| format!("create temp for remote blob {name}"))?;
        let mut reader = reader;
        // Race the transfer against cancellation. Without this a Ctrl-C during a
        // large pull is ignored until the copy ends — and the copy is exactly
        // where a wedged connection sits, so the run the user just cancelled
        // keeps hanging.
        let copied = tokio::select! {
            biased;
            () = ctoken.cancelled() => {
                drop(file);
                drop(std::fs::remove_file(&temp));
                return Ok(None);
            }
            r = tokio::io::copy(&mut reader, &mut file) => r,
        };
        if let Err(e) = copied {
            // Mid-stream network error from the cache → best-effort miss.
            cache.note_err(
                "blob download",
                &anyhow::Error::new(e).context(format!("stream remote blob {name}")),
            );
            drop(file);
            drop(std::fs::remove_file(&temp));
            return Ok(None);
        }
        file.shutdown()
            .await
            .with_context(|| format!("flush temp for remote blob {name}"))?;
        cache.note_ok();
        Ok(Some(temp))
    }
}

/// The unbounded body of [`RemoteCacheSet::read_small`], split out so the
/// timeout can wrap it as a single future.
async fn read_small_inner(cache: &ConfiguredCache, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
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

/// Longest a readable key segment may be before it is truncated and hashed.
/// Object stores cap the whole key (GCS at 1024 bytes), and a segment past this
/// has stopped being readable anyway.
const KEY_SEGMENT_MAX: usize = 96;

/// Marker separating the readable part of a rewritten segment from its hash.
/// A segment containing it is never kept verbatim, so the *last* occurrence in
/// an emitted segment is always the marker — that keeps the mapping injective.
const KEY_HASH_MARKER: &str = "--";

/// The address's target name plus its args, as one segment: `name@k=v,k=v`.
fn addr_name_segment(addr: &Addr) -> String {
    if addr.args.is_empty() {
        return addr.name.clone();
    }
    let mut s = addr.name.clone();
    s.push('@');
    for (i, (k, v)) in addr.args.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

/// Whether a segment can be used verbatim in an object key.
fn key_segment_is_plain(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= KEY_SEGMENT_MAX
        && raw != "."
        && raw != ".."
        && !raw.contains(KEY_HASH_MARKER)
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '=' | ','))
}

/// Render one path component readable but unambiguous. A plain segment passes
/// through untouched; anything else has its unsafe characters replaced, is
/// truncated, and carries a hash of the *original* so two distinct inputs can
/// never collapse onto the same segment.
fn key_segment(raw: &str) -> String {
    if key_segment_is_plain(raw) {
        return raw.to_string();
    }
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '=' | ',') {
                c
            } else {
                '_'
            }
        })
        .take(KEY_SEGMENT_MAX)
        .collect();
    out.push_str(KEY_HASH_MARKER);
    out.push_str(&format!(
        "{:016x}",
        xxhash_rust::xxh3::xxh3_64(raw.as_bytes())
    ));
    out
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
    /// **The task must always finish.** Both exit paths spin until `bg_pending`
    /// hits zero (`tui/backend/ci.rs`, `tui/backend/interactive.rs`) with no
    /// timeout of their own, and the CI backend has no `q` escape — so a push
    /// that never returns means `heph` never exits. Hence the two escapes below:
    /// the request's cancellation token, and [`UPLOAD_DEADLINE`] as a backstop.
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
            // Queue for one of the `MAX_CONCURRENT_UPLOADS` slots. Taken *after*
            // the start event so a push waiting its turn still shows as an
            // in-flight `↑` op — the wait is real time the upload is pending.
            // A closed semaphore is impossible (it is a process-lifetime static),
            // but treat it as "skip the push" rather than unwrapping: an upload is
            // best-effort and must never fail the build.
            //
            // The whole thing — queueing included — races cancellation and a
            // deadline, so this task can never be what keeps the process alive.
            let ctoken = rs.ctoken().clone_arc();
            let push = async {
                match UPLOAD_SLOTS.acquire().await {
                    Ok(_permit) => engine.upload_to_remote(&addr, &hashin).await,
                    Err(e) => warn!(error = ?e, %addr, "remote cache upload slot unavailable"),
                }
            };
            tokio::select! {
                biased;
                () = ctoken.cancelled() => {}
                r = tokio::time::timeout(UPLOAD_DEADLINE, push) => {
                    if r.is_err() {
                        warn!(
                            %addr,
                            "remote cache upload abandoned after {UPLOAD_DEADLINE:?}; \
                             the revision stays in the local cache",
                        );
                    }
                }
            }
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

        // Encode every blob to a temp file (synchronous local I/O, on the
        // blocking pool via `run_codec`; the non-`Send` local reader stays on
        // that thread and never crosses an await). Each artifact is gzipped or
        // copied verbatim per `compression_for`, and the chosen encoding is
        // recorded so the remote manifest is self-describing.
        let local_cache = self.local_cache.clone();
        let artifacts = manifest.artifacts.clone();
        let prepared: Vec<(String, PathBuf, ManifestArtifactEncoding)> = {
            let addr = addr.clone();
            let hashin = hashin.to_string();
            let tmp_dir = tmp_dir.clone();
            run_codec("upload encode", move || {
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
            })
            .await?
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

    /// Locate a revision on the remote and, if every blob a caller could ask for
    /// is still there, mirror its manifest into the local cache — **without
    /// downloading a single output blob**.
    ///
    /// This is the remote half of the hit/miss decision. `output_groups` are the
    /// groups the target's callers may ask for; the blobs backing them (plus
    /// support files, never logs — see [`Engine::needed_artifacts`]) are
    /// presence-checked before the hit is accepted, so a revision whose blobs the
    /// remote has expired degrades to a miss (execute) instead of failing later,
    /// mid-read. Returns the mirrored local manifest plus the located
    /// [`RemoteRevision`], which callers keep to pull their own outputs on demand
    /// via [`Self::pull_remote_blobs`].
    ///
    /// Must be called under the per-addr **write** lock: it writes the manifest
    /// into the local cache, and the write lock excludes GC and other writers.
    ///
    /// The mirrored manifest deliberately names blobs that are not local yet — it
    /// records the revision's identity and hashouts, not its residency. Every
    /// read path materializes what it needs first (see
    /// [`Engine::missing_local_blobs`]).
    pub(crate) async fn probe_remote_revision(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        output_groups: &[String],
    ) -> anyhow::Result<Option<(Manifest, RemoteRevision)>> {
        let Some(rev) = self
            .remote_caches
            .fetch_manifest(ctoken, addr, hashin)
            .await?
        else {
            return Ok(None);
        };

        let local_manifest = local_manifest_from_remote(&rev.manifest, hashin);
        // Resolve groups to blob names through the same "needed" rule the read
        // path uses, so presence is checked against exactly what will be read.
        let needed: Vec<String> = Self::needed_artifacts(&local_manifest, output_groups)
            .map(|a| a.name.clone())
            .collect();
        if !self
            .remote_caches
            .blobs_exist(ctoken, &rev, addr, hashin, &needed)
            .await
        {
            return Ok(None);
        }

        let bytes = borsh::to_vec(&local_manifest).context("serialize local manifest")?;
        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        // Same reasoning as `cache_artifact_locally`: the local-cache writer is
        // synchronous (and not `Send`), so it must not run on a runtime worker.
        hproc::process_supervisor::block_or_inline(move || {
            use std::io::Write;
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, MANIFEST_V1)
                .context("open local writer for remote manifest")?;
            w.write_all(&bytes).context("write remote manifest")?;
            anyhow::Ok(())
        })
        .with_context(|| format!("mirror remote manifest for {addr} {hashin}"))?;

        Ok(Some((local_manifest, rev)))
    }

    /// Pull exactly `names` from `rev` into the local cache, decoding each blob
    /// per its on-remote encoding. Nothing else transfers — this is where a lazy
    /// read materializes, so it must stay scoped to the blobs the caller asked
    /// for.
    ///
    /// Runs under the per-addr riding **read** lock (which excludes GC's
    /// `try_write`, so the revision cannot be reclaimed underneath). Pulls are
    /// single-flighted per blob by the caller, so two output groups needing the
    /// same support file transfer it once.
    ///
    /// A blob the remote no longer serves is an error, not a miss: presence was
    /// already checked when the hit was accepted, so losing it here means the
    /// revision was evicted mid-build.
    pub(crate) async fn pull_remote_blobs(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        rev: &RemoteRevision,
        names: &[String],
    ) -> anyhow::Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let tmp_dir = self.remote_tmp_dir();
        std::fs::create_dir_all(&tmp_dir)
            .with_context(|| format!("create remote temp dir {}", tmp_dir.display()))?;

        // Bounded fan-out — see `REVISION_BLOB_CONCURRENCY`. A target whose caller
        // asks for many output groups must not open every stream at once.
        // Collected eagerly — see the note in `put_revision`.
        let pulls: Vec<_> = names
            .iter()
            .map(|name| self.pull_remote_blob(ctoken, addr, hashin, rev, name, &tmp_dir))
            .collect();
        stream::iter(pulls)
            .buffered(REVISION_BLOB_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    /// Stream one blob into a temp file, then decode it into the local cache.
    /// The temp file is dropped on both paths.
    async fn pull_remote_blob(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        rev: &RemoteRevision,
        name: &str,
        tmp_dir: &Path,
    ) -> anyhow::Result<()> {
        let encoding = rev.artifact(name)?.encoding.clone();
        let Some(temp) = self
            .remote_caches
            .fetch_blob(ctoken, rev, addr, hashin, name, tmp_dir)
            .await?
        else {
            if ctoken.is_cancelled() {
                return Err(crate::engine::error::CancelledError.into());
            }
            anyhow::bail!(
                "remote cache no longer serves blob {name} of {addr} {hashin}: the revision was \
                 evicted after its manifest was read — re-run to rebuild it",
            );
        };

        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let name_owned = name.to_string();
        let temp_for_codec = temp.clone();
        let res = run_codec("download decode", move || -> anyhow::Result<()> {
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, &name_owned)
                .with_context(|| format!("open local writer for downloaded blob {name_owned}"))?;
            match encoding {
                ManifestArtifactEncoding::Gzip => gunzip_from_file(&temp_for_codec, &mut w)
                    .with_context(|| format!("decompress downloaded blob {name_owned}"))?,
                _ => copy_file_to(&temp_for_codec, &mut w)
                    .with_context(|| format!("write downloaded blob {name_owned}"))?,
            }
            Ok(())
        })
        .await;

        drop(std::fs::remove_file(&temp));
        res
    }
}

/// The local view of a remote manifest: same artifacts, but `encoding = None`
/// because the local cache always stores decoded bytes.
fn local_manifest_from_remote(remote: &RemoteManifest, hashin: &str) -> Manifest {
    Manifest {
        version: "1.0.0".to_string(),
        target: remote.target.clone(),
        created_at_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0),
        hashin: hashin.to_string(),
        artifacts: remote
            .artifacts
            .iter()
            .map(|a| ManifestArtifact {
                hashout: a.hashout.clone(),
                group: a.group.clone(),
                name: a.name.clone(),
                size: a.size,
                r#type: a.r#type.clone(),
                content_type: a.content_type.clone(),
                encoding: ManifestArtifactEncoding::None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hconfig::DEFAULT_CACHE_CONCURRENCY;

    fn def(name: &str, uri: &str, read: bool, write: bool) -> RemoteCacheDef {
        RemoteCacheDef {
            name: name.to_string(),
            uri: uri.to_string(),
            read,
            write,
            concurrency: DEFAULT_CACHE_CONCURRENCY,
        }
    }

    /// A token that is never cancelled — the default for tests that aren't
    /// exercising cancellation.
    fn never() -> hcore::hasync::StdCancellationToken {
        hcore::hasync::StdCancellationToken::new()
    }

    fn addr() -> Addr {
        Addr::new(
            hmodel::htpkg::PkgBuf::from("p"),
            "t".to_string(),
            Default::default(),
        )
    }

    fn mk_addr(pkg: &str, name: &str, args: &[(&str, &str)]) -> Addr {
        Addr::new(
            hmodel::htpkg::PkgBuf::from(pkg),
            name.to_string(),
            args.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn key_mirrors_the_target_address() {
        assert_eq!(
            RemoteCacheSet::key(&mk_addr("some/pkg", "tgt", &[]), "h1", "out.tar"),
            "some/pkg/tgt/h1/out.tar"
        );
        assert_eq!(
            RemoteCacheSet::key(&mk_addr("", "root_tgt", &[]), "h1", "out.tar"),
            "root_tgt/h1/out.tar",
            "root package contributes no segments"
        );
        assert_eq!(
            RemoteCacheSet::key(
                &mk_addr("some/pkg", "tgt", &[("v", "linux"), ("vp", "arm64")]),
                "h1",
                "out.tar"
            ),
            "some/pkg/tgt@v=linux,vp=arm64/h1/out.tar",
            "args stay readable on the name segment"
        );
    }

    #[test]
    fn key_distinguishes_addrs_that_would_alias_after_sanitizing() {
        // `:` and ` ` are both rewritten to `_`, so the readable part collides —
        // the appended hash is what keeps the two keys apart.
        let a = RemoteCacheSet::key(&mk_addr("p", "a:b", &[]), "h", "o");
        let b = RemoteCacheSet::key(&mk_addr("p", "a b", &[]), "h", "o");
        assert_ne!(a, b);
        assert!(a.starts_with("p/a_b--"), "unexpected key {a}");

        // A package boundary shift must not alias with a deeper package.
        assert_ne!(
            RemoteCacheSet::key(&mk_addr("a", "b", &[]), "h", "o"),
            RemoteCacheSet::key(&mk_addr("a/b", "h", &[]), "o", "o"),
        );
    }

    #[test]
    fn key_segment_never_confuses_a_plain_name_with_a_rewritten_one() {
        // A raw segment that already looks like `<text>--<hash>` must not be kept
        // verbatim, or it could collide with the rewrite of some other segment.
        let forged = format!(
            "a_b{KEY_HASH_MARKER}{:016x}",
            xxhash_rust::xxh3::xxh3_64(b"a b")
        );
        assert_ne!(key_segment(&forged), key_segment("a b"));
    }

    #[test]
    fn key_segment_truncates_and_hashes_long_segments() {
        let long = "x".repeat(KEY_SEGMENT_MAX + 50);
        let seg = key_segment(&long);
        assert!(seg.len() < long.len(), "long segment must shrink");
        assert!(seg.starts_with(&"x".repeat(KEY_SEGMENT_MAX)));
        assert_ne!(seg, key_segment(&"x".repeat(KEY_SEGMENT_MAX + 51)));
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
                .fetch_manifest(&never(), &addr, "h")
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

    /// Backend that records how many times `exists` was called and sleeps a
    /// scripted duration per call, so a latency probe is deterministic.
    struct ScriptedBackend {
        calls: AtomicUsize,
        /// Sleep applied to call N (clamped to the last entry once exhausted).
        delays: Vec<Duration>,
    }

    #[async_trait]
    impl RemoteCacheBackend for ScriptedBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            anyhow::bail!("unused")
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("unused")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            let d = self.delays.get(n).or_else(|| self.delays.last()).copied();
            if let Some(d) = d {
                tokio::time::sleep(d).await;
            }
            Ok(false)
        }
    }

    /// A latency probe discards one warmup call, then times exactly
    /// [`LATENCY_SAMPLES`] calls and reports their median.
    #[tokio::test]
    async fn probe_latency_warms_up_then_medians() {
        // Warmup is slowest so it would dominate a naive single-shot probe; the
        // three measured samples are 30/10/20ms → median 20ms.
        let backend = Arc::new(ScriptedBackend {
            calls: AtomicUsize::new(0),
            delays: vec![
                Duration::from_millis(80),
                Duration::from_millis(30),
                Duration::from_millis(10),
                Duration::from_millis(20),
            ],
        });
        let cache = ConfiguredCache {
            def: def("x", "memory:///x", true, true),
            backend: backend.clone(),
            health: CacheHealth::default(),
        };

        let lat = cache.probe_latency().await.expect("probe");

        assert_eq!(
            backend.calls.load(Ordering::Relaxed),
            LATENCY_SAMPLES + 1,
            "one warmup call plus {LATENCY_SAMPLES} measured calls"
        );
        // Median is the 20ms sample — well clear of both the 10ms floor and the
        // 80ms warmup, with slack for scheduler jitter.
        assert!(
            lat >= Duration::from_millis(15) && lat < Duration::from_millis(60),
            "median sample expected ~20ms, got {lat:?}"
        );
    }

    /// A probe whose warmup fails surfaces the error rather than reporting a
    /// bogus latency.
    #[tokio::test]
    async fn probe_latency_propagates_failure() {
        let cache = ConfiguredCache {
            def: def("broken", "memory:///broken", true, true),
            backend: Arc::new(FailBackend),
            health: CacheHealth::default(),
        };
        assert!(cache.probe_latency().await.is_err());
    }

    /// A probe against a cache that accepts the connection and then never
    /// answers must not hang. `read_order` is a one-shot init every target's
    /// first remote read waits on — each already holding its per-addr write lock
    /// — so an unbounded probe stalls the whole build.
    #[tokio::test(start_paused = true)]
    async fn probe_latency_times_out_on_a_hung_cache() {
        struct HungBackend;

        #[async_trait]
        impl RemoteCacheBackend for HungBackend {
            async fn open_read(
                &self,
                _key: &str,
            ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
                anyhow::bail!("unused")
            }
            async fn open_write(
                &self,
                _key: &str,
            ) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
                anyhow::bail!("unused")
            }
            async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
                std::future::pending().await
            }
        }

        let cache = ConfiguredCache {
            def: def("hung", "memory:///hung", true, true),
            backend: Arc::new(HungBackend),
            health: CacheHealth::default(),
        };

        // Paused time auto-advances only when every task is idle, so this
        // resolves as soon as the timeout is the sole pending thing — it cannot
        // pass by simply waiting the probe out.
        let err = cache
            .probe_latency()
            .await
            .expect_err("a hung probe must time out, not hang");
        assert!(
            format!("{err:#}").contains("timed out"),
            "error should name the timeout, got: {err:#}"
        );
    }

    /// Backend serving a fixed key→bytes map that records the peak number of
    /// simultaneously-open reads. Each `open_read` holds its slot across a sleep
    /// so overlapping calls are observable.
    struct ReadProbeBackend {
        objects: std::collections::HashMap<String, Vec<u8>>,
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    /// Backend that accepts (and discards) every write, recording the peak number
    /// of simultaneously-open writes.
    struct WriteProbeBackend {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    /// Bump `in_flight`, record it against `peak`, sleep so concurrent callers
    /// overlap, then release. Returns the peak-recording that `assert`s read.
    async fn hold_slot(in_flight: &AtomicUsize, peak: &AtomicUsize) {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    #[async_trait]
    impl RemoteCacheBackend for ReadProbeBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            hold_slot(&self.in_flight, &self.peak).await;
            Ok(self.objects.get(key).map(|b| {
                Box::pin(std::io::Cursor::new(b.clone())) as Pin<Box<dyn AsyncRead + Send>>
            }))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only probe backend")
        }
        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.objects.contains_key(key))
        }
    }

    #[async_trait]
    impl RemoteCacheBackend for WriteProbeBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            Ok(None)
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            hold_slot(&self.in_flight, &self.peak).await;
            Ok(Box::pin(tokio::io::sink()))
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    fn set_with(backend: Arc<dyn RemoteCacheBackend>, home: PathBuf) -> Arc<RemoteCacheSet> {
        Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache {
                def: def("probe", "memory:///probe", true, true),
                backend,
                health: CacheHealth::default(),
            }],
            home,
            config_hash: String::new(),
            read_order: OnceCell::new(),
        })
    }

    /// Artifact count used by the fan-out tests — comfortably above the bound so
    /// an unbounded implementation would show it.
    const FANOUT_ARTIFACTS: usize = REVISION_BLOB_CONCURRENCY * 3;

    fn probe_artifact(i: usize) -> RemoteManifestArtifact {
        RemoteManifestArtifact {
            hashout: format!("ho-{i}"),
            group: "out".to_string(),
            name: format!("blob-{i}"),
            size: 4,
            r#type: ManifestArtifactType::Output,
            content_type: ManifestArtifactContentType::Tar,
            encoding: ManifestArtifactEncoding::None,
        }
    }

    /// Blobs download in parallel, but never more than
    /// [`REVISION_BLOB_CONCURRENCY`] at a time: sequential pulls made a wide
    /// fan-out crawl, while an unbounded one would hold an open temp file and a
    /// live response stream per artifact.
    #[tokio::test]
    async fn blob_pulls_fan_out_within_the_blob_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let artifacts: Vec<RemoteManifestArtifact> =
            (0..FANOUT_ARTIFACTS).map(probe_artifact).collect();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: artifacts.clone(),
        };

        let mut objects = std::collections::HashMap::new();
        objects.insert(
            RemoteCacheSet::key(&addr, "h", MANIFEST_V1),
            borsh::to_vec(&manifest).expect("serialize manifest"),
        );
        for a in &artifacts {
            objects.insert(RemoteCacheSet::key(&addr, "h", &a.name), b"data".to_vec());
        }

        let backend = Arc::new(ReadProbeBackend {
            objects,
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let set = set_with(backend.clone(), dir.path().to_path_buf());

        let rev = set
            .fetch_manifest(&never(), &addr, "h")
            .await
            .expect("fetch manifest")
            .expect("hit");
        // Collected eagerly — see the note in `put_revision`.
        let ctoken = never();
        let fetches: Vec<_> = artifacts
            .iter()
            .map(|a| set.fetch_blob(&ctoken, &rev, &addr, "h", &a.name, dir.path()))
            .collect();
        let temps: Vec<Option<PathBuf>> = stream::iter(fetches)
            .buffered(REVISION_BLOB_CONCURRENCY)
            .try_collect()
            .await
            .expect("fetch blobs");
        assert_eq!(
            temps.iter().filter(|t| t.is_some()).count(),
            FANOUT_ARTIFACTS,
            "every blob must be fetched"
        );

        // The manifest read is serial and precedes the blobs, so the peak is
        // attributable to the blob fan-out alone.
        let peak = backend.peak.load(Ordering::SeqCst);
        assert!(
            peak > 1,
            "blobs must download in parallel, saw peak concurrency {peak}"
        );
        assert!(
            peak <= REVISION_BLOB_CONCURRENCY,
            "blob fan-out must stay bounded, saw peak concurrency {peak}"
        );
    }

    /// The same bound applies on the push side, where each in-flight put also
    /// holds an `object_store` multipart buffer.
    #[tokio::test]
    async fn put_revision_fans_out_within_the_blob_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();

        // One shared temp file is enough — the bound is on how many puts are open
        // at once, not on their contents.
        let blob_src = dir.path().join("src.blob");
        std::fs::write(&blob_src, b"data").expect("write blob");
        let blobs: Vec<(String, PathBuf)> = (0..FANOUT_ARTIFACTS)
            .map(|i| (format!("blob-{i}"), blob_src.clone()))
            .collect();

        let backend = Arc::new(WriteProbeBackend {
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let set = set_with(backend.clone(), dir.path().to_path_buf());

        set.put_revision(&addr, "h", b"manifest", &blobs).await;

        let peak = backend.peak.load(Ordering::SeqCst);
        assert!(
            peak > 1,
            "blobs must upload in parallel, saw peak concurrency {peak}"
        );
        assert!(
            peak <= REVISION_BLOB_CONCURRENCY,
            "blob fan-out must stay bounded, saw peak concurrency {peak}"
        );
    }

    /// A pull in progress must abort when the run is cancelled. The blob copy is
    /// exactly where a wedged connection parks, and it happens under the
    /// target's per-addr write lock — so an uninterruptible copy means Ctrl-C
    /// leaves the build hanging on the very transfer the user gave up on.
    #[tokio::test]
    async fn blob_fetch_aborts_on_cancellation() {
        /// Serves a manifest, then never delivers a single blob byte.
        struct StalledBlobBackend {
            manifest_key: String,
            manifest: Vec<u8>,
        }

        #[async_trait]
        impl RemoteCacheBackend for StalledBlobBackend {
            async fn open_read(
                &self,
                key: &str,
            ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
                if key == self.manifest_key {
                    return Ok(Some(Box::pin(std::io::Cursor::new(self.manifest.clone()))));
                }
                // A reader that is forever pending: the copy can only end via
                // cancellation.
                struct Never;
                impl AsyncRead for Never {
                    fn poll_read(
                        self: Pin<&mut Self>,
                        _cx: &mut std::task::Context<'_>,
                        _buf: &mut tokio::io::ReadBuf<'_>,
                    ) -> std::task::Poll<std::io::Result<()>> {
                        std::task::Poll::Pending
                    }
                }
                Ok(Some(Box::pin(Never)))
            }
            async fn open_write(
                &self,
                _key: &str,
            ) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
                anyhow::bail!("read-only")
            }
            async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: vec![probe_artifact(0)],
        };
        let backend = Arc::new(StalledBlobBackend {
            manifest_key: RemoteCacheSet::key(&addr, "h", MANIFEST_V1),
            manifest: borsh::to_vec(&manifest).expect("serialize manifest"),
        });
        let set = set_with(backend, dir.path().to_path_buf());

        let ctoken = never();
        let rev = set
            .fetch_manifest(&ctoken, &addr, "h")
            .await
            .expect("fetch manifest")
            .expect("hit");
        let fetch = set.fetch_blob(&ctoken, &rev, &addr, "h", "blob-0", dir.path());
        // Cancel once the copy is underway; without the wiring this never
        // resolves and the test times out.
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ctoken.cancel();
        };
        let (res, ()) = tokio::join!(fetch, cancel);

        assert!(
            res.expect("cancellation is a miss, not a hard error")
                .is_none(),
            "a cancelled pull must report a miss"
        );
        // The abandoned temp must not be left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "blob"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "cancelling must not leak partial temp files, found {leftovers:?}"
        );
    }

    /// The codec step must not run on a runtime worker. On a single-worker
    /// runtime an inline (or `block_in_place`-free) implementation would freeze
    /// every other task for the whole compress/decompress; `run_codec` hands it
    /// to the blocking pool, so unrelated tasks keep being polled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn run_codec_does_not_block_the_runtime() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker = tokio::spawn(enclose::enclose!((ticks) async move {
            loop {
                ticks.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));

        let before = ticks.load(Ordering::SeqCst);
        run_codec("test", || {
            std::thread::sleep(Duration::from_millis(200));
            Ok(())
        })
        .await
        .expect("codec");
        let after = ticks.load(Ordering::SeqCst);

        ticker.abort();
        assert!(
            after > before,
            "the runtime must keep polling other tasks while the codec runs \
             (ticks {before} -> {after})"
        );
    }

    #[tokio::test]
    async fn empty_set_is_noop() {
        let set = RemoteCacheSet::empty();
        assert!(set.is_empty());
        assert!(
            set.fetch_manifest(&never(), &addr(), "h")
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
        let rev = set
            .fetch_manifest(&never(), &addr, "h1")
            .await
            .expect("fetch")
            .expect("present");
        assert_eq!(rev.manifest.artifacts.len(), 2);
        assert_eq!(
            rev.manifest.artifacts[0].encoding,
            ManifestArtifactEncoding::Gzip
        );
        assert_eq!(
            rev.manifest.artifacts[1].encoding,
            ManifestArtifactEncoding::None
        );

        let gz_temp = set
            .fetch_blob(&never(), &rev, &addr, "h1", "gz.tar", &fetch_dir)
            .await
            .expect("fetch gz")
            .expect("gz present");
        let plain_temp = set
            .fetch_blob(&never(), &rev, &addr, "h1", "plain.tar", &fetch_dir)
            .await
            .expect("fetch plain")
            .expect("plain present");

        let mut restored_gz = Vec::new();
        gunzip_from_file(&gz_temp, &mut restored_gz).expect("gunzip");
        assert_eq!(restored_gz, raw_gz);

        let restored_plain = std::fs::read(&plain_temp).expect("read plain");
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
            set.fetch_manifest(&never(), &addr, "h1")
                .await
                .expect("fetch")
                .is_none()
        );
    }
}
