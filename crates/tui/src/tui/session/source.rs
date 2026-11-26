//! Where a viewer gets sessions from.
//!
//! [`SessionSource`] is the seam a future daemon slots into: `heph session`
//! takes a list of sources and merges them, and only [`FileSource`] — the
//! session directories under `~/.heph/sessions` — exists today. Nothing in a
//! viewer depends on there being files.
//!
//! A source never writes, with one exception the design names (A13): asking a
//! session for its in-flight report drops `inflight.request` in its directory.
//! It never signals a process and never cleans anything up.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use super::format::{
    CAPABILITY_INFLIGHT, SessionMeta, SessionRecord, SessionRecordKind, SessionSnapshot, files,
};
use super::id::SessionId;
use super::lockfile;
use super::machine::{MachineSummary, NoneReason};
use super::store::{self, OpenError, RecordCursor};

/// How often a follower looks for new records (and at the lock).
pub const FOLLOW_TICK: Duration = Duration::from_millis(100);

/// Records a follower reads before handing them on, so a slow consumer holds
/// the follower back rather than a whole recording buffering in memory.
pub(crate) const FOLLOW_BATCH: usize = 256;

/// What a viewer can say about a session's recording.
#[derive(Debug, Clone)]
pub enum EntryMeta {
    Known(Box<SessionMeta>),
    /// Recorded in a format this heph does not know: a placeholder, never
    /// decoded further.
    TooNew {
        found: u32,
        supported: u32,
    },
}

/// One session as a source lists it: its session row and `status` row, never
/// its records.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: SessionId,
    pub meta: EntryMeta,
    /// The owner held its lock when probed. The probe comes before any row is
    /// read, so an `End` written before the lock was released is in what was.
    pub live: bool,
    /// The latest snapshot (the `status` row), when there is one this heph
    /// reads.
    pub status: Option<SessionSnapshot>,
    /// A `status` row is there but this heph cannot read it (a newer
    /// snapshot version, or a corrupt one). An ended session's state is then
    /// unknown, not lost: its `End` may well be recorded.
    pub status_unreadable: bool,
}

impl SessionEntry {
    pub fn known_meta(&self) -> Option<&SessionMeta> {
        match &self.meta {
            EntryMeta::Known(m) => Some(m),
            EntryMeta::TooNew { .. } => None,
        }
    }
}

/// One item of a [`SessionSource::subscribe`] stream.
#[derive(Debug, Clone)]
pub enum SessionItem {
    /// A record, in the order the owner wrote it. Boxed: a record is an order
    /// of magnitude bigger than the other items.
    Record(Box<SessionRecord>),
    /// Everything recorded so far has been sent. `skipped`: lines that did not
    /// decode, so far.
    CaughtUp { skipped: u64 },
    /// The session is over; nothing follows. `lost`: the owner went away
    /// without writing `End` (killed, or crashed), so `exit` is `None` and the
    /// last events may be missing.
    Ended { exit: Option<i32>, lost: bool },
}

/// A source of sessions. See the module docs.
#[async_trait]
pub trait SessionSource: Send + Sync + std::fmt::Debug {
    /// Every session this source knows, live or ended, unsorted.
    async fn list(&self) -> anyhow::Result<Vec<SessionEntry>>;

    /// One session, or `None` when this source does not have it.
    async fn snapshot(&self, id: SessionId) -> anyhow::Result<Option<SessionEntry>>;

    /// Every record so far, then [`SessionItem::CaughtUp`], then each new record
    /// as it is written, until [`SessionItem::Ended`]. Dropping the stream stops
    /// the follow.
    async fn subscribe(
        &self,
        id: SessionId,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<SessionItem>>>;

    /// Ask the session for its in-flight report and wait up to `timeout` for
    /// it. Refused when the session does not advertise
    /// [`CAPABILITY_INFLIGHT`] or is not live.
    async fn inflight(&self, id: SessionId, timeout: Duration) -> anyhow::Result<String>;

    /// What `show` reads from the session's explorer rows: its per-pool
    /// summaries and its machine figures. From a source that keeps no
    /// recording, no pools and nothing sampled.
    async fn summaries(&self, id: SessionId) -> anyhow::Result<Summaries> {
        let _ = id;
        Ok(Summaries::default())
    }

    /// How a viewer of session `attached` names a lock holder's pid. Empty:
    /// the attached session's own `LockHolder` records fill it in as the
    /// feed reads them (D6), so a viewer names a holder exactly as the run
    /// did, and a pid with no record falls back to the pid.
    fn holder_names(&self, attached: SessionId) -> super::HolderNames {
        let _ = attached;
        super::HolderNames::default()
    }
}

/// A session's summaries, read from one open of its recording.
#[derive(Debug, Clone)]
pub struct Summaries {
    /// Per-pool summaries (D30), by pool name. Empty while the session
    /// runs: they are written at its end.
    pub pools: Vec<store::PoolSummary>,
    /// The machine it ran on (PR 5).
    pub machine: MachineSummary,
}

impl Default for Summaries {
    fn default() -> Self {
        Self {
            pools: Vec::new(),
            machine: MachineSummary::none(NoneReason::NoSamples),
        }
    }
}

impl Summaries {
    /// Read from the recording `conn` holds.
    pub fn read(conn: &rusqlite::Connection) -> anyhow::Result<Self> {
        let mut pools = store::read_pools(conn).context("reading the pool usage")?;
        if !pools.is_empty() {
            let mut vs = super::machine::read_vs_machine(conn)
                .context("setting the pools against the machine")?;
            for p in &mut pools {
                p.machine = vs.remove(&p.pool);
            }
        }
        Ok(Self {
            pools,
            machine: super::machine::read_machine(conn).context("reading the machine figures")?,
        })
    }
}

/// Where a repairing [`FileSource`] says what it does: a full stderr line.
pub type Note = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// The sessions recorded under one directory (`~/.heph/sessions`).
#[derive(Clone)]
pub struct FileSource {
    root: PathBuf,
    /// A command that repairs what it reads (D14): a recording whose
    /// explorer rows an older heph derived is re-derived before they are
    /// read, said through this. `None`: a viewer that never writes.
    repair: Option<Note>,
}

impl std::fmt::Debug for FileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSource")
            .field("root", &self.root)
            .field("repair", &self.repair.is_some())
            .finish()
    }
}

impl FileSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            repair: None,
        }
    }

    /// This source, re-deriving an older derivation's recording before it
    /// reads its explorer rows, and saying so through `note`.
    #[must_use]
    pub fn repairing(mut self, note: Note) -> Self {
        self.repair = Some(note);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, id: SessionId) -> PathBuf {
        self.root.join(id.encode())
    }
}

#[async_trait]
impl SessionSource for FileSource {
    async fn list(&self) -> anyhow::Result<Vec<SessionEntry>> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || list_dir(&root))
            .await
            .context("listing sessions")?
    }

    async fn snapshot(&self, id: SessionId) -> anyhow::Result<Option<SessionEntry>> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_entry(&root, id))
            .await
            .context("reading a session")?
    }

    async fn subscribe(
        &self,
        id: SessionId,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<SessionItem>>> {
        let root = self.root.clone();
        let dir = self.dir(id);
        let (dir, cursor) = tokio::task::spawn_blocking(move || open_follow(&root, dir, id))
            .await
            .context("opening a session to follow")??;
        // Bounded, so a slow consumer holds the follower back rather than
        // buffering a whole recording in memory.
        let (tx, mut rx) = tokio::sync::mpsc::channel(FOLLOW_BATCH);
        std::thread::Builder::new()
            .name("heph-session-follow".to_owned())
            .spawn(move || follow(&dir, cursor, &tx))
            .context("starting the session follower")?;
        Ok(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed())
    }

    async fn inflight(&self, id: SessionId, timeout: Duration) -> anyhow::Result<String> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || request_inflight(&root, id, timeout))
            .await
            .context("asking for the in-flight report")?
    }

    async fn summaries(&self, id: SessionId) -> anyhow::Result<Summaries> {
        let root = self.root.clone();
        let dir = self.dir(id);
        let repair = self.repair.clone();
        tokio::task::spawn_blocking(move || {
            // The waiting split reads the samples by name: a repairing
            // command builds the `(name, time_us)` index on this first need,
            // as `query` does.
            let opened = match &repair {
                Some(note) => {
                    open_repaired_with(&root, &dir, store::Need { indexes: true }, note.as_ref())
                }
                None => store::open_local(&root, &dir).map_err(anyhow::Error::new),
            }
            .with_context(|| format!("opening session {id}'s recording"))?;
            Summaries::read(&opened.conn)
                .with_context(|| format!("reading session {id}'s summaries"))
        })
        .await
        .context("reading a session's summaries")?
    }
}

/// Open the local recording in `dir` to read its explorer rows, re-deriving
/// it first when an older heph derived them (D14): durably, under its lock,
/// then its catalog rows replaced. When it cannot be re-derived in place (a
/// full disk, a read-only directory, a stray journal, its lock held), the
/// rows are derived into a temp copy for this command. A newer derivation is
/// read as it is.
#[cfg(test)]
pub(crate) fn open_repaired(
    root: &Path,
    dir: &Path,
    note: &(dyn Fn(&str) + Send + Sync),
) -> anyhow::Result<store::Opened> {
    open_repaired_with(root, dir, store::Need::default(), note)
}

/// [`open_repaired`], also giving the recording what `need` asks (a query's
/// indexes, built under the same lock on its first need). Only a
/// re-derivation is said through `note` (the session's id and why) and
/// falls back to a temp copy, said too with the reason: a recording that
/// cannot be indexed is read without its indexes.
pub(crate) fn open_repaired_with(
    root: &Path,
    dir: &Path,
    need: store::Need,
    note: &(dyn Fn(&str) + Send + Sync),
) -> anyhow::Result<store::Opened> {
    match store::pending_local(dir, need) {
        Ok(Some(p)) if p.any() => {
            let id = dir
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if p.rederive {
                note(&format!(
                    "heph: re-deriving session {id}: {}\n",
                    p.rederive_cause()
                ));
            }
            match store::upgrade_local(dir, need) {
                Ok(store::Upgraded::Done) if !p.rederive => {}
                Ok(store::Upgraded::Done) => {
                    if let Err(e) = super::catalog::replace(root, dir) {
                        tracing::debug!(
                            dir = %dir.display(),
                            error = %format!("{e:#}"),
                            "cannot replace a re-derived session's catalog rows"
                        );
                    }
                }
                Ok(store::Upgraded::Current) => {}
                failed if p.rederive => {
                    let why = match failed {
                        Err(e) => format!("{e:#}"),
                        Ok(_) => "its lock stayed held".to_owned(),
                    };
                    note(&format!(
                        "heph: session {id} cannot be re-derived in place ({why}); \
                         reading a re-derived temp copy for this command\n"
                    ));
                    return store::derived_copy(root, &dir.join(store::FILE), need);
                }
                failed => tracing::debug!(
                    dir = %dir.display(),
                    outcome = ?failed.map_err(|e| format!("{e:#}")),
                    "reading a recording without its indexes"
                ),
            }
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(
            dir = %dir.display(),
            error = %format!("{e:#}"),
            "cannot tell whether a recording needs re-deriving"
        ),
    }
    store::open_local(root, dir).map_err(anyhow::Error::new)
}

/// `subscribe`'s blocking half: open the recording, refusing a format this
/// heph cannot read before reading any record.
fn open_follow(
    root: &Path,
    dir: PathBuf,
    id: SessionId,
) -> anyhow::Result<(PathBuf, RecordCursor)> {
    let opened = store::open_local(root, &dir)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("opening session {id}'s recording"))?;
    Ok((dir, RecordCursor::new(opened)))
}

pub(crate) fn list_dir(root: &Path) -> anyhow::Result<Vec<SessionEntry>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        // Not an id (a `.removing-…` tomb, anything else): not a session.
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| SessionId::parse(s).ok())
        else {
            continue;
        };
        match read_entry(root, id) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => {}
            Err(e) => tracing::debug!(session = %id, error = %format!("{e:#}"), "skipping session"),
        }
    }
    Ok(out)
}

/// Open the recording (probing the lock first), then read its session row and
/// `status` row. Never its records. `None`: no recording, or not a heph one
/// (a session killed before it created its file, or removed meanwhile).
fn read_entry(root: &Path, id: SessionId) -> anyhow::Result<Option<SessionEntry>> {
    let dir = root.join(id.encode());
    let opened = match store::open_local(root, &dir) {
        Ok(opened) => opened,
        Err(OpenError::Missing | OpenError::NotASession) => return Ok(None),
        Err(OpenError::TooNew { found }) => {
            let live = lockfile::is_held(&dir.join(files::LOCK))
                .with_context(|| format!("probing session {id}'s lock"))?;
            return Ok(Some(SessionEntry {
                id,
                meta: EntryMeta::TooNew {
                    found,
                    supported: store::FORMAT,
                },
                live,
                status: None,
                status_unreadable: false,
            }));
        }
        Err(OpenError::Other(e)) => {
            return Err(e.context(format!("opening session {id}'s recording")));
        }
    };
    let meta = store::read_meta(&opened.conn)
        .with_context(|| format!("reading session {id}'s session row"))?;
    let mut status_unreadable = false;
    let status = match store::read_status(&opened.conn)
        .with_context(|| format!("reading session {id}'s status"))?
    {
        Some(json) => match SessionSnapshot::decode(json.as_bytes()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(session = %id, error = %e, "ignoring session status");
                status_unreadable = true;
                None
            }
        },
        None => None,
    };
    Ok(Some(SessionEntry {
        id,
        meta: EntryMeta::Known(Box::new(meta)),
        live: opened.live,
        status,
        status_unreadable,
    }))
}

/// The follower thread behind [`FileSource::subscribe`]: read what is there,
/// say so, then poll every [`FOLLOW_TICK`] — lock first, then the tail — until
/// `End`, a free lock, or the stream is dropped.
///
/// Records are read a batch at a time and sent after. A batch holding `End`
/// drops the cursor before it is sent: the owner seals the file once `End`
/// is committed, and a reader still open on it (behind a slow consumer)
/// would hold the seal up.
fn follow(
    dir: &Path,
    cursor: RecordCursor,
    tx: &tokio::sync::mpsc::Sender<anyhow::Result<SessionItem>>,
) {
    let mut cursor = Some(cursor);
    let lock = dir.join(files::LOCK);
    let mut caught_up = false;
    let mut skipped = 0;
    let mut batch: Vec<SessionRecord> = Vec::with_capacity(FOLLOW_BATCH);
    loop {
        let held = match lockfile::is_held(&lock) {
            Ok(held) => held,
            Err(e) => {
                drop(tx.blocking_send(Err(anyhow::Error::new(e).context("probing the lock"))));
                return;
            }
        };
        let mut end = None;
        let mut more = false;
        if let Some(c) = cursor.as_mut() {
            while batch.len() < FOLLOW_BATCH && end.is_none() {
                match c.next_record() {
                    Ok(Some(record)) => {
                        if let Some(SessionRecordKind::End { exit_code, .. }) = &record.session {
                            end = Some(*exit_code);
                        }
                        batch.push(record);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        drop(tx.blocking_send(Err(
                            anyhow::Error::new(e).context("reading the session's records"),
                        )));
                        return;
                    }
                }
            }
            more = end.is_none() && batch.len() >= FOLLOW_BATCH;
            skipped = c.skipped();
        }
        if end.is_some() {
            cursor = None;
        }
        for record in batch.drain(..) {
            if tx
                .blocking_send(Ok(SessionItem::Record(Box::new(record))))
                .is_err()
            {
                return;
            }
        }
        if more {
            continue;
        }
        if !caught_up {
            caught_up = true;
            if tx
                .blocking_send(Ok(SessionItem::CaughtUp { skipped }))
                .is_err()
            {
                return;
            }
        }
        let ended = match end {
            Some(exit) => Some(SessionItem::Ended { exit, lost: false }),
            None if !held => Some(SessionItem::Ended {
                exit: None,
                lost: true,
            }),
            None => None,
        };
        if let Some(item) = ended {
            drop(tx.blocking_send(Ok(item)));
            return;
        }
        if tx.is_closed() {
            return;
        }
        std::thread::sleep(FOLLOW_TICK);
    }
}

/// A13: drop `inflight.request` (temp + rename) and wait for the owner's
/// `InflightWritten` record, then read `inflight.txt`.
fn request_inflight(root: &Path, id: SessionId, timeout: Duration) -> anyhow::Result<String> {
    let dir = &root.join(id.encode());
    let Some(entry) = read_entry(root, id)? else {
        anyhow::bail!("no session {id}");
    };
    let meta = match &entry.meta {
        EntryMeta::Known(m) => m,
        EntryMeta::TooNew { found, supported } => {
            anyhow::bail!(
                "session {id} is recorded in format {found}; this heph reads up to format {supported}"
            );
        }
    };
    if !meta.has_capability(CAPABILITY_INFLIGHT) {
        anyhow::bail!(
            "session {id} was recorded by heph {}, which does not answer in-flight requests",
            meta.heph_version
        );
    }
    if !entry.live {
        anyhow::bail!("session {id} has ended; only a running session has an in-flight report");
    }
    // Follow from the current end: only an answer to this request counts.
    let opened = store::open_local(root, dir)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("opening session {id}'s recording"))?;
    let mut reader = RecordCursor::new(opened);
    reader
        .skip_to_end()
        .with_context(|| format!("reading session {id}'s last record"))?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating the in-flight request in {}", dir.display()))?;
    io::Write::write_all(tmp.as_file_mut(), b"").context("writing the in-flight request")?;
    tmp.persist(dir.join(files::INFLIGHT_REQUEST))
        .map_err(|e| e.error)
        .context("placing the in-flight request")?;

    let deadline = Instant::now() + timeout;
    loop {
        let held = lockfile::is_held(&dir.join(files::LOCK)).context("probing the lock")?;
        while let Some(record) = reader
            .next_record()
            .context("reading the session's records")?
        {
            match record.session {
                Some(SessionRecordKind::InflightWritten) => {
                    return std::fs::read_to_string(dir.join(files::INFLIGHT_REPORT))
                        .with_context(|| format!("reading session {id}'s inflight.txt"));
                }
                Some(SessionRecordKind::End { .. }) => {
                    anyhow::bail!("session {id} ended before it answered");
                }
                _ => {}
            }
        }
        if !held {
            anyhow::bail!("session {id} ended before it answered");
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "session {id} did not answer within {}s (pid {} may be wedged before its diag thread runs)",
                timeout.as_secs(),
                meta.owner_pid
            );
        }
        std::thread::sleep(FOLLOW_TICK);
    }
}
