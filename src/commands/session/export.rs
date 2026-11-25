//! `heph session export`: a recording as one self-contained `.hephsession`
//! file (spec, Design "Export: `.hephsession`"), or as OTLP/JSON lines
//! (`--format otlp`, spec "OTLP out"). The work is `tui::session::export`'s,
//! shared with the seal-time export, and `tui::session::otlp`'s; this is the
//! command around them.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use hcore::approval_notice::ReadNotice;

use super::reference::{Opener, Resolved, open_ref};
use super::{ErrorKind, ExportArgs, ExportFormat, ViewOpts, kinded};
use crate::tui::session::export::{self as core, Abort};
use crate::tui::session::hephsession::EXTENSION;
use crate::tui::session::otlp;
use crate::tui::session::query::{Explorer, RecordingFile};
use crate::tui::session::recording::{Recording, RecordingState};
use crate::tui::session::{FileSource, SessionId, SessionSource, store};

/// Where an export goes: `-o PATH`, or `-o -` for stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dest {
    File(PathBuf),
    Stdout,
}

impl Dest {
    /// `-o` as given; without one, [`default_path`].
    pub(crate) fn from_arg(arg: Option<&Path>, id: SessionId, format: ExportFormat) -> Self {
        match arg {
            Some(p) if p.as_os_str() == "-" => Self::Stdout,
            Some(p) => Self::File(p.to_path_buf()),
            None => Self::File(default_path(id, format)),
        }
    }
}

/// The extension of an OTLP export: JSON lines.
pub(crate) const OTLP_EXTENSION: &str = "otlp.jsonl";

/// `./heph-<id>.hephsession`, or `./heph-<id>.otlp.jsonl`.
pub(crate) fn default_path(id: SessionId, format: ExportFormat) -> PathBuf {
    let ext = match format {
        ExportFormat::Hephsession => EXTENSION,
        ExportFormat::Otlp => OTLP_EXTENSION,
    };
    PathBuf::from(format!("heph-{}.{ext}", id.encode()))
}

/// What `export` says on stderr when the session is still running. It holds
/// the words every viewer of the file prints (`RecordingState`'s caveat), so
/// one grep finds both.
pub(crate) const SNAPSHOT_NOTE: &str =
    "heph: snapshot of a running session: the export holds the session so far\n";

/// What an export wrote.
#[derive(Debug)]
pub(crate) struct Exported {
    /// The file written; `None` for stdout.
    pub path: Option<PathBuf>,
    /// The copy is a snapshot of a session still running (E3).
    pub snapshot: bool,
}

/// `heph session export <ref>` over the sessions `file` holds: resolve `ref`
/// as the other commands do, then [`export`] the recording, local or
/// foreign. `read` fills a local export's notices (the engine's
/// `read_notice_from_store`); a foreign one keeps its own. `note` takes the
/// stderr lines opening a local recording may say (`--format otlp`).
pub(crate) async fn export_ref(
    file: &FileSource,
    opts: ViewOpts,
    args: &ExportArgs,
    read: ReadNotice,
    stdout: &mut dyn Write,
    opener: &Opener,
    note: Arc<dyn Fn(&str) + Send + Sync>,
) -> anyhow::Result<Exported> {
    if args.format == ExportFormat::Otlp {
        return export_otlp(file, opts, args, stdout, opener, note).await;
    }
    let sources: [&dyn SessionSource; 1] = [file];
    let rec = match open_ref(&sources, &args.reference, opts, opener).await? {
        Resolved::Local { entry, .. } => {
            let id = entry.id;
            // Opening an unsealed recording copies it: blocking work.
            let (root, dir) = (file.root().to_path_buf(), file.root().join(id.encode()));
            hcore::blocking::run(move || Recording::open_local(&root, &dir))
                .await
                .map_err(anyhow::Error::new)
                .with_context(|| format!("opening session {id}"))?
        }
        Resolved::Foreign(rec) => rec,
    };
    let id = rec.meta().context("reading the session row")?.id;
    let dest = Dest::from_arg(args.output.as_deref(), id, args.format);
    if dest == Dest::Stdout && std::io::stdout().is_terminal() {
        anyhow::bail!(
            "a .{EXTENSION} file is binary: not writing it to a terminal (`-o PATH`, or redirect stdout)"
        );
    }
    let logs = (!args.no_logs).then_some(read);
    let tmp = store::tmp_dir(file.root());
    match dest {
        // The VACUUM, the log reads and the compression: on the blocking
        // pool, never on a runtime worker.
        Dest::File(_) => {
            hcore::blocking::run(move || export(&rec, dest, logs, &tmp, &mut std::io::sink())).await
        }
        // The caller's writer is borrowed, so this one runs where it is
        // called: `bootstrap::block_on`'s own thread, not a worker.
        Dest::Stdout => export(&rec, dest, logs, &tmp, stdout),
    }
    .with_context(|| format!("exporting session {id}"))
}

/// Write `rec` to `dest`: a snapshot made self-contained through `logs`
/// (`None`: `--no-logs`), compressed, with temp files under `tmp_dir`. A
/// file is written beside its destination and renamed over it.
pub(crate) fn export(
    rec: &Recording,
    dest: Dest,
    logs: Option<ReadNotice>,
    tmp_dir: &Path,
    stdout: &mut dyn Write,
) -> anyhow::Result<Exported> {
    let abort = Abort::default();
    let db = core::snapshot(rec, logs, tmp_dir, &abort)?;
    let snapshot = rec.state().context("reading the session's state")? == RecordingState::Live;
    let path = match dest {
        Dest::Stdout => {
            core::compress(&db, &mut *stdout, &abort)?;
            None
        }
        Dest::File(path) => {
            core::write_file(&db, &path, false, &abort)?;
            Some(path)
        }
    };
    Ok(Exported { path, snapshot })
}

/// `export --format otlp`: the recording `args.reference` names, read
/// through the explorer names and written as OTLP/JSON lines. A running
/// session's export is the session so far (`snapshot`).
async fn export_otlp(
    file: &FileSource,
    opts: ViewOpts,
    args: &ExportArgs,
    stdout: &mut dyn Write,
    opener: &Opener,
    note: Arc<dyn Fn(&str) + Send + Sync>,
) -> anyhow::Result<Exported> {
    if args.no_logs {
        return Err(kinded(
            ErrorKind::Other,
            anyhow::anyhow!(
                "--no-logs does not apply to --format otlp: OTLP holds no notice text or failure logs"
            ),
        ));
    }
    let sources: [&dyn SessionSource; 1] = [file];
    let (id, rf) = match open_ref(&sources, &args.reference, opts, opener).await? {
        Resolved::Local { entry, .. } => {
            let id = entry.id;
            // Re-deriving and indexing the recording: blocking work.
            let (root, dir) = (file.root().to_path_buf(), file.root().join(id.encode()));
            let rf = hcore::blocking::run(move || RecordingFile::local(&root, &dir, &*note))
                .await
                .with_context(|| format!("opening session {id}"))?;
            (id, rf)
        }
        Resolved::Foreign(rec) => {
            let id = rec.meta().context("reading the session row")?.id;
            let rf =
                RecordingFile::foreign(rec).with_context(|| format!("opening session {id}"))?;
            (id, rf)
        }
    };
    let snapshot = rf.is_live();
    let dest = Dest::from_arg(args.output.as_deref(), id, ExportFormat::Otlp);
    let written = match dest {
        Dest::File(path) => {
            let to = path.clone();
            hcore::blocking::run(move || {
                let explorer = Explorer::open(rf)?;
                write_otlp_file(&explorer, &to)
            })
            .await
            .map(|w| (w, Some(path)))
        }
        // The caller's writer is borrowed, so this one runs where it is
        // called: `bootstrap::block_on`'s own thread, not a worker.
        Dest::Stdout => Explorer::open(rf).and_then(|explorer| {
            let w = otlp::write(&explorer, &mut *stdout)?;
            stdout.flush().context("writing to stdout")?;
            Ok((w, None))
        }),
    }
    .with_context(|| format!("exporting session {id}"))?;
    let (counts, path) = written;
    tracing::debug!(
        spans = counts.spans,
        span_events = counts.span_events,
        logs = counts.logs,
        points = counts.points,
        "wrote the OTLP export"
    );
    Ok(Exported { path, snapshot })
}

/// Write `explorer` to `dest` as OTLP/JSON through a temp file beside it,
/// renamed into place: a reader never sees a partial file.
fn write_otlp_file(explorer: &Explorer, dest: &Path) -> anyhow::Result<otlp::Written> {
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut prefix = OsString::from(".");
    prefix.push(dest.file_name().unwrap_or_else(|| OsStr::new("session")));
    prefix.push(".");
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("creating a temp file in {}", parent.display()))?;
    let written = {
        let mut out = io::BufWriter::new(temp.as_file_mut());
        let written = otlp::write(explorer, &mut out)?;
        out.flush().context("writing the export")?;
        written
    };
    temp.as_file().sync_all().context("syncing the export")?;
    temp.persist(dest)
        .map_err(|e| e.error)
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(written)
}
