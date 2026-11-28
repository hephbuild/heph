//! The working-set journal: what one engine's requests used from the cache,
//! recorded so `heph tool cache snapshot` can write exactly that to the next
//! CI job (docs: the CI cache snapshots spec, D2 and D13).
//!
//! # Lifecycle
//!
//! - The [`Engine`](crate::engine::Engine) owns one [`Journal`] when
//!   [`Config::record_working_set`](crate::engine::Config::record_working_set)
//!   is on. No file exists until the first request records something.
//! - At that first record, the file is created as `.<pid>-<nonce>.tmp` with
//!   `O_EXCL`, locked exclusively (std `File::try_lock`: flock on both OSes;
//!   std opens close-on-exec, so no target inherits the lock), given its
//!   header, and renamed to `<home>/cache/working-set/<pid>-<nonce>.log`. The
//!   rename keeps the open file and its lock, and a reader lists only `*.log`,
//!   so no reader sees a journal before its lock is held.
//! - Each request holds an `Arc<Journal>` and batches its entries. When the
//!   request's data drops, the batch and an `End{cancelled, failed}` line are
//!   appended in one job on [`Lane::Bookkeeping`](crate::engine::sandbox_cleaner::Lane),
//!   so a cancelled request still flushes and the result path pays only a set
//!   insert and a `Vec` push.
//! - `Drop for Journal` writes the trailer. Every flush job holds the journal,
//!   so the trailer always follows the last batch. A process that runs no
//!   destructors (`hard_abort`, `kill -9`) leaves no trailer, which a reader
//!   takes as an incomplete run.
//!
//! An engine writes each entry once, however many requests touch it. Writes
//! are not fsynced: a lost tail reads as incomplete, never as a smaller
//! complete set.
//!
//! # Format (journal version 1)
//!
//! JSON lines. The first is the header,
//! `{"magic":"heph-working-set","version":1}`; then entries tagged by `t`:
//!
//! ```text
//! {"t":"Revision","addr":{"package":"a/b","name":"t","args":{"k":"v"}},"hashin":"…"}
//! {"t":"Scratch","slot":"…","scope":"…","generation":3,"content_hash":"…"}
//! {"t":"End","cancelled":false,"failed":false}
//! {"t":"Trailer"}
//! ```
//!
//! The address is structured, not formatted: `Addr`'s display form does not
//! round-trip every argument value (a `"` in one), and the export must land
//! each revision under the key `key_segment` gives the original address. The
//! field set is fixed for version 1; a change bumps [`JOURNAL_VERSION`].

use anyhow::Context as _;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

pub mod export;
pub mod marker;
pub mod take;
pub use export::{
    ExportOutcome, ExportReport, IncompleteJournal, LeftOut, RevisionBytes, SNAPSHOT_LOCK_FILE,
    SnapshotLock, Tally, try_lock_snapshot,
};
pub use marker::{
    GENERATION_PREFIX, Generation, GenerationStats, MARKER_FILE, MARKER_TMP_PREFIX, Marker, NoTier,
    SNAPSHOT_FORMAT, is_generation_name, open_tier, read_marker, write_marker, write_marker_with,
};
pub use take::{Consumption, LiveRecorder, RECORDER_WAIT, RecorderAliveError, SnapshotCancelled};

/// The first line's `magic`: what makes a file a working-set journal.
pub const JOURNAL_MAGIC: &str = "heph-working-set";

/// The journal format this heph writes, and the newest it reads.
///
/// A reader reads every version up to its own and refuses a newer one
/// ([`NewerJournalError`]). Bumping it is a format change governed like the
/// remote wire format: an older `snapshot` refuses the new journals.
pub const JOURNAL_VERSION: u32 = 1;

/// Where every journal of a home lives: `<home>/cache/working-set/`.
///
/// `heph clean` and gc never touch it: a journal's lifetime ends only when
/// the `snapshot` that read it deletes it (I4).
pub fn working_set_dir(home: &Path) -> PathBuf {
    home.join("cache").join("working-set")
}

/// The journals a reader may read: every `*.log` in the working-set dir,
/// sorted by name. Never a `.tmp` (not yet locked) or anything else. An
/// absent directory has none.
pub fn list_journals(home: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let dir = working_set_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name();
        let is_log = name
            .to_str()
            .is_some_and(|n| n.ends_with(".log") && !n.starts_with('.'));
        if is_log {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// An address as the journal writes it: every part, nothing formatted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JournalAddr {
    package: String,
    name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    args: BTreeMap<String, String>,
}

impl JournalAddr {
    fn of(addr: &Addr) -> Self {
        Self {
            package: addr.package.as_str().to_owned(),
            name: addr.name.clone(),
            args: addr
                .args
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    fn into_addr(self) -> Addr {
        Addr::new(PkgBuf::from(self.package), self.name, self.args)
    }
}

/// A scratch slot this engine used in a build, pulled, or pushed: the
/// snapshot meta its lineage descended from then (generation 0 and an empty
/// `content_hash` when it descends from none).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordedScratch {
    pub slot: String,
    pub scope: String,
    pub generation: u64,
    pub content_hash: String,
}

/// One cache revision this engine read from any tier, or wrote.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordedRevision {
    pub addr: Addr,
    pub hashin: String,
}

/// One journal line. `t` is the tag; every field set is fixed for a version.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
enum Line {
    Revision {
        addr: JournalAddr,
        hashin: String,
    },
    Scratch {
        slot: String,
        scope: String,
        generation: u64,
        content_hash: String,
    },
    End {
        cancelled: bool,
        failed: bool,
    },
    Trailer,
}

/// Just the tag, to tell an unknown entry from a malformed one.
#[derive(Deserialize)]
struct TagOnly {
    t: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    magic: String,
    version: u32,
}

/// What a request batches before its drop flushes it.
#[derive(Debug)]
enum Entry {
    Revision { addr: Addr, hashin: Arc<str> },
    Scratch(RecordedScratch),
}

impl Entry {
    fn line(&self) -> Line {
        match self {
            Entry::Revision { addr, hashin } => Line::Revision {
                addr: JournalAddr::of(addr),
                hashin: hashin.to_string(),
            },
            Entry::Scratch(s) => Line::Scratch {
                slot: s.slot.clone(),
                scope: s.scope.clone(),
                generation: s.generation,
                content_hash: s.content_hash.clone(),
            },
        }
    }
}

fn push_line(buf: &mut Vec<u8>, line: &Line) {
    // Serializing a `Line` cannot fail: every field is a string, number,
    // bool or string map.
    if serde_json::to_writer(&mut *buf, line).is_ok() {
        buf.push(b'\n');
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// The open, locked journal file. The lock is the file's own flock, released
/// when the file closes.
#[derive(Debug)]
struct JournalFile {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    /// Set by the first failed append. The file then lacks (part of) a
    /// batch, so nothing more is written to it — the trailer included — and
    /// it reads as incomplete ([`Incomplete::NoTrailer`]), never as a
    /// smaller complete set.
    poisoned: AtomicBool,
}

impl JournalFile {
    fn new(path: PathBuf, file: std::fs::File) -> Self {
        Self {
            path,
            file: Mutex::new(file),
            poisoned: AtomicBool::new(false),
        }
    }

    fn append(&self, bytes: &[u8]) {
        let mut file = self.file.lock();
        // Read and set under the file's mutex, which orders every append.
        if self.poisoned.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = file.write_all(bytes) {
            self.poisoned.store(true, Ordering::Relaxed);
            tracing::warn!(
                path = %self.path.display(), error = %e,
                "writing the working-set journal failed; the next snapshot will read this run as incomplete",
            );
        }
    }
}

/// One engine's working-set journal. See the module docs.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    /// Created at the first record. `None` inside means creation failed:
    /// warned once, and nothing more is recorded.
    file: OnceLock<Option<JournalFile>>,
    /// Every revision this engine has batched, so each is written once: the
    /// `hashin`s of each target. Keyed by target so a lookup borrows, and a
    /// revision already seen allocates nothing.
    seen: Mutex<FxHashMap<Addr, FxHashSet<Arc<str>>>>,
    /// Every scratch entry this engine has batched.
    seen_scratch: Mutex<FxHashSet<RecordedScratch>>,
    /// Set once the run-end reminder of an unwritten journal was sent, so an
    /// engine sends it at most once.
    reminded: AtomicBool,
}

impl Journal {
    /// A journal under `home`'s working-set dir. Creates nothing.
    pub(crate) fn new(home: &Path) -> Self {
        Self {
            dir: working_set_dir(home),
            file: OnceLock::new(),
            seen: Mutex::new(FxHashMap::default()),
            seen_scratch: Mutex::new(FxHashSet::default()),
            reminded: AtomicBool::new(false),
        }
    }

    /// The journal's path, once the first record created it.
    pub fn path(&self) -> Option<&Path> {
        self.file.get()?.as_ref().map(|f| f.path.as_path())
    }

    /// How many distinct revisions this engine has recorded.
    pub fn revisions(&self) -> usize {
        self.seen.lock().values().map(FxHashSet::len).sum()
    }

    /// Record a scratch slot published outside any request (`heph tool
    /// scratch push`). Appended at once, with a clean `End` of its own: no
    /// request's drop would flush it. Written once per engine, like a
    /// request's entries.
    pub(crate) fn record_scratch_now(&self, scratch: RecordedScratch) {
        if !self.seen_scratch.lock().insert(scratch.clone()) {
            return;
        }
        let Some(file) = self.file() else {
            return;
        };
        let mut buf = Vec::with_capacity(256);
        push_line(&mut buf, &Entry::Scratch(scratch).line());
        push_line(
            &mut buf,
            &Line::End {
                cancelled: false,
                failed: false,
            },
        );
        file.append(&buf);
    }

    /// The open file, creating it on the first call.
    fn file(&self) -> Option<&JournalFile> {
        self.file
            .get_or_init(|| match create_journal_file(&self.dir) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(
                        dir = %self.dir.display(), error = %format!("{e:#}"),
                        "could not create the working-set journal; this process records nothing",
                    );
                    None
                }
            })
            .as_ref()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // Only a journal that exists gets a trailer; an engine that never
        // recorded leaves no file at all.
        if let Some(Some(file)) = self.file.get() {
            let mut buf = Vec::new();
            push_line(&mut buf, &Line::Trailer);
            file.append(&buf);
        }
    }
}

/// Create, lock, head and publish a journal file under `dir`.
fn create_journal_file(dir: &Path) -> anyhow::Result<JournalFile> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let pid = std::process::id();
    // A few attempts in case of a nonce collision with a leftover temp file.
    let mut last_err = None;
    for _ in 0..4 {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let stem = format!("{pid}-{}", nonce.get(..16).unwrap_or(&nonce));
        let tmp = dir.join(format!(".{stem}.tmp"));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
        };
        let published = (|| -> anyhow::Result<PathBuf> {
            file.try_lock()
                .with_context(|| format!("locking {}", tmp.display()))?;
            let mut buf = serde_json::to_vec(&Header {
                magic: JOURNAL_MAGIC.to_owned(),
                version: JOURNAL_VERSION,
            })
            .context("encoding the journal header")?;
            buf.push(b'\n');
            file.write_all(&buf)
                .with_context(|| format!("writing the header of {}", tmp.display()))?;
            let path = dir.join(format!("{stem}.log"));
            std::fs::rename(&tmp, &path)
                .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
            Ok(path)
        })();
        return match published {
            Ok(path) => Ok(JournalFile::new(path, file)),
            Err(e) => {
                drop(file);
                drop(std::fs::remove_file(&tmp));
                Err(e)
            }
        };
    }
    Err(last_err.map_or_else(
        || anyhow::anyhow!("no free journal name"),
        anyhow::Error::from,
    ))
    .with_context(|| format!("creating a journal in {}", dir.display()))
}

/// One request's side of the journal: its batch, and whether it failed.
///
/// Held in `RequestStateData`; flushed by its drop ([`Self::flush`]).
#[derive(Debug)]
pub(crate) struct RequestJournal {
    journal: Arc<Journal>,
    batch: Mutex<Vec<Entry>>,
    failed: AtomicBool,
}

impl RequestJournal {
    pub(crate) fn new(journal: Arc<Journal>) -> Self {
        Self {
            journal,
            batch: Mutex::new(Vec::new()),
            failed: AtomicBool::new(false),
        }
    }

    /// Record a revision this request resolved. Cheap: one set probe, and a
    /// push for a revision this engine has not recorded yet. The first record
    /// of an engine creates its journal file (I5: the lock is held from the
    /// first recording request on).
    pub(crate) fn record_revision(&self, addr: &Addr, hashin: &str) {
        let key: Arc<str> = {
            let mut seen = self.journal.seen.lock();
            if let Some(hashins) = seen.get_mut(addr) {
                if hashins.contains(hashin) {
                    return;
                }
                let key: Arc<str> = Arc::from(hashin);
                hashins.insert(Arc::clone(&key));
                key
            } else {
                let key: Arc<str> = Arc::from(hashin);
                seen.insert(addr.clone(), FxHashSet::from_iter([Arc::clone(&key)]));
                key
            }
        };
        if self.journal.file().is_none() {
            return;
        }
        self.batch.lock().push(Entry::Revision {
            addr: addr.clone(),
            hashin: key,
        });
    }

    /// Record a scratch slot this request used.
    pub(crate) fn record_scratch(&self, scratch: RecordedScratch) {
        if !self.journal.seen_scratch.lock().insert(scratch.clone()) {
            return;
        }
        if self.journal.file().is_none() {
            return;
        }
        self.batch.lock().push(Entry::Scratch(scratch));
    }

    pub(crate) fn mark_failed(&self) {
        self.failed.store(true, Ordering::Relaxed);
    }

    /// Append the batch and the request's `End` line, in one job on the
    /// bookkeeping lane. A request that recorded nothing and ended cleanly
    /// writes nothing; one that failed or was cancelled writes its `End` even
    /// with an empty batch, so the run reads as incomplete.
    pub(crate) fn flush(
        &self,
        cancelled: bool,
        bg_pending: &crate::engine::sandbox_cleaner::PendingCounter,
    ) {
        let batch = std::mem::take(&mut *self.batch.lock());
        let failed = self.failed.load(Ordering::Relaxed);
        if batch.is_empty() && !failed && !cancelled {
            return;
        }
        if self.journal.file().is_none() {
            return;
        }
        let journal = Arc::clone(&self.journal);
        crate::engine::sandbox_cleaner::enqueue(
            crate::engine::sandbox_cleaner::Lane::Bookkeeping,
            format!("working-set journal: {} entries", batch.len()),
            Box::new(move || {
                let mut buf = Vec::with_capacity(batch.len() * 128 + 48);
                for entry in &batch {
                    push_line(&mut buf, &entry.line());
                }
                push_line(&mut buf, &Line::End { cancelled, failed });
                if let Some(file) = journal.file() {
                    file.append(&buf);
                }
                // `journal` drops here at the latest: when it is the last
                // reference, the trailer lands after this batch.
                drop(journal);
                Ok(())
            }),
            Arc::clone(bg_pending),
        );
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A journal written by a newer heph. The `snapshot` command exits 4 on it;
/// deleting the file is safe (it only makes the next snapshot smaller).
#[derive(Debug, thiserror::Error)]
#[error(
    "{path} is a working-set journal of version {version}, newer than this heph reads \
     ({JOURNAL_VERSION}); a newer heph wrote it. Deleting it is safe."
)]
pub struct NewerJournalError {
    pub path: PathBuf,
    pub version: u32,
}

/// Why a journal makes its run incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incomplete {
    /// No trailer: the process died without running destructors, or is still
    /// writing.
    NoTrailer,
    /// A request was cancelled.
    Cancelled,
    /// A target failed in a request.
    Failed,
    /// An entry tag this heph does not know, on line `line` (1-based).
    UnknownEntry { line: usize, tag: String },
    /// A line that does not decode and is not the last one.
    BadLine { line: usize },
    /// The first line is not a journal header.
    BadHeader,
    /// Something follows the trailer.
    AfterTrailer { line: usize },
}

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Incomplete::NoTrailer => f.write_str("a recorder ended without its trailer"),
            Incomplete::Cancelled => f.write_str("a request was cancelled"),
            Incomplete::Failed => f.write_str("a target failed"),
            Incomplete::UnknownEntry { line, tag } => {
                write!(f, "unknown journal entry {tag:?} on line {line}")
            }
            Incomplete::BadLine { line } => write!(f, "unreadable journal line {line}"),
            Incomplete::BadHeader => f.write_str("unreadable journal header"),
            Incomplete::AfterTrailer { line } => {
                write!(f, "journal line {line} follows the trailer")
            }
        }
    }
}

/// One journal, read.
#[derive(Debug, Clone)]
pub struct JournalContents {
    pub path: PathBuf,
    pub version: u32,
    /// In file order, each once.
    pub revisions: Vec<RecordedRevision>,
    pub scratch: Vec<RecordedScratch>,
    /// The first reason this journal makes its run incomplete; `None` when it
    /// has a clean trailer and every `End` is clean.
    pub incomplete: Option<Incomplete>,
}

/// Read one journal.
///
/// A torn last line (or any undecodable last line) is skipped. A newer
/// version is [`NewerJournalError`]; an unreadable file is an error.
pub fn read_journal(path: &Path) -> anyhow::Result<JournalContents> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse_journal(path, &bytes)
}

fn parse_journal(path: &Path, bytes: &[u8]) -> anyhow::Result<JournalContents> {
    let mut out = JournalContents {
        path: path.to_path_buf(),
        version: 0,
        revisions: Vec::new(),
        scratch: Vec::new(),
        incomplete: None,
    };
    let note = |out: &mut JournalContents, why: Incomplete| {
        if out.incomplete.is_none() {
            out.incomplete = Some(why);
        }
    };

    // `split` leaves an empty last piece after a final newline; anything else
    // there is a line the writer never finished.
    let mut pieces: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if pieces.last().is_some_and(|l| l.is_empty()) {
        pieces.pop();
    }
    let last = pieces.len().saturating_sub(1);

    let Some(header) = pieces.first() else {
        note(&mut out, Incomplete::BadHeader);
        return Ok(out);
    };
    match serde_json::from_slice::<Header>(header) {
        Ok(h) if h.magic == JOURNAL_MAGIC && h.version > JOURNAL_VERSION => {
            return Err(NewerJournalError {
                path: path.to_path_buf(),
                version: h.version,
            }
            .into());
        }
        Ok(h) if h.magic == JOURNAL_MAGIC && h.version >= 1 => out.version = h.version,
        _ => {
            note(&mut out, Incomplete::BadHeader);
            return Ok(out);
        }
    }

    let mut seen: FxHashSet<RecordedRevision> = FxHashSet::default();
    let mut seen_scratch: FxHashSet<RecordedScratch> = FxHashSet::default();
    let mut trailer = false;
    for (i, piece) in pieces.iter().enumerate().skip(1) {
        let line_no = i + 1;
        let line = match serde_json::from_slice::<Line>(piece) {
            Ok(line) => line,
            Err(_) if i == last => break,
            Err(_) => {
                let why = match serde_json::from_slice::<TagOnly>(piece) {
                    Ok(TagOnly { t })
                        if !matches!(t.as_str(), "Revision" | "Scratch" | "End" | "Trailer") =>
                    {
                        Incomplete::UnknownEntry {
                            line: line_no,
                            tag: t,
                        }
                    }
                    _ => Incomplete::BadLine { line: line_no },
                };
                note(&mut out, why);
                continue;
            }
        };
        if trailer {
            note(&mut out, Incomplete::AfterTrailer { line: line_no });
        }
        match line {
            Line::Revision { addr, hashin } => {
                let rev = RecordedRevision {
                    addr: addr.into_addr(),
                    hashin,
                };
                if seen.insert(rev.clone()) {
                    out.revisions.push(rev);
                }
            }
            Line::Scratch {
                slot,
                scope,
                generation,
                content_hash,
            } => {
                let s = RecordedScratch {
                    slot,
                    scope,
                    generation,
                    content_hash,
                };
                if seen_scratch.insert(s.clone()) {
                    out.scratch.push(s);
                }
            }
            Line::End { cancelled, failed } => {
                if cancelled {
                    note(&mut out, Incomplete::Cancelled);
                }
                if failed {
                    note(&mut out, Incomplete::Failed);
                }
            }
            Line::Trailer => trailer = true,
        }
    }
    if !trailer {
        note(&mut out, Incomplete::NoTrailer);
    }
    Ok(out)
}

/// The union of several journals: what one job used.
#[derive(Debug, Clone, Default)]
pub struct WorkingSet {
    /// The journals read, in the order given.
    pub journals: Vec<PathBuf>,
    /// Each revision once, in first-seen order.
    pub revisions: Vec<RecordedRevision>,
    /// Each scratch entry once, in first-seen order.
    pub scratch: Vec<RecordedScratch>,
    /// Every journal that makes the run incomplete, with its reason.
    pub incomplete: Vec<(PathBuf, Incomplete)>,
}

impl WorkingSet {
    /// Merge journals already read.
    pub fn from_journals(journals: impl IntoIterator<Item = JournalContents>) -> Self {
        let mut ws = WorkingSet::default();
        let mut seen: FxHashSet<RecordedRevision> = FxHashSet::default();
        let mut seen_scratch: FxHashSet<RecordedScratch> = FxHashSet::default();
        for j in journals {
            for rev in j.revisions {
                if seen.insert(rev.clone()) {
                    ws.revisions.push(rev);
                }
            }
            for s in j.scratch {
                if seen_scratch.insert(s.clone()) {
                    ws.scratch.push(s);
                }
            }
            if let Some(why) = j.incomplete {
                ws.incomplete.push((j.path.clone(), why));
            }
            ws.journals.push(j.path);
        }
        ws
    }

    /// Complete: every journal has a clean trailer and every `End` is clean.
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }

    /// Nothing recorded at all.
    pub fn is_empty(&self) -> bool {
        self.revisions.is_empty() && self.scratch.is_empty()
    }
}

/// The paths a tree walk rooted at `root` must prune for the snapshot dir
/// `dir` (I1), when the walker compares exact lexical paths.
///
/// `dir` as configured, always. When it exists, also its canonical form, and
/// that form re-rooted under `root` as written when it lies under `root`'s
/// canonical form: an absolute `dir` spelled through a symlink (macOS's
/// `/var` for `/private/var`, a symlinked checkout) names the directory the
/// walker reaches under another spelling, and a root glob would otherwise
/// hash the snapshot.
pub fn snapshot_skip_dirs(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return out;
    };
    if let Ok(root_canonical) = std::fs::canonicalize(root)
        && let Ok(rel) = canonical.strip_prefix(&root_canonical)
    {
        let rerooted = hcore::paths::lexical(&root.join(rel));
        if !out.contains(&rerooted) {
            out.push(rerooted);
        }
    }
    if !out.contains(&canonical) {
        out.push(canonical);
    }
    out
}

impl crate::engine::Engine {
    /// This engine's working-set journal, when it records one
    /// ([`Config::record_working_set`](crate::engine::Config::record_working_set)).
    pub fn working_set_journal(&self) -> Option<&Arc<Journal>> {
        self.working_set.as_ref()
    }

    /// The cache snapshot dir this engine was configured with, resolved;
    /// `None` when snapshots are off.
    pub fn cache_snapshot_dir(&self) -> Option<&Path> {
        self.cfg.cache_snapshot.as_deref()
    }

    /// Under CI ([`Config::ci`](crate::engine::Config::ci)), remind the run's
    /// log that its journal holds revisions no `heph tool cache snapshot` has
    /// written yet: a
    /// [`SnapshotJournalPending`](crate::engine::event::BuildEventKind::SnapshotJournalPending)
    /// on `rs`, at most once per engine. Called when `heph run` ends.
    pub fn remind_unconsumed_journal(&self, rs: &crate::engine::request_state::RequestState) {
        if !self.cfg.ci {
            return;
        }
        let Some(journal) = &self.working_set else {
            return;
        };
        let revisions = journal.revisions();
        if revisions == 0 || journal.reminded.swap(true, Ordering::Relaxed) {
            return;
        }
        rs.emit(
            crate::engine::event::BuildEventKind::SnapshotJournalPending {
                revisions: u64::try_from(revisions).unwrap_or(u64::MAX),
            },
        );
    }
}

/// The [`CacheSnapshotState`](crate::engine::event::BuildEventKind::CacheSnapshotState)
/// of the snapshot dir `dir` as `Engine::new` opened it: `opened` is what
/// [`open_tier`] answered, `tier` whether the remote set then set the tier up
/// (a generation it could not open is reported unreadable). `dir` is shown
/// relative to `root` when it is under it.
pub(crate) fn snapshot_state_event(
    root: &Path,
    dir: &Path,
    opened: &Result<Generation, NoTier>,
    tier: bool,
) -> crate::engine::event::BuildEventKind {
    use hcore::events::SnapshotTierState;
    let shown = dir.strip_prefix(root).unwrap_or(dir);
    let shown = if shown.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        shown.display().to_string()
    };
    let skipped =
        |state, format, reason| crate::engine::event::BuildEventKind::CacheSnapshotState {
            dir: shown.clone(),
            state,
            format,
            heph: String::new(),
            incomplete: false,
            revisions: None,
            bytes: None,
            reason,
        };
    match opened {
        Ok(generation) if tier => {
            let m = &generation.marker;
            crate::engine::event::BuildEventKind::CacheSnapshotState {
                dir: shown.clone(),
                state: SnapshotTierState::Serving,
                format: Some(u64::from(m.format)),
                heph: m.heph.clone(),
                incomplete: m.incomplete,
                revisions: m.revisions,
                bytes: m.bytes,
                reason: None,
            }
        }
        Ok(generation) => skipped(
            SnapshotTierState::Unreadable,
            Some(u64::from(generation.marker.format)),
            Some(format!(
                "generation {} could not be opened",
                generation.marker.current
            )),
        ),
        Err(NoTier::NoDir) => skipped(SnapshotTierState::Absent, None, None),
        Err(NoTier::NoMarker) => skipped(SnapshotTierState::NoMarker, None, None),
        Err(no_tier) => skipped(
            SnapshotTierState::Unreadable,
            no_tier.format(),
            Some(no_tier.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(pkg: &str, name: &str, args: &[(&str, &str)]) -> Addr {
        Addr::new(
            PkgBuf::from(pkg),
            name.to_string(),
            args.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// Write lines (each `\n`-terminated) to `name` under `dir`.
    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write journal");
        p
    }

    const HEADER: &str = "{\"magic\":\"heph-working-set\",\"version\":1}\n";
    const REV_A: &str =
        "{\"t\":\"Revision\",\"addr\":{\"package\":\"p\",\"name\":\"a\"},\"hashin\":\"ha\"}\n";
    const REV_B: &str =
        "{\"t\":\"Revision\",\"addr\":{\"package\":\"p\",\"name\":\"b\"},\"hashin\":\"hb\"}\n";
    const END_OK: &str = "{\"t\":\"End\",\"cancelled\":false,\"failed\":false}\n";
    const TRAILER: &str = "{\"t\":\"Trailer\"}\n";

    /// Record through a real journal, drop it, read it back.
    fn record_and_read(f: impl FnOnce(&RequestJournal)) -> JournalContents {
        let home = tempfile::tempdir().expect("home");
        let journal = Arc::new(Journal::new(home.path()));
        let pending = crate::engine::sandbox_cleaner::PendingCounter::default();
        {
            let rj = RequestJournal::new(Arc::clone(&journal));
            f(&rj);
            rj.flush(false, &pending);
        }
        drop(journal);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(std::time::Instant::now() < deadline, "flush never drained");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let journals = list_journals(home.path()).expect("list");
        assert_eq!(journals.len(), 1, "one journal per engine");
        read_journal(&journals[0]).expect("read")
    }

    #[test]
    fn journal_reader_skips_a_torn_tail() {
        let dir = tempfile::tempdir().expect("dir");

        // A truncated last line is skipped; what precedes it is read. No
        // trailer follows, so the run is incomplete for that reason.
        let torn = format!("{HEADER}{REV_A}{{\"t\":\"Revision\",\"addr\":{{\"pack");
        let j = read_journal(&write(dir.path(), "torn.log", &torn)).expect("read torn");
        assert_eq!(j.revisions.len(), 1);
        assert_eq!(j.revisions[0].addr, addr("p", "a", &[]));
        assert_eq!(j.incomplete, Some(Incomplete::NoTrailer));

        // The same bytes with a trailer before the torn bit would be odd, but
        // a complete journal must stay complete when the tail is clean.
        let clean = format!("{HEADER}{REV_A}{END_OK}{TRAILER}");
        let j = read_journal(&write(dir.path(), "clean.log", &clean)).expect("read clean");
        assert_eq!(j.incomplete, None);

        // A bad line that is not the last makes the run incomplete, and the
        // lines around it are still read.
        let bad = format!("{HEADER}{REV_A}not json\n{REV_B}{END_OK}{TRAILER}");
        let j = read_journal(&write(dir.path(), "bad.log", &bad)).expect("read bad");
        assert_eq!(j.revisions.len(), 2);
        assert_eq!(j.incomplete, Some(Incomplete::BadLine { line: 3 }));

        // An unknown entry tag makes it incomplete, naming the tag.
        let unknown = format!("{HEADER}{REV_A}{{\"t\":\"Future\",\"x\":1}}\n{END_OK}{TRAILER}");
        let j = read_journal(&write(dir.path(), "unknown.log", &unknown)).expect("read unknown");
        assert_eq!(
            j.incomplete,
            Some(Incomplete::UnknownEntry {
                line: 3,
                tag: "Future".into()
            })
        );

        // A known tag with the wrong fields is a bad line, not an unknown one.
        let malformed = format!("{HEADER}{{\"t\":\"Revision\",\"hashin\":1}}\n{END_OK}{TRAILER}");
        let j = read_journal(&write(dir.path(), "malformed.log", &malformed)).expect("read");
        assert_eq!(j.incomplete, Some(Incomplete::BadLine { line: 2 }));

        // A failed or cancelled request's `End` makes it incomplete.
        let failed = format!(
            "{HEADER}{REV_A}{{\"t\":\"End\",\"cancelled\":false,\"failed\":true}}\n{TRAILER}"
        );
        let j = read_journal(&write(dir.path(), "failed.log", &failed)).expect("read failed");
        assert_eq!(j.incomplete, Some(Incomplete::Failed));
        let cancelled =
            format!("{HEADER}{{\"t\":\"End\",\"cancelled\":true,\"failed\":false}}\n{TRAILER}");
        let j = read_journal(&write(dir.path(), "cancelled.log", &cancelled)).expect("read");
        assert_eq!(j.incomplete, Some(Incomplete::Cancelled));

        // Anything after the trailer is not a clean end.
        let after = format!("{HEADER}{TRAILER}{REV_B}");
        let j = read_journal(&write(dir.path(), "after.log", &after)).expect("read after");
        assert_eq!(j.incomplete, Some(Incomplete::AfterTrailer { line: 3 }));
    }

    #[test]
    fn journal_versions() {
        let dir = tempfile::tempdir().expect("dir");

        // This version (the oldest there is) reads.
        let current = format!("{HEADER}{REV_A}{TRAILER}");
        let j = read_journal(&write(dir.path(), "v1.log", &current)).expect("read v1");
        assert_eq!(j.version, 1);
        assert_eq!(j.incomplete, None);

        // A newer one is refused with a typed error naming the file, and
        // unknown header fields do not matter for the decision.
        let newer = format!(
            "{{\"magic\":\"heph-working-set\",\"version\":{},\"extra\":true}}\n{TRAILER}",
            JOURNAL_VERSION + 1
        );
        let path = write(dir.path(), "v2.log", &newer);
        let err = read_journal(&path).expect_err("a newer journal is refused");
        let newer_err = err
            .downcast_ref::<NewerJournalError>()
            .expect("typed NewerJournalError");
        assert_eq!(newer_err.path, path);
        assert_eq!(newer_err.version, JOURNAL_VERSION + 1);
        assert!(err.to_string().contains("v2.log"), "names the file: {err}");

        // Not a journal at all: no header, an incomplete run, not an error.
        let j = read_journal(&write(dir.path(), "junk.log", "hello\n")).expect("read junk");
        assert_eq!(j.incomplete, Some(Incomplete::BadHeader));

        // What the writer writes is what the reader reads.
        let j = record_and_read(|rj| rj.record_revision(&addr("p", "a", &[]), "ha"));
        assert_eq!(j.version, JOURNAL_VERSION);
        assert_eq!(j.incomplete, None);
    }

    /// Wait for every queued flush job.
    fn drain(pending: &crate::engine::sandbox_cleaner::PendingCounter) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(std::time::Instant::now() < deadline, "flush never drained");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// A batch that failed to append is never followed by a trailer, even
    /// when the file is writable again by the time the journal drops: the
    /// run reads as incomplete, not as a complete run missing that batch.
    #[test]
    fn a_failed_append_leaves_the_journal_incomplete() {
        let home = tempfile::tempdir().expect("home");
        let journal = Arc::new(Journal::new(home.path()));
        let pending = crate::engine::sandbox_cleaner::PendingCounter::default();
        let request = |name: &str, hashin: &str| {
            let rj = RequestJournal::new(Arc::clone(&journal));
            rj.record_revision(&addr("p", name, &[]), hashin);
            rj.flush(false, &pending);
            drain(&pending);
        };
        request("a", "ha");
        let path = journal.path().expect("created").to_path_buf();
        let jf = journal
            .file
            .get()
            .and_then(Option::as_ref)
            .expect("journal open");

        // The next batch fails: its writes go to a read-only handle...
        let read_only = std::fs::File::open(&path).expect("open read-only");
        let writable = std::mem::replace(&mut *jf.file.lock(), read_only);
        request("b", "hb");
        // ...and the file is writable again for the trailer, as a disk that
        // freed up would be.
        *jf.file.lock() = writable;
        request("c", "hc");
        drop(journal);

        let j = read_journal(&path).expect("read");
        assert_eq!(j.incomplete, Some(Incomplete::NoTrailer));
        assert_eq!(
            j.revisions
                .iter()
                .map(|r| r.hashin.as_str())
                .collect::<Vec<_>>(),
            vec!["ha"],
            "nothing is written after the failed batch"
        );
    }

    /// A snapshot dir named through a symlink is pruned under the spelling
    /// the walker reaches it by, in both directions: the root through the
    /// link and the dir by its real path, and the other way round.
    #[test]
    fn a_symlinked_snapshot_dir_is_pruned_under_the_walked_spelling() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("real");
        let snap = real.join("snap");
        std::fs::create_dir_all(&snap).expect("mkdir");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let real_canonical = std::fs::canonicalize(&real).expect("canonical");

        // Walked through the link; configured by the real path.
        let dirs = snapshot_skip_dirs(&link, &real_canonical.join("snap"));
        assert!(dirs.contains(&link.join("snap")), "{dirs:?}");

        // Walked by the real path; configured through the link.
        let dirs = snapshot_skip_dirs(&real_canonical, &link.join("snap"));
        assert!(dirs.contains(&real_canonical.join("snap")), "{dirs:?}");
        assert!(dirs.contains(&link.join("snap")), "{dirs:?}");

        // A dir that does not exist yet keeps its lexical form only.
        let absent = link.join("absent");
        assert_eq!(snapshot_skip_dirs(&link, &absent), vec![absent]);
    }

    #[test]
    fn an_entry_is_written_once_per_engine() {
        let home = tempfile::tempdir().expect("home");
        let journal = Arc::new(Journal::new(home.path()));
        let pending = crate::engine::sandbox_cleaner::PendingCounter::default();
        let a = addr("p", "a", &[]);
        let scratch = RecordedScratch {
            slot: "s".into(),
            scope: "main".into(),
            generation: 2,
            content_hash: "c".into(),
        };
        // Many requests over one engine, each touching the same revision.
        for _ in 0..50 {
            let rj = RequestJournal::new(Arc::clone(&journal));
            rj.record_revision(&a, "ha");
            rj.record_revision(&a, "ha");
            rj.record_scratch(scratch.clone());
            rj.flush(false, &pending);
        }
        // Another revision of the same addr is a different entry.
        let rj = RequestJournal::new(Arc::clone(&journal));
        rj.record_revision(&a, "ha2");
        rj.flush(false, &pending);
        drop(rj);
        assert_eq!(journal.revisions(), 2);
        let path = journal.path().expect("created").to_path_buf();
        drop(journal);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pending.load(Ordering::Acquire) > 0 {
            assert!(std::time::Instant::now() < deadline, "flush never drained");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let text = std::fs::read_to_string(&path).expect("read journal");
        let count = |needle: &str| text.lines().filter(|l| l.contains(needle)).count();
        assert_eq!(count("\"hashin\":\"ha\""), 1, "{text}");
        assert_eq!(count("\"hashin\":\"ha2\""), 1, "{text}");
        assert_eq!(count("\"t\":\"Scratch\""), 1, "{text}");
        // Requests that recorded nothing new and ended cleanly write nothing,
        // not even an `End`.
        assert_eq!(count("\"t\":\"End\""), 2, "{text}");
        let j = read_journal(&path).expect("read");
        assert_eq!(j.incomplete, None);
        assert_eq!(j.scratch, vec![scratch]);
    }

    #[test]
    fn a_hash_suffixed_key_segment_round_trips() {
        // Names and argument values that `key_segment` cannot use verbatim
        // (too long, unsafe characters), plus a value the formatted `Addr`
        // would not round-trip (a `"`).
        let long = "n".repeat(300);
        let originals = [
            addr("p/q", &long, &[]),
            addr("p", "t", &[("os", "linux x\"y"), ("empty", "")]),
            addr("weird pkg/ü", "a:b", &[("k", "a/b,c|d")]),
        ];
        let j = record_and_read(|rj| {
            for (i, a) in originals.iter().enumerate() {
                rj.record_revision(a, &format!("h{i}"));
            }
        });
        let read: Vec<&Addr> = j.revisions.iter().map(|r| &r.addr).collect();
        assert_eq!(read, originals.iter().collect::<Vec<_>>());
        for (orig, rec) in originals.iter().zip(&j.revisions) {
            let key = crate::engine::RemoteCacheSet::revision_prefix(orig, &rec.hashin);
            // Each original takes the hash-suffix path (`--<hash>`).
            assert!(
                key.contains("--"),
                "{orig} must take the hash-suffix path: {key}"
            );
            assert_eq!(
                crate::engine::RemoteCacheSet::revision_prefix(&rec.addr, &rec.hashin),
                key,
                "{orig} lands under the key it was written for",
            );
        }
    }

    #[test]
    fn one_incomplete_journal_makes_the_run_incomplete() {
        let dir = tempfile::tempdir().expect("dir");
        let clean1 = read_journal(&write(
            dir.path(),
            "1.log",
            &format!("{HEADER}{REV_A}{END_OK}{TRAILER}"),
        ))
        .expect("read 1");
        let clean2 = read_journal(&write(
            dir.path(),
            "2.log",
            &format!("{HEADER}{REV_A}{REV_B}{END_OK}{TRAILER}"),
        ))
        .expect("read 2");
        let complete = WorkingSet::from_journals([clean1.clone(), clean2.clone()]);
        assert!(complete.is_complete());
        assert_eq!(
            complete.revisions.len(),
            2,
            "a revision in two journals once"
        );

        let no_trailer = read_journal(&write(
            dir.path(),
            "3.log",
            &format!("{HEADER}{REV_B}{END_OK}"),
        ))
        .expect("read 3");
        let ws = WorkingSet::from_journals([clean1, no_trailer, clean2]);
        assert!(!ws.is_complete());
        assert_eq!(
            ws.incomplete,
            vec![(dir.path().join("3.log"), Incomplete::NoTrailer)]
        );
        assert_eq!(ws.journals.len(), 3);
        assert!(!ws.is_empty());
    }

    /// `HEPH_CACHE_SNAPSHOT` wins over `cacheSnapshot.dir`, an empty value
    /// from either is off, a relative dir is under the (normalized) root,
    /// and `resolve` alone never consults the environment.
    #[test]
    fn env_then_config() {
        use crate::engine::config::ConfigYamlExt as _;
        use std::ffi::OsStr;

        let tmp = tempfile::tempdir().expect("root");
        let root = hconfig::normalize(tmp.path()).expect("normalize root");
        let yaml = |s: &str| -> hconfig::ConfigYaml { serde_yaml::from_str(s).expect("yaml") };
        let snap = |cfg: &hconfig::ConfigYaml, env: Option<&str>| {
            // Through a root spelled with a `.`, as `HEPH_CWD=.` can give.
            cfg.resolve_with_env(&root.join("."), env.map(OsStr::new))
                .expect("resolve")
                .cache_snapshot
        };

        let configured = yaml("cacheSnapshot:\n  dir: ci/../.snap\n");
        let off = yaml("cacheSnapshot:\n  dir: \"\"\n");
        let none = yaml("{}\n");

        assert_eq!(snap(&configured, None), Some(root.join(".snap")));
        assert_eq!(snap(&configured, Some("env")), Some(root.join("env")));
        assert_eq!(snap(&configured, Some("")), None);
        assert_eq!(snap(&off, None), None);
        assert_eq!(snap(&off, Some("env")), Some(root.join("env")));
        assert_eq!(snap(&none, None), None);
        assert_eq!(snap(&none, Some("/abs/s")), Some(PathBuf::from("/abs/s")));
        assert_eq!(
            configured.resolve(&root).expect("resolve").cache_snapshot,
            Some(root.join(".snap")),
            "resolve is resolve_with_env with no environment"
        );
        // Resolving the dir never turns recording on: that is the front end's
        // call, per engine.
        assert!(
            !configured
                .resolve(&root)
                .expect("resolve")
                .record_working_set
        );
    }
}
