//! A recording opened for viewing: [`Recording::Local`], from this user's
//! sessions directory, or [`Recording::Foreign`], from a path or URL.
//!
//! Spec I7: a foreign recording is read only from itself. [`Foreign`] holds
//! the open file and nothing else (no sessions root, no home, no workspace),
//! so it cannot probe a lock, scan pids, read a run log, read `inflight` or
//! read the artifact store: its state comes from its session row, its
//! notices from its `notices` table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use hcore::approval_notice::{NOTICE_READ_CAP, NOTICE_UNAVAILABLE, NoticeRoots, ReadNotice};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::export::{FAILURE_LOG_CAP, FAILURE_LOGS_CAP};
use super::format::{ApprovalPromptNotice, SessionMeta, SessionRecordKind, SessionSnapshot};
use super::hephsession::{self, ForeignError, ForeignFile};
use super::id::SessionId;
use super::source::{EntryMeta, FOLLOW_BATCH, SessionEntry, SessionItem, SessionSource};
use super::store::{self, OpenError, Opened, RecordCursor};

/// A recording opened for viewing.
#[derive(Debug)]
pub enum Recording {
    Local(Local),
    Foreign(Foreign),
}

/// A recording in this user's sessions directory.
#[derive(Debug)]
pub struct Local {
    /// The session directory.
    pub dir: PathBuf,
    pub opened: Opened,
}

/// A recording opened by path or URL. Holds no handle to the local disk.
#[derive(Debug)]
pub struct Foreign {
    file: ForeignFile,
    /// The download a URL was fetched into, removed with this.
    download: Option<tempfile::TempPath>,
    /// What the user named, for messages: the path, or the URL without its
    /// credentials.
    origin: String,
}

/// Where a recording stands, as a viewer shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    /// Local only: its owner holds the lock.
    Live,
    /// Sealed with an end: `exit_code` and `panicked` as the owner wrote.
    Finished {
        exit_code: Option<i32>,
        panicked: bool,
    },
    /// An export taken while its session ran (E3): the session row says
    /// `snapshot=1` and the copy has no end.
    Snapshot,
    /// No end recorded: not sealed, or sealed by cleanup after its owner
    /// died. A foreign recording never says live: it cannot know.
    Incomplete,
}

impl RecordingState {
    /// What a viewer says of a recording that is not a whole session, on its
    /// first line: `None` for a live or finished one.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Self::Snapshot => Some("snapshot of a running session"),
            Self::Incomplete => Some("incomplete"),
            Self::Live | Self::Finished { .. } => None,
        }
    }
}

impl Recording {
    /// The local recording in `dir`, under the sessions `root`.
    pub fn open_local(root: &Path, dir: &Path) -> Result<Self, OpenError> {
        Ok(Self::Local(Local {
            dir: dir.to_path_buf(),
            opened: store::open_local(root, dir)?,
        }))
    }

    /// The `.hephsession` (or bare `session.db`) at `path`, decompressing
    /// into `tmp_dir` when it must.
    pub fn open_foreign(path: &Path, tmp_dir: &Path) -> Result<Self, ForeignError> {
        Ok(Self::Foreign(Foreign {
            file: hephsession::open(path, tmp_dir)?,
            download: None,
            origin: path.display().to_string(),
        }))
    }

    /// A `.hephsession` downloaded into `download` from `origin` (the URL,
    /// without credentials). The download lives as long as the recording.
    pub fn open_download(
        download: tempfile::NamedTempFile,
        origin: String,
        tmp_dir: &Path,
    ) -> Result<Self, ForeignError> {
        let download = download.into_temp_path();
        let file = hephsession::open(&download, tmp_dir).map_err(|e| match e {
            // The temp path means nothing to the user: name the URL.
            ForeignError::Io(e) => ForeignError::Io(e.context(format!("reading {origin}"))),
            other => other,
        })?;
        Ok(Self::Foreign(Foreign {
            file,
            download: Some(download),
            origin,
        }))
    }

    pub fn is_foreign(&self) -> bool {
        matches!(self, Self::Foreign(_))
    }

    pub fn conn(&self) -> &Connection {
        match self {
            Self::Local(l) => &l.opened.conn,
            Self::Foreign(f) => &f.file.conn,
        }
    }

    pub fn meta(&self) -> rusqlite::Result<SessionMeta> {
        store::read_meta(self.conn())
    }

    /// Local: live while the lock was held when opened; otherwise, and
    /// always for a foreign recording, from the session row.
    pub fn state(&self) -> rusqlite::Result<RecordingState> {
        if let Self::Local(l) = self
            && l.opened.live
        {
            return Ok(RecordingState::Live);
        }
        row_state(self.conn())
    }

    /// The text of `notice`, notice `notice.name` of approval request `seq`
    /// of `addr`. Local: read from the store under `roots` through `read`.
    /// Foreign: from the `notices` table the export filled; neither `roots`
    /// nor `read` is touched. [`NOTICE_UNAVAILABLE`] when there is none.
    pub fn notice_text(
        &self,
        addr: &str,
        seq: u64,
        notice: &ApprovalPromptNotice,
        roots: &NoticeRoots,
        read: ReadNotice,
    ) -> String {
        let text = match self {
            Self::Local(_) => notice.read(roots, read).ok(),
            Self::Foreign(f) => f
                .recorded_notice(addr, seq, &notice.name)
                .unwrap_or_else(|e| {
                    tracing::debug!(error = %e, "cannot read a recorded notice");
                    None
                }),
        };
        text.unwrap_or_else(|| NOTICE_UNAVAILABLE.to_string())
    }
}

impl Foreign {
    /// What was opened: the path the user named.
    pub fn source(&self) -> &Path {
        &self.file.source
    }

    /// The text an export recorded for notice `name` of request `seq` of
    /// `addr`, cut to `NOTICE_READ_CAP` bytes.
    pub fn recorded_notice(
        &self,
        addr: &str,
        seq: u64,
        name: &str,
    ) -> rusqlite::Result<Option<String>> {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        let cap = i64::try_from(NOTICE_READ_CAP).unwrap_or(i64::MAX);
        let text = self
            .file
            .conn
            .query_row(
                "SELECT CAST(substr(CAST(text AS BLOB), 1, ?4) AS BLOB) FROM notices
                 WHERE addr = ?1 AND seq = ?2 AND name = ?3 ORDER BY rowid LIMIT 1",
                params![addr, seq, name, cap],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten();
        Ok(text.map(|t| String::from_utf8_lossy(&t).into_owned()))
    }

    /// The root failure logs an export recorded for `addr`, in order: each
    /// log's kept tail and how many bytes were cut before it, cut as a
    /// viewer cuts them.
    pub fn failure_logs(&self, addr: &str) -> rusqlite::Result<Vec<(Vec<u8>, u64)>> {
        Ok(
            read_failure_logs(&self.file.conn, Some(addr), FAILURE_LOGS_CAP)?
                .into_iter()
                .map(|l| (l.log, l.cut_bytes))
                .collect(),
        )
    }

    /// What the user named: the path, or the URL without its credentials.
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl Recording {
    /// The foreign recording as a [`SessionSource`] for the viewers; a local
    /// one is read through [`super::FileSource`] instead.
    pub fn into_source(self) -> anyhow::Result<ForeignSource> {
        match self {
            Self::Foreign(f) => ForeignSource::new(f),
            Self::Local(l) => anyhow::bail!(
                "{} is a local recording: read it through the sessions directory",
                l.dir.display()
            ),
        }
    }
}

/// The most notice text a viewer loads from one foreign recording, in all:
/// an export holds at most `NOTICE_READ_CAP` per notice, but a crafted file
/// can hold any number of them.
const RECORDED_NOTICES_CAP: usize = 64 << 20;

/// The approval notices a foreign recording carries, by (addr, request seq,
/// notice name).
pub type RecordedNotices = Arc<HashMap<(String, u64, String), String>>;

/// One root failure's log, as an export recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureLog {
    pub addr: String,
    /// The log's last bytes, at most `FAILURE_LOG_CAP`.
    pub log: Vec<u8>,
    /// What came before them and was left out.
    pub cut_bytes: u64,
}

/// A foreign recording as a [`SessionSource`] (I7): the one session it holds,
/// read only from itself. Never live: its records are replayed once, by the
/// first `subscribe`, and the stream then ends, with `lost` when the
/// recording has no `End` (a snapshot, or an incomplete recording).
#[derive(Debug)]
pub struct ForeignSource {
    entry: SessionEntry,
    origin: String,
    state: RecordingState,
    pools: Vec<store::PoolSummary>,
    notices: RecordedNotices,
    failure_logs: Vec<FailureLog>,
    cursor: Mutex<Option<RecordCursor>>,
}

impl ForeignSource {
    fn new(f: Foreign) -> anyhow::Result<Self> {
        let conn = &f.file.conn;
        let what = || format!("reading {}", f.origin);
        let meta = store::read_meta(conn).with_context(what)?;
        let state = row_state(conn).with_context(what)?;
        let (status, status_unreadable) = match store::read_status(conn).with_context(what)? {
            Some(json) => match SessionSnapshot::decode(json.as_bytes()) {
                Ok(s) => (Some(s), false),
                Err(e) => {
                    tracing::debug!(error = %e, "ignoring a recording's status");
                    (None, true)
                }
            },
            None => (None, false),
        };
        let pools = store::read_pools(conn).with_context(what)?;
        let notices = read_notices(conn, RECORDED_NOTICES_CAP).with_context(what)?;
        let failure_logs = read_failure_logs(conn, None, FAILURE_LOGS_CAP).with_context(what)?;
        let Foreign {
            file,
            download,
            origin,
        } = f;
        let (conn, decompressed) = file.into_parts();
        let download = download.and_then(|d| match d.keep() {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::debug!(error = %e, "cannot keep a downloaded session");
                None
            }
        });
        let files = decompressed.into_iter().chain(download).collect();
        Ok(Self {
            entry: SessionEntry {
                id: meta.id,
                meta: EntryMeta::Known(Box::new(meta)),
                live: false,
                status,
                status_unreadable,
            },
            origin,
            state,
            pools,
            notices: Arc::new(notices),
            failure_logs,
            cursor: Mutex::new(Some(RecordCursor::new(Opened::foreign(conn, files)))),
        })
    }

    /// The session in the recording.
    pub fn entry(&self) -> &SessionEntry {
        &self.entry
    }

    /// What the user named: the path, or the URL without its credentials.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn state(&self) -> RecordingState {
        self.state
    }

    pub fn notices(&self) -> RecordedNotices {
        Arc::clone(&self.notices)
    }

    /// Every root failure log the export recorded, in order.
    pub fn failure_logs(&self) -> &[FailureLog] {
        &self.failure_logs
    }
}

#[async_trait]
impl SessionSource for ForeignSource {
    async fn list(&self) -> anyhow::Result<Vec<SessionEntry>> {
        Ok(vec![self.entry.clone()])
    }

    async fn snapshot(&self, id: SessionId) -> anyhow::Result<Option<SessionEntry>> {
        Ok((id == self.entry.id).then(|| self.entry.clone()))
    }

    async fn subscribe(
        &self,
        id: SessionId,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<SessionItem>>> {
        anyhow::ensure!(id == self.entry.id, "no session {id} in {}", self.origin);
        let cursor = self
            .cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .with_context(|| format!("{} is already being read", self.origin))?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(FOLLOW_BATCH);
        std::thread::Builder::new()
            .name("heph-session-replay".to_owned())
            .spawn(move || replay(cursor, &tx))
            .context("starting the recording's reader")?;
        Ok(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed())
    }

    async fn inflight(&self, _id: SessionId, _timeout: Duration) -> anyhow::Result<String> {
        anyhow::bail!(
            "{} is a recording opened from a file or URL: only a running local session has an \
             in-flight report",
            self.origin
        )
    }

    async fn pools(&self, id: SessionId) -> anyhow::Result<Vec<store::PoolSummary>> {
        Ok(if id == self.entry.id {
            self.pools.clone()
        } else {
            Vec::new()
        })
    }
}

/// Send every record of `cursor` up to its `End`, then `CaughtUp`, then
/// `Ended`. A recording with no `End` ends `lost`: nothing more will come.
fn replay(mut cursor: RecordCursor, tx: &tokio::sync::mpsc::Sender<anyhow::Result<SessionItem>>) {
    let mut end = None;
    loop {
        match cursor.next_record() {
            Ok(Some(record)) => {
                if let Some(SessionRecordKind::End { exit_code, .. }) = &record.session {
                    end = Some(*exit_code);
                }
                if tx
                    .blocking_send(Ok(SessionItem::Record(Box::new(record))))
                    .is_err()
                {
                    return;
                }
                if end.is_some() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                drop(tx.blocking_send(Err(
                    anyhow::Error::new(e).context("reading the recording's records"),
                )));
                return;
            }
        }
    }
    let skipped = cursor.skipped();
    drop(cursor);
    if tx
        .blocking_send(Ok(SessionItem::CaughtUp { skipped }))
        .is_err()
    {
        return;
    }
    drop(tx.blocking_send(Ok(SessionItem::Ended {
        exit: end.flatten(),
        lost: end.is_none(),
    })));
}

/// What one row costs a budget beyond its bytes, so a row of empty strings
/// (or of NULLs, skipped) is not free.
const ROW_COST: usize = 64;

/// The `notices` table, each text cut to `NOTICE_READ_CAP` bytes, until
/// `budget` bytes in all: every row is charged its addr, name, text and
/// [`ROW_COST`].
fn read_notices(
    conn: &Connection,
    budget: usize,
) -> rusqlite::Result<HashMap<(String, u64, String), String>> {
    let mut stmt = conn.prepare(
        "SELECT addr, seq, name, CAST(substr(CAST(text AS BLOB), 1, ?1) AS BLOB) FROM notices \
         ORDER BY rowid",
    )?;
    let cap = i64::try_from(NOTICE_READ_CAP).unwrap_or(i64::MAX);
    let mut rows = stmt.query(params![cap])?;
    let mut out = HashMap::new();
    let mut left = budget;
    while let Some(r) = rows.next()? {
        let addr = r.get::<_, Option<String>>(0)?;
        let seq = r.get::<_, Option<i64>>(1)?;
        let name = r.get::<_, Option<String>>(2)?;
        let text = r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default();
        let cost = [
            addr.as_ref().map_or(0, String::len),
            name.as_ref().map_or(0, String::len),
        ]
        .iter()
        .fold(text.len().saturating_add(ROW_COST), |a, b| {
            a.saturating_add(*b)
        });
        let Some(rest) = left.checked_sub(cost) else {
            break;
        };
        left = rest;
        let (Some(addr), Some(seq), Some(name)) =
            (addr, seq.and_then(|s| u64::try_from(s).ok()), name)
        else {
            continue;
        };
        out.entry((addr, seq, name))
            .or_insert_with(|| String::from_utf8_lossy(&text).into_owned());
    }
    Ok(out)
}

/// The `failure_logs` table (only `addr`'s rows, given one), each log cut to
/// its last `FAILURE_LOG_CAP` bytes, until `budget` bytes in all: every row
/// is charged its addr, kept log and [`ROW_COST`].
fn read_failure_logs(
    conn: &Connection,
    addr: Option<&str>,
    budget: u64,
) -> rusqlite::Result<Vec<FailureLog>> {
    let cap = i64::try_from(FAILURE_LOG_CAP).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT addr, substr(CAST(log AS BLOB), -?1), cut_bytes, length(CAST(log AS BLOB)) \
         FROM failure_logs WHERE ?2 IS NULL OR addr = ?2 ORDER BY nth, rowid",
    )?;
    let mut rows = stmt.query(params![cap, addr])?;
    let mut out = Vec::new();
    let mut left = budget;
    while let Some(r) = rows.next()? {
        let addr = r.get::<_, Option<String>>(0)?;
        let log = r.get::<_, Option<Vec<u8>>>(1)?.unwrap_or_default();
        let kept = log.len() as u64;
        let cost = (ROW_COST as u64)
            .saturating_add(kept)
            .saturating_add(addr.as_ref().map_or(0, |a| a.len() as u64));
        let Some(rest) = left.checked_sub(cost) else {
            break;
        };
        left = rest;
        let Some(addr) = addr else {
            continue;
        };
        let len = r
            .get::<_, Option<i64>>(3)?
            .and_then(|l| u64::try_from(l).ok())
            .unwrap_or(0);
        let cut = r
            .get::<_, Option<i64>>(2)?
            .and_then(|c| u64::try_from(c).ok())
            .unwrap_or(0);
        out.push(FailureLog {
            addr,
            log,
            cut_bytes: cut.saturating_add(len.saturating_sub(kept)),
        });
    }
    Ok(out)
}

/// A recording's state from its session row alone.
fn row_state(conn: &Connection) -> rusqlite::Result<RecordingState> {
    let (sealed, exit_code, panicked, snapshot): (bool, Option<i32>, Option<bool>, bool) = conn
        .query_row(
            "SELECT sealed, exit_code, panicked, snapshot FROM session",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let panicked = panicked.unwrap_or(false);
    Ok(if sealed && (exit_code.is_some() || panicked) {
        RecordingState::Finished {
            exit_code,
            panicked,
        }
    } else if snapshot {
        RecordingState::Snapshot
    } else {
        RecordingState::Incomplete
    })
}

#[cfg(test)]
#[path = "recording_tests.rs"]
mod tests;
