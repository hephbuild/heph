//! `heph session export`: a recording as one self-contained `.hephsession`
//! file (spec, Design "Export: `.hephsession`"). The work is
//! `tui::session::export`'s, shared with the seal-time export; this is the
//! command around it.

use std::io::{IsTerminal as _, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use hcore::approval_notice::ReadNotice;

use super::reference::{Opener, Resolved, open_ref};
use super::{ExportArgs, ViewOpts};
use crate::tui::session::export::{self as core, Abort};
use crate::tui::session::hephsession::EXTENSION;
use crate::tui::session::recording::{Recording, RecordingState};
use crate::tui::session::{FileSource, SessionId, SessionSource, store};

/// Where an export goes: `-o PATH`, or `-o -` for stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dest {
    File(PathBuf),
    Stdout,
}

impl Dest {
    /// `-o` as given; without one, `./heph-<id>.hephsession`.
    pub(crate) fn from_arg(arg: Option<&Path>, id: SessionId) -> Self {
        match arg {
            Some(p) if p.as_os_str() == "-" => Self::Stdout,
            Some(p) => Self::File(p.to_path_buf()),
            None => Self::File(default_path(id)),
        }
    }
}

/// `./heph-<id>.hephsession`.
pub(crate) fn default_path(id: SessionId) -> PathBuf {
    PathBuf::from(format!("heph-{}.{EXTENSION}", id.encode()))
}

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
/// `read_notice_from_store`); a foreign one keeps its own.
pub(crate) async fn export_ref(
    file: &FileSource,
    opts: ViewOpts,
    args: &ExportArgs,
    read: ReadNotice,
    stdout: &mut dyn Write,
    opener: &Opener,
) -> anyhow::Result<Exported> {
    let sources: [&dyn SessionSource; 1] = [file];
    let rec = match open_ref(&sources, &args.reference, opts, opener).await? {
        Resolved::Local { entry, .. } => {
            let id = entry.id;
            Recording::open_local(file.root(), &file.root().join(id.encode()))
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("opening session {id}"))?
        }
        Resolved::Foreign(rec) => rec,
    };
    let id = rec.meta().context("reading the session row")?.id;
    let dest = Dest::from_arg(args.output.as_deref(), id);
    if dest == Dest::Stdout && std::io::stdout().is_terminal() {
        anyhow::bail!(
            "a .{EXTENSION} file is binary: not writing it to a terminal (`-o PATH`, or redirect stdout)"
        );
    }
    let logs = (!args.no_logs).then_some(read);
    export(&rec, dest, logs, &store::tmp_dir(file.root()), stdout)
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
