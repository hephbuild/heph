//! A recording opened for viewing: [`Recording::Local`], from this user's
//! sessions directory, or [`Recording::Foreign`], from a path or URL.
//!
//! Spec I7: a foreign recording is read only from itself. [`Foreign`] holds
//! the open file and nothing else (no sessions root, no home, no workspace),
//! so it cannot probe a lock, scan pids, read a run log, read `inflight` or
//! read the artifact store: its state comes from its session row, its
//! notices from its `notices` table.

use std::path::{Path, PathBuf};

use hcore::approval_notice::{NOTICE_UNAVAILABLE, NoticeRoots, ReadNotice};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::format::{ApprovalPromptNotice, SessionMeta};
use super::hephsession::{self, ForeignError, ForeignFile};
use super::store::{self, OpenError, Opened};

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
    /// `addr`.
    pub fn recorded_notice(
        &self,
        addr: &str,
        seq: u64,
        name: &str,
    ) -> rusqlite::Result<Option<String>> {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        self.file
            .conn
            .query_row(
                "SELECT text FROM notices WHERE addr = ?1 AND seq = ?2 AND name = ?3
                 ORDER BY rowid LIMIT 1",
                params![addr, seq, name],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// The root failure logs an export recorded for `addr`, in order: each
    /// log's kept tail and how many bytes were cut before it.
    pub fn failure_logs(&self, addr: &str) -> rusqlite::Result<Vec<(Vec<u8>, u64)>> {
        let mut stmt = self.file.conn.prepare(
            "SELECT log, cut_bytes FROM failure_logs WHERE addr = ?1 ORDER BY nth, rowid",
        )?;
        stmt.query_map(params![addr], |r| {
            Ok((
                r.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(1)?
                    .and_then(|c| u64::try_from(c).ok())
                    .unwrap_or(0),
            ))
        })?
        .collect()
    }
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
