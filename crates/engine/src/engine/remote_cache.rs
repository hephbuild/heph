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
//!
//! Above them sits the per-cache request ceiling
//! ([`RemoteCacheDef::concurrency`]), and that ceiling is **split in two** —
//! metadata requests and bulk blob transfers each get their own slice (see
//! [`META_SLOT_RESERVE`]). They are not interchangeable: a blob stream occupies a
//! request slot for its whole multi-second transfer, while a manifest read is a
//! few hundred bytes on the critical path under the target's per-addr write lock
//! and bounded by [`METADATA_TIMEOUT`]. Sharing one budget lets bulk traffic
//! starve the metadata a build is actually blocked on.

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
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OnceCell, Semaphore};
use tracing::{debug, warn};

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

/// Age past which a file under `remote-tmp` cannot belong to a live transfer,
/// used by [`sweep_abandoned_temps`].
///
/// [`UPLOAD_DEADLINE`] is the only deadline over a temp's lifetime — the pull
/// side has none of its own — so the margin on top is what actually covers a
/// slow download. Past it, a file can only be residue from a run hard-killed
/// before its [`TempBlob`] could be dropped. Sizing it this way is what makes the
/// sweep safe to run while *another* heph process is mid-transfer: its live temps
/// are younger than this, so they are never touched.
const TEMP_SWEEP_AGE: Duration = UPLOAD_DEADLINE.saturating_add(Duration::from_secs(10 * 60));

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
/// the dedicated blocking pool, bounded by [`CODEC_SLOTS`].
///
/// It must **not** run on a runtime worker. Compressing or decompressing a
/// revision takes hundreds of milliseconds to seconds of straight CPU; with
/// hundreds of targets pulling and pushing at once, doing it inline occupies
/// every worker thread and the runtime stops polling *everything* — in-flight
/// transfers, their inactivity deadlines, the TUI. The build looks hung even
/// though no lock is actually deadlocked. (The previous `block_or_inline` did
/// exactly that: inline on Linux, `block_in_place` on macOS.)
///
/// `hcore::blocking` rather than `spawn_blocking`: the latter's `JoinHandle`
/// wake-up rides tokio's cross-thread waker, observed to drop wake-ups on macOS
/// under load (`RCA_MACOS_WAKER.md`) — the same load this path generates.
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
    hcore::blocking::run(f)
        .await
        .with_context(|| format!("remote cache {what}"))
}

/// Extension every transient blob under `remote-tmp` carries, so the sweep can
/// tell its own residue from anything else that ends up in the directory.
const TEMP_BLOB_EXT: &str = "blob";

/// A transient file under `remote-tmp`, unlinked when dropped.
///
/// Every temp here is consumed and discarded within the operation that made it —
/// none is ever promoted to a final path — so the guard is unconditional and
/// needs no disarm.
///
/// It has to be RAII rather than a `remove_file` on each exit, because most of
/// the exits are not `return`s. The future can be dropped at any `await`: a
/// sibling in the same [`REVISION_BLOB_CONCURRENCY`] `buffered` stream failed,
/// the request was cancelled, or the whole background push was abandoned at
/// [`UPLOAD_DEADLINE`]. A [`run_codec`] closure that nobody is waiting for is the
/// sharp case — it still runs to completion on the blocking pool, so the
/// abandoned encode leaves behind a *complete* file. Nothing else deletes these
/// (the age-gated [`sweep_abandoned_temps`] is a crash backstop, not a mechanism),
/// so a missed path is disk that leaks past the process.
#[derive(Debug)]
pub(crate) struct TempBlob(PathBuf);

impl TempBlob {
    /// A fresh unique path under `dir`. The file itself is created by the caller;
    /// dropping a `TempBlob` whose file was never created is a no-op.
    fn new(dir: &Path) -> Self {
        Self(dir.join(format!("{}.{TEMP_BLOB_EXT}", uuid::Uuid::new_v4())))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempBlob {
    fn drop(&mut self) {
        // A guard that cannot unlink is the exact failure this type exists to
        // prevent, and the sweep won't look at the file for another
        // `TEMP_SWEEP_AGE` — say so rather than swallowing it. `NotFound` is
        // ordinary: the path is reserved before the file is created.
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            debug!(
                error = %e,
                path = %self.0.display(),
                "could not reclaim a remote cache temp blob",
            );
        }
    }
}

/// Remove temp blobs left by runs that died before their [`TempBlob`] could be
/// dropped (SIGKILL, panic-abort, power loss) — nothing else reclaims those.
///
/// Age-gated on purpose, and best-effort throughout: the temp dir is shared by
/// every heph process against this home, so an unconditional sweep would delete a
/// concurrent run's live temps — and for a pull, a temp vanishing mid-transfer is
/// a fatal error, not a miss. See [`TEMP_SWEEP_AGE`].
fn sweep_abandoned_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let (mut swept, mut kept) = (0usize, 0usize);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != TEMP_BLOB_EXT) {
            continue;
        }
        // Unreadable metadata or an mtime in the future (clock skew, a foreign
        // filesystem) both read as "not provably old" — keep it.
        let abandoned = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= TEMP_SWEEP_AGE);
        if !abandoned {
            kept += 1;
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            swept += 1;
        }
    }
    if swept > 0 {
        debug!(
            swept,
            kept,
            dir = %dir.display(),
            "reclaimed abandoned remote cache temp blobs",
        );
    }
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
    /// Final key segment of every object under `prefix`.
    ///
    /// One request enumerates a whole revision, which is why the read path checks
    /// blob presence by listing rather than by a `HEAD` per artifact: a build over
    /// thousands of targets turns `outputs × targets` metadata requests into one
    /// per target. Object keys mirror the target address (see
    /// [`RemoteCacheSet::key`]), so a revision is exactly one key prefix.
    async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>>;
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

/// Requests reserved for *metadata* — manifest reads and blob presence checks —
/// out of a cache's request budget ([`RemoteCacheDef::concurrency`]).
///
/// Metadata and bulk blob transfers are not interchangeable, so they must not
/// share one budget. A blob download holds its request slot for the entire
/// stream — seconds to minutes; a manifest read is a few hundred bytes, sits on
/// the critical path under the target's per-addr **write** lock, and is bounded
/// by [`METADATA_TIMEOUT`].
///
/// Pooled together the bulk traffic wins and the build loses: a wide `//...` over
/// a fully-cached graph resolves thousands of targets at once, every request slot
/// fills with blob streams, and each queued manifest read waits out its whole
/// 60s timeout *without ever reaching the network*. Three of those in a row trip
/// the breaker below and a perfectly healthy cache drops out of the run — the
/// build then rebuilds everything from scratch and looks hung.
///
/// So metadata gets a reserve of its own that a blob stream can never occupy.
///
/// This is a **floor, not a ceiling**. An earlier version partitioned the budget
/// outright — metadata could use only these slots even while every bulk slot sat
/// idle. That inverts on a metadata-heavy run: resolving a fully-cached graph is
/// almost entirely manifest reads and presence checks (a target whose manifest is
/// local but whose blobs are not still has to ask the remote), so a build would
/// serialize thousands of targets through 32 slots while 224 went unused. See
/// [`ConfiguredCache::meta_op`] for how the floor is preserved without the cap.
const META_SLOT_RESERVE: usize = 32;

/// Split a cache's request budget into `(metadata reserve, shared pool)` slot
/// counts.
///
/// The reserve is [`META_SLOT_RESERVE`], or half the budget when that is smaller,
/// so a deliberately tiny `concurrency` still leaves room for both classes. The
/// two sum to the budget, so the store's own request cap (`LimitStore`) is never
/// the binding constraint and therefore never a place where the two classes
/// contend again.
fn split_request_budget(concurrency: usize) -> (usize, usize) {
    let total = concurrency.max(2);
    let reserve = META_SLOT_RESERVE.min(total / 2).max(1);
    (reserve, (total - reserve).max(1))
}

/// After this many consecutive failures a cache is circuit-broken: skipped
/// without further network calls or log lines. Stops a down or misconfigured
/// (e.g. auth-failing) cache from slowing every target and flooding the logs on a
/// wide build.
const FAILURE_THRESHOLD: usize = 3;

/// How long the breaker stays open the first time it trips, doubling on each
/// consecutive trip up to [`BREAKER_COOLDOWN_MAX`].
///
/// The breaker is a *pause*, not a death sentence. Tripping it permanently means
/// three transient errors early in a long build cost the remote cache for the
/// whole run — every later target rebuilds and re-uploads, which is far more
/// expensive than retrying. Backing off exponentially keeps a genuinely dead or
/// misconfigured cache cheap (a handful of probes over the run) while letting a
/// blip heal.
const BREAKER_COOLDOWN: Duration = Duration::from_secs(15);

/// Ceiling on the exponential breaker backoff.
const BREAKER_COOLDOWN_MAX: Duration = Duration::from_secs(5 * 60);

/// Monotonic base for the breaker's cooldown arithmetic. `tokio::time::Instant`
/// (not `std`) so `tokio::time::pause` can drive it in tests.
static PROCESS_START: LazyLock<tokio::time::Instant> = LazyLock::new(tokio::time::Instant::now);

fn elapsed_ms() -> u64 {
    u64::try_from(PROCESS_START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Per-cache failure tracking. The first error for a cache is logged once, then
/// every later error is suppressed; after [`FAILURE_THRESHOLD`] consecutive
/// failures the cache is paused for a backing-off cooldown so we stop hitting it
/// at all. A success resets the consecutive-failure run *and* the backoff.
#[derive(Default)]
struct CacheHealth {
    warned: AtomicBool,
    consecutive_failures: AtomicUsize,
    /// Milliseconds since [`PROCESS_START`] until which the breaker stays open;
    /// `0` when the cache has never tripped.
    disabled_until_ms: AtomicU64,
    /// Cooldown to apply on the next trip, in milliseconds; `0` before the first.
    cooldown_ms: AtomicU64,
}

/// A configured cache: its definition, the constructed backend, its health, and
/// its request budget (see [`META_SLOT_RESERVE`]).
struct ConfiguredCache {
    def: RemoteCacheDef,
    backend: Arc<dyn RemoteCacheBackend>,
    health: CacheHealth,
    /// Metadata-only permits. Nothing bulk may take one, so a manifest read can
    /// always find a slot that no blob stream is sitting on.
    meta_reserve: Semaphore,
    /// The rest of the budget. Blob transfers draw only from here; metadata
    /// borrows from here first, so idle bulk capacity is usable.
    shared_slots: Semaphore,
}

impl ConfiguredCache {
    fn new(def: RemoteCacheDef, backend: Arc<dyn RemoteCacheBackend>) -> Self {
        let (reserve, shared) = split_request_budget(def.concurrency);
        Self {
            def,
            backend,
            health: CacheHealth::default(),
            meta_reserve: Semaphore::new(reserve),
            shared_slots: Semaphore::new(shared),
        }
    }

    /// Run one metadata request — manifest read/write, revision listing, blob
    /// presence — under the cache's metadata reserve and a hard
    /// [`METADATA_TIMEOUT`].
    ///
    /// The slot is taken **before** the clock starts. The timeout is a liveness
    /// bound on the request; charging it for time spent queued turns ordinary
    /// backpressure into a fake failure, and three of those trip the breaker and
    /// cost the whole run its cache. Queue time is bounded instead by *where* we
    /// queue, below.
    ///
    /// Slot order — the shared pool first, the reserve only as a fallback:
    ///
    /// - `try_acquire` on the shared pool, never `acquire`. A success means bulk
    ///   capacity was idle and we borrow it, which is what lets a metadata-heavy
    ///   run use the whole budget instead of [`META_SLOT_RESERVE`] of it. It is a
    ///   non-blocking barge, so we never *wait* behind a blob stream.
    /// - On failure — bulk is saturated — fall back to the metadata reserve and
    ///   wait there. Nothing bulk can hold one of those, so anything ahead of us
    ///   is another short metadata request. That is the [`META_SLOT_RESERVE`]
    ///   guarantee, and it is the reason the fallback order is not the other way
    ///   round: draining the reserve first would leave later metadata queued on
    ///   the shared pool, behind exactly the multi-GiB transfers this reserve
    ///   exists to avoid.
    ///
    /// The timeout also has to be *ours*: object_store's own budget
    /// (`retry_config`) is sized for resuming multi-GiB blobs, so left to it a
    /// wedged metadata request parks the target — and everything waiting on it —
    /// for minutes.
    async fn meta_op<T, F>(&self, what: &str, fut: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let _slot = match self.shared_slots.try_acquire() {
            Ok(slot) => slot,
            Err(_) => {
                // Bulk is full and we are about to queue on the reserve. This is
                // the limiter worth reporting: metadata never *waits* on
                // `shared_slots` (it barges with `try_acquire`), so a gauge there
                // reads zero during exactly the metadata starvation it would be
                // meant to catch.
                let d = crate::engine::diag::global();
                d.limiter("remote-cache-metadata")
                    .observe(self.meta_reserve.available_permits(), d.now_ms());
                self.meta_reserve
                    .acquire()
                    .await
                    .with_context(|| format!("acquire remote cache metadata slot for {what}"))?
            }
        };
        tokio::time::timeout(METADATA_TIMEOUT, fut)
            .await
            .with_context(|| format!("{what} timed out after {METADATA_TIMEOUT:?}"))?
    }

    /// Whether the cache's breaker is currently open and it should be skipped.
    fn broken(&self) -> bool {
        let until = self.health.disabled_until_ms.load(Ordering::Relaxed);
        until != 0 && elapsed_ms() < until
    }

    /// A successful op clears the consecutive-failure run and re-arms the
    /// breaker: the cache has demonstrably recovered, so the next outage starts
    /// its backoff from scratch rather than inheriting the old one.
    fn note_ok(&self) {
        self.health.consecutive_failures.store(0, Ordering::Relaxed);
        self.health.disabled_until_ms.store(0, Ordering::Relaxed);
        self.health.cooldown_ms.store(0, Ordering::Relaxed);
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
        if n >= FAILURE_THRESHOLD && !self.broken() {
            // Double the previous cooldown (starting at `BREAKER_COOLDOWN`), so a
            // cache that keeps failing is probed ever more rarely while a blip
            // costs one short pause.
            let prev = self.health.cooldown_ms.load(Ordering::Relaxed);
            let cooldown = if prev == 0 {
                u64::try_from(BREAKER_COOLDOWN.as_millis()).unwrap_or(u64::MAX)
            } else {
                prev.saturating_mul(2)
            }
            .min(u64::try_from(BREAKER_COOLDOWN_MAX.as_millis()).unwrap_or(u64::MAX));
            self.health.cooldown_ms.store(cooldown, Ordering::Relaxed);
            self.health
                .disabled_until_ms
                .store(elapsed_ms().saturating_add(cooldown), Ordering::Relaxed);
            // Start the next window clean so recovery needs only one success, and
            // a still-broken cache needs another full run of failures to re-trip.
            self.health.consecutive_failures.store(0, Ordering::Relaxed);
            warn!(
                cache = %self.def.name,
                "remote cache paused for {:?} after {n} consecutive failures",
                Duration::from_millis(cooldown),
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

/// Hard cap on one metadata request: manifest read or write, revision listing,
/// blob presence check.
///
/// All of them move at most a few hundred bytes, so one that hasn't answered in a
/// minute is stuck rather than slow. The bound matters because every one of them
/// happens under a target's per-addr lock: left to the backend's own budget
/// (`retry_config`, sized for resuming multi-GiB blobs) a single wedged request
/// parks that target — and everything waiting on it — for minutes.
///
/// Applied by [`ConfiguredCache::meta_op`], which takes the request's slot before
/// starting the clock.
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard cap on a single write call of a blob upload, queue-free (the slot is
/// already held) but generous enough to cover a full multipart part plus
/// object_store's retries of it.
///
/// The download side already has [`InactivityReader`](super::remote_cache_objstore)
/// bounding a stalled stream; without this the *upload* side had no stall bound at
/// all short of [`UPLOAD_DEADLINE`], so a wedged push held one of the cache's bulk
/// slots for half an hour.
const BLOB_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
            caches.push(ConfiguredCache::new(def.clone(), Arc::new(backend)));
        }
        let config_hash = config_hash(defs);
        Ok(Arc::new(Self {
            caches,
            home,
            config_hash,
            read_order: OnceCell::new(),
        }))
    }

    /// Test-only: a set of exactly one readable+writable cache over `backend`, so
    /// a test can drive the read path against a stub object store.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn RemoteCacheBackend>, home: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            caches: vec![ConfiguredCache::new(
                RemoteCacheDef {
                    name: "stub".to_string(),
                    uri: "memory:///stub".to_string(),
                    read: true,
                    write: true,
                    concurrency: 4,
                },
                backend,
            )],
            home,
            config_hash: String::new(),
            read_order: OnceCell::new(),
        })
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
        let mut key = Self::revision_prefix(addr, hashin);
        key.push('/');
        key.push_str(&key_segment(name));
        key
    }

    /// Key prefix shared by every object of one revision — [`Self::key`] without
    /// the artifact segment. One `list_names` of this prefix enumerates the whole
    /// revision, which is how presence is checked without a request per artifact.
    fn revision_prefix(addr: &Addr, hashin: &str) -> String {
        let mut key = String::new();
        for c in addr.package.components() {
            key.push_str(&key_segment(c));
            key.push('/');
        }
        key.push_str(&key_segment(&addr_name_segment(addr)));
        key.push('/');
        key.push_str(&key_segment(hashin));
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
    /// A timeout surfaces as an ordinary error, which the caller already treats as
    /// "this cache didn't serve it", so a wedged cache falls through to the next
    /// one instead of parking the target. See [`ConfiguredCache::meta_op`] for the
    /// slot/timeout ordering.
    async fn read_small(&self, cache_idx: usize, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let cache = self
            .caches
            .get(cache_idx)
            .context("remote cache index out of range")?;
        cache
            .meta_op(
                &format!("read remote object {key}"),
                read_small_inner(cache, key),
            )
            .await
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
                    async move {
                        // Uploads are bulk too, and background: they must never
                        // hold a slot a critical-path metadata read needs.
                        let _slot =
                            cache.shared_slots.acquire().await.with_context(|| {
                                format!("acquire remote cache blob slot for {key}")
                            })?;
                        stream_file_to_backend(cache.backend.as_ref(), &key, path).await
                    }
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
            match cache
                .meta_op(
                    &format!("write remote manifest {manifest_key}"),
                    write_bytes_to_backend(cache.backend.as_ref(), &manifest_key, manifest_bytes),
                )
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
    ///
    /// **One request, not one per blob.** A revision is exactly one key prefix
    /// (see [`Self::revision_prefix`]), so a single listing answers for every
    /// artifact. A `HEAD` per artifact instead makes this cost
    /// `outputs × targets` metadata requests — on a graph of thousands of targets
    /// that is tens of thousands of round trips, every one of them on the critical
    /// path under a per-addr lock, which is enough to make a healthy cache look
    /// broken. Listing keeps the fail-soft guarantee at O(1) per revision.
    pub(crate) async fn blobs_exist(
        &self,
        ctoken: &dyn Cancellable,
        rev: &RemoteRevision,
        addr: &Addr,
        hashin: &str,
        names: &[String],
    ) -> bool {
        if names.is_empty() {
            return true;
        }
        let Some(cache) = self.caches.get(rev.cache_idx) else {
            return false;
        };
        if ctoken.is_cancelled() {
            return false;
        }
        let prefix = Self::revision_prefix(addr, hashin);
        let present = match cache
            .meta_op(
                &format!("list remote revision {prefix}"),
                cache.backend.list_names(&prefix),
            )
            .await
        {
            Ok(names) => names,
            Err(e) => {
                cache.note_err("revision listing", &e);
                return false;
            }
        };
        cache.note_ok();
        // Compare in key space: the listing returns the segment `Self::key` wrote,
        // which is `key_segment(name)`, not the logical artifact name.
        let present: std::collections::HashSet<&str> = present.iter().map(String::as_str).collect();
        names
            .iter()
            .all(|name| present.contains(key_segment(name).as_str()))
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
    ) -> anyhow::Result<Option<TempBlob>> {
        let cache = self
            .caches
            .get(rev.cache_idx)
            .context("remote cache index out of range")?;
        // Bulk half of the request budget, held for the whole stream — the
        // download *is* the slot's occupancy. Keeping it out of the metadata
        // reserve is what stops a wide pull from starving the manifest reads and
        // presence checks that decide every other target's hit
        // (see `META_SLOT_RESERVE`).
        let _slot = cache
            .shared_slots
            .acquire()
            .await
            .with_context(|| format!("acquire remote cache blob slot for {name}"))?;
        let key = Self::key(addr, hashin, name);
        // Opening the stream is a request/response round trip with no body yet, so
        // it gets the metadata bound — the body that follows is covered by
        // `InactivityReader`. Without a bound here a wedged GET rides
        // object_store's full retry budget while holding a bulk slot.
        let opened = tokio::time::timeout(METADATA_TIMEOUT, cache.backend.open_read(&key))
            .await
            .with_context(|| {
                format!("open remote blob {name} timed out after {METADATA_TIMEOUT:?}")
            });
        let reader = match opened.and_then(|r| r) {
            Ok(Some(reader)) => reader,
            // Manifest names a blob the cache no longer has → incomplete.
            Ok(None) => return Ok(None),
            Err(e) => {
                cache.note_err("blob download", &e);
                return Ok(None);
            }
        };
        // Every early exit below — and every point the caller can drop this
        // future — unlinks the partial temp through `TempBlob`'s drop.
        let temp = TempBlob::new(dest_dir);
        // Temp-file I/O is local and genuinely fatal — propagate.
        let mut file = tokio::fs::File::create(temp.path())
            .await
            .with_context(|| format!("create temp for remote blob {name}"))?;
        let mut reader = reader;
        // Race the transfer against cancellation. Without this a Ctrl-C during a
        // large pull is ignored until the copy ends — and the copy is exactly
        // where a wedged connection sits, so the run the user just cancelled
        // keeps hanging.
        let copied = tokio::select! {
            biased;
            () = ctoken.cancelled() => return Ok(None),
            r = tokio::io::copy(&mut reader, &mut file) => r,
        };
        if let Err(e) = copied {
            // Mid-stream network error from the cache → best-effort miss.
            cache.note_err(
                "blob download",
                &anyhow::Error::new(e).context(format!("stream remote blob {name}")),
            );
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
///
/// Copied by hand rather than with `tokio::io::copy` so every write can carry a
/// [`BLOB_WRITE_STALL_TIMEOUT`]: the upload side has no equivalent of the download
/// side's `InactivityReader`, so without a per-write bound a wedged push holds one
/// of the cache's bulk slots until [`UPLOAD_DEADLINE`] — half an hour of a slot a
/// critical-path pull could have used.
async fn stream_file_to_backend(
    backend: &dyn RemoteCacheBackend,
    key: &str,
    path: &Path,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut src = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open temp blob {}", path.display()))?;
    let mut w = bounded(backend.open_write(key), "open", key).await?;
    // One part of object_store's `BufWriter` (10 MiB) is flushed per full buffer,
    // so this is the granularity a stall is detected at.
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = src
            .read(&mut buf)
            .await
            .with_context(|| format!("read temp blob {}", path.display()))?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).context("short read from temp blob")?;
        bounded(w.write_all(chunk), "stream blob to", key).await?;
    }
    bounded(w.shutdown(), "finalize", key).await?;
    Ok(())
}

/// Write a small in-memory buffer (the manifest) to a backend object.
async fn write_bytes_to_backend(
    backend: &dyn RemoteCacheBackend,
    key: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let mut w = bounded(backend.open_write(key), "open", key).await?;
    bounded(w.write_all(bytes), "write", key).await?;
    bounded(w.shutdown(), "finalize", key).await?;
    Ok(())
}

/// Await one step of an upload under [`BLOB_WRITE_STALL_TIMEOUT`], naming the step
/// and the object so a stall says which write wedged.
async fn bounded<T, E, F>(fut: F, what: &str, key: &str) -> anyhow::Result<T>
where
    F: Future<Output = Result<T, E>>,
    E: Into<anyhow::Error>,
{
    tokio::time::timeout(BLOB_WRITE_STALL_TIMEOUT, fut)
        .await
        .with_context(|| {
            format!("{what} remote object {key} stalled for {BLOB_WRITE_STALL_TIMEOUT:?}")
        })?
        .map_err(Into::into)
        .with_context(|| format!("{what} remote object {key}"))
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

    /// The directory for transient gzip temp files — alongside the cache, so temp
    /// and final live on the same filesystem — created, and swept once per engine
    /// of anything a previous run abandoned.
    ///
    /// The path is only reachable through here, so it cannot be named without
    /// having been created. The sweep is once, not per transfer: it is a
    /// `read_dir` plus a `stat` per entry, and this sits on the path of every
    /// upload and every pull — at a hundred thousand targets, re-walking the
    /// directory each time would cost more than the leak it reclaims. On the
    /// blocking pool because a directory holding a large backlog is exactly the
    /// case where it is slow.
    async fn remote_tmp_dir(&self) -> anyhow::Result<&Path> {
        let dir = self
            .remote_tmp_ready
            .get_or_try_init(|| {
                let dir = self.home.join("cache").join("remote-tmp");
                async move {
                    hcore::blocking::run(move || {
                        std::fs::create_dir_all(&dir)
                            .with_context(|| format!("create remote temp dir {}", dir.display()))?;
                        sweep_abandoned_temps(&dir);
                        anyhow::Ok(dir)
                    })
                    .await
                }
            })
            .await?;
        Ok(dir.as_path())
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

        let tmp_dir = self.remote_tmp_dir().await?;

        // Encode every blob to a temp file (synchronous local I/O, off the runtime
        // workers via `run_codec`; the non-`Send` local reader stays on that
        // thread and never crosses an await). Each artifact is gzipped or copied
        // verbatim per `compression_for`, and the chosen encoding is recorded so
        // the remote manifest is self-describing.
        //
        // One `run_codec` **per artifact**, not one for the whole revision: a
        // single closure over every artifact holds one [`CODEC_SLOTS`] permit for
        // the entire revision and gzips it serially, so on a cold build — where
        // every target uploads — a wide revision compresses on one core while the
        // rest idle, and a critical-path *download* decode queues behind it.
        let encodes: Vec<_> = manifest
            .artifacts
            .iter()
            .map(|a| {
                let (local_cache, addr, hashin, tmp_dir) = (
                    self.local_cache.clone(),
                    addr.clone(),
                    hashin.to_string(),
                    tmp_dir.to_path_buf(),
                );
                let (name, size) = (a.name.clone(), a.size);
                async move {
                    run_codec("upload encode", move || {
                        use std::io::Read;
                        let sized = local_cache
                            .reader(&addr, &hashin, &name)
                            .with_context(|| format!("open local blob {name}"))?;
                        let encoding = compression_for(size);
                        // Guarded from the moment the path exists: this closure
                        // runs to completion on the blocking pool even when the
                        // awaiting future is long gone, and the `TempBlob` it
                        // returns is then dropped with the unwanted answer.
                        let temp = TempBlob::new(&tmp_dir);
                        let reader = sized.reader.take(sized.size);
                        match encoding {
                            ManifestArtifactEncoding::Gzip => gzip_to_file(reader, temp.path())
                                .with_context(|| format!("compress local blob {name}"))?,
                            _ => copy_to_file(reader, temp.path())
                                .with_context(|| format!("copy local blob {name}"))?,
                        }
                        Ok((name, temp, encoding))
                    })
                    .await
                }
            })
            .collect();
        // Bounded so a revision with thousands of artifacts doesn't open a temp
        // file per artifact at once; `CODEC_SLOTS` bounds the CPU underneath.
        let prepared: Vec<(String, TempBlob, ManifestArtifactEncoding)> = stream::iter(encodes)
            .buffered(REVISION_BLOB_CONCURRENCY)
            .try_collect()
            .await?;

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
            .map(|(name, temp, _)| (name.clone(), temp.path().to_path_buf()))
            .collect();
        self.remote_caches
            .put_revision(addr, hashin, &manifest_bytes, &temps)
            .await;

        // `prepared` still owns every `TempBlob`, so the encodes are unlinked
        // here — and equally on each `?` above, and if this whole push is
        // abandoned at `UPLOAD_DEADLINE` mid-`put_revision`.
        drop(prepared);
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
        hcore::blocking::run(move || {
            use std::io::Write;
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, MANIFEST_V1)
                .context("open local writer for remote manifest")?;
            w.write_all(&bytes).context("write remote manifest")?;
            anyhow::Ok(())
        })
        .await
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
    /// `Ok(false)` when the remote could not serve one of them — the object was
    /// evicted between the presence check and the read, or its transfer failed.
    /// That is **not** an error: the caller falls back to executing the target,
    /// exactly as it would have on a plain cache miss. Only genuinely fatal
    /// failures (local temp/codec I/O) and cancellation propagate as `Err`.
    pub(crate) async fn pull_remote_blobs(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        rev: &RemoteRevision,
        names: &[String],
    ) -> anyhow::Result<bool> {
        if names.is_empty() {
            return Ok(true);
        }
        let tmp_dir = self.remote_tmp_dir().await?;

        // Bounded fan-out — see `REVISION_BLOB_CONCURRENCY`. A target whose caller
        // asks for many output groups must not open every stream at once.
        // Collected eagerly — see the note in `put_revision`.
        let pulls: Vec<_> = names
            .iter()
            .map(|name| self.pull_remote_blob(ctoken, addr, hashin, rev, name, tmp_dir))
            .collect();
        let served: Vec<bool> = stream::iter(pulls)
            .buffered(REVISION_BLOB_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(served.into_iter().all(|ok| ok))
    }

    /// Stream one blob into a temp file, then decode it into the local cache. The
    /// temp file is dropped on both paths. `Ok(false)` if the remote could not
    /// serve it — see [`Self::pull_remote_blobs`].
    ///
    /// The [`TempBlob`] is handed **into** the decode closure rather than held
    /// here. Once [`run_codec`] has queued the job it runs whatever the caller
    /// does, so a guard left on this side would unlink the temp out from under a
    /// decode that is about to read it — and the local-cache writer publishes
    /// what it has when dropped, so that loses the race by writing an empty blob
    /// over a manifest that already claims the artifact is present.
    async fn pull_remote_blob(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        rev: &RemoteRevision,
        name: &str,
        tmp_dir: &Path,
    ) -> anyhow::Result<bool> {
        let encoding = rev.artifact(name)?.encoding.clone();
        let Some(temp) = self
            .remote_caches
            .fetch_blob(ctoken, rev, addr, hashin, name, tmp_dir)
            .await?
        else {
            if ctoken.is_cancelled() {
                return Err(crate::engine::error::CancelledError.into());
            }
            // Evicted between the presence check and the read, or the transfer
            // failed (already noted on the cache). Degrade to a miss.
            debug!(
                %addr,
                hashin,
                blob = name,
                "remote cache could not serve blob; rebuilding target",
            );
            return Ok(false);
        };

        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let name_owned = name.to_string();
        run_codec("download decode", move || -> anyhow::Result<()> {
            // `temp` is captured by value, so it is unlinked when this closure
            // ends — whether it ran, or was dropped un-run because the caller
            // went away before the pool picked the job up.
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, &name_owned)
                .with_context(|| format!("open local writer for downloaded blob {name_owned}"))?;
            match encoding {
                ManifestArtifactEncoding::Gzip => gunzip_from_file(temp.path(), &mut w)
                    .with_context(|| format!("decompress downloaded blob {name_owned}"))?,
                _ => copy_file_to(temp.path(), &mut w)
                    .with_context(|| format!("write downloaded blob {name_owned}"))?,
            }
            Ok(())
        })
        .await
        .map(|()| true)
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
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            anyhow::bail!("auth failed")
        }
    }

    fn failing_set(home: PathBuf) -> Arc<RemoteCacheSet> {
        Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache::new(
                def("broken", "memory:///broken", true, true),
                Arc::new(FailBackend),
            )],
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
        let cache =
            ConfiguredCache::new(def("x", "memory:///x", true, true), Arc::new(FailBackend));
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

    /// The budget split always leaves both classes at least one slot and never
    /// oversubscribes the cache's request ceiling — oversubscribing would push
    /// the contention back down into the store's own request cap, which is
    /// exactly the pool the split exists to keep metadata out of.
    #[test]
    fn request_budget_split_reserves_metadata_without_oversubscribing() {
        for concurrency in [0, 1, 2, 3, 4, 8, 64, DEFAULT_CACHE_CONCURRENCY, 4096] {
            let (meta, blob) = split_request_budget(concurrency);
            assert!(meta >= 1 && blob >= 1, "concurrency {concurrency} starved");
            assert!(
                meta + blob <= concurrency.max(2),
                "concurrency {concurrency} oversubscribed: {meta} + {blob}"
            );
            assert!(
                meta <= META_SLOT_RESERVE,
                "concurrency {concurrency} over-reserved metadata: {meta}"
            );
        }
        // A large budget gives metadata its full reserve and bulk the rest.
        assert_eq!(
            split_request_budget(DEFAULT_CACHE_CONCURRENCY),
            (
                META_SLOT_RESERVE,
                DEFAULT_CACHE_CONCURRENCY - META_SLOT_RESERVE
            ),
        );
    }

    /// The breaker is a pause, not a death sentence: after its cooldown the cache
    /// is retried, and a consecutive trip backs off further.
    ///
    /// Tripping permanently is what turned three transient errors into a whole
    /// run without a remote cache — every later target rebuilding and re-pushing,
    /// which costs far more than a retry.
    #[tokio::test(start_paused = true)]
    async fn breaker_reopens_after_a_cooldown_and_backs_off() {
        let cache =
            ConfiguredCache::new(def("x", "memory:///x", true, true), Arc::new(FailBackend));
        let e = anyhow::anyhow!("boom");

        for _ in 0..FAILURE_THRESHOLD {
            cache.note_err("op", &e);
        }
        assert!(cache.broken(), "threshold failures must trip the breaker");

        tokio::time::sleep(BREAKER_COOLDOWN + Duration::from_secs(1)).await;
        assert!(
            !cache.broken(),
            "the breaker must reopen so a recovered cache rejoins the run"
        );

        // Still failing → trips again, this time for twice as long.
        for _ in 0..FAILURE_THRESHOLD {
            cache.note_err("op", &e);
        }
        assert!(cache.broken());
        tokio::time::sleep(BREAKER_COOLDOWN + Duration::from_secs(1)).await;
        assert!(
            cache.broken(),
            "a consecutive trip must back off beyond the first cooldown"
        );

        // A success re-arms it completely: the next outage starts from the base
        // cooldown rather than inheriting the accumulated backoff.
        cache.note_ok();
        assert!(!cache.broken());
        for _ in 0..FAILURE_THRESHOLD {
            cache.note_err("op", &e);
        }
        tokio::time::sleep(BREAKER_COOLDOWN + Duration::from_secs(1)).await;
        assert!(
            !cache.broken(),
            "note_ok must reset the backoff, not just the failure run"
        );
    }

    /// Backend that counts requests per operation, so a test can assert how many
    /// round trips a presence check costs.
    #[derive(Default)]
    struct CountingListBackend {
        present: std::collections::HashSet<String>,
        lists: AtomicUsize,
        heads: AtomicUsize,
    }

    #[async_trait]
    impl RemoteCacheBackend for CountingListBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            Ok(None)
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only")
        }
        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            self.heads.fetch_add(1, Ordering::SeqCst);
            Ok(self.present.contains(key))
        }
        async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .present
                .iter()
                .filter_map(|k| k.strip_prefix(prefix)?.strip_prefix('/'))
                .map(str::to_string)
                .collect())
        }
    }

    /// A revision's presence costs **one** request regardless of how many
    /// artifacts it has.
    ///
    /// One `HEAD` per artifact makes the fail-soft check cost `outputs × targets`
    /// metadata round trips. Across thousands of targets that is tens of thousands
    /// of requests, all on the critical path under per-addr locks — enough on its
    /// own to make a healthy cache unusable. The check must scale with revisions,
    /// not with artifacts.
    #[tokio::test]
    async fn revision_presence_costs_one_request_per_revision() {
        let addr = addr();
        let names: Vec<String> = (0..64).map(|i| format!("out-{i}.tar")).collect();
        let prefix = RemoteCacheSet::revision_prefix(&addr, "h");

        let backend = Arc::new(CountingListBackend {
            present: names
                .iter()
                .map(|n| RemoteCacheSet::key(&addr, "h", n))
                .collect(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::with_backend(backend.clone(), dir.path().to_path_buf());
        let rev = RemoteRevision {
            cache_idx: 0,
            manifest: RemoteManifest {
                version: REMOTE_MANIFEST_VERSION.to_string(),
                target: addr.format(),
                hashin: "h".to_string(),
                artifacts: Vec::new(),
            },
        };

        assert!(
            set.blobs_exist(&never(), &rev, &addr, "h", &names).await,
            "every listed blob is present"
        );
        assert_eq!(
            backend.lists.load(Ordering::SeqCst),
            1,
            "presence must cost one listing for the whole revision"
        );
        assert_eq!(
            backend.heads.load(Ordering::SeqCst),
            0,
            "presence must not fall back to a request per artifact"
        );
        assert!(
            prefix.starts_with("p/t/"),
            "revision prefix should mirror the addr, got {prefix}"
        );

        // A revision missing even one blob is not servable.
        let mut missing = names.clone();
        missing.push("absent.tar".to_string());
        assert!(!set.blobs_exist(&never(), &rev, &addr, "h", &missing).await);
    }

    /// `AsyncRead` that holds a request permit for its whole lifetime — how
    /// `object_store`'s `LimitStore` accounts a GET, where the permit rides the
    /// response stream.
    struct HeldReader<R> {
        inner: R,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for HeldReader<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    /// A stream that never delivers a byte and never ends — a blob transfer in
    /// progress, occupying its request slot.
    struct NeverReader;

    impl AsyncRead for NeverReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    /// Backend with one shared request budget for every operation, where a blob
    /// GET holds its permit for the whole stream — a faithful stand-in for
    /// `LimitStore`. This is the shape that let a build starve itself: with a
    /// single pool, in-flight blob streams take every slot and a manifest read
    /// waits out its timeout without ever reaching the network.
    struct SharedBudgetBackend {
        budget: Arc<Semaphore>,
        manifest: Vec<u8>,
    }

    #[async_trait]
    impl RemoteCacheBackend for SharedBudgetBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            let permit = Arc::clone(&self.budget)
                .acquire_owned()
                .await
                .context("budget closed")?;
            if key.ends_with(MANIFEST_V1) {
                return Ok(Some(Box::pin(HeldReader {
                    inner: std::io::Cursor::new(self.manifest.clone()),
                    _permit: permit,
                })));
            }
            Ok(Some(Box::pin(HeldReader {
                inner: NeverReader,
                _permit: permit,
            })))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("unused")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            let _permit = self.budget.acquire().await.context("budget closed")?;
            Ok(true)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            let _permit = self.budget.acquire().await.context("budget closed")?;
            Ok(Vec::new())
        }
    }

    /// In-flight blob transfers must never consume the slots a manifest read
    /// needs.
    ///
    /// The regression: one pooled request budget per cache, with a blob GET
    /// holding its slot for the entire transfer. A wide build resolves thousands
    /// of targets at once, every slot fills with blob streams, and each queued
    /// manifest read — a few hundred bytes, on the critical path under the addr's
    /// write lock — burns its whole [`METADATA_TIMEOUT`] in the queue. Three
    /// of those trip the breaker and a healthy, fast cache drops out of the run.
    ///
    /// Paused time auto-advances only when every task is idle, so a manifest read
    /// stuck behind the blob streams resolves as a timeout here rather than
    /// hanging the test.
    #[tokio::test(start_paused = true)]
    async fn saturated_blob_transfers_do_not_starve_a_manifest_read() {
        const CONCURRENCY: usize = 8;
        let (meta_reserve, shared_slots) = split_request_budget(CONCURRENCY);

        let addr = addr();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: vec![RemoteManifestArtifact {
                hashout: "ho".to_string(),
                group: "out".to_string(),
                name: "out".to_string(),
                size: 1,
                r#type: ManifestArtifactType::Output,
                content_type: ManifestArtifactContentType::Tar,
                encoding: ManifestArtifactEncoding::None,
            }],
        };
        let budget = Arc::new(Semaphore::new(CONCURRENCY));
        let backend = Arc::new(SharedBudgetBackend {
            budget: Arc::clone(&budget),
            manifest: borsh::to_vec(&manifest).expect("serialize"),
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let mut cache_def = def("slow", "memory:///slow", true, true);
        cache_def.concurrency = CONCURRENCY;
        let set = Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache::new(cache_def, backend)],
            home: dir.path().to_path_buf(),
            config_hash: String::new(),
            read_order: OnceCell::new(),
        });

        // More blob pulls than there are bulk slots, so the bulk half is
        // saturated *and* has a queue behind it.
        let rev = Arc::new(RemoteRevision {
            cache_idx: 0,
            manifest,
        });
        let tmp = tempfile::tempdir().expect("tempdir");
        for i in 0..shared_slots + 4 {
            let (set, rev, addr, tmp) = (
                Arc::clone(&set),
                Arc::clone(&rev),
                addr.clone(),
                tmp.path().to_path_buf(),
            );
            tokio::spawn(async move {
                drop(
                    set.fetch_blob(&never(), &rev, &addr, "h", &format!("blob{i}"), &tmp)
                        .await,
                );
            });
        }

        // Wait for the bulk half to actually fill before reading the manifest —
        // otherwise the test could pass without ever reproducing contention.
        for _ in 0..10_000 {
            if budget.available_permits() <= meta_reserve {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            budget.available_permits() <= meta_reserve,
            "blob transfers never saturated their half of the budget"
        );

        let found = set
            .fetch_manifest(&never(), &addr, "h")
            .await
            .expect("a manifest read must not be starved by in-flight blob transfers")
            .is_some();
        assert!(found, "manifest read must resolve to a hit");
    }

    /// Backend that parks every read on `gate` and records how many were in
    /// flight at once, so a test can observe the real metadata concurrency
    /// rather than infer it from timing.
    struct ConcurrencyProbeBackend {
        gate: Arc<Semaphore>,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        manifest: Vec<u8>,
    }

    #[async_trait]
    impl RemoteCacheBackend for ConcurrencyProbeBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let _open = self.gate.acquire().await.context("gate closed")?;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Some(Box::pin(std::io::Cursor::new(self.manifest.clone()))))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("unused")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Metadata must be able to use idle bulk capacity, not just its reserve.
    ///
    /// The regression this guards: the reserve was implemented as a partition, so
    /// metadata could never exceed [`META_SLOT_RESERVE`] slots even with every
    /// bulk slot idle. Resolving a fully-cached graph is almost entirely metadata
    /// — a target whose manifest is local but whose blobs are not still has to ask
    /// the remote — so a wide build serialized thousands of targets through 32
    /// slots while the other 224 went unused, and the run looked hung.
    ///
    /// With no blob transfer in flight, `CONCURRENCY` concurrent manifest reads
    /// must all be in flight together. Under the old split the peak would pin to
    /// the reserve.
    #[tokio::test]
    async fn metadata_uses_idle_bulk_capacity() {
        const CONCURRENCY: usize = 8;
        let (meta_reserve, _shared) = split_request_budget(CONCURRENCY);

        let addr = addr();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: Vec::new(),
        };

        // No permits: every read parks inside the backend until released, so all
        // of them pile up and the peak is observable.
        let gate = Arc::new(Semaphore::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = Arc::new(ConcurrencyProbeBackend {
            gate: Arc::clone(&gate),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::clone(&peak),
            manifest: borsh::to_vec(&manifest).expect("serialize"),
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let mut cache_def = def("probe", "memory:///probe", true, true);
        cache_def.concurrency = CONCURRENCY;
        let set = Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache::new(cache_def, backend)],
            home: dir.path().to_path_buf(),
            config_hash: String::new(),
            read_order: OnceCell::new(),
        });

        let reads: Vec<_> = (0..CONCURRENCY)
            .map(|i| {
                let (set, addr) = (Arc::clone(&set), addr.clone());
                tokio::spawn(async move {
                    drop(set.fetch_manifest(&never(), &addr, &format!("h{i}")).await);
                })
            })
            .collect();

        for _ in 0..10_000 {
            if peak.load(Ordering::SeqCst) >= CONCURRENCY {
                break;
            }
            tokio::task::yield_now().await;
        }
        let observed = peak.load(Ordering::SeqCst);

        // Release before asserting so a failure doesn't leave the reads parked.
        gate.add_permits(CONCURRENCY);
        for r in reads {
            drop(r.await);
        }

        assert!(
            observed > meta_reserve,
            "metadata peaked at {observed} concurrent reads, capped by the {meta_reserve}-slot \
             reserve — idle bulk capacity is not being borrowed"
        );
        assert_eq!(
            observed, CONCURRENCY,
            "metadata should reach the cache's full request budget when no blob transfer is \
             holding slots"
        );
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
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
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
        let cache = ConfiguredCache::new(def("x", "memory:///x", true, true), backend.clone());

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
        let cache = ConfiguredCache::new(
            def("broken", "memory:///broken", true, true),
            Arc::new(FailBackend),
        );
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
            async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
                std::future::pending().await
            }
        }

        let cache = ConfiguredCache::new(
            def("hung", "memory:///hung", true, true),
            Arc::new(HungBackend),
        );

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
        async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(self
                .objects
                .keys()
                .filter_map(|k| k.strip_prefix(prefix)?.strip_prefix('/'))
                .map(str::to_string)
                .collect())
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
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn set_with(backend: Arc<dyn RemoteCacheBackend>, home: PathBuf) -> Arc<RemoteCacheSet> {
        Arc::new(RemoteCacheSet {
            caches: vec![ConfiguredCache::new(
                def("probe", "memory:///probe", true, true),
                backend,
            )],
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
        let temps: Vec<Option<TempBlob>> = stream::iter(fetches)
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

    /// Serves a manifest, then never delivers a single blob byte.
    struct StalledBlobBackend {
        manifest_key: String,
        manifest: Vec<u8>,
    }

    impl StalledBlobBackend {
        /// A set whose one cache serves `addr`'s manifest and stalls on its blobs.
        fn set(addr: &Addr, home: PathBuf) -> Arc<RemoteCacheSet> {
            let manifest = RemoteManifest {
                version: REMOTE_MANIFEST_VERSION.to_string(),
                target: addr.format(),
                hashin: "h".to_string(),
                artifacts: vec![probe_artifact(0)],
            };
            set_with(
                Arc::new(Self {
                    manifest_key: RemoteCacheSet::key(addr, "h", MANIFEST_V1),
                    manifest: borsh::to_vec(&manifest).expect("serialize manifest"),
                }),
                home,
            )
        }
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
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Temp blobs currently sitting in `dir`.
    fn temp_blobs(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == TEMP_BLOB_EXT))
            .collect()
    }

    /// A pull in progress must abort when the run is cancelled. The blob copy is
    /// exactly where a wedged connection parks, and it happens under the
    /// target's per-addr write lock — so an uninterruptible copy means Ctrl-C
    /// leaves the build hanging on the very transfer the user gave up on.
    #[tokio::test]
    async fn blob_fetch_aborts_on_cancellation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let set = StalledBlobBackend::set(&addr, dir.path().to_path_buf());

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
        let leftovers = temp_blobs(dir.path());
        assert!(
            leftovers.is_empty(),
            "cancelling must not leak partial temp files, found {leftovers:?}"
        );
    }

    /// Cancellation is only one of the ways a pull ends early — the plainer one
    /// is the future simply being dropped at its `await`, which is what happens
    /// to every other blob in a `buffered` fan-out the moment one sibling fails.
    /// No code runs on that path, so only `TempBlob`'s drop can reclaim the
    /// partial file.
    #[tokio::test]
    async fn a_dropped_blob_fetch_reclaims_its_partial_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let set = StalledBlobBackend::set(&addr, dir.path().to_path_buf());

        let ctoken = never();
        let rev = set
            .fetch_manifest(&ctoken, &addr, "h")
            .await
            .expect("fetch manifest")
            .expect("hit");
        // `Box::pin`, not `tokio::pin!`: the latter shadows the future with a
        // `Pin<&mut _>`, so `drop`ping that name is a no-op and the future — and
        // its guard — would outlive the assertion below.
        let mut fetch = Box::pin(set.fetch_blob(&ctoken, &rev, &addr, "h", "blob-0", dir.path()));

        // Poll it far enough to have created the temp and parked on the copy.
        for _ in 0..200 {
            tokio::select! {
                _ = &mut fetch => panic!("a stalled backend must never complete the copy"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            if !temp_blobs(dir.path()).is_empty() {
                break;
            }
        }
        assert_eq!(
            temp_blobs(dir.path()).len(),
            1,
            "the fetch should have a temp file open by now"
        );

        drop(fetch);

        let leftovers = temp_blobs(dir.path());
        assert!(
            leftovers.is_empty(),
            "dropping a pull mid-copy must not leak its temp, found {leftovers:?}"
        );
    }

    /// The sharp edge of the blocking pool: a `run_codec` closure runs to
    /// completion whether or not anyone still wants the answer, so an abandoned
    /// encode finishes writing a *complete* temp file after its future is gone.
    /// Nothing async is left to clean that up — the guard has to travel with the
    /// value, so that dropping the unwanted answer is what unlinks the file.
    // Multi-thread: the handshake with the closure is a blocking `recv`, so the
    // runtime needs a worker other than the one parked on it to drive the task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_encode_reclaims_its_finished_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp_dir = dir.path().to_path_buf();
        let (created_tx, created_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let encode = tokio::spawn(async move {
            run_codec("test encode", move || {
                let temp = TempBlob::new(&tmp_dir);
                std::fs::write(temp.path(), b"encoded").expect("write temp");
                created_tx.send(()).expect("signal created");
                // Hold the closure open past the point the caller gives up.
                release_rx.recv().expect("await release");
                // The other half of the contract: a job still holding its guard
                // keeps its file. This is what `pull_remote_blob`'s decode reads
                // — a guard the *caller* held would have unlinked it by now.
                assert_eq!(
                    std::fs::read(temp.path()).expect("temp must outlive the caller"),
                    b"encoded",
                );
                Ok(temp)
            })
            .await
        });

        created_rx.recv().expect("encode should start");
        assert_eq!(
            temp_blobs(dir.path()).len(),
            1,
            "the encode should have written its temp"
        );

        // The caller gives up while the blocking closure is still running.
        encode.abort();
        drop(encode.await);
        release_tx.send(()).expect("release the encode");

        // The closure now finishes and hands back a `TempBlob` nobody wants; it
        // is dropped on the blocking thread, which is where the unlink happens.
        for _ in 0..200 {
            if temp_blobs(dir.path()).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "an abandoned encode leaked its temp: {:?}",
            temp_blobs(dir.path())
        );
    }

    /// A run killed outright (SIGKILL, panic-abort) never gets to drop anything,
    /// so the sweep is the only thing that reclaims what it left. It is age-gated
    /// because the directory is shared with any concurrent heph process, whose
    /// live temps must survive untouched — for a pull, one vanishing mid-transfer
    /// is fatal, not a miss.
    #[test]
    fn the_sweep_reclaims_only_provably_abandoned_temps() {
        let dir = tempfile::tempdir().expect("tempdir");

        let plant = |name: &str, age: Option<Duration>| {
            let path = dir.path().join(name);
            let f = std::fs::File::create(&path).expect("create planted file");
            if let Some(age) = age {
                let when = std::time::SystemTime::now() - age;
                f.set_times(std::fs::FileTimes::new().set_modified(when))
                    .expect("backdate planted file");
            }
            path
        };

        let stale = plant("stale.blob", Some(TEMP_SWEEP_AGE + Duration::from_secs(60)));
        // Old enough to be an abandoned *download*, young enough to still be a
        // live upload from another process — the case the margin exists for.
        let borderline = plant("borderline.blob", Some(TEMP_SWEEP_AGE / 2));
        let fresh = plant("fresh.blob", None);
        let foreign = plant("notours.txt", Some(TEMP_SWEEP_AGE * 10));

        sweep_abandoned_temps(dir.path());

        assert!(!stale.exists(), "an abandoned temp must be reclaimed");
        assert!(
            borderline.exists(),
            "a temp that could still belong to a live transfer must be left alone"
        );
        assert!(fresh.exists(), "a live temp must be left alone");
        assert!(
            foreign.exists(),
            "the sweep must only touch files it could have written"
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
        gunzip_from_file(gz_temp.path(), &mut restored_gz).expect("gunzip");
        assert_eq!(restored_gz, raw_gz);

        let restored_plain = std::fs::read(plain_temp.path()).expect("read plain");
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
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
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
            .map(|i| {
                ConfiguredCache::new(
                    def(&format!("c{i}"), &format!("memory:///c{i}"), true, true),
                    Arc::new(BarrierBackend {
                        barrier: barrier.clone(),
                    }),
                )
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
