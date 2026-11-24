//! Shared, on-demand, cross-run cached filesystem walker.
//!
//! Every tree-walking plugin (the `fs` glob driver, the buildfile package
//! discovery) repeats the same `readdir` + `stat` + content-hash work each run
//! over a tree that rarely changes. This walker caches that work *per path*,
//! independent of who asks: two consumers reading the same directory share one
//! cached listing, and one consumer hashing a file shares it with another.
//!
//! It is **consumer-agnostic** and **on-demand**: the walker only answers
//! "what's in this directory?" ([`read_dir`]) and "what's this file's content
//! hash?" ([`file_hash`]). Filtering (globs, excludes, skip dirs) and the
//! decision of whether to recurse belong to the *consumer* — so a requester that
//! stops shallow and one that recurses deep both reuse the dirs they share and
//! independently cache the ones they don't.
//!
//! Validation is stat based (a fast-path proxy for content identity, the same
//! tradeoff accepted elsewhere): a directory's listing is reused while its
//! [`StatKey`] — mtime, ctime and inode — is unchanged (mtime bumps on any entry
//! add/remove/rename); a file's hash is reused while its size, [`StatKey`]
//! and exec bit are. The ctime and inode catch what mtime alone misses: a
//! writer that puts the old mtime back (`cp -p`, `rsync -a`, `tar -x`, a
//! nix-store copy) still moves the ctime, or replaces the inode, and so does a
//! `chmod` that flips the exec bit folded into the hash.
//!
//! A read is cached only once it is settled: when the clock, sampled before the
//! stat, is at least [`RACY_WINDOW`] past the entry's newer of mtime and ctime.
//! A write landing in the same timestamp tick as the read can leave every stat
//! field unchanged, so a read that recent is used but never stored — in-process
//! or durably — and the next read takes it from disk again. Nothing promotes it
//! later. The ctime counts because the mtime can lie: an mtime-preserving
//! writer stamps an old mtime, and two such writes within one ctime tick would
//! otherwise be cached as settled and leave the key unchanged.
//!
//! The rule trusts the kernel to stamp file times from the same clock heph
//! samples. It does not hold on a workspace whose times are stamped by another
//! machine — NFS or SMB, or a VM-shared mount such as a Docker Desktop bind
//! mount — whose clock lags heph's by more than [`RACY_WINDOW`]: a read there
//! can look settled while a write in the same tick is still possible. Nor does
//! it hold on a read-only image filesystem that reports stored times rather
//! than stamping them — squashfs, erofs, iso9660 — where ctime is the image's
//! mtime, and a reproducible image normalizes times and assigns inodes
//! deterministically: mounting one image and then the next at the same path
//! can leave a same-size edit with an identical key. Both are accepted
//! exemptions for the same reason — the times are not stamped from heph's
//! clock — not supported configurations.
//!
//! Upgrading to [`FSWALK_SCHEMA_VERSION`] 2 drops every stored row, so the first
//! run after it re-reads every directory and re-hashes every input once. Target
//! cache keys do not move: a re-read of an unchanged file hashes the same.
//! Anything changed in the last [`RACY_WINDOW`] is re-read on every call until it
//! settles.
//!
//! Everything is correct-by-fallback — a missing/locked db, a decode error, or
//! any mismatch just re-reads from disk.
//!
//! Entry names are UTF-8 by construction: a directory holding a name that is not
//! valid UTF-8 fails [`read_dir`] rather than listing without it, so no
//! walker-backed consumer can hash, glob, or name a path the rest of heph cannot
//! represent. See [`DIR_LISTING_VERSION`] for how that guarantee survives a warm
//! cache. It binds only what goes through here: code that calls `std::fs` and
//! matches on a lossy name — `pluginbuildfile`'s `build_files_in_dir`, used by
//! the formatter and the LSP — can still act on a file the build refuses.
//!
//! Backed by a dedicated `fswalk.db` (separate from the artifact cache so it can
//! be pruned independently). Rows carry a last-access stamp; [`prune`] drops
//! stale and orphaned rows so the db cannot grow without bound.
//!
//! [`read_dir`]: CachedWalker::read_dir
//! [`file_hash`]: CachedWalker::file_hash
//! [`prune`]: CachedWalker::prune

use crate::migrate_lock::MIGRATE_LOCK_TIMEOUT;
use anyhow::{Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use parking_lot::Mutex;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::Xxh3;

/// Default time-to-live for fswalk rows: entries untouched for this long are
/// dropped by [`CachedWalker::prune`].
pub const DEFAULT_TTL: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 60 * 60);

/// A row's last-access stamp is refreshed only once it is at least this old.
///
/// The stamp exists for one reader, [`CachedWalker::prune`]'s TTL. Rewriting a
/// stamp that is minutes old moves nothing that pruning can see, and it was the
/// one write a warm, unchanged run made — which in turn forced the read-write
/// connection open and its close-time checkpoint and WAL unlink at every exit.
/// The cost of the coarser stamp: a row used in the last day may be up to this
/// much older than its true last use, so a TTL prune can drop it up to a day
/// early — a re-read from disk, since every row is rebuildable. Against the
/// 14-day [`DEFAULT_TTL`] that is a row used within the last 13 days, always
/// kept.
pub const TOUCH_REFRESH: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// How far the clock must be past an entry's newer of mtime and ctime for a
/// read of it to be cached (see the module docs).
///
/// A write in the same timestamp tick as an earlier read can leave mtime, size
/// and even ctime where the read saw them, so the read's result would be served
/// for content it never saw. Two seconds is the coarsest timestamp granularity
/// heph can meet (FAT's 2 s; HFS+ and ext3 are 1 s; ext4 and APFS store
/// nanoseconds but stamp from a clock that ticks in milliseconds). One constant
/// on every platform: what a file's hash resolves to must not depend on which
/// host read it. The cost is that a file or directory changed in the last two
/// seconds is re-read on every call until it settles.
///
/// The ctime is what makes the rule hold: the kernel always stamps it from its
/// own clock, while the mtime is whatever the writer set. Judged on mtime
/// alone, a file `tar -x`, `cp -p` or `rsync -a` just wrote with an old mtime
/// would be cached at once, and a second such write within the same ctime tick
/// would then leave the whole key unchanged and the old hash served. The same
/// rule as the plugin checksum stamps' (`stamp_refusal` in the engine's
/// `plugin_load.rs`).
const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a read of an entry with `key`, whose stat came after the clock read
/// `clock_ns`, may be cached: the clock is at least [`RACY_WINDOW`] past the
/// newer of its mtime and ctime. A stamp ahead of the clock (a clock stepped
/// back, a future mtime) is never settled.
fn settled(key: &StatKey, clock_ns: i64) -> bool {
    let window = i64::try_from(RACY_WINDOW.as_nanos()).unwrap_or(i64::MAX);
    clock_ns.saturating_sub(key.newest_ns()) >= window
}

/// Log a read that is returned but not cached because it is not [`settled`].
fn log_racy_skip(path: &Path, kind: &'static str, key: &StatKey, clock_ns: i64) {
    tracing::debug!(
        path = %path.display(),
        kind,
        age_ms = clock_ns.saturating_sub(key.newest_ns()) / 1_000_000,
        "fswalk: read too recent to cache, served from disk",
    );
}

/// Log a cached read discarded because the live stat no longer matches it.
fn log_key_mismatch(path: &Path, kind: &'static str, reason: &'static str) {
    tracing::debug!(
        path = %path.display(),
        kind,
        reason,
        "fswalk: cached read invalidated by a stat change, re-reading",
    );
}

/// Whether a row last stamped at `accessed_ns` needs a refresh at `now_ns`. A
/// stamp in the future (the clock stepped back) is refreshed, so it cannot hold
/// a row's stamp ahead of its real use.
fn touch_is_stale(accessed_ns: i64, now_ns: i64) -> bool {
    let refresh = i64::try_from(TOUCH_REFRESH.as_nanos()).unwrap_or(i64::MAX);
    !(0..refresh).contains(&now_ns.saturating_sub(accessed_ns))
}

/// Semantic version of a cached `dirs` listing — **bump on any change to which
/// entries [`read_dir_uncached`] puts in a [`DirListing`]**.
///
/// A `dirs` row is otherwise revalidated by directory mtime alone, and a
/// filtering change does not touch any mtime. Without this, a row written by an
/// older binary that silently dropped an entry would be decoded and served
/// verbatim by a newer one that would have refused it — the fix would be inert
/// on every warm cache, and the same tree would build on a machine with a stale
/// db and fail on a freshly-cloned one, decided by a per-machine sqlite file
/// that is in nobody's hash. Same rationale as `GO_TESTMAIN_FORMAT_VERSION`.
///
/// Compared per row rather than stamped db-wide so that a mixed fleet is safe in
/// both directions: an older binary's `INSERT` omits the column and lands on the
/// `DEFAULT 0`, which no current version accepts.
///
/// - 1: `read_dir_uncached` errors on a non-UTF-8 entry name instead of dropping
///   it (rows written before this may be missing entries).
const DIR_LISTING_VERSION: i64 = 1;

/// Escape hatch: set `HEPH_DEBUG_CACHED_WALKER=0` to bypass caching entirely and
/// fall back to reading every directory listing and file hash straight from disk
/// (no in-process front, no durable store). Any other value (or unset) keeps the
/// cache on. Cached in a `OnceLock` because `std::env::var` takes a global libc
/// mutex; the value can't change within a run.
fn cache_bypassed() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        matches!(
            std::env::var("HEPH_DEBUG_CACHED_WALKER").as_deref(),
            Ok("0")
        )
    })
}

/// The kind of a directory entry, from the platform `d_type` (no extra stat).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry in a directory listing: its file name (not a full path) and kind.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// A cached directory listing. Entries are sorted by name for determinism.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
}

/// A cached file content hash plus the stat fields that validate it.
#[derive(Clone, Debug)]
pub struct FileHash {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub exec: bool,
    /// xxh3 of the file content with the exec bit folded in (see [`file_hashout`]).
    pub hashout: String,
}

/// Identity of one sourced `fs` file — an `@heph/fs:file` artifact or one
/// `@heph/fs:glob` entry: its workspace-relative `out_path` (`/`-separated)
/// bound to its [`file_hashout`].
///
/// A consumer's `hashin` folds the *sorted* hashouts of its inputs and nothing
/// else about them — no names, no paths. A glob addr names only its pattern, so
/// with a bare content hash per entry, renaming a matched file (same bytes) or
/// swapping two files' contents left the consumer's key unchanged and served a
/// result computed for another tree. Binding the path here is the only place it
/// can reach the key.
///
/// `fs:file` uses the same form even though its addr already names the path:
/// one file has one hashout whichever way it was sourced, so `file("a.go")` and
/// a `glob("a.go")` that matches only it produce the same artifact identity.
///
/// The tag keeps this domain apart from any other xxh3 of a string; the NUL
/// separators cannot occur in a path or in a hex digest, so `(path, hash)` pairs
/// cannot be re-split into another pair with the same input bytes.
pub fn fs_entry_hashout(out_path: &str, content_hashout: &str) -> String {
    let mut h = Xxh3::new();
    h.update(b"fs_entry\0");
    h.update(out_path.as_bytes());
    h.update(b"\0");
    h.update(content_hashout.as_bytes());
    format!("{:x}", h.digest())
}

/// Content identity for a sourced file: a hash of its bytes plus the executable
/// marker. Deliberately ignores size and mtime — only the content and the `x`
/// bit determine the artifact, so a file rewritten with identical bytes (new
/// mtime, same content) hashes the same and stays a cache hit. (mtime/size are
/// used only as a *cache validation* fast-path, never as the identity.)
pub fn file_hashout(path: &Path, x: bool) -> Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open file for hashing '{}'", path.display()))?;
    let mut h = ContentHasher::new();
    // Stream in chunks so large inputs never load wholesale into memory. The
    // chunks are large enough that a BufReader would add nothing.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read file for hashing '{}'", path.display()))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            h.update(chunk);
        }
    }
    Ok(h.finish(x))
}

/// [`file_hashout`] of a file holding `bytes`, computed from memory: for a
/// caller that has just written those bytes, or is about to, and must know
/// what the file hashes to without re-reading it (a read could see a later
/// edit instead). The codegen index records it as the bytes about to land.
pub fn bytes_hashout(bytes: &[u8], x: bool) -> String {
    let mut h = ContentHasher::new();
    h.update(bytes);
    h.finish(x)
}

/// The one content hash [`file_hashout`], [`bytes_hashout`],
/// [`CachedWalker::file_hash`] and the engine's passthrough verification
/// share, so they cannot drift: the content streamed in, then the exec marker
/// folded in after it. It feeds cache keys, so its bytes never change.
pub struct ContentHasher(Xxh3);

impl std::fmt::Debug for ContentHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentHasher").finish_non_exhaustive()
    }
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentHasher {
    pub fn new() -> Self {
        Self(Xxh3::new())
    }

    /// Stream the next `bytes` of the content in.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The hashout of the content streamed in, for a file whose exec bit is
    /// `x`.
    pub fn finish(mut self, x: bool) -> String {
        self.0.update(&[x as u8]);
        format!("{:x}", self.0.digest())
    }
}

impl FileHash {
    /// A hash of a file stat'ed as `st`. Its stat fields are `-1`/`0` when the
    /// stat had no [`StatKey`]; such a hash is never cached.
    fn new(st: &Stat, hashout: String) -> Self {
        Self {
            size: st.size,
            mtime_ns: st.key.map_or(-1, |k| k.mtime_ns),
            ctime_ns: st.key.map_or(-1, |k| k.ctime_ns),
            ino: st.key.map_or(0, |k| k.ino),
            exec: st.exec,
            hashout,
        }
    }

    fn key(&self) -> StatKey {
        StatKey {
            mtime_ns: self.mtime_ns,
            ctime_ns: self.ctime_ns,
            ino: self.ino,
        }
    }

    /// Which validated field of this cached hash differs from the live stat
    /// `st` (whose key is `key`), or `None` when the hash is still valid.
    fn mismatch(&self, st: &Stat, key: &StatKey) -> Option<&'static str> {
        if self.size != st.size {
            Some("size")
        } else if self.exec != st.exec {
            Some("exec")
        } else {
            self.key().mismatch(key)
        }
    }
}

/// The stat fields a cached read is validated against: a change to any of them
/// means the path may have changed since the read (see the module docs).
///
/// The device id is left out on purpose. Rows are keyed by path, under one
/// workspace's db, and `st_dev` changes under a path whose content did not:
/// btrfs subvolumes, NFS, overlayfs and macOS external volumes can report a new
/// one across remounts or reboots, which would turn every warm run there into
/// a full re-read. What it would catch — another filesystem mounted over the
/// path between runs — moves ctime wherever the kernel stamps it, and a reused
/// loop device keeps its id, so the device would not catch it either. Where the
/// kernel does not stamp it (a read-only image filesystem, whose times come from
/// the image), neither the device nor the key is a guarantee; see the module
/// docs.
///
/// The key, and [`settled`], assume the local kernel stamps file times from
/// the clock heph samples; see the module docs for the mounts where it does
/// not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StatKey {
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
}

impl StatKey {
    /// The newer of the mtime and ctime: when the entry last changed, as far
    /// as [`settled`] is concerned.
    fn newest_ns(&self) -> i64 {
        self.mtime_ns.max(self.ctime_ns)
    }

    /// Which field of this (cached) key differs from `live`, or `None`.
    fn mismatch(&self, live: &StatKey) -> Option<&'static str> {
        if self.mtime_ns != live.mtime_ns {
            Some("mtime")
        } else if self.ctime_ns != live.ctime_ns {
            Some("ctime")
        } else if self.ino != live.ino {
            Some("ino")
        } else {
            None
        }
    }
}

/// What [`CachedWalker`] reads from one stat of a path (following symlinks).
#[derive(Clone, Copy, Debug)]
struct Stat {
    is_dir: bool,
    size: u64,
    exec: bool,
    /// `None` when a timestamp is not representable in `i64` nanoseconds since
    /// the epoch: the read is then served from disk and never cached.
    key: Option<StatKey>,
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        let key = match (
            epoch_ns(meta.mtime(), meta.mtime_nsec()),
            epoch_ns(meta.ctime(), meta.ctime_nsec()),
        ) {
            (Some(mtime_ns), Some(ctime_ns)) => Some(StatKey {
                mtime_ns,
                ctime_ns,
                ino: meta.ino(),
            }),
            _ => None,
        };
        Self {
            is_dir: meta.is_dir(),
            size: meta.len(),
            exec: is_exec(meta),
            key,
        }
    }
}

/// The production stat: `stat(2)`, following symlinks.
fn stat_path(path: &Path) -> std::io::Result<Stat> {
    std::fs::metadata(path).map(|m| Stat::of(&m))
}

/// The clock a walker samples before each stat, in nanoseconds since the epoch.
type ClockFn = Box<dyn Fn() -> i64 + Send + Sync>;
/// The stat a walker validates against.
type StatFn = Box<dyn Fn(&Path) -> std::io::Result<Stat> + Send + Sync>;

fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// A `stat` timestamp (`secs`, `nsec`) in nanoseconds since the epoch, or
/// `None` when that does not fit an `i64`.
fn epoch_ns(secs: i64, nsec: i64) -> Option<i64> {
    secs.checked_mul(1_000_000_000)?.checked_add(nsec)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn entry_kind(ft: std::fs::FileType) -> EntryKind {
    if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }
}

/// Shared cached filesystem walker. Cheap to clone (`Arc` the whole thing).
pub struct CachedWalker {
    /// When set, all caching is skipped: every call reads straight from disk.
    /// Transparent to consumers — the API is identical, only slower.
    bypass: bool,
    /// The durable store's db, opened on first use — `None` for a walker with no
    /// durable store at all (disabled, bypassing).
    store_path: Option<PathBuf>,
    /// The durable store, once opened. Holds `None` when the open failed: the
    /// walker then degrades to its in-process front for the rest of its life,
    /// exactly as an eager open failure did.
    store: std::sync::OnceLock<Option<FsWalkStore>>,
    /// In-process front for directory listings, validated against the live
    /// [`StatKey`].
    dirs: Mutex<FxHashMap<PathBuf, (StatKey, Arc<DirListing>)>>,
    /// In-process front for file hashes, validated against the live size and
    /// [`StatKey`].
    files: Mutex<FxHashMap<PathBuf, Arc<FileHash>>>,
    /// Paths read-served from the durable store this process; their last-access
    /// stamps are refreshed in one batch on drop (so reads stay write-free).
    touched_dirs: Mutex<Vec<String>>,
    touched_files: Mutex<Vec<String>>,
    /// The clock sampled before each stat, to decide whether the read is
    /// settled (see [`RACY_WINDOW`]). The system clock outside tests.
    clock: ClockFn,
    /// The stat each read is validated against. `stat(2)` outside tests.
    stat: StatFn,
}

impl CachedWalker {
    fn new(bypass: bool, store_path: Option<PathBuf>) -> Self {
        Self {
            bypass,
            store_path,
            store: std::sync::OnceLock::new(),
            dirs: Mutex::new(FxHashMap::default()),
            files: Mutex::new(FxHashMap::default()),
            touched_dirs: Mutex::new(Vec::new()),
            touched_files: Mutex::new(Vec::new()),
            clock: Box::new(now_ns),
            stat: Box::new(stat_path),
        }
    }

    /// The walker backed by the sqlite db at `db_path`, opened (or created) on
    /// first use — a command that never walks a tree never opens it. On any
    /// db-open failure the walker still works — it just degrades to its
    /// in-process front, as [`Self::disabled`] (no cross-run cache).
    pub fn open(db_path: &Path) -> Self {
        if cache_bypassed() {
            tracing::warn!(
                "cached walker bypassed (HEPH_DEBUG_CACHED_WALKER=0): reading from disk"
            );
            return Self::bypassing();
        }
        Self::new(false, Some(db_path.to_path_buf()))
    }

    /// A disabled walker (no durable store, in-process front only) for tests /
    /// contexts without a db.
    pub fn disabled() -> Self {
        Self::new(false, None)
    }

    /// A fully bypassing walker: no in-process front, no durable store — every
    /// call reads straight from disk. Used when `HEPH_DEBUG_CACHED_WALKER=0`.
    pub fn bypassing() -> Self {
        Self::new(true, None)
    }

    /// This walker with `clock` in place of the system clock.
    #[cfg(test)]
    fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// This walker with `stat` in place of `stat(2)`.
    #[cfg(test)]
    fn with_stat(
        mut self,
        stat: impl Fn(&Path) -> std::io::Result<Stat> + Send + Sync + 'static,
    ) -> Self {
        self.stat = Box::new(stat);
        self
    }

    /// The durable store, opened on the first call. `None` when this walker has
    /// none, or when opening it failed (logged once).
    fn store(&self) -> Option<&FsWalkStore> {
        let path = self.store_path.as_deref()?;
        self.store
            .get_or_init(|| match FsWalkStore::open(path) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        db = %path.display(),
                        "fswalk cache disabled; results unaffected, walks slower. \
                         Delete {} to rebuild it",
                        path.display()
                    );
                    None
                }
            })
            .as_ref()
    }

    /// Whether the durable store has been opened (successfully or not).
    #[cfg(test)]
    fn store_opened(&self) -> bool {
        self.store.get().is_some()
    }

    /// The cached listing of directory `dir` (absolute). A missing directory
    /// lists empty. The caller filters entries and decides whether to recurse.
    pub fn read_dir(&self, dir: &Path) -> Result<Arc<DirListing>> {
        if self.bypass {
            return Ok(Arc::new(read_dir_uncached(dir)?));
        }
        // Sampled before the stat: a write after this instant stamps an mtime at
        // least this late, so it cannot share a tick with an mtime
        // `RACY_WINDOW` older — the only kind of read that is cached.
        let clock = (self.clock)();
        let st = match (self.stat)(dir) {
            Ok(st) => st,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Arc::new(DirListing::default()));
            }
            Err(e) => return Err(e).with_context(|| format!("stat dir '{}'", dir.display())),
        };
        if !st.is_dir {
            return Ok(Arc::new(DirListing::default()));
        }
        let Some(key) = st.key else {
            return Ok(Arc::new(read_dir_uncached(dir)?));
        };

        // In-process front: reuse while the live key matches.
        let front = self.dirs.lock().get(dir).cloned();
        if let Some((cached_key, listing)) = front {
            match cached_key.mismatch(&key) {
                None => return Ok(listing),
                Some(reason) => log_key_mismatch(dir, "dir", reason),
            }
        }

        // Durable store: reuse while the recorded key matches.
        if let Some(store) = self.store()
            && let Some((stored_key, blob, accessed_ns)) = store.get_dir(dir)
        {
            match stored_key.mismatch(&key) {
                Some(reason) => log_key_mismatch(dir, "dir", reason),
                None => {
                    if let Ok(listing) = borsh::from_slice::<DirListing>(&blob) {
                        let listing = Arc::new(listing);
                        self.dirs
                            .lock()
                            .insert(dir.to_path_buf(), (key, listing.clone()));
                        if touch_is_stale(accessed_ns, now_ns()) {
                            self.touched_dirs.lock().push(path_key(dir));
                        }
                        return Ok(listing);
                    }
                }
            }
        }

        // Miss: read the directory, and cache it once it is settled.
        let listing = Arc::new(read_dir_uncached(dir)?);
        if !settled(&key, clock) {
            log_racy_skip(dir, "dir", &key, clock);
        } else {
            if let Some(store) = self.store()
                && let Ok(blob) = borsh::to_vec(&*listing)
            {
                store.put_dir(dir, &key, &blob, now_ns());
            }
            self.dirs
                .lock()
                .insert(dir.to_path_buf(), (key, listing.clone()));
        }
        Ok(listing)
    }

    /// The cached content hash (and exec bit) of file `file` (absolute), following
    /// symlinks. Validated by size and [`StatKey`]; re-hashed on a mismatch.
    pub fn file_hash(&self, file: &Path) -> Result<Arc<FileHash>> {
        // Sampled before the stat, as in `read_dir`.
        let clock = (self.clock)();
        let st = (self.stat)(file).with_context(|| format!("stat file '{}'", file.display()))?;
        // A directory (e.g. a symlink resolving to one) is not hashable — error so
        // the caller skips it rather than trying to read it.
        anyhow::ensure!(!st.is_dir, "'{}' is a directory", file.display());

        if self.bypass {
            return Ok(Arc::new(FileHash::new(&st, file_hashout(file, st.exec)?)));
        }
        let Some(key) = st.key else {
            return Ok(Arc::new(FileHash::new(&st, file_hashout(file, st.exec)?)));
        };

        // In-process front: reuse while size, exec bit and key match.
        let front = self.files.lock().get(file).cloned();
        if let Some(found) = front {
            match found.mismatch(&st, &key) {
                None => return Ok(found),
                Some(reason) => log_key_mismatch(file, "file", reason),
            }
        }

        // Durable store.
        if let Some(store) = self.store()
            && let Some((fh, accessed_ns)) = store.get_file(file)
        {
            match fh.mismatch(&st, &key) {
                Some(reason) => log_key_mismatch(file, "file", reason),
                None => {
                    let fh = Arc::new(fh);
                    self.files.lock().insert(file.to_path_buf(), fh.clone());
                    if touch_is_stale(accessed_ns, now_ns()) {
                        self.touched_files.lock().push(path_key(file));
                    }
                    return Ok(fh);
                }
            }
        }

        // Miss: hash the file, and cache it once it is settled.
        let fh = Arc::new(FileHash::new(&st, file_hashout(file, st.exec)?));
        if !settled(&key, clock) {
            log_racy_skip(file, "file", &key, clock);
        } else {
            if let Some(store) = self.store() {
                store.put_file(file, &fh, now_ns());
            }
            self.files.lock().insert(file.to_path_buf(), fh.clone());
        }
        Ok(fh)
    }

    /// Drop rows untouched for longer than `ttl`, and (when `check_orphans`)
    /// rows whose path no longer exists on disk. Returns the number removed.
    pub fn prune(&self, ttl: std::time::Duration, check_orphans: bool) -> Result<usize> {
        match self.store() {
            Some(store) => store.prune(ttl, check_orphans),
            None => Ok(0),
        }
    }

    /// Flush this process's accumulated last-access stamps. Called on drop;
    /// exposed for tests.
    fn flush_touches(&self) {
        let dirs = std::mem::take(&mut *self.touched_dirs.lock());
        let files = std::mem::take(&mut *self.touched_files.lock());
        // Nothing touched: never open the store just to write nothing.
        if dirs.is_empty() && files.is_empty() {
            return;
        }
        let Some(store) = self.store() else {
            return;
        };
        let now = now_ns();
        if !dirs.is_empty() {
            store.touch("dirs", &dirs, now);
        }
        if !files.is_empty() {
            store.touch("files", &files, now);
        }
    }
}

impl Default for CachedWalker {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Drop for CachedWalker {
    fn drop(&mut self) {
        self.flush_touches();
    }
}

/// Read a directory directly (no cache), returning a sorted [`DirListing`].
fn read_dir_uncached(dir: &Path) -> Result<DirListing> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DirListing::default()),
        Err(e) => return Err(e).with_context(|| format!("read dir '{}'", dir.display())),
    };
    let mut entries = Vec::new();
    for entry in rd {
        let entry = entry.with_context(|| format!("read dir entry in '{}'", dir.display()))?;
        // `d_type` is free on Linux, but a `DT_UNKNOWN` filesystem makes this an
        // `lstat`, which can fail. Only a vanished entry is skippable — it is
        // genuinely not in the tree, and a concurrent delete must not fail a
        // walk. Anything else (EACCES, EIO) is a real entry this listing would
        // otherwise silently omit, and the omission would then be cached under
        // the directory's current mtime and outlive the run.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("stat dir entry '{}'", entry.path().display()));
            }
        };
        // A name that is not valid UTF-8 is a hard error, not a skipped entry.
        // Dropping it silently removed a real path from every consumer's view:
        // a `*.BUILD` file that names a package (so the package, and its
        // ancestors, vanish from the def-hash-bearing package list) or a source
        // file a glob would have matched (so its content hash never reaches the
        // parent's `hashin` — edit it and the stale artifact is served). heph
        // represents every path as UTF-8 all the way to addresses and cache
        // manifests, so such a name cannot be built with; refusing to walk past
        // it is the only outcome that is neither wrong nor silent.
        let name = entry.file_name().into_string().map_err(|raw| {
            anyhow::anyhow!(
                // `{:?}` on the `OsStr`, not a lossy render: `caf\xe9` and
                // `caf\xea` both *display* as `caf\u{FFFD}`, so the one message
                // that has to identify a name precisely must not use the
                // rendering this whole change exists to reject. Debug escapes
                // the invalid bytes, which is injective and pasteable.
                "directory entry name is not valid UTF-8: {:?} (in '{}'); heph paths must be \
                 UTF-8 — rename it, or `fs.skip` the containing directory",
                raw,
                dir.display(),
            )
        })?;
        entries.push(DirEntry {
            name,
            kind: entry_kind(ft),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(DirListing { entries })
}

/// A path's db key.
///
/// `to_string_lossy` is **not** injective — `x\xffy` and `x\xfey` both render to
/// `x\u{FFFD}y`, and `path` is the `dirs`/`files` PRIMARY KEY, so two distinct
/// paths would share one row and could serve each other's listing or content
/// hash under a matching mtime. What rules that out is not the mtime/size guard
/// (mtime collides routinely after `tar -x`, `cp -p`, or `rsync -t`) but the
/// walk's own invariant: every walked path is `root.join(<name from
/// read_dir_uncached>)`, and that function rejects non-UTF-8 names outright, so
/// the only possible source of lossiness is `root` itself — and a single root
/// mangles to a single consistent prefix, while each workspace's db lives under
/// its own heph home, so two roots that render alike never share a `dirs` table.
/// The one way to break that is to point two roots at one db by hand, which
/// `crates/plugin-go-cdylib`'s `walk_db` option makes possible.
fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ── Durable store ────────────────────────────────────────────────────────────

struct FsWalkStore {
    db_path: PathBuf,
    read_pool: r2d2::Pool<SqliteConnectionManager>,
    /// The read-write connection, opened by the first write. A run that only
    /// reads the store (a warm, unchanged tree, with its access stamps still
    /// fresh — see [`TOUCH_REFRESH`]) never opens it, and so never pays the
    /// checkpoint and WAL/shm unlink that closing the last read-write
    /// connection does at exit. Holds `None` if opening it failed: writes are
    /// best-effort, and are then skipped for the rest of the run.
    write: std::sync::OnceLock<Option<Mutex<Connection>>>,
}

impl FsWalkStore {
    fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating fswalk dir {parent:?}"))?;
        }
        let read_pool = Self::read_pool(db_path)?;
        let write = std::sync::OnceLock::new();
        // The schema check reads `user_version` on a pooled read connection —
        // the one the first lookup needs anyway. Only a missing db (a read-only
        // open of it would fail, and r2d2 would retry that until its timeout)
        // or one behind the current version opens the read-write connection
        // now, to migrate; it is then kept for the writes. A db the check
        // cannot read goes to the migration too, whose read-write open reports
        // the real error.
        let current = db_path.exists()
            && read_pool
                .get_timeout(SCHEMA_CHECK_TIMEOUT)
                .ok()
                .and_then(|c| {
                    c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                        .ok()
                })
                .is_some_and(|v| v >= FSWALK_SCHEMA_VERSION);
        if !current {
            let conn = open_write_connection(db_path)?;
            migrate_fswalk_schema(db_path, &conn)
                .with_context(|| format!("migrating fswalk db {}", db_path.display()))?;
            drop(write.set(Some(Mutex::new(conn))));
        }
        Ok(Self {
            db_path: db_path.to_path_buf(),
            read_pool,
            write,
        })
    }

    /// The read-write connection, opened on first use (see the field docs).
    fn writer(&self) -> Option<parking_lot::MutexGuard<'_, Connection>> {
        self.write
            .get_or_init(|| match open_write_connection(&self.db_path) {
                Ok(c) => Some(Mutex::new(c)),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        db = %self.db_path.display(),
                        "fswalk cache writes disabled; results unaffected, walks slower. \
                         Delete {} to rebuild it",
                        self.db_path.display()
                    );
                    None
                }
            })
            .as_ref()
            .map(Mutex::lock)
    }

    /// Whether the read-write connection has been opened.
    #[cfg(test)]
    fn write_opened(&self) -> bool {
        self.write.get().is_some()
    }

    fn read_pool(db_path: &Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
        let manager = SqliteConnectionManager::file(db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA busy_timeout = 10000;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA mmap_size = 268435456;",
                )
            });
        // Its helper thread exits with the pool; see `hsqlite`.
        hsqlite::pool_builder()
            .max_size(16)
            // Zero: the store is opened by its first use, and that use opens
            // the read connection it needs rather than one more up front.
            .min_idle(Some(0))
            // Explicit and short. r2d2's 30 s default applies to a connection
            // that cannot be *opened* as well as to an exhausted pool, and it
            // retries the open the whole time: an unreadable db stalled the
            // store's first use (the schema check) for 30 s before degrading.
            // Every lookup here is one indexed read with an on-disk fallback,
            // so waiting longer than this buys nothing. (The schema check
            // itself waits only SCHEMA_CHECK_TIMEOUT.)
            .connection_timeout(FSWALK_POOL_TIMEOUT)
            // Pinned like the engine's pool: no `is_valid` round trip per
            // checkout should a feature unification turn it on.
            .test_on_check_out(false)
            .build(manager)
            .context("building fswalk read pool")
    }
}

/// Open the fswalk read-write connection.
fn open_write_connection(db_path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(db_path).with_context(|| format!("opening fswalk db at {db_path:?}"))?;
    conn
        // This db is a pure optimization cache: every row is reconstructable
        // from disk, so corruption is tolerable (a bad db just re-reads and
        // rebuilds). We trade crash durability for speed.
        //   - WAL stays: required so multiple processes read concurrently
        //     while one writes. busy_timeout handles cross-process write
        //     contention. Neither is a durability knob, so both are kept.
        //   - synchronous = OFF (vs NORMAL): drops the remaining checkpoint
        //     fsyncs. An app crash is still safe (the OS flushes the pages);
        //     only an OS crash / power loss can corrupt — which we accept.
        //   synchronous affects only crash durability, never concurrent-access
        //   correctness, so multi-process behavior is unchanged.
        .execute_batch(
            "PRAGMA busy_timeout = 10000;
             PRAGMA synchronous = OFF;
             PRAGMA temp_store = MEMORY;
             PRAGMA mmap_size = 268435456;",
        )
        .with_context(|| format!("configuring the fswalk write connection to {db_path:?}"))?;
    Ok(conn)
}

/// The fswalk schema's version, in the db's `PRAGMA user_version`. A db at this
/// version has both tables with every column, so opening it runs no DDL; a db
/// below it (fresh, or from before the version existed) goes through
/// [`migrate_fswalk_schema`]. Bump it, and extend the migration, with any schema
/// change.
///
/// What an older binary does with a newer db: one from before the version
/// existed ignores it and re-runs its own `CREATE TABLE IF NOT EXISTS`, a
/// no-op; a version-1 binary reads 2 as current (its check is `>=`) and skips
/// its migration. Either one then reads and writes the tables as they are,
/// which the version-2 rows below are built to survive.
///
/// - 1: `user_version` stamped; both tables with `listing_version`.
/// - 2: `ctime_ns` and `ino` on both tables, and rows are written only once
///   settled ([`RACY_WINDOW`]). The migration deletes every row: one written
///   before it was validated by mtime alone and may have been racy, so none of
///   them can be trusted. The columns are nullable and a row with either one
///   NULL never validates. An older binary's `INSERT OR REPLACE` names
///   neither column, so a row it writes into a version-2 db — new, or over a
///   version-2 row — has both NULL and is re-read by this binary rather than
///   trusted; an older binary itself reads a version-2 row by mtime and size,
///   as it read its own.
const FSWALK_SCHEMA_VERSION: i64 = 2;

/// How long a pooled fswalk read waits for a connection — see [`FsWalkStore::read_pool`].
const FSWALK_POOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the open-time schema check waits for its pooled read connection.
///
/// Nothing else holds a connection yet, and opening a read-only connection
/// takes no lock, so the only way to wait at all is an open that fails — which
/// r2d2 retries until the timeout, failing nothing but stalling the first walk.
const SCHEMA_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Bring the fswalk db up to [`FSWALK_SCHEMA_VERSION`] on `write`.
///
/// Serialized across processes on an exclusive `flock` of
/// `<db>-migrate.lock` (bounded by [`MIGRATE_LOCK_TIMEOUT`]), re-reading the
/// version under it, because switching a
/// fresh db to WAL takes its exclusive lock without the busy handler: two
/// processes creating it at once failed one of them (which then ran with the
/// durable cache disabled). DDL and version stamp commit in one transaction.
///
/// Returns how many rows the migration dropped, or `None` when the db was
/// already current under the lock (another process migrated it first).
fn migrate_fswalk_schema(db_path: &Path, write: &Connection) -> Result<Option<usize>> {
    let _lock = crate::migrate_lock::lock_migration(db_path, MIGRATE_LOCK_TIMEOUT)
        .context("serializing the fswalk schema migration")?;
    let version: i64 = write
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("re-reading the fswalk schema version")?;
    if version >= FSWALK_SCHEMA_VERSION {
        return Ok(None);
    }
    write
        .execute_batch("PRAGMA journal_mode = WAL;")
        .context("switching the fswalk db to WAL")?;
    write
        .execute_batch(
            "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS dirs (
                     path        TEXT PRIMARY KEY,
                     mtime_ns    INTEGER NOT NULL,
                     entries     BLOB NOT NULL,
                     accessed_ns INTEGER NOT NULL,
                     listing_version INTEGER NOT NULL DEFAULT 0,
                     ctime_ns    INTEGER,
                     ino         INTEGER
                 );
                 CREATE TABLE IF NOT EXISTS files (
                     path        TEXT PRIMARY KEY,
                     size        INTEGER NOT NULL,
                     mtime_ns    INTEGER NOT NULL,
                     exec        INTEGER NOT NULL,
                     hashout     TEXT NOT NULL,
                     accessed_ns INTEGER NOT NULL,
                     ctime_ns    INTEGER,
                     ino         INTEGER
                 );",
        )
        .context("initialising fswalk schema")?;
    // Columns a db created by an older version lacks. Probed rather than
    // attempted-and-ignored: on a fresh db the `CREATE TABLE` above already
    // declares them, so an unconditional `ALTER` fails every time and
    // swallowing that failure would also swallow a real one — and a real one
    // leaves `get_*`/`put_*` unable to prepare, which disables the durable
    // cache silently and forever.
    for (table, column, decl) in [
        ("dirs", "listing_version", "INTEGER NOT NULL DEFAULT 0"),
        ("dirs", "ctime_ns", "INTEGER"),
        ("dirs", "ino", "INTEGER"),
        ("files", "ctime_ns", "INTEGER"),
        ("files", "ino", "INTEGER"),
    ] {
        if !has_column(write, table, column)? {
            write
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))
                .with_context(|| format!("adding {column} to the fswalk {table} table"))?;
        }
    }
    // Every row below version 2 was stored by mtime alone, racy or not: none
    // can be trusted (see `FSWALK_SCHEMA_VERSION`). A one-time re-read.
    let mut dropped = 0;
    for table in ["dirs", "files"] {
        dropped += write
            .execute(&format!("DELETE FROM {table}"), [])
            .with_context(|| {
                format!("dropping the fswalk {table} rows written before schema version 2")
            })?;
    }
    write
        .execute_batch(&format!(
            "PRAGMA user_version = {FSWALK_SCHEMA_VERSION}; COMMIT;"
        ))
        .context("committing the fswalk schema")?;
    if dropped > 0 {
        tracing::info!(
            db = %db_path.display(),
            from_version = version,
            to_version = FSWALK_SCHEMA_VERSION,
            rows_dropped = dropped,
            "fswalk cache migrated: dropped rows are re-read once; cache keys do not move",
        );
    }
    Ok(Some(dropped))
}

/// Whether fswalk `table` has `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut cols = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("inspecting the fswalk {table} schema"))?;
    let found = cols
        .query_map([], |r| r.get::<_, String>(1))
        .with_context(|| format!("reading the fswalk {table} columns"))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    Ok(found)
}

impl FsWalkStore {
    /// `(key, entries, accessed_ns)` of a cached listing. A row with no
    /// `ctime_ns` or `ino` (an older binary wrote it) is not one.
    fn get_dir(&self, path: &Path) -> Option<(StatKey, Vec<u8>, i64)> {
        let conn = self.read_pool.get().ok()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT mtime_ns, ctime_ns, ino, entries, accessed_ns FROM dirs \
                 WHERE path = ?1 AND listing_version = ?2 \
                 AND ctime_ns IS NOT NULL AND ino IS NOT NULL",
            )
            .ok()?;
        stmt.query_row(
            rusqlite::params![path_key(path), DIR_LISTING_VERSION],
            |r| {
                Ok((
                    StatKey {
                        mtime_ns: r.get(0)?,
                        ctime_ns: r.get(1)?,
                        ino: r.get::<_, i64>(2)?.cast_unsigned(),
                    },
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .ok()
    }

    fn put_dir(&self, path: &Path, key: &StatKey, entries: &[u8], now: i64) {
        let Some(conn) = self.writer() else {
            return;
        };
        // `prepare_cached`, not `execute`: this runs once per walked directory
        // under the single write mutex, and `execute` re-parses and re-plans the
        // statement every time — with concurrent discovery, every cache miss
        // queues behind that parse. The connection is WAL with
        // `synchronous = OFF`, so the implicit commit is a WAL append and it is
        // the parse, not the transaction, that is worth removing.
        let Ok(mut stmt) = conn.prepare_cached(
            "INSERT OR REPLACE INTO dirs \
             (path, mtime_ns, ctime_ns, ino, entries, accessed_ns, listing_version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        ) else {
            return;
        };
        drop(stmt.execute(rusqlite::params![
            path_key(path),
            key.mtime_ns,
            key.ctime_ns,
            key.ino.cast_signed(),
            entries,
            now,
            DIR_LISTING_VERSION
        ]));
    }

    /// A cached file hash and its `accessed_ns`. A row with no `ctime_ns` or
    /// `ino` (an older binary wrote it) is not one.
    fn get_file(&self, path: &Path) -> Option<(FileHash, i64)> {
        let conn = self.read_pool.get().ok()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT size, mtime_ns, ctime_ns, ino, exec, hashout, accessed_ns FROM files \
                 WHERE path = ?1 AND ctime_ns IS NOT NULL AND ino IS NOT NULL",
            )
            .ok()?;
        stmt.query_row([path_key(path)], |r| {
            Ok((
                FileHash {
                    size: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                    mtime_ns: r.get(1)?,
                    ctime_ns: r.get(2)?,
                    ino: r.get::<_, i64>(3)?.cast_unsigned(),
                    exec: r.get::<_, i64>(4)? != 0,
                    hashout: r.get(5)?,
                },
                r.get(6)?,
            ))
        })
        .ok()
    }

    fn put_file(&self, path: &Path, fh: &FileHash, now: i64) {
        let Some(conn) = self.writer() else {
            return;
        };
        // Prepared for the same reason as `put_dir`, and more so: this runs once
        // per hashed file, not per directory.
        let Ok(mut stmt) = conn.prepare_cached(
            "INSERT OR REPLACE INTO files \
             (path, size, mtime_ns, ctime_ns, ino, exec, hashout, accessed_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        ) else {
            return;
        };
        drop(stmt.execute(rusqlite::params![
            path_key(path),
            i64::try_from(fh.size).unwrap_or(i64::MAX),
            fh.mtime_ns,
            fh.ctime_ns,
            fh.ino.cast_signed(),
            fh.exec as i64,
            fh.hashout,
            now
        ]));
    }

    /// Refresh last-access for a batch of paths in `table` (chunked under the
    /// sqlite variable limit). Best-effort.
    fn touch(&self, table: &str, paths: &[String], now: i64) {
        let Some(conn) = self.writer() else {
            return;
        };
        for chunk in paths.chunks(400) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("UPDATE {table} SET accessed_ns = ? WHERE path IN ({placeholders})");
            let Ok(mut stmt) = conn.prepare(&sql) else {
                continue;
            };
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
            params.push(&now);
            for p in chunk {
                params.push(p);
            }
            drop(stmt.execute(params.as_slice()));
        }
    }

    fn prune(&self, ttl: std::time::Duration, check_orphans: bool) -> Result<usize> {
        let cutoff = now_ns().saturating_sub(i64::try_from(ttl.as_nanos()).unwrap_or(i64::MAX));
        let conn = self
            .writer()
            .context("pruning the fswalk cache: its db could not be opened for writing")?;
        let mut removed = 0usize;
        removed += conn
            .execute("DELETE FROM dirs WHERE accessed_ns < ?1", [cutoff])
            .context("pruning stale fswalk dirs")?;
        removed += conn
            .execute("DELETE FROM files WHERE accessed_ns < ?1", [cutoff])
            .context("pruning stale fswalk files")?;

        if check_orphans {
            for table in ["dirs", "files"] {
                let paths: Vec<String> = {
                    let mut stmt = conn.prepare(&format!("SELECT path FROM {table}"))?;
                    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                    rows.filter_map(|r| r.ok())
                        .filter(|p| !Path::new(p).exists())
                        .collect()
                };
                for chunk in paths.chunks(400) {
                    let placeholders = std::iter::repeat_n("?", chunk.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("DELETE FROM {table} WHERE path IN ({placeholders})");
                    let params: Vec<&dyn rusqlite::ToSql> =
                        chunk.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                    removed += conn.execute(&sql, params.as_slice()).unwrap_or(0);
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use tempfile::tempdir;

    /// A walker on `<dir>/fswalk.db` whose clock runs an hour ahead, so the
    /// files a test has just written are settled and get cached — what most
    /// tests here are about. The racy tests drive the clock themselves.
    fn walker(dir: &Path) -> CachedWalker {
        CachedWalker::open(&dir.join("fswalk.db")).with_clock(|| now_ns() + 3_600_000_000_000)
    }

    fn racy_window_ns() -> i64 {
        i64::try_from(RACY_WINDOW.as_nanos()).unwrap()
    }

    /// A fixed mtime well in the past: settled against the real clock.
    fn old_mtime() -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000)
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        std::fs::File::open(path).unwrap().set_modified(at).unwrap();
    }

    fn rows(db: &Path, table: &str) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn names(listing: &DirListing) -> Vec<&str> {
        listing.entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn table_exists(db: &Path, name: &str) -> bool {
        Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }

    fn fswalk_version(db: &Path) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// `bytes_hashout` over bytes in memory is `file_hashout` over a file
    /// holding them, for either exec bit — including a body past one read
    /// chunk, and an empty one.
    #[test]
    fn bytes_hashout_matches_file_hashout() {
        let tmp = tempdir().unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        for (i, bytes) in [&b""[..], b"hello\n", &big].into_iter().enumerate() {
            let f = tmp.path().join(format!("f{i}"));
            std::fs::write(&f, bytes).unwrap();
            for x in [false, true] {
                assert_eq!(
                    bytes_hashout(bytes, x),
                    file_hashout(&f, x).unwrap(),
                    "body {i}, x={x}"
                );
            }
        }
        assert_ne!(bytes_hashout(b"a", false), bytes_hashout(b"a", true));
    }

    /// The hashout of a file over several read chunks is pinned: it reaches
    /// every input's hashin, so a change to how the file is read must not
    /// move it.
    #[test]
    fn file_hashout_over_several_chunks_is_pinned() {
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("big");
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&f, &body).unwrap();
        assert_eq!(file_hashout(&f, false).unwrap(), "c25fbb19f343cf66");
        assert_eq!(file_hashout(&f, true).unwrap(), "82a2c1fe43996d65");
    }

    /// The content hash feeds cache keys and every recorded hashout, so its
    /// bytes are pinned: xxh3-64 over the content then one exec-bit byte, in
    /// lowercase hex without padding, however the content is chunked.
    #[test]
    fn content_hasher_bytes_are_pinned() {
        for (x, want) in [(false, "3a944c9bd3adc9bc"), (true, "755faaf580db378f")] {
            let mut h = ContentHasher::new();
            h.update(b"hel");
            h.update(b"");
            h.update(b"lo\n");
            let got = h.finish(x);
            let mut oneshot = b"hello\n".to_vec();
            oneshot.push(u8::from(x));
            assert_eq!(got, format!("{:x}", xxhash_rust::xxh3::xxh3_64(&oneshot)));
            assert_eq!(got, want, "x={x}");
        }
    }

    /// Opening a walker opens no database, and a walker that never walked
    /// writes nothing at drop either.
    #[test]
    fn the_store_opens_on_first_use_not_at_construction() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let w = walker(tmp.path());
        assert!(!w.store_opened());
        drop(w);
        assert!(!db.exists(), "an unused walker must not create its db");

        let w = walker(tmp.path());
        w.read_dir(tmp.path()).unwrap();
        assert!(w.store_opened());
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);
    }

    /// At the current version opening runs no DDL (a table dropped by hand stays
    /// dropped); below it the DDL runs again and stamps the version.
    #[test]
    fn fswalk_ddl_runs_only_when_the_schema_version_is_behind() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        walker(tmp.path()).read_dir(tmp.path()).unwrap();

        Connection::open(&db)
            .unwrap()
            .execute_batch("DROP TABLE files;")
            .unwrap();
        walker(tmp.path()).read_dir(tmp.path()).unwrap();
        assert!(
            !table_exists(&db, "files"),
            "DDL must be skipped when current"
        );

        Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA user_version = 0;")
            .unwrap();
        walker(tmp.path()).read_dir(tmp.path()).unwrap();
        assert!(table_exists(&db, "files"), "DDL must run on an older db");
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);
    }

    fn dir_accessed_ns(db: &Path, dir: &Path) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT accessed_ns FROM dirs WHERE path = ?1",
                [path_key(dir)],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A warm run over an unchanged tree whose access stamps are fresh writes
    /// nothing: it never even opens the read-write connection, so its exit pays
    /// no checkpoint or WAL unlink. A stamp older than `TOUCH_REFRESH` is
    /// still refreshed, so pruning keeps seeing rows that are in use.
    #[test]
    fn a_warm_read_only_walk_writes_nothing_until_a_stamp_goes_stale() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let tree = tmp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("f"), b"x").unwrap();
        // Cold: the listing is written.
        walker(tmp.path()).read_dir(&tree).unwrap();
        let first = dir_accessed_ns(&db, &tree);

        // Warm, fresh stamp: served from the store, nothing written.
        let w = walker(tmp.path());
        w.read_dir(&tree).unwrap();
        assert!(
            !w.store().unwrap().write_opened(),
            "a read-only warm walk must not open the write connection"
        );
        drop(w);
        assert_eq!(
            dir_accessed_ns(&db, &tree),
            first,
            "fresh stamp not rewritten"
        );

        // Stale stamp: refreshed at drop.
        let stale = first - i64::try_from(TOUCH_REFRESH.as_nanos()).unwrap() - 1;
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE dirs SET accessed_ns = ?1 WHERE path = ?2",
                rusqlite::params![stale, path_key(&tree)],
            )
            .unwrap();
        walker(tmp.path()).read_dir(&tree).unwrap();
        assert!(
            dir_accessed_ns(&db, &tree) >= first,
            "a stale stamp must be refreshed"
        );
    }

    #[test]
    fn touch_staleness_boundaries() {
        let refresh = i64::try_from(TOUCH_REFRESH.as_nanos()).unwrap();
        let now = 1_000 * refresh;
        assert!(!touch_is_stale(now, now));
        assert!(!touch_is_stale(now - refresh + 1, now));
        assert!(touch_is_stale(now - refresh, now));
        // A stamp from the future is refreshed rather than trusted.
        assert!(touch_is_stale(now + 1, now));
    }

    fn file_accessed_ns(db: &Path, file: &Path) -> i64 {
        Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT accessed_ns FROM files WHERE path = ?1",
                [path_key(file)],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn set_file_accessed_ns(db: &Path, file: &Path, ns: i64) {
        Connection::open(db)
            .unwrap()
            .execute(
                "UPDATE files SET accessed_ns = ?1 WHERE path = ?2",
                rusqlite::params![ns, path_key(file)],
            )
            .unwrap();
    }

    /// The `file_hash` → `touched_files` → drop path: a fresh stamp served from
    /// the store is neither queued nor rewritten (and opens no write
    /// connection); a stamp older than `TOUCH_REFRESH`, or one in the future,
    /// is queued and refreshed to now when the walker drops.
    #[test]
    fn file_hash_refreshes_only_a_stale_or_future_stamp() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        let refresh = i64::try_from(TOUCH_REFRESH.as_nanos()).unwrap();
        // Cold: hashed and written.
        walker(tmp.path()).file_hash(&file).unwrap();
        let first = file_accessed_ns(&db, &file);

        // Fresh: served from the store, nothing queued, nothing written.
        let w = walker(tmp.path());
        w.file_hash(&file).unwrap();
        assert!(w.touched_files.lock().is_empty(), "a fresh stamp is queued");
        assert!(!w.store().unwrap().write_opened());
        drop(w);
        assert_eq!(file_accessed_ns(&db, &file), first, "fresh stamp rewritten");

        // Stale, then future: each is queued, and refreshed at drop.
        for (what, stamp) in [
            ("stale", first - refresh - 1),
            ("future", first + 10 * refresh),
        ] {
            set_file_accessed_ns(&db, &file, stamp);
            let w = walker(tmp.path());
            w.file_hash(&file).unwrap();
            assert_eq!(w.touched_files.lock().len(), 1, "{what} stamp not queued");
            drop(w);
            let now = now_ns();
            let got = file_accessed_ns(&db, &file);
            assert!(
                (first..=now).contains(&got),
                "{what} stamp {stamp} not refreshed to now: {got} (first {first}, now {now})"
            );
        }
    }

    /// A db file that is not a database fails the store's open promptly, with
    /// an error naming the db — no 30 s pool retry, no lock wait — and the
    /// walker degrades to reading from disk.
    #[test]
    fn a_corrupt_db_fails_the_open_promptly_naming_it_and_the_walker_still_reads() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        std::fs::write(&db, b"definitely not a sqlite database\n".repeat(64)).unwrap();
        std::fs::write(tmp.path().join("f"), b"x").unwrap();

        let start = std::time::Instant::now();
        let Err(err) = FsWalkStore::open(&db) else {
            panic!("a garbage db must not open")
        };
        let took = start.elapsed();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(took < FSWALK_POOL_TIMEOUT, "took {took:?}: {msg}");

        let w = walker(tmp.path());
        let listing = w.read_dir(tmp.path()).unwrap();
        assert!(listing.entries.iter().any(|e| e.name == "f"));
        assert!(w.store().is_none(), "the store must be disabled");
    }

    /// A db this process cannot open at all fails the store's open within the
    /// schema check's bound, naming the db. r2d2 retries a failing connection
    /// open until its timeout, so with the pool's default this took 30 s.
    #[test]
    fn an_unopenable_db_fails_the_open_promptly_naming_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        walker(tmp.path()).read_dir(tmp.path()).unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&db).is_ok() {
            eprintln!("skipping: a mode-000 file is readable here (running as root?)");
            return;
        }
        let start = std::time::Instant::now();
        let res = FsWalkStore::open(&db);
        let took = start.elapsed();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        let Err(err) = res else {
            panic!("an unopenable db must not open")
        };
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(took < FSWALK_POOL_TIMEOUT, "took {took:?}: {msg}");
    }

    /// A db in a directory this process cannot write fails the open promptly,
    /// naming the db's path.
    #[test]
    fn an_unwritable_cache_dir_fails_the_open_promptly_naming_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempdir().unwrap();
        let ro = tmp.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(ro.join("probe"), b"").is_ok() {
            eprintln!("skipping: the read-only dir is writable here (running as root?)");
            return;
        }
        let db = ro.join("fswalk.db");
        let start = std::time::Instant::now();
        let Err(err) = FsWalkStore::open(&db) else {
            panic!("an unwritable dir must not open")
        };
        let took = start.elapsed();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(took < FSWALK_POOL_TIMEOUT, "took {took:?}: {msg}");
        // Let the tempdir clean up.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The DDL batch the previous binary (before `user_version`, 372319d9) ran
    /// on every open, verbatim. It then probed `listing_version` and skipped
    /// its `ALTER`, since a current db has the column.
    const PRE_VERSIONING_FSWALK_DDL: &str = "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 10000;
                 PRAGMA synchronous = OFF;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA mmap_size = 268435456;
                 CREATE TABLE IF NOT EXISTS dirs (
                     path        TEXT PRIMARY KEY,
                     mtime_ns    INTEGER NOT NULL,
                     entries     BLOB NOT NULL,
                     accessed_ns INTEGER NOT NULL,
                     listing_version INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE IF NOT EXISTS files (
                     path        TEXT PRIMARY KEY,
                     size        INTEGER NOT NULL,
                     mtime_ns    INTEGER NOT NULL,
                     exec        INTEGER NOT NULL,
                     hashout     TEXT NOT NULL,
                     accessed_ns INTEGER NOT NULL
                 );";

    /// A current db opened by the previous binary in between (a downgrade and
    /// back) keeps its version and its rows, and the next open migrates
    /// nothing: it never takes the migration lock.
    #[test]
    fn a_db_reopened_by_the_previous_binary_is_still_current() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let tree = tmp.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("f"), b"x").unwrap();
        walker(tmp.path()).read_dir(&tree).unwrap();
        let first = dir_accessed_ns(&db, &tree);

        Connection::open(&db)
            .unwrap()
            .execute_batch(PRE_VERSIONING_FSWALK_DDL)
            .unwrap();
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);

        let lock = crate::migrate_lock::migrate_lock_path(&db);
        std::fs::remove_file(&lock).unwrap();
        let w = walker(tmp.path());
        let listing = w.read_dir(&tree).unwrap();
        assert!(listing.entries.iter().any(|e| e.name == "f"));
        assert!(
            !w.store().unwrap().write_opened(),
            "served from the store, no migration"
        );
        drop(w);
        assert!(!lock.exists(), "a current db must not be migrated");
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);
        assert_eq!(dir_accessed_ns(&db, &tree), first, "row intact");
    }

    /// Walkers in several processes creating the same fresh db at once all get
    /// a working durable store. Switching to WAL skips the busy handler, so
    /// without the migration lock one of them lost the store.
    #[test]
    fn concurrent_first_opens_of_a_fresh_fswalk_db_all_get_a_store() {
        const N: usize = 8;
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let barrier = Arc::new(std::sync::Barrier::new(N));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let (root, barrier) = (root.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    FsWalkStore::open(&root.join("fswalk.db"))
                        .map(drop)
                        .map_err(|e| format!("{e:#}"))
                })
            })
            .collect();
        for t in threads {
            if let Err(e) = t.join().unwrap() {
                panic!("every walker must open the durable store: {e}");
            }
        }
    }

    /// Create a file whose name is the raw byte sequence `raw`, or `None` when
    /// the filesystem refuses it. APFS rejects names that are not valid UTF-8
    /// with `EILSEQ`, so this fixture is unconstructible on
    /// `aarch64-apple-darwin` — the caller must skip loudly rather than assert
    /// on a file it never created.
    fn try_create_non_utf8_file(dir: &Path, raw: &[u8]) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let path = dir.join(std::ffi::OsStr::from_bytes(raw));
        match std::fs::write(&path, b"") {
            Ok(()) => Some(path),
            Err(e) => {
                // macOS is the only supported target where this is expected. On
                // Linux — the one platform that exercises the fix at all — a
                // refusal means `$TMPDIR` is on a filesystem that cannot host
                // the fixture, and skipping would turn the only real coverage
                // into a silent green.
                assert!(
                    cfg!(target_os = "macos"),
                    "the non-UTF-8 fixture must be constructible on this target, \
                     but {dir:?} refused it: {e}"
                );
                eprintln!(
                    "SKIP: this filesystem refuses non-UTF-8 file names ({e}); \
                     the non-UTF-8 fixture cannot be built here"
                );
                None
            }
        }
    }

    #[test]
    fn read_dir_rejects_a_non_utf8_entry_name() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("pkg");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("ok.txt"), b"").unwrap();
        let Some(_bad) = try_create_non_utf8_file(&dir, b"caf\xe9.txt") else {
            return;
        };

        let w = walker(tmp.path());
        let err = w
            .read_dir(&dir)
            .expect_err("a non-UTF-8 entry name must fail the walk, not vanish from the listing");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid UTF-8"),
            "error should name the problem, got: {msg}"
        );
        assert!(
            msg.contains("pkg"),
            "error should name the directory, got: {msg}"
        );
    }

    /// Two names that differ only in bytes `to_string_lossy` would both render
    /// as `U+FFFD`. Silently dropping them (the old behavior) hid two real
    /// paths; rendering them lossily would have fused them into one. Neither is
    /// acceptable for something that ends up in a package name and a db key.
    #[test]
    fn read_dir_rejects_names_that_would_collide_under_lossy_rendering() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("pkg");
        std::fs::create_dir(&dir).unwrap();
        let Some(_a) = try_create_non_utf8_file(&dir, b"x\xffy") else {
            return;
        };
        let Some(_b) = try_create_non_utf8_file(&dir, b"x\xfey") else {
            return;
        };
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "the two names must be distinct on disk for this test to mean anything"
        );

        let w = walker(tmp.path());
        let err = w
            .read_dir(&dir)
            .expect_err("names that render to the same lossy string must not reach a listing");
        // The message must distinguish the two, or it sends the user to rename
        // a file they cannot tell from its neighbour. `\u{FFFD}` in the text
        // would mean the lossy render leaked into the one place precision is
        // the whole point.
        let msg = format!("{err:#}");
        assert!(
            msg.contains(r"\xFF") || msg.contains(r"\xFE"),
            "error must name the offending bytes escaped, got: {msg}"
        );
        assert!(
            !msg.contains('\u{FFFD}'),
            "error must not render the name lossily, got: {msg}"
        );
    }

    /// A `dirs` row is revalidated by directory mtime alone, so a listing
    /// written under older filtering rules is byte-identical-valid to the mtime
    /// guard. Only [`DIR_LISTING_VERSION`] stops it being served — without it
    /// the entry-name fix is inert on every warm cache.
    #[test]
    fn a_listing_cached_under_an_older_version_is_not_reused() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let pkg = root.join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"a").unwrap();

        let w = walker(root);
        assert_eq!(w.read_dir(&pkg).unwrap().entries.len(), 1);
        drop(w);

        // Rewrite the cached row the way a pre-fix binary would have left it:
        // same path, same mtime, but a listing missing an entry.
        let conn = Connection::open(root.join("fswalk.db")).unwrap();
        let truncated = borsh::to_vec(&DirListing::default()).unwrap();
        let updated = conn
            .execute(
                "UPDATE dirs SET entries = ?1, listing_version = 0 WHERE path = ?2",
                rusqlite::params![truncated, pkg.to_str().unwrap()],
            )
            .unwrap();
        assert_eq!(updated, 1, "the first walk should have cached one dirs row");
        drop(conn);

        let w2 = walker(root);
        let listing = w2.read_dir(&pkg).unwrap();
        assert_eq!(
            listing.entries.len(),
            1,
            "a listing cached under an older listing version must be re-read, not served"
        );
    }

    /// The `ALTER TABLE` path is never taken on a db this suite creates (the
    /// `CREATE TABLE` already declares the column), so without this test the
    /// migration every existing user's db actually takes is the one line no
    /// test executes. Builds the pre-migration schema by hand and asserts the
    /// walker both works on it and writes a current row.
    #[test]
    fn a_pre_migration_db_is_upgraded_in_place() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let pkg = root.join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"a").unwrap();

        let db = root.join("fswalk.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE dirs (
                 path        TEXT PRIMARY KEY,
                 mtime_ns    INTEGER NOT NULL,
                 entries     BLOB NOT NULL,
                 accessed_ns INTEGER NOT NULL
             );",
        )
        .unwrap();
        drop(conn);

        let w = walker(root);
        assert_eq!(w.read_dir(&pkg).unwrap().entries.len(), 1);
        drop(w);

        let conn = Connection::open(&db).unwrap();
        let (version, ctime_ns, ino): (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT listing_version, ctime_ns, ino FROM dirs WHERE path = ?1",
                [pkg.to_str().unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("the migrated db must hold a row for the walked dir");
        assert_eq!(
            version, DIR_LISTING_VERSION,
            "the migrated column must be written with the current version"
        );
        assert!(
            ctime_ns.is_some() && ino.is_some(),
            "the migrated key columns must be written"
        );
    }

    /// The inverse of the test above, and the reason it is worth having: a
    /// version predicate that never matches is *invisibly* correct — every
    /// directory quietly falls back to disk and the durable cache stops
    /// existing. Nothing else asserts a durable **hit** (equal content passes
    /// whether the row was served or re-read), so a future
    /// [`DIR_LISTING_VERSION`] written by `put_dir` and not accepted by
    /// `get_dir` would cost the whole dir cache with nothing going red.
    #[test]
    fn a_current_listing_is_served_from_the_durable_cache() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let pkg = root.join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"a").unwrap();

        let w = walker(root);
        assert_eq!(w.read_dir(&pkg).unwrap().entries.len(), 1);
        drop(w);

        // Empty the stored listing behind the cache's back, its key untouched,
        // so only a walker that served the row sees no entries. (Changing the
        // directory itself cannot do this: its ctime moves with any change.)
        let updated = Connection::open(root.join("fswalk.db"))
            .unwrap()
            .execute(
                "UPDATE dirs SET entries = ?1 WHERE path = ?2",
                rusqlite::params![
                    borsh::to_vec(&DirListing::default()).unwrap(),
                    path_key(&pkg)
                ],
            )
            .unwrap();
        assert_eq!(updated, 1);

        let w2 = walker(root);
        assert_eq!(
            w2.read_dir(&pkg).unwrap().entries.len(),
            0,
            "the durable listing was not reused — the dirs cache is not being hit at all"
        );
    }

    #[test]
    fn read_dir_caches_and_revalidates() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg").join("a.txt"), b"a").unwrap();

        let w = walker(root);
        let l1 = w.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l1.entries.len(), 1);
        assert_eq!(l1.entries[0].name, "a.txt");
        assert_eq!(l1.entries[0].kind, EntryKind::File);

        // A fresh walker sharing the db reloads the listing without re-reading
        // (we can't observe the readdir directly, but the content must match).
        drop(w);
        let w2 = walker(root);
        let l2 = w2.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l2.entries.len(), 1);

        // Add a file → dir mtime bumps → listing refreshes.
        std::fs::write(root.join("pkg").join("b.txt"), b"b").unwrap();
        std::fs::File::open(root.join("pkg"))
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();
        let l3 = w2.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l3.entries.len(), 2);
    }

    #[test]
    fn missing_dir_lists_empty() {
        let tmp = tempdir().unwrap();
        let w = walker(tmp.path());
        let l = w.read_dir(&tmp.path().join("nope")).unwrap();
        assert!(l.entries.is_empty());
    }

    #[test]
    fn file_hash_caches_and_revalidates() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let f = root.join("f");
        std::fs::write(&f, b"hello").unwrap();

        let w = walker(root);
        let h1 = w.file_hash(&f).unwrap();
        // Matches the direct hash.
        assert_eq!(h1.hashout, file_hashout(&f, h1.exec).unwrap());

        // Fresh walker reloads the same hash from the db.
        drop(w);
        let w2 = walker(root);
        let h2 = w2.file_hash(&f).unwrap();
        assert_eq!(h1.hashout, h2.hashout);

        // Content change (different size) → re-hash, different value.
        std::fs::write(&f, b"a different, longer body").unwrap();
        let h3 = w2.file_hash(&f).unwrap();
        assert_ne!(h1.hashout, h3.hashout);
    }

    #[test]
    fn prune_drops_stale_rows() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        let w = walker(root);
        w.read_dir(&root.join("d")).unwrap();
        w.flush_touches();

        // TTL of zero ⇒ everything is stale ⇒ pruned.
        let removed = w.prune(std::time::Duration::from_secs(0), false).unwrap();
        assert!(removed >= 1, "stale dir row pruned");
    }

    #[test]
    fn prune_drops_orphaned_rows() {
        let tmp = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let gone = tmp.path().join("gone");
        std::fs::create_dir(&gone).unwrap();

        let w = walker(dbdir.path());
        w.read_dir(&gone).unwrap();

        // Delete the directory, then prune with a long TTL: the row survives TTL
        // (just written) but is removed as an orphan (its path no longer exists).
        std::fs::remove_dir_all(&gone).unwrap();
        let removed = w.prune(DEFAULT_TTL, true).unwrap();
        assert!(removed >= 1, "orphaned dir row pruned");
        // And a second prune finds nothing.
        assert_eq!(w.prune(DEFAULT_TTL, true).unwrap(), 0);
    }

    #[test]
    fn disabled_walker_still_reads() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d").join("x"), b"").unwrap();
        let w = CachedWalker::disabled();
        let l = w.read_dir(&tmp.path().join("d")).unwrap();
        assert_eq!(l.entries.len(), 1);
    }

    /// A disabled walker whose stat is the real one except for the ctime,
    /// which reads `ctime` (see [`pinned_ctime_walker`]), and whose clock
    /// reads `clock()`.
    fn disabled_pinned_walker(
        ctime: &Arc<AtomicI64>,
        clock: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> CachedWalker {
        let ctime = Arc::clone(ctime);
        CachedWalker::disabled()
            .with_clock(clock)
            .with_stat(move |p| {
                let mut st = stat_path(p)?;
                if let Some(key) = &mut st.key {
                    key.ctime_ns = ctime.load(Ordering::Relaxed);
                }
                Ok(st)
            })
    }

    /// Rewrite `f` with `body`, and add `extra` to `pkg`, leaving every
    /// stat key the walker sees as it was: the mtimes are set back to
    /// [`old_mtime`] and the ctime is pinned by the caller.
    fn rewrite_behind_the_key(pkg: &Path, f: &Path, body: &[u8], extra: &str) {
        std::fs::write(f, body).unwrap();
        std::fs::write(pkg.join(extra), b"").unwrap();
        set_mtime(f, old_mtime());
        set_mtime(pkg, old_mtime());
    }

    /// A walker with no durable store still keeps a settled read in its
    /// in-process front: with every key unchanged, the second read is the
    /// first one's result, not a fresh read of the disk.
    #[test]
    fn disabled_walker_serves_a_settled_read_from_its_front() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = disabled_pinned_walker(&ctime, || now_ns() + 3_600_000_000_000);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );

        rewrite_behind_the_key(&pkg, &f, b"bbbb", "g");
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"], "from the front");
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false),
            "from the front"
        );

        // A moved key is still a miss.
        ctime.store(2, Ordering::Relaxed);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f", "g"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"bbbb", false)
        );
    }

    /// A read that is not settled is not kept in a disabled walker's front
    /// either: with every key unchanged, the next read goes to the disk.
    #[test]
    fn disabled_walker_does_not_keep_a_racy_read() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        // The clock sits at the mtime: every read is inside the racy window.
        let mtime_ns =
            i64::try_from(old_mtime().duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap();
        let w = disabled_pinned_walker(&ctime, move || mtime_ns);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );

        rewrite_behind_the_key(&pkg, &f, b"bbbb", "g");
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f", "g"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"bbbb", false)
        );
    }

    #[test]
    fn bypassing_walker_reads_disk_without_caching() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        let f = root.join("d").join("x");
        std::fs::write(&f, b"hello").unwrap();

        let w = CachedWalker::bypassing();
        // Reads and hashes correctly, matching the uncached primitives.
        let l = w.read_dir(&root.join("d")).unwrap();
        assert_eq!(l.entries.len(), 1);
        let h = w.file_hash(&f).unwrap();
        assert_eq!(h.hashout, file_hashout(&f, h.exec).unwrap());

        // Bypass keeps no in-process front: a content change is seen immediately,
        // with no stale cached value (and the dir's mtime is never consulted).
        std::fs::write(&f, b"a different, longer body").unwrap();
        let h2 = w.file_hash(&f).unwrap();
        assert_ne!(h.hashout, h2.hashout);

        // No durable store ⇒ nothing to prune.
        assert_eq!(
            w.prune(std::time::Duration::from_secs(0), false).unwrap(),
            0
        );
    }

    /// The walker must stay correct with many threads writing at once.
    ///
    /// Every miss goes through one write mutex, so concurrent discovery — the
    /// point of the phase this precedes — turns this into the contended path for
    /// the whole build. `execute` re-parsed and re-planned the statement on every
    /// call while holding that mutex; `prepare_cached` reuses it.
    ///
    /// Asserted on behaviour rather than on statement reuse, which has no
    /// observable seam: every thread must get the right listing and the right
    /// hash whichever order they interleave in, and a second pass must agree with
    /// the first now that it is reading back what the concurrent writes stored.
    #[test]
    fn concurrent_walks_agree_with_the_uncached_primitives() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for i in 0..8 {
            let d = root.join(format!("d{i}"));
            std::fs::create_dir(&d).unwrap();
            for j in 0..8 {
                std::fs::write(d.join(format!("f{j}")), format!("body {i} {j}")).unwrap();
            }
        }

        let w = Arc::new(walker(root));
        // Overlapping, not disjoint: threads contend on the same rows.
        let pass = |w: &CachedWalker| {
            for i in 0..8 {
                let d = root.join(format!("d{i}"));
                let listing = w.read_dir(&d).expect("read_dir");
                assert_eq!(listing.entries.len(), 8, "d{i} must list every file");
                for j in 0..8 {
                    let f = d.join(format!("f{j}"));
                    let h = w.file_hash(&f).expect("file_hash");
                    assert_eq!(
                        h.hashout,
                        file_hashout(&f, h.exec).unwrap(),
                        "cached hash must match the uncached primitive",
                    );
                }
            }
        };

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| pass(&w));
            }
        });
        // Second pass now reads back what the concurrent writes stored.
        pass(&w);
    }

    #[test]
    fn settled_boundaries() {
        let key = StatKey {
            mtime_ns: 1_000 * racy_window_ns(),
            ctime_ns: 0,
            ino: 1,
        };
        assert!(settled(&key, key.mtime_ns + racy_window_ns()));
        assert!(!settled(&key, key.mtime_ns + racy_window_ns() - 1));
        assert!(!settled(&key, key.mtime_ns));
        // An mtime ahead of the clock is never settled.
        assert!(!settled(&key, key.mtime_ns - 1));
        assert!(!settled(&key, i64::MIN));

        // An old mtime with a recent ctime (`tar -x`, `cp -p`, `rsync -a`
        // just wrote it) is judged by the ctime: the window runs from the
        // newer of the two, whichever it is.
        let restamped = StatKey {
            mtime_ns: 0,
            ctime_ns: 1_000 * racy_window_ns(),
            ino: 1,
        };
        assert!(settled(&restamped, restamped.ctime_ns + racy_window_ns()));
        assert!(!settled(
            &restamped,
            restamped.ctime_ns + racy_window_ns() - 1
        ));
        assert!(!settled(&restamped, restamped.ctime_ns - 1));
    }

    /// The key every path reports under [`fixed_key_walker`].
    const FIXED_KEY: StatKey = StatKey {
        mtime_ns: 1_700_000_000_000_000_000,
        ctime_ns: 1_700_000_000_000_000_000,
        ino: 7,
    };

    /// A walker on `<dir>/fswalk.db` whose stat reports [`FIXED_KEY`] for every
    /// path, whatever happens to it on disk, and whose clock reads `clock`. A
    /// change on disk is then invisible to validation, so whether a read sees
    /// it says whether the earlier read was cached.
    fn fixed_key_walker(dir: &Path, clock: &Arc<AtomicI64>) -> CachedWalker {
        let clock = Arc::clone(clock);
        CachedWalker::open(&dir.join("fswalk.db"))
            .with_clock(move || clock.load(Ordering::Relaxed))
            .with_stat(|p| {
                let mut st = stat_path(p)?;
                st.key = Some(FIXED_KEY);
                Ok(st)
            })
    }

    /// D30: a listing read less than `RACY_WINDOW` after its mtime is returned
    /// but cached nowhere — the next read goes to disk again — and nothing
    /// promotes it later: the first settled read re-reads and stores what it
    /// read itself.
    #[test]
    fn racy_read_dir_is_not_stored() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("a"), b"").unwrap();
        let clock = Arc::new(AtomicI64::new(FIXED_KEY.mtime_ns + racy_window_ns() - 1));
        let w = fixed_key_walker(tmp.path(), &clock);

        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["a"]);
        assert_eq!(rows(&db, "dirs"), 0, "a racy listing must not be stored");
        assert!(w.dirs.lock().is_empty(), "nor kept in-process");

        // Same key, new entry: only a re-read sees it.
        std::fs::write(pkg.join("b"), b"").unwrap();
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["a", "b"]);
        assert_eq!(
            names(&fixed_key_walker(tmp.path(), &clock).read_dir(&pkg).unwrap()),
            ["a", "b"]
        );

        // Settled: read from disk once more, then stored and served.
        clock.store(FIXED_KEY.mtime_ns + racy_window_ns(), Ordering::Relaxed);
        std::fs::write(pkg.join("c"), b"").unwrap();
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["a", "b", "c"]);
        assert_eq!(rows(&db, "dirs"), 1, "a settled listing is stored");
        std::fs::write(pkg.join("d"), b"").unwrap();
        assert_eq!(
            names(&w.read_dir(&pkg).unwrap()),
            ["a", "b", "c"],
            "a settled listing is served in-process"
        );
        assert_eq!(
            names(&fixed_key_walker(tmp.path(), &clock).read_dir(&pkg).unwrap()),
            ["a", "b", "c"],
            "and from the durable store"
        );
    }

    /// D30 for `file_hash`, which reaches input hashes: a same-size save within
    /// the tick must not file the new content under the old hash.
    #[test]
    fn racy_file_hash_is_not_stored() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let f = tmp.path().join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        let clock = Arc::new(AtomicI64::new(FIXED_KEY.mtime_ns + racy_window_ns() - 1));
        let w = fixed_key_walker(tmp.path(), &clock);
        let hash = |w: &CachedWalker| w.file_hash(&f).unwrap().hashout.clone();

        assert_eq!(hash(&w), bytes_hashout(b"aaaa", false));
        assert_eq!(rows(&db, "files"), 0, "a racy hash must not be stored");
        assert!(w.files.lock().is_empty(), "nor kept in-process");

        // Same size, same key, new bytes: only a re-hash sees them.
        std::fs::write(&f, b"bbbb").unwrap();
        assert_eq!(hash(&w), bytes_hashout(b"bbbb", false));
        assert_eq!(
            hash(&fixed_key_walker(tmp.path(), &clock)),
            bytes_hashout(b"bbbb", false)
        );

        // Settled: hashed from disk once more, then stored and served.
        clock.store(FIXED_KEY.mtime_ns + racy_window_ns(), Ordering::Relaxed);
        std::fs::write(&f, b"cccc").unwrap();
        assert_eq!(hash(&w), bytes_hashout(b"cccc", false));
        assert_eq!(rows(&db, "files"), 1, "a settled hash is stored");
        std::fs::write(&f, b"dddd").unwrap();
        assert_eq!(
            hash(&w),
            bytes_hashout(b"cccc", false),
            "a settled hash is served in-process"
        );
        assert_eq!(
            hash(&fixed_key_walker(tmp.path(), &clock)),
            bytes_hashout(b"cccc", false),
            "and from the durable store"
        );
    }

    /// A walker on `<dir>/fswalk.db` (clock an hour ahead) whose stat is the
    /// real one except for the ctime, which reads `ctime`. A test moves the
    /// ctime itself: a kernel that stamps ctime from a millisecond-tick clock
    /// (Linux without multigrain timestamps) can leave it unchanged across two
    /// operations a test makes back to back, and a test may not sleep.
    fn pinned_ctime_walker(dir: &Path, ctime: &Arc<AtomicI64>) -> CachedWalker {
        let ctime = Arc::clone(ctime);
        walker(dir).with_stat(move |p| {
            let mut st = stat_path(p)?;
            if let Some(key) = &mut st.key {
                key.ctime_ns = ctime.load(Ordering::Relaxed);
            }
            Ok(st)
        })
    }

    /// D33: a rewrite that keeps size and mtime (`cp -p`, `rsync -a`, `tar -x`
    /// in place) still moves the ctime, and that alone re-reads the file and
    /// the directory, in-process and from the durable store.
    #[test]
    fn a_new_ctime_with_the_same_size_and_mtime_is_reread() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = pinned_ctime_walker(tmp.path(), &ctime);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );

        std::fs::write(&f, b"bbbb").unwrap();
        std::fs::write(pkg.join("g"), b"").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        ctime.store(2, Ordering::Relaxed);

        let fresh = pinned_ctime_walker(tmp.path(), &ctime);
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f", "g"], "{what}");
            assert_eq!(
                w.file_hash(&f).unwrap().hashout,
                bytes_hashout(b"bbbb", false),
                "{what}"
            );
        }
    }

    /// D33: a replacement with the same size, mtime and ctime but a new inode
    /// (a file or directory renamed over the old one) is re-read.
    #[test]
    fn a_new_inode_with_the_same_size_and_mtime_is_reread() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = pinned_ctime_walker(tmp.path(), &ctime);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );
        let (old_file_ino, old_dir_ino) = (w.files.lock()[&f].ino, w.dirs.lock()[&pkg].0.ino);

        // The new directory holds the new file; the old one moves aside so
        // its inodes stay allocated and cannot be reused.
        let next = tmp.path().join("next");
        std::fs::create_dir(&next).unwrap();
        std::fs::write(next.join("f"), b"bbbb").unwrap();
        std::fs::write(next.join("g"), b"").unwrap();
        set_mtime(&next.join("f"), old_mtime());
        set_mtime(&next, old_mtime());
        std::fs::rename(&pkg, tmp.path().join("old")).unwrap();
        std::fs::rename(&next, &pkg).unwrap();
        set_mtime(&pkg, old_mtime());

        let fresh = pinned_ctime_walker(tmp.path(), &ctime);
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f", "g"], "{what}");
            assert_eq!(
                w.file_hash(&f).unwrap().hashout,
                bytes_hashout(b"bbbb", false),
                "{what}"
            );
        }
        assert_ne!(w.files.lock()[&f].ino, old_file_ino);
        assert_ne!(w.dirs.lock()[&pkg].0.ino, old_dir_ino);
    }

    /// D33: `chmod` leaves size and mtime alone but moves the ctime, so the
    /// exec bit folded into the hash is refreshed rather than served stale.
    #[test]
    fn chmod_refreshes_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("tool");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        set_mtime(&f, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = pinned_ctime_walker(tmp.path(), &ctime);
        let before = w.file_hash(&f).unwrap();
        assert!(!before.exec);

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        ctime.store(2, Ordering::Relaxed);

        let fresh = pinned_ctime_walker(tmp.path(), &ctime);
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            let after = w.file_hash(&f).unwrap();
            assert!(after.exec, "{what}");
            assert_eq!(after.hashout, file_hashout(&f, true).unwrap(), "{what}");
            assert_ne!(after.hashout, before.hashout, "{what}");
        }
    }

    /// The schema version 1 DDL, verbatim (`listing_version`, no key columns).
    const V1_FSWALK_DDL: &str = "PRAGMA journal_mode = WAL;
         CREATE TABLE dirs (
             path        TEXT PRIMARY KEY,
             mtime_ns    INTEGER NOT NULL,
             entries     BLOB NOT NULL,
             accessed_ns INTEGER NOT NULL,
             listing_version INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE files (
             path        TEXT PRIMARY KEY,
             size        INTEGER NOT NULL,
             mtime_ns    INTEGER NOT NULL,
             exec        INTEGER NOT NULL,
             hashout     TEXT NOT NULL,
             accessed_ns INTEGER NOT NULL
         );
         PRAGMA user_version = 1;";

    /// Write rows the way a schema-1 binary does — its `INSERT`s verbatim —
    /// for `dir` (listing nothing) and `file` (hashed as `stale`), each with
    /// its live size and mtime, so the mtime guard alone would serve them.
    fn put_v1_rows(db: &Path, dir: &Path, file: &Path) {
        let conn = Connection::open(db).unwrap();
        let dir_mtime = stat_path(dir).unwrap().key.unwrap().mtime_ns;
        let file_meta = std::fs::metadata(file).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO dirs (path, mtime_ns, entries, accessed_ns, listing_version) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                path_key(dir),
                dir_mtime,
                borsh::to_vec(&DirListing::default()).unwrap(),
                now_ns(),
                DIR_LISTING_VERSION
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime_ns, exec, hashout, accessed_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                path_key(file),
                i64::try_from(file_meta.len()).unwrap(),
                Stat::of(&file_meta).key.unwrap().mtime_ns,
                0,
                "stale",
                now_ns()
            ],
        )
        .unwrap();
    }

    /// D34: opening a schema-1 db migrates it to 2 and drops every row it
    /// held, since any of them may have been stored racy. A one-time re-read.
    #[test]
    fn fswalk_v1_rows_are_dropped_on_migration() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        Connection::open(&db)
            .unwrap()
            .execute_batch(V1_FSWALK_DDL)
            .unwrap();
        put_v1_rows(&db, &pkg, &f);

        let w = walker(tmp.path());
        assert!(w.store().is_some());
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);
        assert_eq!((rows(&db, "dirs"), rows(&db, "files")), (0, 0));

        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );
        assert_eq!((rows(&db, "dirs"), rows(&db, "files")), (1, 1));
    }

    /// D34, the mixed fleet: a schema-1 binary keeps writing to a db at schema
    /// 2 (it reads the version as current), and its `INSERT`s leave the key
    /// columns NULL. Such a row is never served.
    #[test]
    fn old_fswalk_row_reads_unverified() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        let w = walker(tmp.path());
        w.read_dir(&pkg).unwrap();
        w.file_hash(&f).unwrap();
        drop(w);

        put_v1_rows(&db, &pkg, &f);

        let w = walker(tmp.path());
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );
        drop(w);
        // The re-read replaced each skipped row with a fully keyed one.
        let conn = Connection::open(&db).unwrap();
        for table in ["dirs", "files"] {
            let unkeyed: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE ctime_ns IS NULL OR ino IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!((rows(&db, table), unkeyed), (1, 0), "{table}");
        }
    }

    /// A walker on `<dir>/fswalk.db` whose stat reports `key` for every path
    /// and whose clock reads `clock`; each stat advances the clock by `step`
    /// first, so a clock sampled after the stat reads later than one sampled
    /// before it.
    fn clock_stepping_walker(
        dir: &Path,
        clock: &Arc<AtomicI64>,
        key: StatKey,
        step: i64,
    ) -> CachedWalker {
        let (read, stepped) = (Arc::clone(clock), Arc::clone(clock));
        CachedWalker::open(&dir.join("fswalk.db"))
            .with_clock(move || read.load(Ordering::SeqCst))
            .with_stat(move |p| {
                stepped.fetch_add(step, Ordering::SeqCst);
                let mut st = stat_path(p)?;
                st.key = Some(key);
                Ok(st)
            })
    }

    /// The clock is sampled before the stat: an entry that is one nanosecond
    /// short of settled when the read starts is not cached, even though the
    /// clock has passed the window by the time the stat returns.
    #[test]
    fn the_settled_clock_is_sampled_before_the_stat() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        let clock = Arc::new(AtomicI64::new(FIXED_KEY.newest_ns() + racy_window_ns() - 1));
        let w = clock_stepping_walker(tmp.path(), &clock, FIXED_KEY, 1);

        w.read_dir(&pkg).unwrap();
        // The dir's stat moved the clock to settled; reset it for the file.
        clock.store(
            FIXED_KEY.newest_ns() + racy_window_ns() - 1,
            Ordering::SeqCst,
        );
        w.file_hash(&f).unwrap();
        assert!(
            clock.load(Ordering::SeqCst) >= FIXED_KEY.newest_ns() + racy_window_ns(),
            "a clock read after the stat would have been settled"
        );
        assert_eq!((rows(&db, "dirs"), rows(&db, "files")), (0, 0));
        assert!(w.dirs.lock().is_empty() && w.files.lock().is_empty());
    }

    /// A walker whose stat is the real one with the ctime pinned to `ctime`,
    /// and which runs `after` once, right after the first stat of `target`
    /// returns — a write landing between the stat and the read. `after` bumps
    /// `ctime` itself, as the kernel would.
    fn write_after_first_stat_walker(
        dir: &Path,
        ctime: &Arc<AtomicI64>,
        target: &Path,
        after: impl FnOnce() + Send + 'static,
    ) -> CachedWalker {
        let ctime = Arc::clone(ctime);
        let target = target.to_path_buf();
        let after = Mutex::new(Some(after));
        walker(dir).with_stat(move |p| {
            let mut st = stat_path(p)?;
            if let Some(key) = &mut st.key {
                key.ctime_ns = ctime.load(Ordering::SeqCst);
            }
            if p == target
                && let Some(after) = after.lock().take()
            {
                after();
            }
            Ok(st)
        })
    }

    /// A write between the stat and the read: what was read is filed under
    /// the stat taken before the write, which the write's ctime no longer
    /// matches. So once the file is put back the way it was, mtime and size
    /// included (`cp -p`), the next call re-reads rather than serving what the
    /// racing write left.
    #[test]
    fn a_write_between_the_stat_and_the_hash_is_not_served_later() {
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = write_after_first_stat_walker(tmp.path(), &ctime, &f, {
            let (f, ctime) = (f.clone(), Arc::clone(&ctime));
            move || {
                std::fs::write(&f, b"bbbb").unwrap();
                set_mtime(&f, old_mtime());
                ctime.store(2, Ordering::SeqCst);
            }
        });
        // The read saw the racing write's bytes.
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"bbbb", false)
        );

        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        ctime.store(3, Ordering::SeqCst);
        let fresh = pinned_ctime_walker(tmp.path(), &ctime);
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            assert_eq!(
                w.file_hash(&f).unwrap().hashout,
                bytes_hashout(b"aaaa", false),
                "{what}"
            );
        }
    }

    /// [`a_write_between_the_stat_and_the_hash_is_not_served_later`] for a
    /// directory listing: an entry created between the stat and the read, then
    /// removed with the directory's mtime put back.
    #[test]
    fn a_write_between_the_stat_and_the_listing_is_not_served_later() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("a"), b"").unwrap();
        set_mtime(&pkg, old_mtime());
        let ctime = Arc::new(AtomicI64::new(1));
        let w = write_after_first_stat_walker(tmp.path(), &ctime, &pkg, {
            let (pkg, ctime) = (pkg.clone(), Arc::clone(&ctime));
            move || {
                std::fs::write(pkg.join("b"), b"").unwrap();
                set_mtime(&pkg, old_mtime());
                ctime.store(2, Ordering::SeqCst);
            }
        });
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["a", "b"]);

        std::fs::remove_file(pkg.join("b")).unwrap();
        set_mtime(&pkg, old_mtime());
        ctime.store(3, Ordering::SeqCst);
        let fresh = pinned_ctime_walker(tmp.path(), &ctime);
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["a"], "{what}");
        }
    }

    /// On the real clock and the real stat, an entry stamped an hour in the
    /// future is never settled, however often it is read: nothing is stored.
    #[test]
    fn a_future_mtime_is_never_stored() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        set_mtime(&f, future);
        set_mtime(&pkg, future);
        let w = CachedWalker::open(&db);
        for _ in 0..3 {
            assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
            assert_eq!(
                w.file_hash(&f).unwrap().hashout,
                bytes_hashout(b"aaaa", false)
            );
        }
        assert!(w.dirs.lock().is_empty() && w.files.lock().is_empty());
        drop(w);
        assert_eq!((rows(&db, "dirs"), rows(&db, "files")), (0, 0));
    }

    /// On the real clock and the real stat, a file and directory just given
    /// an old mtime — as `tar -x`, `cp -p` and `rsync -a` leave them — are not
    /// stored: their ctime is fresh, and the window runs from it.
    #[test]
    fn an_old_mtime_with_a_fresh_ctime_is_not_stored() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        set_mtime(&f, old_mtime());
        set_mtime(&pkg, old_mtime());
        let w = CachedWalker::open(&db);
        assert_eq!(names(&w.read_dir(&pkg).unwrap()), ["f"]);
        assert_eq!(
            w.file_hash(&f).unwrap().hashout,
            bytes_hashout(b"aaaa", false)
        );
        assert!(w.dirs.lock().is_empty() && w.files.lock().is_empty());
        drop(w);
        assert_eq!((rows(&db, "dirs"), rows(&db, "files")), (0, 0));
    }

    /// The inode is stored as the bit pattern of an `i64`: the largest `u64`
    /// comes back unchanged from both tables.
    #[test]
    fn the_largest_inode_round_trips_through_the_store() {
        let tmp = tempdir().unwrap();
        let store = FsWalkStore::open(&tmp.path().join("fswalk.db")).unwrap();
        let key = StatKey {
            mtime_ns: 1,
            ctime_ns: 2,
            ino: u64::MAX,
        };
        let path = tmp.path().join("p");
        store.put_dir(&path, &key, b"entries", 0);
        assert_eq!(store.get_dir(&path).unwrap().0, key);

        let fh = FileHash {
            size: 4,
            mtime_ns: key.mtime_ns,
            ctime_ns: key.ctime_ns,
            ino: key.ino,
            exec: true,
            hashout: "h".into(),
        };
        store.put_file(&path, &fh, 0);
        let (got, _) = store.get_file(&path).unwrap();
        assert_eq!((got.key(), got.size, got.exec), (key, 4, true));
    }

    /// Several processes opening the same schema-1 db at once migrate it
    /// exactly once: the one that takes the lock first drops the rows, and
    /// every other finds it current under the lock.
    #[test]
    fn concurrent_opens_of_a_v1_db_migrate_once() {
        const N: usize = 8;
        let tmp = tempdir().unwrap();
        let db = tmp.path().join("fswalk.db");
        let pkg = tmp.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        let f = pkg.join("f");
        std::fs::write(&f, b"aaaa").unwrap();
        Connection::open(&db)
            .unwrap()
            .execute_batch(V1_FSWALK_DDL)
            .unwrap();
        put_v1_rows(&db, &pkg, &f);

        let barrier = Arc::new(std::sync::Barrier::new(N));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let (db, barrier) = (db.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    let conn = open_write_connection(&db).unwrap();
                    barrier.wait();
                    migrate_fswalk_schema(&db, &conn).map_err(|e| format!("{e:#}"))
                })
            })
            .collect();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().unwrap().unwrap())
            .collect();
        assert_eq!(
            outcomes.iter().filter(|o| o.is_some()).collect::<Vec<_>>(),
            [&Some(2)],
            "exactly one migration, dropping both v1 rows: {outcomes:?}"
        );
        assert_eq!(fswalk_version(&db), FSWALK_SCHEMA_VERSION);
        FsWalkStore::open(&db).unwrap();
    }

    /// The exec bit is validated on its own, not only through the ctime a
    /// `chmod` moves: a cached hash whose exec bit differs from the live one
    /// is re-read, in-process and from the durable store.
    #[test]
    fn an_exec_bit_change_alone_is_reread() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempdir().unwrap();
        let f = tmp.path().join("tool");
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Every key field pinned: only the exec bit can tell the reads apart.
        let pinned = |dir: &Path| {
            walker(dir).with_stat(|p| {
                let mut st = stat_path(p)?;
                st.key = Some(FIXED_KEY);
                Ok(st)
            })
        };
        let w = pinned(tmp.path());
        assert!(!w.file_hash(&f).unwrap().exec);

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fresh = pinned(tmp.path());
        for (what, w) in [("durable", &fresh), ("in-process", &w)] {
            let after = w.file_hash(&f).unwrap();
            assert!(after.exec, "{what}");
            assert_eq!(after.hashout, file_hashout(&f, true).unwrap(), "{what}");
        }
    }
}
