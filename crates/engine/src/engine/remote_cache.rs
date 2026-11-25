//! Remote (shared) cache: an ordered set of object-store backends fronting the
//! local cache. Configured via the `caches:` map in `.hephconfig2`.
//!
//! Semantics (see [`RemoteCacheSet`]):
//! - **write** — push to every writable cache in parallel; within each cache the
//!   manifest is written *last*, so a manifest that appears was complete at
//!   upload time. Blobs are independent objects, though, and a bucket lifecycle
//!   rule can expire them out from under a surviving manifest — hence the
//!   presence check in [`RemoteCacheSet::locate`].
//! - **read** — try caches one-by-one in ascending-latency order. The first
//!   cache whose manifest is present, names its own key, and holds the blobs
//!   the caller needs serves the revision; a cache that fails any of those is
//!   passed over. A blob that cache then cannot serve whole is pulled from
//!   another cache whose manifest names the very same entry
//!   ([`RemoteRevision::entry_for`]) — never a blob of another build.
//!
//! **Reads are lazy.** A remote hit is decided from the *manifest alone* — it
//! carries every artifact's `hashout`, which is all a dependent needs to compute
//! its own `hashin`. Output blobs transfer only when a caller actually reads
//! them, and only the groups it asked for (see
//! [`Engine::pull_remote_blob_from_any`]).
//! A target resolved purely to feed a dependent's hash — the common case in a
//! fully-cached build — therefore moves a few hundred manifest bytes, not its
//! outputs.
//!
//! **Separate manifest, same bytes.** The remote uses its own [`RemoteManifest`]
//! (object `manifest-v2.borsh`), distinct from the local [`Manifest`]. Each
//! remote artifact records its `encoding`, which is the local blob's: the local
//! cache already compressed what is worth compressing when it wrote it (see
//! [`cache_codec`](crate::engine::cache_codec)), so upload and download copy
//! blobs verbatim and spend no CPU on encoding. Entries an older heph wrote
//! (`manifest-v1.borsh`, gzip) are not read.
//!
//! **Streaming.** No blob is ever held whole in memory. Each blob moves through a
//! temp file: on upload the engine copies the local blob into a temp file
//! (synchronous, on the blocking pool — see [`run_codec`]) and the set streams
//! that file to the backend via object_store multipart; on download the set
//! streams the backend object into a temp file and the engine copies it into the
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
use crate::engine::cache_codec::{
    PulledLength, check_pulled_length, local_entry_name_of_remote, remote_entry_name,
};
use crate::engine::local_cache::{
    MANIFEST_CURRENT, MANIFEST_V3_VERSION, Manifest, ManifestArtifact, ManifestArtifactContentType,
    ManifestArtifactEncoding, ManifestArtifactType, artifact_is_needed, support_files_needed_for,
};
use crate::engine::remote_cache_latency::{UNREACHABLE, load_order, store_order};
use crate::engine::remote_cache_objstore::{ObjStoreBackend, StoreOptions, cache_env_prefix};
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
use tracing::{debug, info, warn};

/// Max blobs of a *single* revision transferred concurrently, on both the pull
/// and the push side.
///
/// Fanning a revision out is what makes a multi-output target fast, but it must
/// stay bounded: each in-flight download holds an open temp file plus a live
/// response stream, and each in-flight upload holds an `object_store` multipart
/// buffer (10 MiB per in-flight part; the writer is pinned to one part, so 10
/// MiB). A target with thousands of artifacts, multiplied by the
/// engine's own target-level parallelism, would otherwise run the process out of
/// file descriptors or memory. Requests past this bound simply queue.
pub(crate) const REVISION_BLOB_CONCURRENCY: usize = 32;

/// Max background upload *tasks* in flight process-wide.
///
/// [`Engine::spawn_remote_upload`] detaches one task per newly-cached target, so
/// without a bound a wide build spawns hundreds at once. Each holds a copy of
/// every one of its blobs as a temp file for its whole lifetime, and competes with the
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
/// The floor is derived: [`UPLOAD_DEADLINE`] bounds the push end to end, queue
/// wait included, and the temp is created inside that bound; the pull side has no
/// deadline of its own but stays fresh because a progressing transfer keeps
/// advancing the temp's mtime, and a *stalled* one is killed by the backend's
/// inactivity bound (see [`RemoteCacheBackend`]). Past that, plus the gap between
/// the last byte and the copy into the local cache, a file can only be residue from a run
/// hard-killed before its [`TempBlob`] could be dropped.
///
/// The value sits far above that floor on purpose, because the two directions are
/// not symmetric. Too *large* only delays reclaiming residue — disk, bounded by
/// time rather than by crash count, so a crash loop retains at most this much no
/// matter how often it crashes. Too *small* unlinks a live temp out from under a
/// concurrent heph against the same home, which on the pull side is a hard error
/// rather than a miss. The mtime-freshness premise also assumes the peer is
/// *running*: a suspended laptop or a `SIGSTOP`ed process stops advancing mtime
/// while its transfer is still perfectly live, and only a margin this wide keeps
/// that from looking abandoned.
///
/// Because the `.blob` naming is unchanged, the first run of this binary reclaims
/// what every pre-fix run left behind.
const TEMP_SWEEP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Permits for the synchronous local-cache I/O that brackets every transfer
/// (blobs cross verbatim; nothing is compressed or decompressed here).
///
/// That work is blocking and runs on the blocking pool (see
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

/// Run the synchronous transfer step (copying a blob between the local cache
/// and a temp file) through `hcore::blocking::run`, bounded by [`CODEC_SLOTS`].
///
/// It must **not** run on a runtime worker. Copying a revision's blobs is
/// blocking I/O that takes hundreds of milliseconds to seconds; with
/// hundreds of targets pulling and pushing at once, doing it inline occupies
/// every worker thread and the runtime stops polling *everything* — in-flight
/// transfers, their inactivity deadlines, the TUI. The build looks hung even
/// though no lock is actually deadlocked. (The previous `block_or_inline` did
/// exactly that: inline on Linux, `block_in_place` on macOS.)
///
/// `hcore::blocking` rather than a bare `spawn_blocking` for its concurrency
/// bound — `CODEC_SLOTS` caps this class tighter still.
///
/// The closure is `Send`, but the values it builds are not required to be — the
/// non-`Send` local-cache reader/writer is created and dropped entirely inside
/// it and never crosses an `await`.
async fn run_codec<F, R>(what: &'static str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    // Observe before queueing, so "saturated" times the wait rather than the
    // moment a permit came free. `codec` is declared by `diag::global()` and was
    // never fed, so its saturation was structurally unreportable.
    let d = crate::engine::diag::global();
    let codec = d.limiter("codec");
    // Idempotent, so arming it here costs one atomic load per call and saves a
    // separate one-time hook. `CODEC_SLOTS` is a process-wide static, so the
    // closure captures nothing.
    codec.attach_gauge(|| CODEC_SLOTS.available_permits());
    codec.observe(CODEC_SLOTS.available_permits(), d.now_ms());
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

/// Write buffer in front of a downloading blob's temp file. Bounded memory: one
/// per in-flight download, and downloads are capped by the bulk slots.
const BLOB_TEMP_WRITE_BUFFER: usize = 1 << 20;

/// The smallest such buffer: `tokio::io::copy`'s own chunk, below which a
/// buffer saves no write.
const BLOB_TEMP_WRITE_BUFFER_MIN: usize = 8 * 1024;

/// The write buffer for a blob the manifest says is `size` bytes: no larger
/// than the blob, so a small blob does not allocate a whole
/// [`BLOB_TEMP_WRITE_BUFFER`], and never under [`BLOB_TEMP_WRITE_BUFFER_MIN`].
/// `size` is the decoded size; the stored bytes of a zstd blob are usually
/// fewer, and a buffer that ends up a little short costs one more write.
fn blob_temp_write_buffer(size: u64) -> usize {
    usize::try_from(size)
        .unwrap_or(usize::MAX)
        .clamp(BLOB_TEMP_WRITE_BUFFER_MIN, BLOB_TEMP_WRITE_BUFFER)
}

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
/// abandoned copy leaves behind a *complete* file. Nothing else deletes these
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
    // "Now" comes from the directory's own filesystem, not the process clock.
    // The two can disagree — an NFS server running behind, a laptop resumed from
    // sleep — and comparing across them can read a *live* temp as provably old.
    // Deleting one out from under a transfer is not a miss on that pull, it is a
    // hard error. Same clock on both sides, and any skew cancels.
    let now = match sweep_clock(dir) {
        Some(now) => now,
        None => {
            debug!(dir = %dir.display(), "no usable mtime clock; skipping temp sweep");
            return;
        }
    };
    let (mut swept, mut kept, mut failed) = (0usize, 0usize, 0usize);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != TEMP_BLOB_EXT) {
            continue;
        }
        // Unreadable metadata or an mtime in the future both read as "not
        // provably old" — keep it.
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
        match std::fs::remove_file(&path) {
            Ok(()) => swept += 1,
            // A `.blob` directory, EPERM, an immutable flag: nothing will ever
            // reclaim it and the sweep is the only thing that looks, so it has to
            // be said out loud rather than counted as kept.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                failed += 1;
                debug!(
                    error = %e,
                    path = %path.display(),
                    "could not reclaim an abandoned remote cache temp blob",
                );
            }
        }
    }
    if swept > 0 || failed > 0 {
        debug!(
            swept,
            kept,
            failed,
            dir = %dir.display(),
            "reclaimed abandoned remote cache temp blobs",
        );
    }
}

/// The current time as `dir`'s own filesystem records it, by writing a file there
/// and reading back its mtime.
///
/// Deliberately not [`std::time::SystemTime::now`] — see [`sweep_abandoned_temps`].
/// The name is outside [`TEMP_BLOB_EXT`], so a concurrent sweep never treats it as
/// residue; two processes racing on it only overwrite each other's timestamp,
/// which is all either one wanted.
fn sweep_clock(dir: &Path) -> Option<std::time::SystemTime> {
    let probe = dir.join(".sweep-clock");
    let now = std::fs::File::create(&probe)
        .and_then(|f| f.metadata()?.modified())
        .ok();
    drop(std::fs::remove_file(&probe));
    now
}

/// A streaming object store. The set layers cache semantics (manifest affinity,
/// ordering, parallel fan-out) on top; a backend only moves bytes.
///
/// **A reader must bound its own inactivity.** `fetch_blob` puts a
/// [`METADATA_TIMEOUT`] on opening the stream, but the body copy that follows is
/// raced only against cancellation — an implementation that can sit forever
/// mid-response holds a bulk request slot indefinitely, and leaves a temp file
/// whose mtime stops advancing, which is the premise [`TEMP_SWEEP_AGE`] rests on.
/// [`ObjStoreBackend`] does this with its `InactivityReader`.
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
    /// The command that renews this backend's credential (`az login`), shown
    /// when it is refused. Only for a credential with a known way to renew it
    /// — a CLI login — and `None` otherwise: a remedy that is a guess sends the
    /// user the wrong way.
    fn auth_remedy(&self) -> Option<&str> {
        None
    }
}

/// One cache entry from `caches:` — name plus URI, permissions, and request cap.
///
/// Credentials are not part of it: they come from the environment — the ambient
/// cloud variables, heph's per-kind `HEPH_S3_*` namespace, or this cache's own
/// `HEPH_CACHE_<NAME>_*` namespace, which is why `name` is load-bearing beyond
/// diagnostics.
/// Plain data so it can live in [`crate::engine::Config`] (Clone/Debug/PartialEq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCacheDef {
    pub name: String,
    pub uri: String,
    pub read: bool,
    pub write: bool,
    /// Max in-flight requests to this cache (object_store `LimitStore`).
    pub concurrency: usize,
    /// Base URL of an S3-compatible service to talk to instead of AWS. `s3://`
    /// caches only — see [`StoreOptions`].
    pub endpoint: Option<String>,
    /// Region to sign S3 requests for. `s3://` caches only.
    pub region: Option<String>,
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
/// so a deliberately tiny `concurrency` still leaves room for both classes.
///
/// The two sum to the budget, so the store's own request cap (`LimitStore`) is
/// never the binding constraint and therefore never a place where the two
/// classes contend again — but that holds only while **one slot means one store
/// request**. A multipart writer left at `object_store`'s default concurrency
/// takes a store permit per in-flight part, and one blob slot silently becomes
/// eight; see `ObjStoreBackend::open_write`, which pins it to one.
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

/// A remote cache refused who we are, or we could not establish who we are: a
/// 401/403 from the store, or a credential that could not be obtained for a
/// reason other than the network (an expired login, a revoked key, an
/// unreadable credential file).
///
/// Unlike every other failure this does not heal within a run — the user has to
/// log in again — so the breaker does not back off and re-probe on it: the
/// first one disables the cache for the rest of the run. A backend marks the
/// failures it can only classify at its own seam (a token-endpoint rejection
/// arrives as an opaque store error) by putting this in the error chain;
/// [`is_auth_failure`] recognizes it, and the store's own 401/403 variants, from
/// there.
#[derive(Debug)]
pub(crate) struct AuthError(pub(crate) Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authentication failed")
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// The one-line warning for an auth failure that disables cache `cache`: what
/// failed and why, as briefly as the error allows; what that costs; and the
/// backend's [`auth_remedy`] when it has one, last, so the command copies
/// cleanly.
///
/// The full cause chain is logged separately at debug. A refusal's reason is
/// the response body the server sent — a token endpoint's multi-line JSON, an
/// S3 or Azure XML document — which otherwise dominates the warning; when the
/// warning shows less than the whole chain it says how to see the rest.
///
/// [`auth_remedy`]: RemoteCacheBackend::auth_remedy
pub(crate) fn auth_warning(cache: &str, e: &anyhow::Error, remedy: Option<&str>) -> String {
    let (reason, condensed) = match refusal_reason(e) {
        Some(reason) => (reason, true),
        None => one_line(&hcore::hmemoizer::chain_message(e)),
    };
    let mut msg = format!(
        "remote cache {cache:?}: authentication failed ({reason}); \
         builds continue without it for the rest of this run"
    );
    if condensed {
        msg.push_str(" (HEPH_LOG=debug shows the full error)");
    }
    msg.push('.');
    if let Some(remedy) = remedy {
        msg.push_str(" To sign in again, run `");
        msg.push_str(remedy);
        msg.push('`');
    }
    msg
}

/// Longest error text a warning carries, in chars. A response body the
/// reason parser does not recognize (a proxy's HTML page) can be kilobytes.
const WARNING_DETAIL_MAX_CHARS: usize = 300;

/// `text` on one line, cut to [`WARNING_DETAIL_MAX_CHARS`] with a trailing `…`;
/// and whether anything was cut.
fn one_line(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len().min(WARNING_DETAIL_MAX_CHARS * 4));
    let mut chars = 0;
    for word in text.split_whitespace() {
        let sep = (!out.is_empty()).then_some(' ');
        for c in sep.into_iter().chain(word.chars()) {
            if chars >= WARNING_DETAIL_MAX_CHARS {
                out.push('…');
                return (out, true);
            }
            out.push(c);
            chars += 1;
        }
    }
    (out, false)
}

/// The server's own words for a refusal, read from the response body at the
/// tail of the innermost link (object_store reports a refused request as
/// `…400 Bad Request: <body>`), in the shapes the supported stores answer with:
///
/// - an OAuth 2.0 token endpoint (Google, Entra ID; RFC 6749 §5.2):
///   `{"error": "invalid_grant", "error_description": "…"}`;
/// - a Google API: `{"error": {"code": 403, "message": "…"}}`;
/// - S3 and Azure Blob: `<Error><Code>…</Code><Message>…</Message></Error>`.
///
/// `None` for anything else, and the caller falls back to the cause chain.
fn refusal_reason(e: &anyhow::Error) -> Option<String> {
    let last = e.chain().last()?.to_string();
    let reason = json_reason(&last).or_else(|| xml_reason(&last))?;
    // Collapse the body's line breaks and indentation: the warning is one line.
    Some(one_line(&reason).0)
}

fn json_reason(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum ErrorField {
        OAuth(String),
        Api { message: String },
    }
    #[derive(serde::Deserialize)]
    struct Body {
        error: ErrorField,
        error_description: Option<String>,
    }
    let body: Body = serde_json::from_str(text.get(text.find('{')?..)?).ok()?;
    Some(match (body.error, body.error_description) {
        (ErrorField::OAuth(code), Some(d)) if !d.trim().is_empty() => format!("{code}: {d}"),
        (ErrorField::OAuth(code), _) => code,
        (ErrorField::Api { message }, _) => message,
    })
}

fn xml_reason(text: &str) -> Option<String> {
    let element = |name: &str| {
        let (_, rest) = text.split_once(&format!("<{name}>"))?;
        let (value, _) = rest.split_once(&format!("</{name}>"))?;
        Some(value.trim()).filter(|v| !v.is_empty())
    };
    let code = element("Code")?;
    Some(match element("Message") {
        Some(message) => format!("{code}: {message}"),
        None => code.to_owned(),
    })
}

/// Whether `e` is an [`AuthError`]-class failure anywhere in its chain.
pub(crate) fn is_auth_failure(e: &anyhow::Error) -> bool {
    e.chain().any(is_auth_link)
}

/// One link of [`is_auth_failure`]. An `io::Error` built around another error
/// (object_store's `BufWriter` reports a failed upload that way) hides it from
/// `source()`, so it is unwrapped by hand.
fn is_auth_link(e: &(dyn std::error::Error + 'static)) -> bool {
    if e.is::<AuthError>() {
        return true;
    }
    if let Some(os) = e.downcast_ref::<object_store::Error>() {
        return matches!(
            os,
            object_store::Error::Unauthenticated { .. }
                | object_store::Error::PermissionDenied { .. }
        );
    }
    e.downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        .is_some_and(|inner| is_auth_link(inner))
}

/// Per-cache failure tracking. The first error for a cache is logged once, then
/// every later error is suppressed; after [`FAILURE_THRESHOLD`] consecutive
/// failures the cache is paused for a backing-off cooldown so we stop hitting it
/// at all. A success resets the consecutive-failure run *and* the backoff.
///
/// An [`AuthError`] skips all of that: the cache is disabled for the rest of the
/// run on the first one, and no later success re-arms it.
#[derive(Default)]
struct CacheHealth {
    warned: AtomicBool,
    /// Set by the first [`AuthError`]; the cache is skipped from then on.
    auth_failed: AtomicBool,
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
    /// Set by the first damaged entry this cache served ([`Self::note_damage`]):
    /// that one is a warning, every later one is at debug.
    damage_warned: AtomicBool,
    /// Set the first time a read skips this cache for its open breaker, so the
    /// skip is logged once per process, not once per target.
    skip_logged: AtomicBool,
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
            damage_warned: AtomicBool::new(false),
            skip_logged: AtomicBool::new(false),
        }
    }

    /// Record that this cache answered with a damaged entry under `key` — a
    /// manifest that does not deserialize, names a gzip blob or another
    /// revision, or a blob that is not the length its manifest records — and
    /// was passed over for it. The cache answered: nothing counts against its
    /// breaker.
    ///
    /// The first one per cache per process is a warning, since a cache that
    /// stores damaged entries needs a human; the rest are at debug, or one
    /// restore that died part-way would print a line per target.
    fn note_damage(&self, addr: &Addr, hashin: &str, key: &str, reason: &str) {
        if self.damage_warned.swap(true, Ordering::Relaxed) {
            debug!(
                cache = %self.def.name,
                %addr,
                hashin,
                key,
                reason,
                "remote cache entry is damaged; passed over",
            );
        } else {
            warn!(
                cache = %self.def.name,
                %addr,
                hashin,
                key,
                reason,
                "remote cache entry is damaged; passed over. Further damaged entries on this cache are logged at debug.",
            );
        }
    }

    /// Log, once per process, that a read skipped this cache for its open
    /// breaker; the failures that tripped it were already warned about.
    fn note_skipped(&self) {
        if !self.skip_logged.swap(true, Ordering::Relaxed) {
            debug!(
                cache = %self.def.name,
                "remote cache paused after repeated failures; reads skip it until it recovers",
            );
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

    /// A slot from the bulk half of the request budget, for a transfer of the
    /// object `id`, held for as long as its bytes move. Reported to the
    /// saturation watchdog before queueing (see [`observe_transfer_slots`]).
    ///
    /// Keeping transfers out of the metadata reserve is what stops a wide pull
    /// from starving the manifest reads and presence checks that decide every
    /// other target's hit (see [`META_SLOT_RESERVE`]).
    async fn bulk_slot(&self, id: &str) -> anyhow::Result<tokio::sync::SemaphorePermit<'_>> {
        observe_transfer_slots(self);
        self.shared_slots
            .acquire()
            .await
            .with_context(|| format!("acquire remote cache blob slot for {id}"))
    }

    /// Open `key` for a bulk read under [`METADATA_TIMEOUT`]: the open is a
    /// request/response round trip with no body yet, while the body that
    /// follows is bounded by the backend's inactivity reader. Without a bound
    /// here a wedged GET rides `object_store`'s full retry budget while holding
    /// a bulk slot. A timeout reads `open <kind> <id> timed out after …`.
    async fn open_bounded(
        &self,
        key: &str,
        kind: &str,
        id: &str,
    ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
        tokio::time::timeout(METADATA_TIMEOUT, self.backend.open_read(key))
            .await
            .with_context(|| format!("open {kind} {id} timed out after {METADATA_TIMEOUT:?}"))?
    }

    /// Whether the cache's breaker is currently open and it should be skipped.
    fn broken(&self) -> bool {
        if self.health.auth_failed.load(Ordering::Relaxed) {
            return true;
        }
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
    /// trip the breaker after [`FAILURE_THRESHOLD`] consecutive failures — or
    /// disable the cache outright on an [`AuthError`].
    fn note_err(&self, op: &str, e: &anyhow::Error) {
        if is_auth_failure(e) {
            // Retrying cannot fix a rejected credential, so there is no
            // backoff: every probe would fail the same way, add its latency to
            // a target, and repeat the warning. Ops already in flight land here
            // too and stay quiet.
            self.health.warned.store(true, Ordering::Relaxed);
            if !self.health.auth_failed.swap(true, Ordering::Relaxed) {
                // `auth_warning` stays one tested string; the fields carry
                // what a log filter needs.
                warn!(
                    cache = %self.def.name,
                    op,
                    "{}",
                    auth_warning(&self.def.name, e, self.backend.auth_remedy()),
                );
                debug!(
                    cache = %self.def.name,
                    op,
                    error = %hcore::hmemoizer::chain_message(e),
                    "remote cache authentication failure",
                );
            }
            return;
        }
        if !self.health.warned.swap(true, Ordering::Relaxed) {
            // `chain_message` is the cause chain with each cause once, no
            // backtrace (`{e:?}` would dump the full `Caused by:` ladder plus a
            // process backtrace); `one_line` keeps a response body it carries
            // from taking over the warning. The whole chain goes to debug.
            let (detail, cut) = one_line(&hcore::hmemoizer::chain_message(e));
            let more = if cut {
                " (HEPH_LOG=debug shows the full error)"
            } else {
                ""
            };
            warn!(
                cache = %self.def.name,
                op,
                "remote cache unavailable, skipping it for {op}: {detail}{more}. Further errors for this cache are suppressed.",
            );
            debug!(
                cache = %self.def.name,
                op,
                error = %hcore::hmemoizer::chain_message(e),
                "remote cache unavailable",
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
/// [`Manifest`], so the two layers can evolve separately. Today they store the
/// same bytes: each entry's [`encoding`](RemoteManifestArtifact::encoding) is the
/// local blob's, and upload and download copy blobs verbatim (see
/// [`cache_codec`](crate::engine::cache_codec)).
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
    /// How this artifact's object is stored on the remote: the local blob's
    /// encoding (`None` or `Zstd`), since the remote stores it verbatim.
    pub encoding: ManifestArtifactEncoding,
}

/// Remote manifest format version — independent of the local manifest's, so the
/// two can evolve separately. Informational, like the local one: the object
/// name ([`REMOTE_MANIFEST`]) is what keeps an older heph from reading it.
const REMOTE_MANIFEST_VERSION: &str = "2.0.0";

/// The object name of a revision's manifest on the remote. Its own constant,
/// not the local [`MANIFEST_CURRENT`]: the local manifest moved to v3 (host
/// paths, v3 blob names) while the remote format did not, so every heph reads
/// and writes the remote under the same keys. The blob keys stay v2's
/// [`entry_name`](crate::engine::cache_codec::entry_name) too (see
/// [`remote_entry_name`]).
pub(crate) const REMOTE_MANIFEST: &str = "manifest-v2.borsh";

/// A revision *located* on one remote cache — the decision half of a remote hit.
///
/// Holds no blob bytes. The manifest names the revision's artifacts, records
/// each one's `hashout` (enough to answer "already built" and to feed a
/// dependent's `hashin`) and its on-remote [`encoding`](RemoteManifestArtifact::encoding),
/// so individual blobs can be pulled later, on demand, from the cache the
/// manifest came from.
pub(crate) struct RemoteRevision {
    /// The cache that served the manifest, where this revision's blobs are
    /// pulled from first. A blob this cache cannot serve is pulled from
    /// another cache whose manifest names the same entry ([`Self::entry_for`]).
    cache_idx: usize,
    pub manifest: RemoteManifest,
    /// The revision as every *other* readable cache holds it, looked up once,
    /// the first time a blob of this revision falls through
    /// ([`Self::others`]), and shared by every later one: a revision costs
    /// one manifest read per cache, however many of its blobs fall through.
    others: OnceCell<Others>,
}

/// The other caches' copies of a [`RemoteRevision`], in read order.
struct Others {
    revs: Vec<RemoteRevision>,
    /// Whether a cache passed over on the way held a damaged manifest.
    damaged: bool,
}

/// What a cache must hold, beyond a manifest that matches its key, for
/// [`RemoteCacheSet::locate`] to take it.
pub(crate) enum Need<'a> {
    /// The manifest alone: enough for the hashouts, and for a lazy pull, which
    /// checks each blob as it streams it and falls through on its own.
    Manifest,
    /// Every blob a reader of these output groups needs, support files
    /// included, present on the cache: the hit decision, which commits to
    /// "already built" before any byte moves.
    Groups(&'a [String]),
}

/// The caches a walk of [`RemoteCacheSet`] has examined, so a later walk asks
/// none of them again, and whether one of them held a damaged manifest.
#[derive(Default)]
struct Tried {
    tiers: Vec<usize>,
    damaged: bool,
}

/// What [`RemoteCacheSet::read_revision`] found on one cache.
enum ReadRevision {
    /// No manifest under the key: the cache does not hold the revision.
    Absent,
    /// The read failed; noted against the cache's breaker.
    Unreadable,
    /// A manifest the cache answered with, but cannot serve; logged by
    /// [`ConfiguredCache::note_damage`].
    Damaged,
    Valid(RemoteManifest),
}

impl RemoteRevision {
    fn new(cache_idx: usize, manifest: RemoteManifest) -> Self {
        Self {
            cache_idx,
            manifest,
            others: OnceCell::new(),
        }
    }

    /// This revision's entry for the local manifest's `want`: the same entry
    /// in every field — name, type, group, hashout, size, encoding and
    /// content type.
    ///
    /// The name alone is not enough. The local manifest may have been
    /// mirrored from another cache, or by an older heph, and a revision of
    /// the same `(addr, hashin)` built twice by a target that is not
    /// reproducible names the same blob with another hashout: pulling it would
    /// hand a reader bytes its dependents' keys were never derived from. A
    /// `manifest-v1.borsh` an older heph mirrored names bare blobs, which no
    /// v2 revision names, so it never matches either.
    ///
    /// What this guarantees is the entry, not the bytes: the hashout
    /// identifies the content and the v2 name's suffix its encoding, but
    /// nothing hashes what a cache stores under that name (E-hashout in
    /// [`check_pulled_length`]). An entry with an empty hashout (a log)
    /// identifies nothing and never matches. `None` means this cache cannot
    /// serve `want`.
    ///
    /// `want.name` is the local name: a v3 one is compared under its
    /// [`remote_entry_name`].
    ///
    /// [`check_pulled_length`]: crate::engine::cache_codec::check_pulled_length
    pub(crate) fn entry_for(&self, want: &ManifestArtifact) -> Option<&RemoteManifestArtifact> {
        if want.hashout.is_empty() {
            return None;
        }
        let remote = remote_entry_name(&want.name);
        self.manifest.artifacts.iter().find(|a| {
            a.name == remote
                && a.r#type == want.r#type
                && a.group == want.group
                && a.hashout == want.hashout
                && a.size == want.size
                && a.encoding == want.encoding
                && a.content_type == want.content_type
        })
    }

    /// Every other readable cache's copy of this revision ([`Others`]),
    /// looked up on first use: a walk over the whole read order — caches
    /// before this one included — that skips this one, reads each manifest
    /// once and lists nothing, since a blob a cache lacks costs one GET that
    /// answers 404, which is cheaper than a listing and no failure.
    ///
    /// `Err` only on cancellation, which leaves the cell unset for a later
    /// caller.
    async fn others(
        &self,
        set: &RemoteCacheSet,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<&Others> {
        self.others
            .get_or_try_init(|| async {
                let mut tried = Tried {
                    tiers: vec![self.cache_idx],
                    damaged: false,
                };
                let mut revs = Vec::new();
                while let Some(rev) = set
                    .walk(ctoken, addr, hashin, &Need::Manifest, &mut tried)
                    .await
                {
                    revs.push(rev);
                }
                if ctoken.is_cancelled() {
                    return Err(crate::engine::error::CancelledError.into());
                }
                Ok(Others {
                    revs,
                    damaged: tried.damaged,
                })
            })
            .await
    }

    /// The artifact entry for the local name `name`, or an error if the manifest
    /// never named it — a caller asking for a blob outside the revision is a
    /// bug, not a miss.
    fn artifact(&self, name: &str) -> anyhow::Result<&RemoteManifestArtifact> {
        let remote = remote_entry_name(name);
        self.manifest
            .artifacts
            .iter()
            .find(|a| a.name == remote)
            .with_context(|| {
                format!(
                    "remote manifest for {} does not name blob {name}",
                    self.manifest.target
                )
            })
    }
}

/// Buffer a scratch snapshot is streamed through, both ways: what a pull or a
/// push holds in memory instead of the whole archive.
pub(crate) const SNAPSHOT_STREAM_BUF: usize = 128 * 1024;

/// One readable cache as the scratch lineage reads it, handed out by
/// [`RemoteCacheSet::scratch_sources`].
///
/// Each read is bounded the way the matching artifact read is — a listing or a
/// `.meta` read as a metadata request, a snapshot as a blob — and its outcome
/// feeds the cache's breaker, so scratch and artifact reads see one health
/// record: either path's failures pause the cache for both.
pub struct ScratchSource<'a> {
    cache: &'a ConfiguredCache,
}

impl std::fmt::Debug for ScratchSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchSource")
            .field("cache", &self.cache.def.name)
            .finish()
    }
}

impl ScratchSource<'_> {
    /// The cache's configured name.
    pub fn name(&self) -> &str {
        &self.cache.def.name
    }

    /// Every object name under `prefix`.
    pub async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let what = format!("list remote scratch heads under {prefix}");
        self.checked(
            "scratch list",
            self.cache
                .meta_op(&what, self.cache.backend.list_names(prefix)),
        )
        .await
    }

    /// A small object, whole — a snapshot's `.meta`. `None` when it is absent.
    ///
    /// Open and body share one [`METADATA_TIMEOUT`]: the object is a few
    /// hundred bytes, so a body that outlasts the bound is a wedged connection,
    /// not a large transfer.
    pub async fn read_small(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let what = format!("read remote scratch meta {key}");
        self.checked(
            "scratch meta read",
            self.cache.meta_op(&what, read_small_inner(self.cache, key)),
        )
        .await
    }

    /// A snapshot, streamed into `dest`: the number of bytes written, or `None`
    /// when it is absent.
    ///
    /// Streamed rather than returned whole because a snapshot is a whole
    /// compressed cache, and holding one in memory makes the peak RSS of a pull
    /// the size of the cache.
    ///
    /// Read as a blob is: a bulk slot held for the stream, the open under
    /// [`METADATA_TIMEOUT`] (it is a round trip with no body yet), and the body
    /// under the backend's inactivity bound, which a large snapshot on a slow
    /// link can outlast legitimately and a stalled one cannot.
    ///
    /// A failure to write `dest` is reported but not held against the cache:
    /// only the remote side's failures feed its breaker.
    pub async fn read_snapshot_into<W>(
        &self,
        key: &str,
        dest: &mut W,
    ) -> anyhow::Result<Option<u64>>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        let read = async {
            let _slot = self.cache.bulk_slot(key).await?;
            let opened = self
                .cache
                .open_bounded(key, "remote scratch snapshot", key)
                .await?;
            let Some(mut r) = opened else {
                return Ok(None);
            };
            // Copied by hand rather than with `tokio::io::copy` so a local write
            // failure is told apart from a remote one: it ends the copy as an
            // `Ok` the breaker does not count.
            let mut buf = vec![0u8; SNAPSHOT_STREAM_BUF];
            let mut written = 0u64;
            loop {
                let n = tokio::io::AsyncReadExt::read(&mut r, &mut buf)
                    .await
                    .with_context(|| format!("download remote scratch snapshot {key}"))?;
                if n == 0 {
                    break;
                }
                let chunk = buf.get(..n).context("short read from scratch snapshot")?;
                if let Err(e) = dest.write_all(chunk).await {
                    return Ok(Some(Err(e)));
                }
                written += n as u64;
            }
            Ok(Some(Ok(written)))
        };
        match self.checked("scratch pull", read).await? {
            None => Ok(None),
            Some(Ok(written)) => Ok(Some(written)),
            Some(Err(e)) => {
                Err(e).with_context(|| format!("write remote scratch snapshot {key} locally"))
            }
        }
    }

    /// Run `fut` unless the breaker opened since this source was handed out,
    /// and record its outcome on the breaker.
    ///
    /// Rechecked per read because a head walk makes several in a row — a list
    /// and a `.meta` per entry, for each scope — and a cache that trips partway
    /// through must stop costing a timeout per remaining request.
    async fn checked<T>(
        &self,
        op: &str,
        fut: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        if self.cache.broken() {
            anyhow::bail!(
                "remote cache `{}` is paused after repeated failures",
                self.cache.def.name
            );
        }
        let res = fut.await;
        match &res {
            Ok(_) => self.cache.note_ok(),
            Err(e) => self.cache.note_err(op, e),
        }
        res
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
        // Each cache reads credentials from a namespace derived from its name.
        // The derivation is not injective — `a-b` and `a_b` both flatten to
        // `HEPH_CACHE_A_B_` — so two caches can end up sharing one namespace.
        // That is always a config mistake, and the cost of not catching it is a
        // cache silently signing with another cache's credentials, so it is
        // rejected by name rather than resolved by position.
        let mut namespaces: std::collections::HashMap<String, &str> =
            std::collections::HashMap::with_capacity(defs.len());
        for def in defs {
            let prefix = cache_env_prefix(&def.name);
            if let Some(other) = namespaces.insert(prefix.clone(), &def.name) {
                anyhow::bail!(
                    "caches `{other}` and `{}` both read their credentials from `{prefix}*`; \
                     rename one so each cache has an environment namespace of its own",
                    def.name
                );
            }
        }

        let mut caches = Vec::with_capacity(defs.len());
        for def in defs {
            let cache_env_prefix = cache_env_prefix(&def.name);
            let backend = ObjStoreBackend::from_uri(
                &def.uri,
                def.concurrency,
                &StoreOptions {
                    endpoint: def.endpoint.as_deref(),
                    region: def.region.as_deref(),
                    cache_env_prefix: Some(&cache_env_prefix),
                },
            )
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
        Self::with_backends(vec![("stub", backend)], home)
    }

    /// Test-only: a set of readable+writable caches over `backends`, read in
    /// the order given — no latency probe — so a test can make one cache fail
    /// in a way the next must cover.
    #[cfg(test)]
    pub(crate) fn with_backends(
        backends: Vec<(&str, Arc<dyn RemoteCacheBackend>)>,
        home: PathBuf,
    ) -> Arc<Self> {
        let order: Vec<usize> = (0..backends.len()).collect();
        Self::with_backends_in_order(backends, order, home)
    }

    /// Test-only: [`Self::with_backends`], read in `order` (indices into
    /// `backends`) rather than in the order given.
    #[cfg(test)]
    pub(crate) fn with_backends_in_order(
        backends: Vec<(&str, Arc<dyn RemoteCacheBackend>)>,
        order: Vec<usize>,
        home: PathBuf,
    ) -> Arc<Self> {
        let caches = backends
            .into_iter()
            .map(|(name, backend)| {
                ConfiguredCache::new(
                    RemoteCacheDef {
                        name: name.to_string(),
                        uri: format!("memory:///{name}"),
                        read: true,
                        write: true,
                        concurrency: 4,
                        endpoint: None,
                        region: None,
                    },
                    backend,
                )
            })
            .collect();
        Arc::new(Self {
            caches,
            home,
            config_hash: String::new(),
            read_order: OnceCell::new_with(Some(order)),
        })
    }

    /// Test-only: fix the read order to `names`, fastest-first, instead of
    /// measuring it, so a test can say which cache is read first. Must be
    /// called before the first read; an order already settled is an error.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pin_read_order_for_tests(&self, names: &[&str]) -> anyhow::Result<()> {
        let order = names
            .iter()
            .map(|name| {
                self.caches
                    .iter()
                    .position(|c| c.def.name == *name && c.def.read)
                    .with_context(|| format!("no readable cache named `{name}`"))
            })
            .collect::<anyhow::Result<Vec<usize>>>()?;
        self.read_order
            .set(order)
            .context("the read order is already settled")
    }

    /// An empty set with no home. Test-only: production builds every set through
    /// [`Self::new`] with the resolved home, even when no cache is configured.
    #[cfg(any(test, feature = "test-support"))]
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            caches: Vec::new(),
            home: PathBuf::new(),
            config_hash: String::new(),
            read_order: OnceCell::new(),
        })
    }

    /// Readable caches, fastest-first, as `(name, backend)`.
    ///
    /// Exposed for the scratch lineage, which lives in its own key namespace
    /// (`scratch/v1/…`) and cannot go through the revision/manifest paths above —
    /// a scratch is keyed by a declaration and a branch, not by an input hash, so
    /// none of the addr/hashin plumbing applies to it.
    ///
    /// A [`ScratchSource`], not the bare backend: every read through it takes
    /// the same breaker, metadata bound and request budget as an artifact read.
    /// Handing out the backend let a scratch pull go around all three, so a
    /// cache whose artifact reads had already timed out and tripped the breaker
    /// still held a scratch prepare — and the slot lock under it — for as long
    /// as object_store kept retrying.
    pub async fn scratch_sources(&self) -> Vec<ScratchSource<'_>> {
        let order = self.read_order().await;
        order
            .iter()
            .filter_map(|&i| self.caches.get(i))
            .filter(|c| !c.broken())
            .map(|cache| ScratchSource { cache })
            .collect()
    }

    /// Writable caches, as `(name, backend)`. Same rationale as
    /// [`readable_backends`](Self::readable_backends).
    pub fn writable_backends(&self) -> Vec<(String, Arc<dyn RemoteCacheBackend>)> {
        self.caches
            .iter()
            .filter(|c| c.def.write)
            .map(|c| (c.def.name.clone(), Arc::clone(&c.backend)))
            .collect()
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
    pub(crate) fn revision_prefix(addr: &Addr, hashin: &str) -> String {
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
    /// artifact's name and the temp file holding its bytes as the local cache
    /// stores them. Within a cache all blobs upload concurrently and the manifest is
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
                        let _slot = cache.bulk_slot(&key).await?;
                        stream_file_to_backend(cache.backend.as_ref(), &key, path).await
                    }
                })
                .collect();
            // Bounded fan-out: each in-flight put holds a multipart buffer, so a
            // revision with thousands of artifacts must not open them all at
            // once. Unordered, so a slow blob does not hold the next one back
            // from a free slot. Returning on the first error drops the stream,
            // abandoning the rest — a cache that just failed isn't worth
            // finishing.
            let uploaded: anyhow::Result<()> = stream::iter(blob_puts)
                .buffer_unordered(REVISION_BLOB_CONCURRENCY)
                .try_collect()
                .await;
            if let Err(e) = uploaded {
                cache.note_err("blob upload", &e);
                return;
            }
            // Manifest last: its presence implies every blob is already stored.
            let manifest_key = Self::key(addr, hashin, REMOTE_MANIFEST);
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

    /// Locate `(addr, hashin)` on the readable caches, in latency order: the
    /// first cache that passes the manifest read, the key check, and holds what
    /// `need` asks for — **manifest and presence only, no blob transfer**.
    ///
    /// This is the whole remote hit/miss decision: the manifest carries every
    /// artifact's `hashout`, so "already built" is answerable, and a dependent's
    /// `hashin` computable, without moving a single output byte. Blobs follow
    /// later, per caller, via [`Self::fetch_blob`].
    ///
    /// Every per-cache rejection passes that cache over, so a partial or
    /// damaged entry on one never hides the revision on another: a manifest
    /// that is absent, unreadable, does not deserialize, names a gzip blob, or
    /// names another target or `hashin` than its key ([`read_revision`]); a
    /// blob the listing lacks. Only a transport error counts against the
    /// cache's breaker — a listing that succeeded and lacks a blob is the cache
    /// answering, not failing.
    ///
    /// `None` when no cache can serve it, or on cancellation.
    ///
    /// [`read_revision`]: Self::read_revision
    pub(crate) async fn locate(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        need: Need<'_>,
    ) -> Option<RemoteRevision> {
        if !self.has_readable() {
            return None;
        }
        self.walk(ctoken, addr, hashin, &need, &mut Tried::default())
            .await
    }

    /// [`Self::locate`]'s walk, over the caches `tried` does not name yet.
    /// Every cache it examines goes into `tried`, the one it returns included,
    /// so walking again with the same `tried` resumes where this one stopped
    /// and asks no cache twice; a damaged manifest on the way sets
    /// `tried.damaged`.
    async fn walk(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        need: &Need<'_>,
        tried: &mut Tried,
    ) -> Option<RemoteRevision> {
        // v2 only. A v1 entry (written gzip by an older heph) is not read: a
        // fallback would cost a second round trip on every miss, which is most
        // lookups of a cold build, to save one rebuild per target on upgrade.
        let manifest_key = Self::key(addr, hashin, REMOTE_MANIFEST);
        for &i in self.read_order().await {
            if tried.tiers.contains(&i) {
                continue;
            }
            if ctoken.is_cancelled() {
                return None;
            }
            tried.tiers.push(i);
            let Some(cache) = self.caches.get(i) else {
                continue;
            };
            // Skip caches the breaker has tripped; the reads below record
            // success/failure so a flaky cache trips.
            if cache.broken() {
                cache.note_skipped();
                continue;
            }
            let manifest = match self
                .read_revision(i, cache, &manifest_key, addr, hashin)
                .await
            {
                ReadRevision::Valid(manifest) => manifest,
                ReadRevision::Damaged => {
                    tried.damaged = true;
                    continue;
                }
                ReadRevision::Absent | ReadRevision::Unreadable => continue,
            };
            if let Need::Groups(groups) = need {
                // Resolve groups to blob names through the same "needed" rule
                // the read path uses, so presence is checked against exactly
                // what will be read.
                let has_output = manifest
                    .artifacts
                    .iter()
                    .any(|a| a.r#type == ManifestArtifactType::Output);
                let support = support_files_needed_for(has_output, groups);
                let names: Vec<&str> = manifest
                    .artifacts
                    .iter()
                    .filter(|a| artifact_is_needed(&a.r#type, &a.group, groups, support))
                    .map(|a| a.name.as_str())
                    .collect();
                if !self
                    .blobs_present(ctoken, cache, addr, hashin, names.into_iter())
                    .await
                {
                    continue;
                }
            }
            debug!(
                cache = %cache.def.name,
                %addr,
                hashin,
                "remote cache holds the revision",
            );
            return Some(RemoteRevision::new(i, manifest));
        }
        None
    }

    /// Read cache `i`'s manifest for `(addr, hashin)` and check it can serve
    /// that revision: it deserializes, names no gzip blob, and names the target
    /// and `hashin` of the key it was stored under.
    ///
    /// Only a failed read counts against the cache's breaker: a manifest that
    /// is there and wrong is a damaged entry — a copy cut short, a restore that
    /// died part-way, an object moved under another key — and another cache
    /// may hold it whole.
    async fn read_revision(
        &self,
        i: usize,
        cache: &ConfiguredCache,
        manifest_key: &str,
        addr: &Addr,
        hashin: &str,
    ) -> ReadRevision {
        let bytes = match self.read_small(i, manifest_key).await {
            Ok(Some(bytes)) => {
                cache.note_ok();
                bytes
            }
            Ok(None) => {
                cache.note_ok();
                debug!(
                    cache = %cache.def.name,
                    %addr,
                    hashin,
                    key = manifest_key,
                    "remote cache has no manifest for the revision",
                );
                return ReadRevision::Absent;
            }
            Err(e) => {
                cache.note_err("manifest read", &e);
                return ReadRevision::Unreadable;
            }
        };
        let manifest = match borsh::from_slice::<RemoteManifest>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                cache.note_damage(
                    addr,
                    hashin,
                    manifest_key,
                    &format!("manifest does not deserialize: {e}"),
                );
                return ReadRevision::Damaged;
            }
        };
        // Downloads store the remote bytes verbatim, so the local cache must be
        // able to decode every encoding the manifest names. No heph writes gzip
        // under this manifest name; refuse it rather than store bytes nothing
        // can read.
        if let Some(a) = manifest
            .artifacts
            .iter()
            .find(|a| a.encoding == ManifestArtifactEncoding::Gzip)
        {
            cache.note_damage(
                addr,
                hashin,
                manifest_key,
                &format!("manifest names gzip blob {}", a.name),
            );
            return ReadRevision::Damaged;
        }
        // The key is the revision's identity; a manifest under it that names
        // another one would serve that revision's hashouts and blobs as this
        // one's.
        if manifest.hashin != hashin || manifest.target != addr.format() {
            cache.note_damage(
                addr,
                hashin,
                manifest_key,
                &format!(
                    "manifest names another revision than its key: {} {}",
                    manifest.target, manifest.hashin
                ),
            );
            return ReadRevision::Damaged;
        }
        ReadRevision::Valid(manifest)
    }

    /// Whether `cache` holds every one of `names` of revision `(addr, hashin)` —
    /// presence only, no bytes transferred.
    ///
    /// This is what keeps a lazy pull fail-*soft*. The hit is decided from the
    /// manifest, but the blobs it names are separate objects that a bucket
    /// lifecycle rule can expire independently, so "manifest present" alone does
    /// not prove the revision is still servable. Checking presence up front, at
    /// decision time, turns an evicted revision back into an ordinary cache miss
    /// (the target executes, or the next cache serves it) instead of a failure
    /// discovered later, mid-read, after the engine already committed to
    /// "already built".
    ///
    /// Any error (or cancellation) answers `false`: unproven presence must never
    /// be reported as a hit. A listing that succeeds is a healthy cache whatever
    /// it lacks, so only an error counts against the breaker.
    ///
    /// **One request, not one per blob.** A revision is exactly one key prefix
    /// (see [`Self::revision_prefix`]), so a single listing answers for every
    /// artifact. A `HEAD` per artifact instead makes this cost
    /// `outputs × targets` metadata requests — on a graph of thousands of targets
    /// that is tens of thousands of round trips, every one of them on the critical
    /// path under a per-addr lock, which is enough to make a healthy cache look
    /// broken. Listing keeps the fail-soft guarantee at O(1) per revision.
    ///
    /// `names` are local names, looked up under their [`remote_entry_name`];
    /// so is [`Self::fetch_blob`]'s `name`.
    async fn blobs_present<'n>(
        &self,
        ctoken: &dyn Cancellable,
        cache: &ConfiguredCache,
        addr: &Addr,
        hashin: &str,
        names: impl ExactSizeIterator<Item = &'n str>,
    ) -> bool {
        if names.len() == 0 {
            return true;
        }
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
        // which is `key_segment` of the blob's remote name, not the local name.
        let present: std::collections::HashSet<&str> = present.iter().map(String::as_str).collect();
        let mut names = names;
        if let Some(absent) =
            names.find(|name| !present.contains(key_segment(remote_entry_name(name)).as_str()))
        {
            debug!(
                cache = %cache.def.name,
                %addr,
                hashin,
                blob = absent,
                "remote cache lacks a blob of the revision; passed over",
            );
            return false;
        }
        true
    }

    /// The name of the cache `rev` was located on, for diagnostics.
    pub(crate) fn cache_name(&self, rev: &RemoteRevision) -> &str {
        self.caches
            .get(rev.cache_idx)
            .map_or("unknown", |c| c.def.name.as_str())
    }

    /// Stream one blob of `rev` from the cache that served its manifest into a
    /// temp file under `dest_dir`, as stored (the engine checks its length and
    /// copies it verbatim into the local cache — see
    /// [`Engine::pull_remote_blob_from_any`], and [`Engine::pull_remote_blobs`]
    /// for a pull that does not fall through).
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
        // download *is* the slot's occupancy.
        let _slot = cache.bulk_slot(name).await?;
        let key = Self::key(addr, hashin, remote_entry_name(name));
        let reader = match cache.open_bounded(&key, "remote blob", name).await {
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
        //
        // Buffered because `tokio::io::copy` writes in 8 KiB chunks and every
        // `poll_write` on a bare `tokio::fs::File` is a `spawn_blocking` round
        // trip: ~131k blocking-pool dispatches per GiB pulled. The buffer turns
        // that into one per MiB, or one per reader stall (`copy` flushes when
        // the reader is pending). `shutdown` below flushes the tail.
        let file = tokio::fs::File::create(temp.path())
            .await
            .with_context(|| format!("create temp for remote blob {name}"))?;
        // Sized to the blob when the manifest says it is smaller than the
        // buffer; a name it does not list (callers check first) gets the whole.
        let buffer = rev
            .manifest
            .artifacts
            .iter()
            .find(|a| a.name == name)
            .map_or(BLOB_TEMP_WRITE_BUFFER, |a| blob_temp_write_buffer(a.size));
        let mut file = tokio::io::BufWriter::with_capacity(buffer, file);
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

/// Report the bulk transfer budget's free slots to the saturation watchdog,
/// immediately before queueing on it.
///
/// `remote-cache-transfer` was registered with the [`Limiter`] set and never
/// observed, so the one diagnostic that would have named a saturated blob budget
/// reported nothing — the metadata half was wired, the bulk half was not. Read
/// *before* the `acquire`, so a full budget shows as zero rather than as the one
/// slot this caller is about to take.
///
/// [`Limiter`]: crate::engine::diag::Limiter
fn observe_transfer_slots(cache: &ConfiguredCache) {
    let d = crate::engine::diag::global();
    d.limiter("remote-cache-transfer")
        .observe(cache.shared_slots.available_permits(), d.now_ms());
}

/// The unbounded body of [`RemoteCacheSet::read_small`] and
/// [`ScratchSource::read_small`], split out so the timeout can wrap it as a
/// single future.
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

/// Copy `reader` verbatim into a new file at `dest`.
fn copy_to_file(mut reader: impl std::io::Read, dest: &Path) -> anyhow::Result<()> {
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("create temp {}", dest.display()))?;
    std::io::copy(&mut reader, &mut file).context("copy")?;
    Ok(())
}

/// Stable hash of the definition set (order-independent) used to invalidate the
/// persisted latency order when caches are added, removed, or re-pointed —
/// including re-pointed at a different `endpoint`/`region`, which changes what
/// host a cache talks to and so what its measured latency means.
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
        h.update(&[0]);
        h.update(d.endpoint.as_deref().unwrap_or_default().as_bytes());
        h.update(&[0]);
        h.update(d.region.as_deref().unwrap_or_default().as_bytes());
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
    /// The task takes its own read lock on the revision while it reads it, and
    /// releases it before letting go of the request: the build's read lasts only
    /// as long as the build's artifacts, which can be gone before the push starts.
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
                let _permit = match UPLOAD_SLOTS.acquire().await {
                    Ok(permit) => permit,
                    Err(e) => {
                        warn!(error = ?e, %addr, "remote cache upload slot unavailable");
                        return;
                    }
                };
                // The upload reads the revision's manifest and blobs, so it
                // holds its own read on the revision for exactly that long —
                // the build's read lives only as long as the build's artifacts,
                // and without one a `clean` or a `--force` rebuild in another
                // process could delete or replace the bytes mid-push. Taken
                // after the slot, so a queued push pins nothing. It may wait
                // for the builder to downgrade its write, which it does right
                // after this task is spawned; the builder never waits on us.
                let read = match engine.result_lock().read(&addr, &hashin, rs.ctoken()).await {
                    Ok(read) => read,
                    Err(e) => {
                        warn!(error = ?e, %addr, "remote cache upload could not lock the revision");
                        return;
                    }
                };
                engine.upload_to_remote(&addr, &hashin).await;
                drop(read);
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

            // Release the request before `_dec` gives its slot back. This task
            // can be the last owner of the `RequestStateData` — it outlives the
            // app future — and dropping it is what submits the request's
            // deferred cache-history trims (`RequestState::defer_trim`). Letting
            // `_dec` go first would leave a window where the counter reads zero
            // and the shutdown drain exits with those trims never enqueued.
            drop(rs);
        });
    }

    /// The directory for transient transfer temp files — alongside the cache, so temp
    /// and final live on the same filesystem — created, and swept once per engine
    /// of anything a previous run abandoned.
    ///
    /// The path is only reachable through here, so it cannot be named without
    /// having been created. The sweep runs once, not per transfer: it is a
    /// `read_dir` plus a `stat` per entry, and this sits on the path of every
    /// upload and every pull — at a hundred thousand targets, re-walking the
    /// directory each time would cost far more than the residue it reclaims.
    ///
    /// It is also *detached*, so the first transfer of the run does not wait on
    /// it. Nothing depends on the sweep having finished — its whole job is
    /// deleting files this process will never look at — and a directory holding a
    /// large backlog is precisely the case where it is slow and the case where
    /// waiting would be most visible. Both it and the `create_dir_all` go to the
    /// blocking pool.
    async fn remote_tmp_dir(&self) -> anyhow::Result<&Path> {
        let dir = self
            .remote_tmp_ready
            .get_or_try_init(|| {
                let dir = self.home.join("cache").join("remote-tmp");
                async move {
                    let dir = hcore::blocking::run(move || {
                        std::fs::create_dir_all(&dir)
                            .with_context(|| format!("create remote temp dir {}", dir.display()))?;
                        anyhow::Ok(dir)
                    })
                    .await?;
                    tokio::spawn(enclose::enclose!((dir) async move {
                        hcore::blocking::run(move || sweep_abandoned_temps(&dir)).await;
                    }));
                    anyhow::Ok(dir)
                }
            })
            .await?;
        Ok(dir.as_path())
    }

    /// Push a just-cached revision to the writable remote caches. Reads the
    /// local manifest + blobs, copies each blob as stored into a temp file, and
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
        // Off the worker, not the synchronous `read_manifest`: this task is spawned
        // the moment `cache_locally` returns, so the manifest key it reads is the
        // one just *queued* to the sqlite writer, and `reader` parks on that
        // commit. Up to `MAX_CONCURRENT_UPLOADS` of these run at once on plain
        // runtime workers — the exact shape that stalls the reactor.
        let manifest = {
            let (local_cache, addr, hashin) =
                (self.local_cache.clone(), addr.clone(), hashin.to_string());
            hcore::blocking::run(move || Self::read_manifest_from(&local_cache, &addr, &hashin))
                .await?
        };
        let Some(manifest) = manifest else {
            return Ok(());
        };
        // A revision whose outputs name host paths is valid only on a host that
        // has them, so it never leaves this one (I6). The run already fails a
        // target that declares paths with `cache.remote` on; this refuses an
        // entry that reached here anyway, whatever `cache.remote` says.
        if !manifest.host_paths.is_empty() {
            info!(
                %addr,
                hashin,
                host_paths = manifest.host_paths.len(),
                "not uploading a cache entry that declares host paths",
            );
            return Ok(());
        }

        let tmp_dir = self.remote_tmp_dir().await?;

        // Copy every blob, as stored, to a temp file (synchronous local I/O, off
        // the runtime workers via `run_codec`; the non-`Send` local reader stays
        // on that thread and never crosses an await). The local cache already
        // chose each blob's encoding when it wrote it (see `cache_codec`), so
        // the remote takes the stored bytes verbatim, and the remote manifest
        // records the local encoding: no blob is encoded here.
        //
        // One `run_codec` **per artifact**, not one for the whole revision: a
        // single closure over every artifact holds one [`CODEC_SLOTS`] permit for
        // the entire revision and copies it serially, so a wide revision would
        // use one core while a critical-path download queues behind it.
        let copies: Vec<_> = manifest
            .artifacts
            .iter()
            .map(|a| {
                let (local_cache, addr, hashin, tmp_dir) = (
                    self.local_cache.clone(),
                    addr.clone(),
                    hashin.to_string(),
                    tmp_dir.to_path_buf(),
                );
                let name = a.name.clone();
                async move {
                    run_codec("upload copy", move || {
                        use std::io::Read;
                        let sized = local_cache
                            .reader(&addr, &hashin, &name)
                            .with_context(|| format!("open local blob {name}"))?;
                        // Guarded from the moment the path exists: this closure
                        // runs to completion on the blocking pool even when the
                        // awaiting future is long gone, and the `TempBlob` it
                        // returns is then dropped with the unwanted answer.
                        let temp = TempBlob::new(&tmp_dir);
                        let reader = sized.reader.take(sized.size);
                        copy_to_file(reader, temp.path())
                            .with_context(|| format!("copy local blob {name}"))?;
                        Ok((name, temp))
                    })
                    .await
                }
            })
            .collect();
        // Bounded so a revision with thousands of artifacts doesn't open a temp
        // file per artifact at once; `CODEC_SLOTS` bounds the CPU underneath.
        // Unordered: each copy is named, so nothing needs them in manifest order.
        let prepared: Vec<(String, TempBlob)> = stream::iter(copies)
            .buffer_unordered(REVISION_BLOB_CONCURRENCY)
            .try_collect()
            .await?;

        // Build the remote manifest from the local one, in its order, each
        // artifact keeping the encoding its blob is stored in, under its remote
        // name.
        let remote_manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: manifest.target.clone(),
            hashin: hashin.to_string(),
            artifacts: manifest
                .artifacts
                .iter()
                .map(|a| RemoteManifestArtifact {
                    hashout: a.hashout.clone(),
                    group: a.group.clone(),
                    name: remote_entry_name(&a.name).to_owned(),
                    size: a.size,
                    r#type: a.r#type.clone(),
                    content_type: a.content_type.clone(),
                    encoding: a.encoding.clone(),
                })
                .collect(),
        };
        let manifest_bytes =
            borsh::to_vec(&remote_manifest).context("serialize remote manifest")?;

        let temps: Vec<(String, PathBuf)> = prepared
            .iter()
            .map(|(name, temp)| {
                (
                    remote_entry_name(name).to_owned(),
                    temp.path().to_path_buf(),
                )
            })
            .collect();
        self.remote_caches
            .put_revision(addr, hashin, &manifest_bytes, &temps)
            .await;

        // `prepared` still owns every `TempBlob`, so the copies are unlinked
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
    /// presence-checked before the hit is accepted, so a revision whose blobs one
    /// cache has expired is located on the next cache that holds it whole, or
    /// degrades to a miss (execute) instead of failing later, mid-read (see
    /// [`RemoteCacheSet::locate`]). Returns the mirrored local manifest plus the located
    /// [`RemoteRevision`], which callers keep to pull their own outputs on demand
    /// via [`Self::pull_remote_blob_from_any`].
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
        // Every blob a caller of these groups could read must still be on the
        // cache for the hit to stand; a cache that lacks one is passed over for
        // the next.
        let Some(rev) = self
            .remote_caches
            .locate(ctoken, addr, hashin, Need::Groups(output_groups))
            .await
        else {
            return Ok(None);
        };

        let local_manifest = local_manifest_from_remote(&rev.manifest, hashin);
        let bytes = borsh::to_vec(&local_manifest).context("serialize local manifest")?;
        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        // Same reasoning as `cache_artifact_locally`: the local-cache writer is
        // synchronous (and not `Send`), so it must not run on a runtime worker.
        hcore::blocking::run(move || {
            use std::io::Write;
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, MANIFEST_CURRENT)
                .context("open local writer for remote manifest")?;
            w.write_all(&bytes).context("write remote manifest")?;
            w.commit().context("commit remote manifest")?;
            anyhow::Ok(())
        })
        .await
        .with_context(|| format!("mirror remote manifest for {addr} {hashin}"))?;
        debug!(
            %addr,
            hashin,
            "mirrored a remote revision with no host paths; its local hits are unchecked",
        );

        Ok(Some((local_manifest, rev)))
    }

    /// Pull exactly `names` from `rev`'s cache into the local cache, as stored.
    /// Nothing else transfers, and nothing falls through to another cache: the
    /// hit path pulls through [`Self::pull_remote_blob_from_any`] instead. This
    /// is for a caller that takes `rev` as it is (the run log, best-effort).
    ///
    /// `Ok(false)` when the cache could not serve one of them — the object was
    /// evicted, its transfer failed, or it is not the length its manifest
    /// records. That is **not** an error. Only local I/O failures and
    /// cancellation propagate as `Err`.
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

        // Bounded fan-out — see `REVISION_BLOB_CONCURRENCY`. A target whose caller
        // asks for many output groups must not open every stream at once.
        // Unordered, so one slow blob does not keep the next from a free slot.
        // Collected eagerly — see the note in `put_revision`.
        let pulls: Vec<_> = names
            .iter()
            .map(|name| async move {
                // Only a blob the revision names is fetched.
                let entry = rev.artifact(name)?;
                self.pull_remote_blob(ctoken, addr, hashin, rev, name, entry)
                    .await
            })
            .collect();
        let pulled: Vec<Pulled> = stream::iter(pulls)
            .buffer_unordered(REVISION_BLOB_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(pulled.into_iter().all(|p| matches!(p, Pulled::Served)))
    }

    /// Pull the local manifest's blob `want` into the local cache from the
    /// first cache that serves it: `first`'s, then each other readable cache,
    /// in read order (caches before `first` included), whose manifest names
    /// the very same entry ([`RemoteRevision::entry_for`]).
    ///
    /// The other caches are looked up once per revision, not once per blob
    /// ([`RemoteRevision::others`]), with no listing: a cache that lacks the
    /// object answers its GET with a 404, which costs no more than a listing
    /// would and counts against nothing.
    ///
    /// A cache is passed over when its manifest names `want` differently, the
    /// object is gone, the transfer fails, or the bytes do not have the length
    /// `want` records (a restore that died part-way). None of those fails the
    /// target: `Ok(false)` when no cache serves it, and the caller rebuilds,
    /// as on a plain miss. Only local failures and cancellation are `Err`.
    ///
    /// A target that is not reproducible can end up rebuilt *and* refused
    /// here: a blob cut short on `first` while the only other cache holds
    /// another build of the revision (another hashout, so no match) leaves
    /// nothing to serve, the target rebuilds, and its new outputs differ from
    /// the hashouts its dependents were keyed on. That ends in the engine's
    /// existing diverged-rebuild stop — loud, and correct: no cache held
    /// bytes those keys were derived from.
    pub(crate) async fn pull_remote_blob_from_any(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        first: &RemoteRevision,
        want: &ManifestArtifact,
    ) -> anyhow::Result<bool> {
        let mut damaged = false;
        if let Some(entry) = self.matching_entry(first, addr, hashin, want) {
            match self
                .pull_remote_blob(ctoken, addr, hashin, first, &want.name, entry)
                .await?
            {
                Pulled::Served => return Ok(true),
                Pulled::Unavailable => {}
                Pulled::Damaged => damaged = true,
            }
        }
        let others = first
            .others(&self.remote_caches, ctoken, addr, hashin)
            .await?;
        damaged |= others.damaged;
        for rev in &others.revs {
            let Some(entry) = self.matching_entry(rev, addr, hashin, want) else {
                continue;
            };
            match self
                .pull_remote_blob(ctoken, addr, hashin, rev, &want.name, entry)
                .await?
            {
                Pulled::Served => {
                    debug!(
                        cache = self.remote_caches.cache_name(rev),
                        %addr,
                        hashin,
                        blob = want.name,
                        "remote blob served by a cache other than the one that located the revision",
                    );
                    return Ok(true);
                }
                Pulled::Unavailable => {}
                Pulled::Damaged => damaged = true,
            }
        }
        // A cache that found damage has been warned about; the rebuild it
        // costs is worth a line of its own. A blob that is simply gone is a
        // miss like any other.
        if damaged {
            warn!(
                %addr,
                hashin,
                blob = want.name,
                "no remote cache could serve blob whole; rebuilding target",
            );
        } else {
            debug!(
                %addr,
                hashin,
                blob = want.name,
                "no remote cache could serve blob; rebuilding target",
            );
        }
        Ok(false)
    }

    /// [`RemoteRevision::entry_for`], logging a cache passed over for naming
    /// no matching entry.
    fn matching_entry<'r>(
        &self,
        rev: &'r RemoteRevision,
        addr: &Addr,
        hashin: &str,
        want: &ManifestArtifact,
    ) -> Option<&'r RemoteManifestArtifact> {
        let entry = rev.entry_for(want);
        if entry.is_none() {
            debug!(
                cache = self.remote_caches.cache_name(rev),
                %addr,
                hashin,
                blob = want.name,
                "remote cache names no blob matching the local manifest's; passed over",
            );
        }
        entry
    }

    /// Stream one blob into a temp file, check its length against `entry`, then
    /// copy it verbatim into the local cache under the local name `name` (the
    /// local manifest's, which `entry` names under its [`remote_entry_name`]).
    /// The temp file is dropped on every path. See [`Pulled`] for what a cache
    /// that could not serve it answers.
    ///
    /// The length check runs on the temp file the bytes were just streamed into,
    /// before anything reaches the local cache, so a blob cut short is never
    /// served: a raw blob's length, or a zstd blob's seek table, must give the
    /// decoded `size` `entry` records ([`cache_codec::check_pulled_length`]).
    /// On the hit path `entry` is [`RemoteRevision::entry_for`] the local
    /// manifest's, which matches its size and encoding, so this is the length
    /// the local manifest records.
    ///
    /// The [`TempBlob`] is handed **into** the copy closure rather than held
    /// here. Once [`run_codec`] has queued the job it runs whatever the caller
    /// does, so a guard left on this side would unlink the temp out from under a
    /// copy that is about to read it — turning a served blob into a spurious
    /// read failure. (The local-cache writer commits only on success, so the
    /// failed copy at least discards rather than publishing a truncated blob.)
    ///
    /// [`cache_codec::check_pulled_length`]: crate::engine::cache_codec::check_pulled_length
    async fn pull_remote_blob(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        rev: &RemoteRevision,
        name: &str,
        entry: &RemoteManifestArtifact,
    ) -> anyhow::Result<Pulled> {
        let tmp_dir = self.remote_tmp_dir().await?;
        let Some(temp) = self
            .remote_caches
            .fetch_blob(ctoken, rev, addr, hashin, name, tmp_dir)
            .await?
        else {
            if ctoken.is_cancelled() {
                return Err(crate::engine::error::CancelledError.into());
            }
            // Evicted between the presence check and the read, or the transfer
            // failed (already noted on the cache).
            debug!(
                cache = self.remote_caches.cache_name(rev),
                %addr,
                hashin,
                blob = name,
                "remote cache could not serve blob; passed over",
            );
            return Ok(Pulled::Unavailable);
        };

        let local_cache = self.local_cache.clone();
        let addr_owned = addr.clone();
        let hashin_owned = hashin.to_string();
        let name_owned = name.to_string();
        let encoding = entry.encoding.clone();
        let size = entry.size;
        // Stored verbatim: the remote holds the bytes in the encoding the local
        // manifest (mirrored from the remote one) records, so nothing decodes.
        let length = run_codec("download copy", move || -> anyhow::Result<PulledLength> {
            use std::io::{Seek, SeekFrom};
            // `temp` is captured by value, so it is unlinked when this closure
            // ends — whether it ran, or was dropped un-run because the caller
            // went away before the pool picked the job up.
            let mut file = std::fs::File::open(temp.path())
                .with_context(|| format!("open temp {}", temp.path().display()))?;
            let length = check_pulled_length(&mut file, &encoding, size)
                .with_context(|| format!("read downloaded blob {name_owned}"))?;
            if let PulledLength::Mismatch(_) = length {
                return Ok(length);
            }
            file.seek(SeekFrom::Start(0))
                .with_context(|| format!("rewind downloaded blob {name_owned}"))?;
            let mut w = local_cache
                .writer(&addr_owned, &hashin_owned, &name_owned)
                .with_context(|| format!("open local writer for downloaded blob {name_owned}"))?;
            std::io::copy(&mut file, &mut w)
                .with_context(|| format!("write downloaded blob {name_owned}"))?;
            w.commit()
                .with_context(|| format!("commit downloaded blob {name_owned}"))?;
            Ok(length)
        })
        .await?;
        match length {
            PulledLength::Matches => Ok(Pulled::Served),
            PulledLength::Mismatch(e) => {
                // The cache answered with damaged bytes, not an outage: the
                // breaker does not count it, and another cache may hold it whole.
                if let Some(cache) = self.remote_caches.caches.get(rev.cache_idx) {
                    cache.note_damage(
                        addr,
                        hashin,
                        &RemoteCacheSet::key(addr, hashin, &entry.name),
                        &hcore::hmemoizer::chain_message(&e),
                    );
                }
                Ok(Pulled::Damaged)
            }
        }
    }
}

/// What one cache answered to [`Engine::pull_remote_blob`].
#[derive(Debug)]
enum Pulled {
    /// The blob is in the local cache now.
    Served,
    /// The cache does not have it, or the transfer failed (noted against its
    /// breaker): a miss on this cache.
    Unavailable,
    /// The cache sent bytes that are not the length its manifest records;
    /// logged by [`ConfiguredCache::note_damage`], not counted as a failure.
    Damaged,
}

/// The local view of a remote manifest: a [`MANIFEST_CURRENT`] revision with the
/// same artifacts and encodings, since a download stores the remote bytes
/// verbatim, each under its local v3 name
/// ([`local_entry_name_of_remote`]).
///
/// It declares no host paths: the remote carries none (an entry that declares
/// some is never uploaded), so a mirrored revision is served without a liveness
/// check.
pub(crate) fn local_manifest_from_remote(remote: &RemoteManifest, hashin: &str) -> Manifest {
    Manifest {
        version: MANIFEST_V3_VERSION.to_string(),
        target: remote.target.clone(),
        created_at_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0),
        hashin: hashin.to_string(),
        artifacts: remote
            .artifacts
            .iter()
            .map(|a| ManifestArtifact {
                hashout: a.hashout.clone(),
                group: a.group.clone(),
                name: local_entry_name_of_remote(&a.name),
                size: a.size,
                r#type: a.r#type.clone(),
                content_type: a.content_type.clone(),
                encoding: a.encoding.clone(),
            })
            .collect(),
        host_paths: Vec::new(),
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
            endpoint: None,
            region: None,
        }
    }

    /// Each cache reads credentials from a namespace derived from its name, and
    /// the derivation flattens everything a shell cannot spell — so two names
    /// can land on one namespace. Resolving that by position would have one
    /// cache silently signing with the other's credentials; name both and stop.
    #[test]
    fn caches_that_share_an_environment_namespace_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = RemoteCacheSet::new(
            &[
                def("build-cache", "memory:///a", true, true),
                def("build.cache", "memory:///b", true, true),
            ],
            dir.path().to_path_buf(),
        )
        .err()
        .expect("a shared namespace must not be accepted");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("build-cache") && msg.contains("build.cache"),
            "{msg}"
        );
        assert!(msg.contains("HEPH_CACHE_BUILD_CACHE_"), "{msg}");

        // Distinct namespaces are of course fine.
        RemoteCacheSet::new(
            &[
                def("build-cache", "memory:///a", true, true),
                def("other", "memory:///b", true, true),
            ],
            dir.path().to_path_buf(),
        )
        .expect("distinct namespaces");
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

    /// `remote-cache-transfer` was registered with the saturation watchdog and
    /// never observed, so the diagnostic that would name a saturated blob budget
    /// reported nothing — the metadata half was wired, the bulk half was not.
    /// Which meant the one failure this tooling exists to explain, a wide pull or
    /// push eating the whole request budget, showed up as a build that had simply
    /// gone quiet.
    #[tokio::test]
    async fn a_saturated_transfer_budget_is_reported_to_the_watchdog() {
        let cache = ConfiguredCache::new(
            def("probe", "memory:///probe", true, true),
            Arc::new(FailBackend) as Arc<dyn RemoteCacheBackend>,
        );
        let bulk = cache.shared_slots.available_permits();
        assert!(bulk > 0, "the cache must start with a bulk budget");

        let d = crate::engine::diag::global();
        // Not saturated while slots are free — an observation must clear, too.
        observe_transfer_slots(&cache);
        assert!(
            !d.saturated(d.now_ms())
                .iter()
                .any(|(name, _)| *name == "remote-cache-transfer"),
            "an available budget must not read as saturated",
        );

        let _held = cache
            .shared_slots
            .acquire_many(u32::try_from(bulk).expect("budget fits"))
            .await
            .expect("take the whole bulk budget");
        observe_transfer_slots(&cache);
        assert!(
            d.saturated(d.now_ms() + 1_000)
                .iter()
                .any(|(name, _)| *name == "remote-cache-transfer"),
            "an exhausted bulk budget must be visible to the watchdog",
        );

        // Leave the global as we found it: it is process-wide.
        drop(_held);
        observe_transfer_slots(&cache);
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

    /// Endpoint and region decide which host a cache actually talks to, so a
    /// change to either has to invalidate the persisted latency order the same
    /// way a uri change does — otherwise a cache re-pointed at a different
    /// service keeps the read position it earned against the old one.
    #[test]
    fn config_hash_covers_endpoint_and_region() {
        let base = vec![def("x", "s3://bucket/p", true, true)];

        let mut with_endpoint = base.clone();
        with_endpoint[0].endpoint = Some("https://accountid.r2.cloudflarestorage.com".to_string());
        assert_ne!(
            config_hash(&base),
            config_hash(&with_endpoint),
            "endpoint change must matter"
        );

        let mut other_endpoint = with_endpoint.clone();
        other_endpoint[0].endpoint = Some("http://localhost:9000".to_string());
        assert_ne!(
            config_hash(&with_endpoint),
            config_hash(&other_endpoint),
            "a different endpoint must matter"
        );

        let mut with_region = base.clone();
        with_region[0].region = Some("auto".to_string());
        assert_ne!(
            config_hash(&base),
            config_hash(&with_region),
            "region change must matter"
        );
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

    /// Backend whose listings fail, counting how many it was asked for.
    #[derive(Default)]
    struct CountingFailBackend {
        lists: AtomicUsize,
    }

    #[async_trait]
    impl RemoteCacheBackend for CountingFailBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            anyhow::bail!("unreachable")
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("unreachable")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            anyhow::bail!("unreachable")
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("unreachable")
        }
    }

    /// Scratch reads share the cache's breaker in both directions: their
    /// failures count towards pausing it, and a paused cache is not handed to
    /// the scratch lineage at all — not even by a source taken out before the
    /// breaker opened.
    ///
    /// Without both halves a dead cache costs every scratch prepare its full
    /// timeout, under the slot lock, after the artifact path has already given
    /// up on it.
    #[tokio::test]
    async fn scratch_reads_share_the_breaker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(CountingFailBackend::default());
        let set = RemoteCacheSet::with_backend(backend.clone(), dir.path().to_path_buf());

        let early = set.scratch_sources().await;
        assert_eq!(early.len(), 1, "a healthy cache is a scratch source");
        for _ in 0..FAILURE_THRESHOLD {
            early[0]
                .list_names("scratch/v1/")
                .await
                .expect_err("the backend fails every listing");
        }

        assert!(
            set.scratch_sources().await.is_empty(),
            "scratch failures must pause the cache for scratch reads",
        );
        let late = early[0]
            .list_names("scratch/v1/")
            .await
            .expect_err("a paused cache must refuse without asking the backend");
        assert!(
            format!("{late:#}").contains("paused"),
            "the refusal names the pause: {late:#}",
        );
        assert_eq!(
            backend.lists.load(Ordering::SeqCst),
            FAILURE_THRESHOLD,
            "no request reaches a paused cache",
        );
    }

    /// Backend serving one healthy object for every key.
    struct OneBodyBackend(Vec<u8>);

    #[async_trait]
    impl RemoteCacheBackend for OneBodyBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            Ok(Some(Box::pin(std::io::Cursor::new(self.0.clone()))))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only")
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// A local sink that accepts `room` bytes, then fails every write.
    struct FullDisk {
        room: usize,
    }

    impl AsyncWrite for FullDisk {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.room == 0 {
                return std::task::Poll::Ready(Err(std::io::Error::other("no space left")));
            }
            let n = buf.len().min(self.room);
            self.room -= n;
            std::task::Poll::Ready(Ok(n))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A snapshot pull that fails on the local side — the disk filled up part
    /// way — names the local write, and is not held against the cache: the
    /// remote served every byte asked of it, so the breaker must not count it.
    ///
    /// Counted, `FAILURE_THRESHOLD` full disks in a row would pause a healthy
    /// cache for every artifact and scratch read in the run.
    #[tokio::test]
    async fn a_local_write_failure_on_a_snapshot_pull_spares_the_breaker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = vec![7u8; 3 * SNAPSHOT_STREAM_BUF];
        let set =
            RemoteCacheSet::with_backend(Arc::new(OneBodyBackend(body)), dir.path().to_path_buf());
        let sources = set.scratch_sources().await;
        assert_eq!(sources.len(), 1);

        for _ in 0..FAILURE_THRESHOLD {
            let mut dest = FullDisk {
                room: SNAPSHOT_STREAM_BUF + 17,
            };
            let err = sources[0]
                .read_snapshot_into("k.tar.gz", &mut dest)
                .await
                .expect_err("the local sink fills up part way");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("write remote scratch snapshot k.tar.gz locally")
                    && msg.contains("no space left"),
                "the error names the local write and its cause: {msg}",
            );
        }

        let cache = set.caches.first().expect("one cache");
        assert_eq!(
            cache.health.consecutive_failures.load(Ordering::Relaxed),
            0,
            "a local write failure is not a remote failure",
        );
        assert!(!cache.broken(), "the breaker stays closed");
        assert_eq!(
            set.scratch_sources().await.len(),
            1,
            "the cache is still handed to the scratch lineage",
        );
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
            let res = set.locate(&never(), &addr, "h", Need::Manifest).await;
            assert!(res.is_none(), "a failing remote read is a miss");
        }
        assert!(
            set.caches.first().expect("cache").broken(),
            "cache must be circuit-broken after {FAILURE_THRESHOLD} consecutive failures"
        );
    }

    /// A bulk open that fails is routed by its caller: a blob fetch notes it on
    /// the cache and degrades to "cannot serve" (`Ok(None)`), while a scratch
    /// snapshot read returns the error.
    #[tokio::test]
    async fn a_failed_bulk_open_is_a_blob_miss_and_a_snapshot_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Not an auth failure, which would disable the cache at once.
        let set = RemoteCacheSet::with_backend(
            Arc::new(CountingFailBackend::default()),
            dir.path().to_path_buf(),
        );
        let addr = addr();
        let rev = RemoteRevision::new(
            0,
            RemoteManifest {
                version: REMOTE_MANIFEST_VERSION.to_string(),
                target: addr.format(),
                hashin: "h".to_string(),
                artifacts: vec![probe_artifact(0)],
            },
        );

        let sources = set.scratch_sources().await;
        let err = sources[0]
            .read_snapshot_into("scratch/v1/snap", &mut tokio::io::sink())
            .await
            .expect_err("a failed snapshot open is an error");
        assert!(format!("{err:#}").contains("unreachable"), "{err:#}");

        for _ in 1..FAILURE_THRESHOLD {
            assert!(!set.caches[0].broken(), "broken before the threshold");
            let temp = set
                .fetch_blob(&never(), &rev, &addr, "h", "blob-0", dir.path())
                .await
                .expect("a failed blob open is not a hard error");
            assert!(temp.is_none(), "a failed blob open cannot serve the blob");
        }
        assert!(
            set.caches[0].broken(),
            "both reads count towards the cache's breaker"
        );
    }

    /// A rejected credential cannot heal within a run, so the first one
    /// disables the cache for good: no threshold, no cooldown to wait out, and
    /// a straggling in-flight success does not re-arm it.
    #[tokio::test(start_paused = true)]
    async fn auth_failure_disables_the_cache_for_the_run() {
        let cache =
            ConfiguredCache::new(def("x", "memory:///x", true, true), Arc::new(FailBackend));
        let e = anyhow::Error::new(AuthError("invalid_grant".into())).context("manifest read");
        assert!(is_auth_failure(&e));
        cache.note_err("op", &e);
        assert!(cache.broken(), "one auth failure must disable the cache");
        tokio::time::advance(BREAKER_COOLDOWN_MAX * 2).await;
        assert!(cache.broken(), "an auth failure has no cooldown");
        cache.note_ok();
        assert!(cache.broken(), "a later success must not re-arm the cache");
    }

    /// The server's reason, read from each response shape the stores refuse
    /// with, and the inputs that must fall back to the cause chain instead.
    #[test]
    fn refusal_reason_reads_each_stores_response() {
        let reason = |innermost: &str| {
            refusal_reason(&anyhow::anyhow!(innermost.to_owned()).context("open remote object k"))
        };
        let ok = |innermost: &str, want: &str| {
            assert_eq!(reason(innermost).as_deref(), Some(want), "{innermost:?}");
        };
        // OAuth 2.0 token endpoints: Google, Entra ID. Pretty-printed bodies
        // collapse to one line.
        ok(
            "400 Bad Request: {\n  \"error\": \"invalid_grant\",\n  \
             \"error_description\": \"reauth related error (invalid_rapt)\"\n}",
            "invalid_grant: reauth related error (invalid_rapt)",
        );
        ok("401: {\"error\": \"invalid_client\"}", "invalid_client");
        ok(
            "401: {\"error\": \"invalid_client\", \"error_description\": \"  \"}",
            "invalid_client",
        );
        ok(
            "400: {\"error\":\"invalid_client\",\"error_description\":\"AADSTS7000215: \
             Invalid client secret.\\r\\nTrace ID: 1\"}",
            "invalid_client: AADSTS7000215: Invalid client secret. Trace ID: 1",
        );
        // A Google API (GCS's own 401/403).
        ok(
            "403 Forbidden: {\"error\": {\"code\": 403, \"message\": \"a@b.c does not have \
             storage.objects.get access\", \"errors\": []}}",
            "a@b.c does not have storage.objects.get access",
        );
        // S3, Azure Blob.
        ok(
            "400: <?xml version=\"1.0\"?><Error><Code>ExpiredToken</Code>\
             <Message>The provided token has expired.</Message></Error>",
            "ExpiredToken: The provided token has expired.",
        );
        ok(
            "403: <Error><Code>AccessDenied</Code></Error>",
            "AccessDenied",
        );
        ok(
            "403: <Error><Code>AuthenticationFailed</Code><Message>Server failed\n\
             to authenticate the request.\nRequestId:é1</Message></Error>",
            "AuthenticationFailed: Server failed to authenticate the request. RequestId:é1",
        );

        // Not a response the stores send: the caller writes the chain instead.
        for innermost in [
            "",
            "401 Unauthorized",
            "{}",
            "{\"error\": 3}",
            "{\"message\": \"no error field\"}",
            "{\"error\": \"x\"} trailing",
            "{not json",
            "<Code></Code>",
            "<Code>unterminated",
            "<Message>only</Message>",
        ] {
            assert_eq!(reason(innermost), None, "{innermost:?}");
        }
    }

    /// The warning without a reason in the body: the cause chain, once, and the
    /// backend's remedy when it has one.
    #[test]
    fn auth_warning_falls_back_to_the_chain() {
        let e = anyhow::Error::new(AuthError("token file unreadable".into()))
            .context("open remote object k");
        assert_eq!(
            auth_warning("main", &e, None),
            "remote cache \"main\": authentication failed (open remote object k: \
             authentication failed: token file unreadable); builds continue without it \
             for the rest of this run.",
        );
        assert_eq!(
            auth_warning("main", &e, Some("az login")),
            "remote cache \"main\": authentication failed (open remote object k: \
             authentication failed: token file unreadable); builds continue without it \
             for the rest of this run. To sign in again, run `az login`",
        );

        // A body nothing recognizes (a proxy's HTML page) is one line, capped,
        // and says where the rest is.
        let page = format!("<html>\n<body>{}</body>\n</html>", "denied ".repeat(100));
        let e = anyhow::Error::new(AuthError(page.into())).context("open remote object k");
        let msg = auth_warning("main", &e, None);
        assert!(!msg.contains('\n'), "{msg}");
        assert!(msg.contains("…); builds continue"), "{msg}");
        assert!(
            msg.contains("(HEPH_LOG=debug shows the full error)"),
            "{msg}"
        );
    }

    #[test]
    fn one_line_collapses_and_caps_on_a_char_boundary() {
        assert_eq!(one_line("a\n  b\tc "), ("a b c".to_owned(), false));
        assert_eq!(one_line(""), (String::new(), false));
        let at_cap = "é".repeat(WARNING_DETAIL_MAX_CHARS);
        assert_eq!(one_line(&at_cap), (at_cap.clone(), false));
        let (cut, was_cut) = one_line(&format!("{at_cap}é"));
        assert!(was_cut);
        assert_eq!(cut, format!("{at_cap}…"));
        // The cap reached exactly at a word's end still cuts what follows.
        assert_eq!(
            one_line(&format!("{at_cap} x")),
            (format!("{at_cap}…"), true)
        );
    }

    /// Every shape an auth failure reaches the breaker in, and the near
    /// misses that must still go through the ordinary backoff.
    #[test]
    fn auth_failure_is_recognized_through_wrappers() {
        let unauth = || object_store::Error::Unauthenticated {
            path: "p".into(),
            source: "401".into(),
        };
        let denied = object_store::Error::PermissionDenied {
            path: "p".into(),
            source: "403".into(),
        };
        // A store's own 401/403, under context.
        assert!(is_auth_failure(
            &anyhow::Error::new(unauth()).context("open remote object")
        ));
        assert!(is_auth_failure(&anyhow::Error::new(denied)));
        // Behind an io::Error, as `BufWriter` reports a failed upload.
        let io = std::io::Error::other(unauth());
        assert!(is_auth_failure(
            &anyhow::Error::new(io).context("upload blob")
        ));
        // Behind a store error, as `AuthClassifying` reports a credential.
        let tagged = object_store::Error::Generic {
            store: "GCS",
            source: Box::new(AuthError("invalid_rapt".into())),
        };
        assert!(is_auth_failure(&anyhow::Error::new(tagged)));

        // Not auth: a generic store error, a missing object, a bare io error
        // whose kind merely reads like one.
        let generic = object_store::Error::Generic {
            store: "GCS",
            source: "503 Service Unavailable".into(),
        };
        assert!(!is_auth_failure(&anyhow::Error::new(generic)));
        let missing = object_store::Error::NotFound {
            path: "p".into(),
            source: "404".into(),
        };
        assert!(!is_auth_failure(&anyhow::Error::new(missing)));
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_auth_failure(&anyhow::Error::new(io)));
        assert!(!is_auth_failure(&anyhow::anyhow!("auth failed")));
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
        let ctoken = never();
        let cache = &set.caches[0];

        assert!(
            set.blobs_present(&ctoken, cache, &addr, "h", names.iter().map(String::as_str))
                .await,
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
        assert!(
            !set.blobs_present(
                &ctoken,
                cache,
                &addr,
                "h",
                missing.iter().map(String::as_str)
            )
            .await
        );
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
            if key.ends_with(REMOTE_MANIFEST) {
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
        let rev = Arc::new(RemoteRevision::new(0, manifest));
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
            .locate(&never(), &addr, "h", Need::Manifest)
            .await
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
                    drop(
                        set.locate(&never(), &addr, &format!("h{i}"), Need::Manifest)
                            .await,
                    );
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
            RemoteCacheSet::key(&addr, "h", REMOTE_MANIFEST),
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
            .locate(&never(), &addr, "h", Need::Manifest)
            .await
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

    /// Fails the first blob's write at once and holds every other blob's open
    /// forever, recording each key it is asked to open.
    #[derive(Default)]
    struct FirstBlobFailsBackend {
        opened: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl RemoteCacheBackend for FirstBlobFailsBackend {
        async fn open_read(
            &self,
            _key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            Ok(None)
        }
        async fn open_write(&self, key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            self.opened.lock().expect("opened").push(key.to_string());
            if key.ends_with("/blob-0") {
                anyhow::bail!("injected blob write failure");
            }
            std::future::pending().await
        }
        async fn exists(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// A failed blob ends the cache's push: the puts still in flight are
    /// dropped, no blob past the fan-out bound is started, and the manifest is
    /// never written. Driving every put to completion first would hang here,
    /// since the other blobs never finish.
    #[tokio::test]
    async fn a_failed_blob_stops_the_revision_upload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let blob_src = dir.path().join("src.blob");
        std::fs::write(&blob_src, b"data").expect("write blob");
        let blobs: Vec<(String, PathBuf)> = (0..FANOUT_ARTIFACTS)
            .map(|i| (format!("blob-{i}"), blob_src.clone()))
            .collect();
        let backend = Arc::new(FirstBlobFailsBackend::default());
        let set = set_with(backend.clone(), dir.path().to_path_buf());

        tokio::time::timeout(
            Duration::from_secs(30),
            set.put_revision(&addr, "h", b"manifest", &blobs),
        )
        .await
        .expect("a failed blob must end the upload, not wait for the others");

        let opened = backend.opened.lock().expect("opened").clone();
        assert!(
            opened.len() <= REVISION_BLOB_CONCURRENCY,
            "no blob past the fan-out bound may start after a failure: {} opened",
            opened.len()
        );
        assert!(
            !opened.iter().any(|k| k.ends_with(REMOTE_MANIFEST)),
            "the manifest must not be written after a failed blob"
        );
    }

    /// Serves a manifest and its blobs, but holds the first blob's open until
    /// every other blob has been opened.
    struct SlowHeadBackend {
        objects: std::collections::HashMap<String, Vec<u8>>,
        head: String,
        others_opened: tokio::sync::watch::Sender<usize>,
        others: usize,
    }

    #[async_trait]
    impl RemoteCacheBackend for SlowHeadBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            if key == self.head {
                let mut opened = self.others_opened.subscribe();
                opened
                    .wait_for(|n| *n >= self.others)
                    .await
                    .context("others_opened closed")?;
            } else if !key.ends_with(REMOTE_MANIFEST) {
                self.others_opened.send_modify(|n| *n += 1);
            }
            Ok(self.objects.get(key).map(|b| {
                Box::pin(std::io::Cursor::new(b.clone())) as Pin<Box<dyn AsyncRead + Send>>
            }))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only backend")
        }
        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.objects.contains_key(key))
        }
        async fn list_names(&self, _prefix: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// An engine over `dir` whose remote set is `remote`.
    fn engine_with_set(dir: &Path, remote: Arc<RemoteCacheSet>) -> Arc<crate::engine::Engine> {
        let mut engine = Arc::new(
            crate::engine::Engine::new(crate::engine::Config::for_tests(dir)).expect("engine"),
        );
        Arc::get_mut(&mut engine)
            .expect("engine must not be shared yet")
            .remote_caches = remote;
        engine
    }

    /// A pull of more blobs than the fan-out bound completes while its first
    /// blob is slow: the blobs behind it take the slots the others free. The
    /// first blob is released only once every other one has been opened, so
    /// an ordered fan-out, which starts nothing past the bound until the first
    /// blob is done, never gets there.
    #[tokio::test]
    async fn a_slow_first_blob_does_not_hold_the_pull_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let count = REVISION_BLOB_CONCURRENCY + 1;
        let artifacts: Vec<RemoteManifestArtifact> = (0..count).map(probe_artifact).collect();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: artifacts.clone(),
        };
        let mut objects = std::collections::HashMap::new();
        objects.insert(
            RemoteCacheSet::key(&addr, "h", REMOTE_MANIFEST),
            borsh::to_vec(&manifest).expect("serialize manifest"),
        );
        for a in &artifacts {
            objects.insert(RemoteCacheSet::key(&addr, "h", &a.name), b"data".to_vec());
        }
        let backend = Arc::new(SlowHeadBackend {
            objects,
            head: RemoteCacheSet::key(&addr, "h", "blob-0"),
            others_opened: tokio::sync::watch::Sender::new(0),
            others: count - 1,
        });
        let engine = engine_with_set(dir.path(), set_with(backend, dir.path().to_path_buf()));

        let rev = engine
            .remote_caches
            .locate(&never(), &addr, "h", Need::Manifest)
            .await
            .expect("hit");
        let names: Vec<String> = artifacts.iter().map(|a| a.name.clone()).collect();
        let served = tokio::time::timeout(
            Duration::from_secs(30),
            engine.pull_remote_blobs(&never(), &addr, "h", &rev, &names),
        )
        .await
        .expect("the pull must not wait on its first blob to start the rest")
        .expect("pull");
        assert!(served, "every blob must be served");
    }

    /// The remote manifest an upload writes lists the local manifest's
    /// artifacts in the local order, each with its local encoding — whatever
    /// order the blob copies finish in.
    #[tokio::test]
    async fn the_uploaded_manifest_mirrors_the_local_one() {
        use crate::engine::driver::outputartifact;

        let dir = tempfile::tempdir().expect("tempdir");
        let remote = RemoteCacheSet::new(
            &[def("m", "memory:///m", true, true)],
            dir.path().to_path_buf(),
        )
        .expect("remote set");
        let engine = engine_with_set(dir.path(), remote);
        let addr = addr();
        // Sizes far apart, so the copies need not finish in manifest order and
        // the large one is stored compressed while the small ones are not.
        let artifacts: Vec<outputartifact::OutputArtifact> = [
            ("big", vec![b'x'; 512 * 1024]),
            ("tiny", b"t".to_vec()),
            ("mid", vec![b'm'; 4096]),
        ]
        .into_iter()
        .map(|(name, data)| outputartifact::OutputArtifact {
            group: format!("g-{name}"),
            name: name.to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                data,
                path: format!("{name}.txt"),
                x: false,
            }),
            hashout: format!("hashout-{name}"),
        })
        .collect();
        engine
            .cache_locally(&never(), &addr, "h", artifacts, false)
            .await
            .expect("cache_locally");
        engine.upload_to_remote(&addr, "h").await;

        let local = engine
            .read_manifest_blocking(&never(), &addr, "h")
            .await
            .expect("read local manifest")
            .expect("local manifest");
        let rev = engine
            .remote_caches
            .locate(&never(), &addr, "h", Need::Manifest)
            .await
            .expect("uploaded");
        let want = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: local.target.clone(),
            hashin: "h".to_string(),
            artifacts: local
                .artifacts
                .iter()
                .map(|a| RemoteManifestArtifact {
                    hashout: a.hashout.clone(),
                    group: a.group.clone(),
                    name: remote_entry_name(&a.name).to_string(),
                    size: a.size,
                    r#type: a.r#type.clone(),
                    content_type: a.content_type.clone(),
                    encoding: a.encoding.clone(),
                })
                .collect(),
        };
        assert_eq!(local.artifacts.len(), 3);
        assert_eq!(
            borsh::to_vec(&rev.manifest).expect("borsh"),
            borsh::to_vec(&want).expect("borsh"),
            "remote manifest {:?} != local {:?}",
            rev.manifest,
            local
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
                    manifest_key: RemoteCacheSet::key(addr, "h", REMOTE_MANIFEST),
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
            .locate(&ctoken, &addr, "h", Need::Manifest)
            .await
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
            .locate(&ctoken, &addr, "h", Need::Manifest)
            .await
            .expect("hit");
        // `Box::pin`, not `tokio::pin!`: the latter shadows the future with a
        // `Pin<&mut _>`, so `drop`ping that name is a no-op and the future — and
        // its guard — would outlive the assertion below.
        let mut fetch = Box::pin(set.fetch_blob(&ctoken, &rev, &addr, "h", "blob-0", dir.path()));

        // Poll it far enough to have created the temp and parked on the copy.
        // The budget is generous rather than tight: this is waiting on a
        // `tokio::fs::File::create` to land, and on a loaded 2-core runner a
        // one-second deadline is a coin flip, not a signal.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while temp_blobs(dir.path()).is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fetch never created its temp file",
            );
            tokio::select! {
                _ = &mut fetch => panic!("a stalled backend must never complete the copy"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        assert_eq!(
            temp_blobs(dir.path()).len(),
            1,
            "the fetch should have exactly one temp file open"
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
    /// copy finishes writing a *complete* temp file after its future is gone.
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
        // Well past any transfer's own bound, but still inside the sweep window —
        // it could belong to a suspended peer, so the margin says keep it.
        let borderline = plant("borderline.blob", Some(TEMP_SWEEP_AGE / 2));
        let fresh = plant("fresh.blob", None);
        let foreign = plant("notours.txt", Some(TEMP_SWEEP_AGE * 10));
        // Clock skew the other way. `duration_since` fails rather than going
        // negative, and that has to read as "keep" — a refactor to
        // `elapsed().unwrap_or(MAX)` would silently invert it into "delete
        // everything".
        let future = {
            let path = dir.path().join("future.blob");
            let f = std::fs::File::create(&path).expect("create planted file");
            let when = std::time::SystemTime::now() + Duration::from_secs(60 * 60);
            f.set_times(std::fs::FileTimes::new().set_modified(when))
                .expect("post-date planted file");
            path
        };

        sweep_abandoned_temps(dir.path());

        assert!(!stale.exists(), "an abandoned temp must be reclaimed");
        assert!(
            future.exists(),
            "an mtime in the future is not evidence of abandonment"
        );
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
            set.locate(&never(), &addr(), "h", Need::Manifest)
                .await
                .is_none()
        );
    }

    fn remote_artifact(
        name: &str,
        size: u64,
        encoding: ManifestArtifactEncoding,
    ) -> RemoteManifestArtifact {
        RemoteManifestArtifact {
            hashout: format!("ho-{name}"),
            group: "out".to_string(),
            name: name.to_string(),
            size,
            r#type: ManifestArtifactType::Output,
            content_type: ManifestArtifactContentType::Tar,
            encoding,
        }
    }

    /// Round-trip a revision through two memory caches: the manifest is found
    /// under `manifest-v2.borsh` with each artifact's encoding intact, and every
    /// blob streams back byte-identical — the remote neither encodes nor decodes.
    #[tokio::test]
    async fn put_then_fetch_revision_roundtrips() {
        let defs = vec![
            def("a", "memory:///a", true, true),
            def("b", "memory:///b", true, true),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = addr();

        // One zstd blob as the local cache stores it, one stored verbatim.
        let zst = zstd::encode_all(&vec![b'x'; 5000][..], 1).expect("zstd");
        let zst_tmp = dir.path().join("zst.blob");
        std::fs::write(&zst_tmp, &zst).expect("write");
        let plain = b"tiny".to_vec();
        let plain_tmp = dir.path().join("plain.blob");
        std::fs::write(&plain_tmp, &plain).expect("write");

        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h1".to_string(),
            artifacts: vec![
                remote_artifact("out_z.tar.zst", 5000, ManifestArtifactEncoding::Zstd),
                remote_artifact("out_p.tar", 4, ManifestArtifactEncoding::None),
            ],
        };
        let manifest_bytes = borsh::to_vec(&manifest).expect("borsh");
        set.put_revision(
            &addr,
            "h1",
            &manifest_bytes,
            &[
                ("out_z.tar.zst".to_string(), zst_tmp),
                ("out_p.tar".to_string(), plain_tmp),
            ],
        )
        .await;

        let fetch_dir = dir.path().join("fetched");
        std::fs::create_dir_all(&fetch_dir).expect("mkdir");
        let rev = set
            .locate(&never(), &addr, "h1", Need::Manifest)
            .await
            .expect("present");
        let encodings: Vec<_> = rev
            .manifest
            .artifacts
            .iter()
            .map(|a| a.encoding.clone())
            .collect();
        assert_eq!(
            encodings,
            [
                ManifestArtifactEncoding::Zstd,
                ManifestArtifactEncoding::None
            ]
        );

        for (name, want) in [("out_z.tar.zst", &zst), ("out_p.tar", &plain)] {
            let temp = set
                .fetch_blob(&never(), &rev, &addr, "h1", name, &fetch_dir)
                .await
                .expect("fetch blob")
                .expect("blob present");
            assert_eq!(&std::fs::read(temp.path()).expect("read"), want, "{name}");
        }
    }

    /// A small blob gets a buffer its own size, not a whole MiB; a tiny one
    /// gets the floor, and a large one the cap.
    #[test]
    fn a_blob_temp_buffer_is_sized_to_the_blob_within_bounds() {
        assert_eq!(blob_temp_write_buffer(0), BLOB_TEMP_WRITE_BUFFER_MIN);
        assert_eq!(blob_temp_write_buffer(100), BLOB_TEMP_WRITE_BUFFER_MIN);
        assert_eq!(blob_temp_write_buffer(100_000), 100_000);
        assert_eq!(
            blob_temp_write_buffer(BLOB_TEMP_WRITE_BUFFER as u64),
            BLOB_TEMP_WRITE_BUFFER
        );
        assert_eq!(blob_temp_write_buffer(u64::MAX), BLOB_TEMP_WRITE_BUFFER);
    }

    /// A blob larger than the temp file's write buffer, whose size is not a
    /// multiple of it, lands whole: the buffered tail is flushed before the
    /// temp is handed back.
    #[tokio::test]
    async fn a_blob_larger_than_the_write_buffer_lands_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let len = 3 * BLOB_TEMP_WRITE_BUFFER + 12_345;
        let blob: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: vec![remote_artifact(
                "big.tar",
                len as u64,
                ManifestArtifactEncoding::None,
            )],
        };
        let mut objects = std::collections::HashMap::new();
        objects.insert(
            RemoteCacheSet::key(&addr, "h", REMOTE_MANIFEST),
            borsh::to_vec(&manifest).expect("serialize manifest"),
        );
        objects.insert(RemoteCacheSet::key(&addr, "h", "big.tar"), blob.clone());
        let backend = Arc::new(ReadProbeBackend {
            objects,
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let set = set_with(backend, dir.path().to_path_buf());

        let rev = set
            .locate(&never(), &addr, "h", Need::Manifest)
            .await
            .expect("hit");
        let temp = set
            .fetch_blob(&never(), &rev, &addr, "h", "big.tar", dir.path())
            .await
            .expect("fetch blob")
            .expect("blob present");
        let got = std::fs::read(temp.path()).expect("read temp");
        assert_eq!(got.len(), len, "every byte, the buffered tail included");
        assert!(got == blob, "blob bytes must round-trip unchanged");
    }

    /// A v2 manifest naming a gzip blob is a miss: downloads store bytes
    /// verbatim, and the local cache cannot read gzip.
    #[tokio::test]
    async fn a_gzip_blob_in_a_v2_manifest_is_a_miss() {
        let defs = vec![def("a", "memory:///a", true, true)];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = addr();
        let blob = dir.path().join("b");
        std::fs::write(&blob, b"x").expect("write");
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h1".to_string(),
            artifacts: vec![remote_artifact(
                "out.tar",
                1,
                ManifestArtifactEncoding::Gzip,
            )],
        };
        set.put_revision(
            &addr,
            "h1",
            &borsh::to_vec(&manifest).expect("borsh"),
            &[("out.tar".to_string(), blob)],
        )
        .await;
        assert!(
            set.locate(&never(), &addr, "h1", Need::Manifest)
                .await
                .is_none()
        );
    }

    /// An entry an older heph wrote (`manifest-v1.borsh`, gzip blobs) is not
    /// read: that heph never reads v2 either, so the two never exchange bytes
    /// the other cannot decode.
    #[tokio::test]
    async fn a_v1_remote_entry_is_not_read() {
        let defs = vec![def("a", "memory:///a", true, true)];
        let dir = tempfile::tempdir().expect("tempdir");
        let set = RemoteCacheSet::new(&defs, dir.path().to_path_buf()).expect("set");
        let addr = addr();
        let manifest = RemoteManifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            hashin: "h1".to_string(),
            artifacts: vec![],
        };
        let cache = &set.caches[0];
        write_bytes_to_backend(
            cache.backend.as_ref(),
            &RemoteCacheSet::key(&addr, "h1", "manifest-v1.borsh"),
            &borsh::to_vec(&manifest).expect("borsh"),
        )
        .await
        .expect("write v1 manifest");
        assert!(
            set.locate(&never(), &addr, "h1", Need::Manifest)
                .await
                .is_none()
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
            if !key.ends_with(REMOTE_MANIFEST) {
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
                std::fs::write(&path, b"x").expect("write");
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
        std::fs::write(&blob_tmp, b"x").expect("write");
        set.put_revision(&addr, "h1", b"m", &[("o.tar".to_string(), blob_tmp)])
            .await;
        assert!(
            set.locate(&never(), &addr, "h1", Need::Manifest)
                .await
                .is_none()
        );
    }

    /// One memory cache named `name`, written to by an engine of its own that
    /// caches `data` as blob `a` with `hashout` and pushes it: the revision a
    /// build of `(addr(), hashin)` left on that cache. Returns the cache's
    /// backend, to be shared with a reader's set, and the writer's engine.
    async fn cache_built_by(
        dir: &Path,
        name: &str,
        hashin: &str,
        data: &[u8],
        hashout: &str,
    ) -> (Arc<dyn RemoteCacheBackend>, Arc<crate::engine::Engine>) {
        cache_built_at(dir, name, &addr(), hashin, data, hashout).await
    }

    /// [`cache_built_by`] for the target `target`.
    async fn cache_built_at(
        dir: &Path,
        name: &str,
        target: &Addr,
        hashin: &str,
        data: &[u8],
        hashout: &str,
    ) -> (Arc<dyn RemoteCacheBackend>, Arc<crate::engine::Engine>) {
        use crate::engine::driver::outputartifact;

        let set = RemoteCacheSet::new(
            &[def(name, &format!("memory:///{name}"), true, true)],
            dir.to_path_buf(),
        )
        .expect("remote set");
        let (_, backend) = set
            .writable_backends()
            .into_iter()
            .next()
            .expect("one writable cache");
        let engine = engine_with_set(dir, set);
        let artifact = outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: "a".to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                data: data.to_vec(),
                path: "a.txt".to_string(),
                x: false,
            }),
            hashout: hashout.to_string(),
        };
        engine
            .cache_locally(&never(), target, hashin, vec![artifact], false)
            .await
            .expect("cache_locally");
        engine.upload_to_remote(target, hashin).await;
        (backend, engine)
    }

    /// The bytes of blob `name` in `engine`'s local cache.
    fn local_blob(engine: &crate::engine::Engine, hashin: &str, name: &str) -> Vec<u8> {
        use std::io::Read as _;
        let mut r = engine
            .local_cache
            .reader(&addr(), hashin, name)
            .expect("local blob");
        let mut bytes = Vec::new();
        r.reader.read_to_end(&mut bytes).expect("read local blob");
        bytes
    }

    /// A target that is not reproducible, built twice for one `(addr, hashin)`,
    /// leaves two revisions naming the same blob with different hashouts. A
    /// reader whose local manifest records one hashout must never be handed
    /// the other build's bytes: the first cache's entry does not match
    /// (`entry_for` is `None`), the pull falls through to the cache that holds
    /// the matching build, and with no such cache it is a miss — nothing is
    /// written locally and the caller rebuilds.
    #[tokio::test]
    async fn a_pulled_blob_with_another_hashout_is_rejected() {
        let orig_dir = tempfile::tempdir().expect("tempdir");
        let rebuilt_dir = tempfile::tempdir().expect("tempdir");
        let (orig, orig_engine) =
            cache_built_by(orig_dir.path(), "orig", "h", b"first build", "ho-first").await;
        let (rebuilt, _rebuilt_engine) = cache_built_by(
            rebuilt_dir.path(),
            "rebuilt",
            "h",
            b"second build, other bytes",
            "ho-second",
        )
        .await;

        // The reader's local manifest is the first build's.
        let local = orig_engine
            .read_manifest_blocking(&never(), &addr(), "h")
            .await
            .expect("read manifest")
            .expect("manifest");
        let want = local.artifacts.first().expect("one artifact").clone();
        let want_bytes = local_blob(&orig_engine, "h", &want.name);

        // Read first: the cache that holds the other build.
        let reader_dir = tempfile::tempdir().expect("tempdir");
        let reader = engine_with_set(
            reader_dir.path(),
            RemoteCacheSet::with_backends(
                vec![("rebuilt", rebuilt.clone()), ("orig", orig)],
                reader_dir.path().to_path_buf(),
            ),
        );
        let first = reader
            .remote_caches
            .locate(&never(), &addr(), "h", Need::Manifest)
            .await
            .expect("a revision");
        assert_eq!(reader.remote_caches.cache_name(&first), "rebuilt");
        let other = first
            .manifest
            .artifacts
            .iter()
            .find(|a| a.name == remote_entry_name(&want.name))
            .expect("the other build names the same blob");
        assert_eq!(other.hashout, "ho-second");
        assert!(
            first.entry_for(&want).is_none(),
            "a blob with another hashout must not match the local manifest's"
        );

        assert!(
            reader
                .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want)
                .await
                .expect("pull"),
            "the cache holding the matching build must serve the blob"
        );
        assert_eq!(
            local_blob(&reader, "h", &want.name),
            want_bytes,
            "the pulled blob must be the matching build's, byte for byte"
        );

        // Only the other build on offer: a miss, and nothing lands locally.
        let lone_dir = tempfile::tempdir().expect("tempdir");
        let lone = engine_with_set(
            lone_dir.path(),
            RemoteCacheSet::with_backends(
                vec![("rebuilt", rebuilt)],
                lone_dir.path().to_path_buf(),
            ),
        );
        let first = lone
            .remote_caches
            .locate(&never(), &addr(), "h", Need::Manifest)
            .await
            .expect("a revision");
        assert!(
            !lone
                .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want)
                .await
                .expect("pull"),
            "no cache holds the matching build: the caller must rebuild"
        );
        assert!(
            !lone
                .local_cache
                .exists(&addr(), "h", &want.name)
                .expect("exists"),
            "the other build's bytes must not reach the local cache"
        );
    }

    /// A cache that answers a listing without a blob the hit needs is passed
    /// over for the next, and its breaker stays closed: the listing succeeded,
    /// so the cache is healthy and merely lacks the entry. Run past the
    /// failure threshold, so a miss counted as a failure would have tripped it.
    #[tokio::test]
    async fn a_presence_miss_leaves_the_breaker_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = addr();
        let manifest = RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr.format(),
            hashin: "h".to_string(),
            artifacts: vec![remote_artifact(
                "out_a.tar",
                4,
                ManifestArtifactEncoding::None,
            )],
        };
        let manifest_key = RemoteCacheSet::key(&addr, "h", REMOTE_MANIFEST);
        let blob_key = RemoteCacheSet::key(&addr, "h", "out_a.tar");
        let manifest_bytes = borsh::to_vec(&manifest).expect("borsh");
        let backend = |objects: Vec<(&String, &[u8])>| -> Arc<dyn RemoteCacheBackend> {
            Arc::new(ReadProbeBackend {
                objects: objects
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.to_vec()))
                    .collect(),
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            })
        };
        let lacking = backend(vec![(&manifest_key, &manifest_bytes)]);
        let whole = backend(vec![(&manifest_key, &manifest_bytes), (&blob_key, b"blob")]);
        let set = RemoteCacheSet::with_backends(
            vec![("lacking", lacking), ("whole", whole)],
            dir.path().to_path_buf(),
        );
        let groups = ["out".to_string()];

        for _ in 0..=FAILURE_THRESHOLD {
            let rev = set
                .locate(&never(), &addr, "h", Need::Groups(&groups))
                .await
                .expect("the second cache holds the whole revision");
            assert_eq!(set.cache_name(&rev), "whole");
        }
        let lacking = set.caches.first().expect("first cache");
        assert!(
            !lacking.broken(),
            "a cache that answered its listing must not be paused"
        );
        assert_eq!(
            lacking.health.consecutive_failures.load(Ordering::Relaxed),
            0,
            "a presence miss is not a failure"
        );
    }

    /// Key→bytes backend that records every GET by key and counts listings.
    /// A GET of `cancel_on_get`'s key cancels its token, then answers.
    #[derive(Default)]
    struct MapBackend {
        objects: std::collections::HashMap<String, Vec<u8>>,
        gets: std::sync::Mutex<Vec<String>>,
        lists: AtomicUsize,
        cancel_on_get: Option<(String, hcore::hasync::StdCancellationToken)>,
    }

    impl MapBackend {
        fn gets_of(&self, key: &str) -> usize {
            let gets = self.gets.lock().expect("gets");
            gets.iter().filter(|k| *k == key).count()
        }
    }

    #[async_trait]
    impl RemoteCacheBackend for MapBackend {
        async fn open_read(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<Pin<Box<dyn AsyncRead + Send>>>> {
            self.gets.lock().expect("gets").push(key.to_string());
            if let Some((at, ctoken)) = &self.cancel_on_get
                && at == key
            {
                ctoken.cancel();
            }
            Ok(self.objects.get(key).map(|b| {
                Box::pin(std::io::Cursor::new(b.clone())) as Pin<Box<dyn AsyncRead + Send>>
            }))
        }
        async fn open_write(&self, _key: &str) -> anyhow::Result<Pin<Box<dyn AsyncWrite + Send>>> {
            anyhow::bail!("read-only map backend")
        }
        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.objects.contains_key(key))
        }
        async fn list_names(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .objects
                .keys()
                .filter_map(|k| k.strip_prefix(prefix)?.strip_prefix('/'))
                .map(str::to_string)
                .collect())
        }
    }

    const X: &[u8] = b"blob x, whole";
    const Y: &[u8] = b"blob y, whole too";

    /// The manifest of revision `(addr(), "h")`: raw blobs `x` and `y`, both
    /// in group `out`.
    fn xy_manifest() -> RemoteManifest {
        RemoteManifest {
            version: REMOTE_MANIFEST_VERSION.to_string(),
            target: addr().format(),
            hashin: "h".to_string(),
            artifacts: vec![
                remote_artifact("x", X.len() as u64, ManifestArtifactEncoding::None),
                remote_artifact("y", Y.len() as u64, ManifestArtifactEncoding::None),
            ],
        }
    }

    /// A [`MapBackend`] holding `manifest` (already serialized, or `None`) and
    /// each `(name, bytes)` as that blob of `(addr(), "h")`.
    fn holding(manifest: Option<Vec<u8>>, blobs: &[(&str, &[u8])]) -> MapBackend {
        let addr = addr();
        let mut objects: std::collections::HashMap<String, Vec<u8>> = blobs
            .iter()
            .map(|(name, bytes)| (RemoteCacheSet::key(&addr, "h", name), bytes.to_vec()))
            .collect();
        if let Some(m) = manifest {
            objects.insert(RemoteCacheSet::key(&addr, "h", REMOTE_MANIFEST), m);
        }
        MapBackend {
            objects,
            ..Default::default()
        }
    }

    fn borsh_of(m: &RemoteManifest) -> Vec<u8> {
        borsh::to_vec(m).expect("borsh")
    }

    /// The local manifest's entry for the remote blob `name`, as a hit mirrors
    /// it (under its local v3 name).
    fn want(manifest: &RemoteManifest, name: &str) -> ManifestArtifact {
        local_manifest_from_remote(manifest, "h")
            .artifacts
            .into_iter()
            .find(|a| remote_entry_name(&a.name) == name)
            .expect("the manifest names it")
    }

    fn manifest_key() -> String {
        RemoteCacheSet::key(&addr(), "h", REMOTE_MANIFEST)
    }

    fn cut(bytes: &[u8]) -> &[u8] {
        bytes.get(..bytes.len() - 3).expect("long enough to cut")
    }

    /// The fall-through reads the whole read order, the tiers before the one
    /// that located the revision included. Read `a` then `b`: `a` lacks `y`,
    /// so the hit decision takes `b`; `b`'s `x` is cut short, and `a`, which
    /// holds `x` whole, serves it. No rebuild, no listing on the way.
    #[tokio::test]
    async fn a_tier_before_the_locating_one_serves_the_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = xy_manifest();
        let a = Arc::new(holding(Some(borsh_of(&m)), &[("x", X)]));
        let b = Arc::new(holding(Some(borsh_of(&m)), &[("x", cut(X)), ("y", Y)]));
        let engine = engine_with_set(
            dir.path(),
            RemoteCacheSet::with_backends(
                vec![("a", a.clone()), ("b", b.clone())],
                dir.path().to_path_buf(),
            ),
        );
        let groups = ["out".to_string()];
        let first = engine
            .remote_caches
            .locate(&never(), &addr(), "h", Need::Groups(&groups))
            .await
            .expect("b holds every blob");
        assert_eq!(engine.remote_caches.cache_name(&first), "b");

        assert!(
            engine
                .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want(&m, "x"))
                .await
                .expect("pull"),
            "a holds x whole and must serve it"
        );
        assert_eq!(local_blob(&engine, "h", &want(&m, "x").name), X);
        assert_eq!(
            a.gets_of(&manifest_key()),
            2,
            "a's manifest is read for the hit and once more for the fall-through"
        );
        assert_eq!(
            a.lists.load(Ordering::SeqCst),
            1,
            "only the hit decision lists"
        );
        let b_cache = engine.remote_caches.caches.get(1).expect("b");
        assert!(!b_cache.broken());
        assert_eq!(
            b_cache.health.consecutive_failures.load(Ordering::Relaxed),
            0,
            "a blob cut short is the cache answering, not failing"
        );
    }

    /// The read order, not the config order, decides which tier locates the
    /// revision and which the fall-through asks next: config `[a, b]`, read
    /// `b` then `a`.
    #[tokio::test]
    async fn the_fall_through_follows_the_read_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = xy_manifest();
        let a = Arc::new(holding(Some(borsh_of(&m)), &[("x", X), ("y", Y)]));
        let b = Arc::new(holding(Some(borsh_of(&m)), &[("x", cut(X)), ("y", Y)]));
        let engine = engine_with_set(
            dir.path(),
            RemoteCacheSet::with_backends_in_order(
                vec![("a", a.clone()), ("b", b.clone())],
                vec![1, 0],
                dir.path().to_path_buf(),
            ),
        );
        let first = engine
            .remote_caches
            .locate(&never(), &addr(), "h", Need::Manifest)
            .await
            .expect("located");
        assert_eq!(engine.remote_caches.cache_name(&first), "b");
        assert_eq!(a.gets_of(&manifest_key()), 0, "b is read first");

        assert!(
            engine
                .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want(&m, "x"))
                .await
                .expect("pull")
        );
        assert_eq!(local_blob(&engine, "h", &want(&m, "x").name), X);
        assert_eq!(a.gets_of(&RemoteCacheSet::key(&addr(), "h", "x")), 1);
    }

    /// Three tiers: the first has no manifest, the second cuts both blobs
    /// short, the third holds them whole. Both blobs come from the third, and
    /// the other tiers' manifests are read once for the revision, not once per
    /// blob: the fall-through is O(revisions). Nothing lists.
    #[tokio::test]
    async fn the_fall_through_reads_each_tier_once_per_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = xy_manifest();
        let t1 = Arc::new(holding(None, &[]));
        let t2 = Arc::new(holding(Some(borsh_of(&m)), &[("x", cut(X)), ("y", cut(Y))]));
        let t3 = Arc::new(holding(Some(borsh_of(&m)), &[("x", X), ("y", Y)]));
        let engine = engine_with_set(
            dir.path(),
            RemoteCacheSet::with_backends(
                vec![("t1", t1.clone()), ("t2", t2.clone()), ("t3", t3.clone())],
                dir.path().to_path_buf(),
            ),
        );
        let first = engine
            .remote_caches
            .locate(&never(), &addr(), "h", Need::Manifest)
            .await
            .expect("located");
        assert_eq!(engine.remote_caches.cache_name(&first), "t2");

        for (name, bytes) in [("x", X), ("y", Y)] {
            assert!(
                engine
                    .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want(&m, name))
                    .await
                    .expect("pull"),
                "t3 serves {name}"
            );
            assert_eq!(local_blob(&engine, "h", &want(&m, name).name), bytes);
        }
        let key = manifest_key();
        assert_eq!(t1.gets_of(&key), 2, "t1: the hit, then one fall-through");
        assert_eq!(t2.gets_of(&key), 1, "t2: the hit only");
        assert_eq!(t3.gets_of(&key), 1, "t3: one fall-through for both blobs");
        for t in [&t1, &t2, &t3] {
            assert_eq!(t.lists.load(Ordering::SeqCst), 0, "nothing lists");
        }
    }

    /// A blob no tier serves whole is a miss: the caller rebuilds, nothing
    /// reaches the local cache or stays in the temp dir, and the breaker stays
    /// closed past the failure threshold — a length rejection is the cache
    /// answering, not failing.
    #[tokio::test]
    async fn a_blob_no_tier_serves_whole_is_a_miss_that_leaves_the_breaker_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = xy_manifest();
        let engine = engine_with_set(
            dir.path(),
            RemoteCacheSet::with_backend(
                Arc::new(holding(Some(borsh_of(&m)), &[("x", cut(X)), ("y", Y)])),
                dir.path().to_path_buf(),
            ),
        );
        for _ in 0..=FAILURE_THRESHOLD {
            let first = engine
                .remote_caches
                .locate(&never(), &addr(), "h", Need::Manifest)
                .await
                .expect("located");
            assert!(
                !engine
                    .pull_remote_blob_from_any(&never(), &addr(), "h", &first, &want(&m, "x"))
                    .await
                    .expect("pull"),
                "no tier serves x whole: the caller rebuilds"
            );
        }
        let cache = engine.remote_caches.caches.first().expect("cache");
        assert!(
            !cache.broken(),
            "a length rejection must not pause the cache"
        );
        assert_eq!(cache.health.consecutive_failures.load(Ordering::Relaxed), 0);
        assert!(cache.damage_warned.load(Ordering::Relaxed));
        assert!(
            !engine
                .local_cache
                .exists(&addr(), "h", "x")
                .expect("exists"),
            "a blob cut short must not reach the local cache"
        );
        let tmp = engine.remote_tmp_dir().await.expect("tmp dir");
        assert_eq!(temp_blobs(tmp), Vec::<PathBuf>::new());
    }

    /// A first tier whose manifest is damaged — garbage, a gzip blob, another
    /// target or another hashin under the key — is passed over for the second,
    /// which holds the revision whole; the first's breaker stays closed.
    #[tokio::test]
    async fn a_damaged_manifest_passes_the_tier_over() {
        let good = xy_manifest();
        let mut gzip = good.clone();
        gzip.artifacts[0].encoding = ManifestArtifactEncoding::Gzip;
        let mut other_target = good.clone();
        other_target.target = mk_addr("p", "u", &[]).format();
        let mut other_hashin = good.clone();
        other_hashin.hashin = "h-other".to_string();
        for (what, bytes) in [
            ("garbage", b"not borsh".to_vec()),
            ("a gzip blob", borsh_of(&gzip)),
            ("another target", borsh_of(&other_target)),
            ("another hashin", borsh_of(&other_hashin)),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let set = RemoteCacheSet::with_backends(
                vec![
                    ("damaged", Arc::new(holding(Some(bytes), &[("x", X)]))),
                    (
                        "whole",
                        Arc::new(holding(Some(borsh_of(&good)), &[("x", X), ("y", Y)])),
                    ),
                ],
                dir.path().to_path_buf(),
            );
            let groups = ["out".to_string()];
            let rev = set
                .locate(&never(), &addr(), "h", Need::Groups(&groups))
                .await
                .unwrap_or_else(|| panic!("{what}: the second tier serves"));
            assert_eq!(set.cache_name(&rev), "whole", "{what}");
            let damaged = set.caches.first().expect("damaged");
            assert!(!damaged.broken(), "{what}: the breaker stays closed");
            assert!(
                damaged.damage_warned.load(Ordering::Relaxed),
                "{what}: reported as damage"
            );
        }
    }

    /// A run cancelled while the fall-through looks for another tier ends in
    /// `CancelledError`, not a miss, and leaves no blob, local or temporary.
    #[tokio::test]
    async fn a_cancelled_fall_through_is_cancelled_and_leaves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = xy_manifest();
        let ctoken = never();
        let mut b = holding(Some(borsh_of(&m)), &[("x", X), ("y", Y)]);
        b.cancel_on_get = Some((manifest_key(), ctoken.clone()));
        let engine = engine_with_set(
            dir.path(),
            RemoteCacheSet::with_backends(
                vec![
                    (
                        "a",
                        Arc::new(holding(Some(borsh_of(&m)), &[("x", cut(X)), ("y", Y)])),
                    ),
                    ("b", Arc::new(b)),
                ],
                dir.path().to_path_buf(),
            ),
        );
        let first = engine
            .remote_caches
            .locate(&ctoken, &addr(), "h", Need::Manifest)
            .await
            .expect("located");
        assert_eq!(engine.remote_caches.cache_name(&first), "a");

        let err = engine
            .pull_remote_blob_from_any(&ctoken, &addr(), "h", &first, &want(&m, "x"))
            .await
            .expect_err("a cancelled fall-through is not a miss");
        assert!(err.is::<crate::engine::error::CancelledError>(), "{err:#}");
        assert!(
            !engine
                .local_cache
                .exists(&addr(), "h", "x")
                .expect("exists")
        );
        let tmp = engine.remote_tmp_dir().await.expect("tmp dir");
        assert_eq!(temp_blobs(tmp), Vec::<PathBuf>::new());
    }

    /// The key check compares the manifest's target with the address it was
    /// looked up under; a target with args must round-trip through upload
    /// and hit, not be rejected as another revision.
    #[tokio::test]
    async fn a_target_with_args_round_trips_the_key_check() {
        let writer_dir = tempfile::tempdir().expect("tempdir");
        let target = mk_addr("p", "t", &[("os", "linux/amd64"), ("mode", "a b")]);
        let (backend, writer) =
            cache_built_at(writer_dir.path(), "c", &target, "h", b"data", "ho-data").await;
        let local = writer
            .read_manifest_blocking(&never(), &target, "h")
            .await
            .expect("read manifest")
            .expect("manifest");
        let want = local.artifacts.first().expect("one artifact").clone();

        let reader_dir = tempfile::tempdir().expect("tempdir");
        let reader = engine_with_set(
            reader_dir.path(),
            RemoteCacheSet::with_backends(vec![("c", backend)], reader_dir.path().to_path_buf()),
        );
        let (_, rev) = reader
            .probe_remote_revision(&never(), &target, "h", &["out".to_string()])
            .await
            .expect("probe")
            .expect("the revision passes the key check");
        assert!(
            reader
                .pull_remote_blob_from_any(&never(), &target, "h", &rev, &want)
                .await
                .expect("pull")
        );
        assert!(
            !reader
                .remote_caches
                .caches
                .first()
                .expect("c")
                .damage_warned
                .load(Ordering::Relaxed),
            "nothing was reported damaged"
        );
    }

    /// `entry_for` matches the whole entry, and never an empty hashout: a log
    /// identifies nothing, so no tier's log is ever taken for another's.
    #[test]
    fn entry_for_matches_the_whole_entry_and_never_an_empty_hashout() {
        let mut m = xy_manifest();
        m.artifacts.push(RemoteManifestArtifact {
            hashout: String::new(),
            group: String::new(),
            name: "log".to_string(),
            size: 3,
            r#type: ManifestArtifactType::Log,
            content_type: ManifestArtifactContentType::Tar,
            encoding: ManifestArtifactEncoding::None,
        });
        let rev = RemoteRevision::new(0, m.clone());
        assert!(rev.entry_for(&want(&m, "x")).is_some(), "the same entry");
        assert!(
            rev.entry_for(&want(&m, "log")).is_none(),
            "an empty hashout never matches, even itself"
        );
        type Change = fn(&mut ManifestArtifact);
        let changes: [(&str, Change); 5] = [
            ("hashout", |w| w.hashout.push('!')),
            ("size", |w| w.size += 1),
            ("encoding", |w| w.encoding = ManifestArtifactEncoding::Zstd),
            ("group", |w| w.group.push('!')),
            ("type", |w| w.r#type = ManifestArtifactType::SupportFile),
        ];
        for (what, change) in changes {
            let mut w = want(&m, "x");
            change(&mut w);
            assert!(rev.entry_for(&w).is_none(), "another {what}");
        }
    }
}
