//! Recordings written without a recorder, for the tests of other crates
//! (`test-support` feature): a session directory as an owner leaves it, live,
//! killed or sealed, holding exactly the records a test names.

use super::format::{SessionMeta, SessionRecord, SessionSnapshot, files};
use super::store;
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// Run `sql` on `dir`'s recording read-write, as damage or another heph
/// would alter it.
pub fn alter(dir: &Path, sql: &str) -> anyhow::Result<()> {
    let path = dir.join(store::FILE);
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(store::BUSY_TIMEOUT)
        .context("setting the busy timeout")?;
    conn.execute_batch(sql)
        .with_context(|| format!("altering {}", path.display()))
}

/// A recording being written, as its owner holds it: the connection open,
/// so the `-wal` and `-shm` exist beside the file.
#[derive(Debug)]
pub struct Recording {
    dir: PathBuf,
    writer: store::Writer,
}

impl Recording {
    /// Create `dir` (and its lock file) with a recording of `meta` holding
    /// `records`, and `status` as its status row. Committed, WAL mode; the
    /// lock is not taken.
    pub fn create(
        dir: &Path,
        meta: &SessionMeta,
        records: &[SessionRecord],
        status: Option<&SessionSnapshot>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = dir.join(files::LOCK);
        std::fs::write(&lock, b"").with_context(|| format!("creating {}", lock.display()))?;
        let conn = store::create(dir, meta, &store::Provenance::default(), false)?;
        let mut rec = Self {
            dir: dir.to_path_buf(),
            writer: store::Writer::new(conn, meta.id),
        };
        rec.append(records)?;
        if let Some(s) = status {
            let json = serde_json::to_string(s).context("encoding the status")?;
            rec.writer
                .upsert_status(s.last_event_unix_ms.unwrap_or(meta.started_unix_ms), &json)
                .context("writing the status row")?;
            rec.writer.commit().context("committing the status row")?;
        }
        Ok(rec)
    }

    /// Append `records` and commit them, as the owner's next flush does.
    pub fn append(&mut self, records: &[SessionRecord]) -> anyhow::Result<()> {
        for r in records {
            let body = serde_json::to_string(r).context("encoding a record")?;
            self.writer.insert(r, &body).context("inserting a record")?;
        }
        self.writer.commit().context("committing the records")
    }

    /// The session directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Close the connection without sealing: what `kill -9` leaves (WAL-mode
    /// header, no `End` unless the records held one).
    pub fn abandon(self) -> PathBuf {
        self.dir
    }

    /// Finish and seal it, as an owner that ended does: rollback mode, one
    /// self-contained file.
    pub fn seal(
        self,
        ended_unix_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
    ) -> anyhow::Result<PathBuf> {
        let conn = self
            .writer
            .finish(ended_unix_ms, exit_code, panicked)
            .context("finishing the recording")?;
        match store::seal(conn, store::SEAL_TIMEOUT).context("sealing the recording")? {
            store::Sealed::Done => Ok(self.dir),
            store::Sealed::CutShort => anyhow::bail!("the seal was cut short"),
        }
    }
}
