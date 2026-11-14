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

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use super::format::{
    CAPABILITY_INFLIGHT, MetaError, SessionMeta, SessionRecord, SessionRecordKind, SessionSnapshot,
    files,
};
use super::id::SessionId;
use super::lockfile;
use super::reader::RecordReader;

/// How often a follower looks for new records (and at the lock).
pub const FOLLOW_TICK: Duration = Duration::from_millis(100);

/// What a viewer can say about a session's `meta.json`.
#[derive(Debug, Clone)]
pub enum EntryMeta {
    Known(Box<SessionMeta>),
    /// Written by a heph whose directory contract this one does not know: a
    /// placeholder, never decoded further.
    TooNew {
        found: u32,
        supported: u32,
    },
}

/// One session as a source lists it. Never built from `events.ndjson`.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: SessionId,
    pub meta: EntryMeta,
    /// The owner held its lock when probed. The probe comes before any file is
    /// read, so an `End` written before the lock was released is in what was.
    pub live: bool,
    /// The latest snapshot (`status.json`), when there is one this heph reads.
    pub status: Option<SessionSnapshot>,
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
    /// A record, in the order the owner wrote it.
    Record(SessionRecord),
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
}

/// The sessions recorded under one directory (`~/.heph/sessions`).
#[derive(Debug, Clone)]
pub struct FileSource {
    root: PathBuf,
}

impl FileSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        let dir = self.dir(id);
        tokio::task::spawn_blocking(move || read_entry(&dir, id))
            .await
            .context("reading a session")?
    }

    async fn subscribe(
        &self,
        id: SessionId,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<SessionItem>>> {
        let dir = self.dir(id);
        // Refuse a format this heph cannot read before reading any record.
        let meta = std::fs::read(dir.join(files::META))
            .with_context(|| format!("reading session {id}'s meta.json"))?;
        SessionMeta::decode(&meta).with_context(|| format!("session {id}"))?;
        let events = match File::open(dir.join(files::EVENTS)) {
            Ok(f) => Some(f),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("opening session {id}'s events.ndjson"));
            }
        };
        // Bounded, so a slow consumer holds the follower back rather than
        // buffering a whole 64 MiB log in memory.
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        std::thread::Builder::new()
            .name("heph-session-follow".to_owned())
            .spawn(move || follow(&dir, events, &tx))
            .context("starting the session follower")?;
        Ok(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed())
    }

    async fn inflight(&self, id: SessionId, timeout: Duration) -> anyhow::Result<String> {
        let dir = self.dir(id);
        tokio::task::spawn_blocking(move || request_inflight(&dir, id, timeout))
            .await
            .context("asking for the in-flight report")?
    }
}

fn list_dir(root: &Path) -> anyhow::Result<Vec<SessionEntry>> {
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
        match read_entry(&entry.path(), id) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => {}
            Err(e) => tracing::debug!(session = %id, error = %format!("{e:#}"), "skipping session"),
        }
    }
    Ok(out)
}

/// Probe the lock, then read `meta.json` and `status.json`. Never
/// `events.ndjson`. `None`: no usable `meta.json` (a session killed between
/// `mkdir` and its first write, or removed meanwhile).
fn read_entry(dir: &Path, id: SessionId) -> anyhow::Result<Option<SessionEntry>> {
    // First: an `End` written before the release is then in what is read next.
    let live = lockfile::is_held(&dir.join(files::LOCK))
        .with_context(|| format!("probing session {id}'s lock"))?;
    let meta = match std::fs::read(dir.join(files::META)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading session {id}'s meta.json")),
    };
    let meta = match SessionMeta::decode(&meta) {
        Ok(m) => EntryMeta::Known(Box::new(m)),
        Err(MetaError::TooNew { found, supported }) => EntryMeta::TooNew { found, supported },
        Err(e @ MetaError::Invalid(_)) => {
            tracing::debug!(session = %id, error = %e, "skipping session");
            return Ok(None);
        }
    };
    let status = match std::fs::read(dir.join(files::STATUS)) {
        Ok(b) => match SessionSnapshot::decode(&b) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(session = %id, error = %e, "ignoring session status");
                None
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading session {id}'s status.json")),
    };
    Ok(Some(SessionEntry {
        id,
        meta,
        live,
        status,
    }))
}

/// The follower thread behind [`FileSource::subscribe`]: read what is there,
/// say so, then poll every [`FOLLOW_TICK`] — lock first, then the tail — until
/// `End`, a free lock, or the stream is dropped.
fn follow(
    dir: &Path,
    events: Option<File>,
    tx: &tokio::sync::mpsc::Sender<anyhow::Result<SessionItem>>,
) {
    let mut reader = events.map(RecordReader::new);
    let lock = dir.join(files::LOCK);
    let mut caught_up = false;
    loop {
        let held = match lockfile::is_held(&lock) {
            Ok(held) => held,
            Err(e) => {
                drop(tx.blocking_send(Err(anyhow::Error::new(e).context("probing the lock"))));
                return;
            }
        };
        let mut end = None;
        if let Some(reader) = reader.as_mut() {
            loop {
                match reader.next_record() {
                    Ok(Some(record)) => {
                        if let Some(SessionRecordKind::End { exit_code, .. }) = &record.session {
                            end = Some(*exit_code);
                        }
                        if tx.blocking_send(Ok(SessionItem::Record(record))).is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        drop(tx.blocking_send(Err(
                            anyhow::Error::new(e).context("reading events.ndjson"),
                        )));
                        return;
                    }
                }
            }
        }
        if !caught_up {
            caught_up = true;
            let skipped = reader.as_ref().map_or(0, RecordReader::skipped);
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
fn request_inflight(dir: &Path, id: SessionId, timeout: Duration) -> anyhow::Result<String> {
    let Some(entry) = read_entry(dir, id)? else {
        anyhow::bail!("no session {id}");
    };
    let meta = match &entry.meta {
        EntryMeta::Known(m) => m,
        EntryMeta::TooNew { found, supported } => {
            return Err(MetaError::TooNew {
                found: *found,
                supported: *supported,
            })
            .with_context(|| format!("session {id}"));
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
    let mut events = File::open(dir.join(files::EVENTS))
        .with_context(|| format!("opening session {id}'s events.ndjson"))?;
    io::Seek::seek(&mut events, io::SeekFrom::End(0)).context("seeking to the end")?;
    let mut reader = RecordReader::new(events);

    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating the in-flight request in {}", dir.display()))?;
    io::Write::write_all(tmp.as_file_mut(), b"").context("writing the in-flight request")?;
    tmp.persist(dir.join(files::INFLIGHT_REQUEST))
        .map_err(|e| e.error)
        .context("placing the in-flight request")?;

    let deadline = Instant::now() + timeout;
    loop {
        let held = lockfile::is_held(&dir.join(files::LOCK)).context("probing the lock")?;
        while let Some(record) = reader.next_record().context("reading events.ndjson")? {
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
