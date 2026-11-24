//! Writing a recording as a `.hephsession` (spec, Design "Export:
//! `.hephsession`"): `VACUUM INTO` a temp file (a consistent snapshot, so a
//! live session exports too), the copy made self-contained, then a zstd frame
//! of it renamed into place.
//!
//! Self-contained (I6): for a local recording the copy gains what a reader on
//! another machine cannot look up — each approval notice's text, read through
//! [`ApprovalPromptNotice::read`] as the live banner reads it, and the tail of
//! each root failure's log from its failure slot. A foreign recording is
//! copied as it is: it has no local disk to read from (I7).
//!
//! Two callers: `heph session export`, and [`to_dir`], the seal-time copy
//! `HEPH_SESSION_EXPORT_DIR` asks for (D7), bounded by [`Abort`].

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use hcore::approval_notice::{NoticeRoots, ReadNotice};
use hcore::events::BuildEventKind;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, params};

use super::format::{SessionRecord, SessionRecordKind};
use super::hephsession::EXTENSION;
use super::id::SessionId;
use super::recording::Recording;
use super::store;

/// The most of one root failure's log an export keeps: its last 1 MiB.
pub const FAILURE_LOG_CAP: u64 = 1 << 20;
/// The most of all root failures' logs an export keeps.
pub const FAILURE_LOGS_CAP: u64 = 32 << 20;
/// How long the seal-time export may take, outside the recorder's own bound.
pub const EXPORT_DIR_BOUND: Duration = Duration::from_secs(30);
/// The zstd level: fast, since the seal-time export runs as the process exits.
const LEVEL: i32 = 3;
/// The failure slots under a heph home, as the engine's `FAILURES_DIR` names
/// them. A recorded log path outside it is not read.
const FAILURES_DIR: &str = "failures";

/// The `.hephsession` file name of session `id`: `<id>.hephsession`.
pub fn file_name(id: SessionId) -> String {
    format!("{}.{EXTENSION}", id.encode())
}

/// Stops an export from another thread: what the seal-time export's bound
/// uses. The export checks it between steps and while compressing; an
/// abort also interrupts a running SQLite statement and removes every temp
/// file the export holds.
#[derive(Debug, Clone, Default)]
pub struct Abort(Arc<AbortInner>);

#[derive(Default)]
struct AbortInner {
    /// Set by [`Abort::abort`] under `temps`' lock.
    aborted: AtomicBool,
    /// The statement to interrupt: the connection the export runs on now.
    sqlite: Mutex<Option<rusqlite::InterruptHandle>>,
    /// Every temp file the export made and still holds. A temp is made, and
    /// the destination written, under this lock after checking `aborted`,
    /// so once [`Abort::abort`] returns nothing more appears.
    temps: Mutex<Temps>,
    /// A test's slow export: held before compressing until it opens.
    gate: Option<super::recorder::Gate>,
}

#[derive(Debug, Default)]
struct Temps {
    files: Vec<PathBuf>,
    /// The destination is written: an abort now stops nothing.
    written: bool,
}

impl std::fmt::Debug for AbortInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortInner")
            .field("aborted", &self.aborted)
            .finish_non_exhaustive()
    }
}

impl Abort {
    /// An abort whose export holds before compressing until `gate` opens
    /// (a test's slow export); `None` is [`Abort::default`].
    pub(crate) fn with_gate(gate: Option<super::recorder::Gate>) -> Self {
        Self(Arc::new(AbortInner {
            gate,
            ..AbortInner::default()
        }))
    }

    /// Stop the export: interrupt its statement, remove every temp file it
    /// holds. `false` when it had already written its destination (it
    /// finished at the bound): nothing was stopped.
    pub fn abort(&self) -> bool {
        let files = {
            let mut temps = self.temps();
            if temps.written {
                return false;
            }
            self.0.aborted.store(true, Ordering::Release);
            std::mem::take(&mut temps.files)
        };
        if let Some(h) = self
            .0
            .sqlite
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            h.interrupt();
        }
        // Removed after the lock: the export, if it is waiting on it, finds
        // itself aborted and makes nothing new.
        for temp in files {
            if let Err(e) = std::fs::remove_file(&temp)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::debug!(file = %temp.display(), error = %e, "cannot remove an export's temp file");
            }
        }
        true
    }

    fn temps(&self) -> std::sync::MutexGuard<'_, Temps> {
        self.0.temps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.aborted.load(Ordering::Acquire),
            "the export was stopped at its time bound"
        );
        Ok(())
    }

    /// Run `make` (which makes temp files, or writes the destination) unless
    /// the export is aborted, holding the lock [`Abort::abort`] takes: what
    /// `make` adds to the list is removed by an abort, and an abort that
    /// comes after it waits for it.
    fn guarded<T>(&self, make: impl FnOnce(&mut Temps) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut temps = self.temps();
        self.check()?;
        make(&mut temps)
    }

    /// Stop tracking `files`: removed (or renamed into place) by their owner.
    fn release(&self, files: &[PathBuf]) {
        self.temps().files.retain(|f| !files.contains(f));
    }

    /// Interrupt `conn` on an abort from now on.
    fn watch(&self, conn: &Connection) -> anyhow::Result<()> {
        *self.0.sqlite.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(conn.get_interrupt_handle());
        // An abort that came before the handle was set interrupted nothing.
        self.check()
    }
}

/// `path` and the files SQLite may make beside it while writing it.
fn with_journals(path: &Path) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        files.push(PathBuf::from(name));
    }
    files
}

/// A self-contained copy of `rec`, uncompressed, in a temp file under
/// `tmp_dir` (deleted when the returned path drops). `logs`: fill a local
/// recording's `notices` and `failure_logs` through this notice reader;
/// `None` (`--no-logs`) leaves both empty, a foreign recording's included.
/// A live local recording's copy says `snapshot=1`.
pub fn snapshot(
    rec: &Recording,
    logs: Option<ReadNotice>,
    tmp_dir: &Path,
    abort: &Abort,
) -> anyhow::Result<tempfile::TempPath> {
    std::fs::create_dir_all(tmp_dir).with_context(|| format!("creating {}", tmp_dir.display()))?;
    // `VACUUM INTO` takes a missing or empty file.
    let temp = abort.guarded(|temps| {
        let temp = tempfile::Builder::new()
            .prefix("export-")
            .suffix(".db")
            .tempfile_in(tmp_dir)
            .with_context(|| format!("creating a temp file in {}", tmp_dir.display()))?
            .into_temp_path();
        temps.files.extend(with_journals(&temp));
        Ok(temp)
    })?;
    let target = temp
        .to_str()
        .with_context(|| format!("{} is not UTF-8", temp.display()))?;
    abort.watch(rec.conn())?;
    vacuum_into(rec, target).context("copying the recording")?;
    let live = matches!(rec, Recording::Local(l) if l.opened.live);

    let copy = open_copy(&temp).with_context(|| format!("opening the copy {}", temp.display()))?;
    abort.watch(&copy)?;
    if live {
        copy.execute("UPDATE session SET snapshot = 1", [])
            .context("marking the copy a snapshot")?;
    }
    match (logs, rec) {
        (Some(read), Recording::Local(_)) => fill(&copy, read, abort)?,
        // A foreign recording keeps what its own export filled.
        (Some(_), Recording::Foreign(_)) => {}
        (None, _) => copy
            .execute_batch("DELETE FROM notices; DELETE FROM failure_logs;")
            .context("leaving the logs out")?,
    }
    // `VACUUM INTO` keeps a WAL-mode header: a sealed export is one file.
    let mode: String = copy
        .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
        .context("switching the copy to rollback mode")?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("delete"),
        "the copy stayed in {mode} mode"
    );
    copy.close()
        .map_err(|(_, e)| e)
        .context("closing the copy")?;
    Ok(temp)
}

/// `VACUUM INTO target` from `rec`. It writes only `target`, but a foreign
/// connection's `query_only` refuses it as a write: lifted for this one
/// statement. The connection stays read-only and `immutable=1`, so its file
/// is still never written.
fn vacuum_into(rec: &Recording, target: &str) -> rusqlite::Result<()> {
    let conn = rec.conn();
    if !rec.is_foreign() {
        return conn.execute("VACUUM INTO ?1", params![target]).map(drop);
    }
    conn.execute_batch("PRAGMA query_only = 0")?;
    let copied = conn.execute("VACUUM INTO ?1", params![target]).map(drop);
    let restored = conn.execute_batch("PRAGMA query_only = 1");
    copied.and(restored)
}

/// The copy, read-write. Its schema came from the recording, a foreign
/// file's included, so nothing in it runs: triggers are off and
/// `trusted_schema=OFF` keeps its views and defaults to harmless functions.
fn open_copy(path: &Path) -> rusqlite::Result<Connection> {
    let copy = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    copy.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)?;
    copy.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    copy.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    Ok(copy)
}

/// Fill `notices` and `failure_logs` of `copy` from the store and the failure
/// slots its session row names.
fn fill(copy: &Connection, read: ReadNotice, abort: &Abort) -> anyhow::Result<()> {
    let meta = store::read_meta(copy).context("reading the session row")?;
    let records = approval_and_result_records(copy)?;
    let tx = copy.unchecked_transaction().context("starting the fill")?;
    match (meta.home_dir.as_deref(), meta.workspace_root.as_deref()) {
        (Some(home), Some(root)) => {
            let roots = NoticeRoots {
                home: PathBuf::from(home),
                root: PathBuf::from(root),
            };
            fill_notices(&tx, &records, &roots, read, abort)?;
        }
        _ => tracing::debug!("the session recorded no workspace: no notice to export"),
    }
    let ended_ms = store::read_row_end(copy)
        .context("reading the session row")?
        .ended_ms;
    match meta.home_dir.as_deref() {
        Some(home) => fill_failure_logs(
            &tx,
            &records,
            &Path::new(home).join(FAILURES_DIR),
            ended_ms,
            abort,
        )?,
        None => tracing::debug!("the session recorded no home: no failure log to export"),
    }
    tx.commit().context("committing the fill")
}

/// Every `Approval` and `ResultEnd` record, in order.
fn approval_and_result_records(conn: &Connection) -> anyhow::Result<Vec<SessionRecord>> {
    let mut stmt = conn
        .prepare("SELECT body FROM records WHERE kind IN ('Approval', 'ResultEnd') ORDER BY seq")
        .context("reading the records")?;
    let bodies = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .context("reading the records")?;
    let mut out = Vec::new();
    for body in bodies {
        let body = body.context("reading a record")?;
        match serde_json::from_str::<SessionRecord>(&body) {
            Ok(r) => out.push(r),
            Err(e) => tracing::debug!(error = %e, "skipping a record that does not decode"),
        }
    }
    Ok(out)
}

/// One row per notice of every request the recording queued, read as the
/// live banner reads it (capped at `NOTICE_READ_CAP`). A notice that cannot
/// be read gets no row: a viewer shows "notice unavailable", as it would here.
fn fill_notices(
    conn: &Connection,
    records: &[SessionRecord],
    roots: &NoticeRoots,
    read: ReadNotice,
    abort: &Abort,
) -> anyhow::Result<()> {
    let mut done: HashSet<(&str, u64, &str)> = HashSet::new();
    let mut insert = conn
        .prepare("INSERT INTO notices (addr, seq, name, text) VALUES (?1, ?2, ?3, ?4)")
        .context("preparing the notices")?;
    let queued = records.iter().filter_map(|r| match &r.session {
        Some(SessionRecordKind::Approval {
            prompt: Some(prompt),
            ..
        }) => Some(prompt.queue.iter()),
        _ => None,
    });
    for q in queued.flatten() {
        for notice in &q.notices {
            if !done.insert((&q.addr, q.seq, &notice.name)) {
                continue;
            }
            abort.check()?;
            let Ok(text) = notice.read(roots, read) else {
                continue;
            };
            insert
                .execute(params![
                    q.addr,
                    i64::try_from(q.seq).unwrap_or(i64::MAX),
                    notice.name,
                    text
                ])
                .context("writing a notice")?;
        }
    }
    Ok(())
}

/// One row per root failure (never a collateral one, nor a stop) whose log
/// is in its failure slot: its last [`FAILURE_LOG_CAP`] bytes, within
/// [`FAILURE_LOGS_CAP`] in all, `cut_bytes` saying what was left out. A
/// slot keeps only a target's last failure: a log modified after the
/// session's `ended_ms` (past [`LATER_SLACK_MS`]) is a later session's, and
/// is left out.
fn fill_failure_logs(
    conn: &Connection,
    records: &[SessionRecord],
    failures: &Path,
    ended_ms: Option<i64>,
    abort: &Abort,
) -> anyhow::Result<()> {
    let mut insert = conn
        .prepare("INSERT INTO failure_logs (addr, nth, log, cut_bytes) VALUES (?1, 0, ?2, ?3)")
        .context("preparing the failure logs")?;
    let mut done: HashSet<&str> = HashSet::new();
    let mut left = FAILURE_LOGS_CAP;
    for r in records {
        let Some(BuildEventKind::ResultEnd {
            addr,
            error: Some(_),
            upstream_of: None,
            cancelled: false,
            left_behind: Some(left_behind),
            ..
        }) = &r.kind
        else {
            continue;
        };
        let Some(path) = left_behind.log_path.as_deref().map(Path::new) else {
            continue;
        };
        // The slot holds the last failure only: one log per target.
        if !in_dir(path, failures) || !done.insert(addr) {
            continue;
        }
        abort.check()?;
        if let Some(ended) = ended_ms
            && modified_ms(path).is_some_and(|m| m > ended.saturating_add(LATER_SLACK_MS))
        {
            tracing::debug!(file = %path.display(), "a failure log newer than the session: skipped");
            continue;
        }
        let (log, cut) = match tail(path, FAILURE_LOG_CAP.min(left)) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(file = %path.display(), error = %e, "cannot read a failure log");
                continue;
            }
        };
        left = left.saturating_sub(log.len() as u64);
        insert
            .execute(params![addr, log, i64::try_from(cut).unwrap_or(i64::MAX)])
            .context("writing a failure log")?;
    }
    Ok(())
}

/// How far past a session's end a failure log's mtime may be and still be
/// its own: room for a filesystem's coarse or skewed clock (NFS), far less
/// than the gap before another session rewrites the slot.
const LATER_SLACK_MS: i64 = 1_000;

/// `path`'s mtime in Unix milliseconds; `None` when it cannot be read.
fn modified_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Whether `path` is an absolute path under `dir`, with no `..`.
fn in_dir(path: &Path, dir: &Path) -> bool {
    path.is_absolute()
        && path.starts_with(dir)
        && !path.components().any(|c| c == Component::ParentDir)
}

/// The last `max` bytes of `path`, and how many came before them.
fn tail(path: &Path, max: u64) -> io::Result<(Vec<u8>, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut log = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    f.take(max).read_to_end(&mut log)?;
    Ok((log, start))
}

/// Write `db` as a zstd frame into `out`.
pub fn compress(db: &Path, out: impl Write, abort: &Abort) -> anyhow::Result<()> {
    let mut src = std::fs::File::open(db).with_context(|| format!("opening {}", db.display()))?;
    let mut enc = zstd::stream::Encoder::new(out, LEVEL).context("starting the compression")?;
    if let Some(gate) = &abort.0.gate {
        super::recorder::wait_for(gate);
    }
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        abort.check()?;
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", db.display())),
        };
        enc.write_all(buf.get(..n).unwrap_or_default())
            .context("compressing")?;
    }
    enc.finish()
        .context("finishing the compression")?
        .flush()
        .context("flushing the export")
}

/// Compress `db` into `dest` through a temp file beside it, renamed into
/// place: a reader never sees a partial file. `noclobber`: fail rather than
/// replace a file already there.
pub fn write_file(db: &Path, dest: &Path, noclobber: bool, abort: &Abort) -> anyhow::Result<()> {
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = dest.file_name().unwrap_or_else(|| OsStr::new("session"));
    let mut prefix = std::ffi::OsString::from(".");
    prefix.push(name);
    prefix.push(".");
    let mut temp = abort.guarded(|temps| {
        let temp = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("creating a temp file in {}", parent.display()))?;
        temps.files.push(temp.path().to_path_buf());
        Ok(temp)
    })?;
    let path = temp.path().to_path_buf();
    let written = compress(db, io::BufWriter::new(temp.as_file_mut()), abort)
        .and_then(|()| temp.as_file().sync_all().context("syncing the export"));
    if let Err(e) = written {
        drop(temp);
        abort.release(&[path]);
        return Err(e);
    }
    // Renamed into place under the abort's lock: an abort either comes
    // first (nothing is written) or finds the destination written.
    abort.guarded(|temps| {
        let persisted = if noclobber {
            temp.persist_noclobber(dest)
        } else {
            temp.persist(dest)
        };
        temps.files.retain(|f| *f != path);
        persisted
            .map_err(|e| e.error)
            .with_context(|| format!("writing {}", dest.display()))?;
        temps.written = true;
        Ok(())
    })
}

/// The seal-time export (D7): the ended session in `dir`, under the sessions
/// `root`, written to `<export_dir>/<id>.hephsession`, never over another
/// file. Run after the recorder let go of the recording.
pub fn to_dir(
    root: &Path,
    dir: &Path,
    id: SessionId,
    export_dir: &Path,
    read: ReadNotice,
    abort: &Abort,
) -> anyhow::Result<PathBuf> {
    let rec = Recording::open_local(root, dir)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("opening session {id}"))?;
    // A copy the open made (an unsealed recording) is the abort's to remove
    // too; aborted already, it is removed as `rec` drops.
    let copies = match &rec {
        Recording::Local(l) => l.opened.temp_files().to_vec(),
        Recording::Foreign(_) => Vec::new(),
    };
    abort.guarded(|temps| {
        temps.files.extend(copies.iter().cloned());
        Ok(())
    })?;
    let db = snapshot(&rec, Some(read), &store::tmp_dir(root), abort)?;
    drop(rec);
    abort.release(&copies);
    std::fs::create_dir_all(export_dir)
        .with_context(|| format!("creating {}", export_dir.display()))?;
    let dest = export_dir.join(file_name(id));
    write_file(&db, &dest, true, abort)?;
    Ok(dest)
}

/// `HEPH_SESSION_EXPORT_DIR`'s value as a directory: `None` unset or empty;
/// an error, to warn with, when it is not an absolute path.
pub fn export_dir_from_env(value: Option<&OsStr>) -> Result<Option<PathBuf>, String> {
    let Some(v) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(v);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Err(format!(
            "{} must be an absolute path, not {:?}",
            hcore::session_record::SESSION_EXPORT_DIR_ENV,
            path.display().to_string()
        ))
    }
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
