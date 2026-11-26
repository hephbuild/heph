//! `heph session list`: one row per session, from `meta.json` and
//! `status.json` alone — never the event log (I3: listing costs a few small
//! reads per session, however long each one ran).

use std::io::Write;

use anyhow::Context as _;
use serde::Serialize;

use super::{
    BlockedOn, Out, SessionState, ViewOpts, Workspace, ago, blocked_on, command_line, derive_state,
    escape, list_all, truncate,
};
use crate::tui::session::{EntryMeta, SessionEntry, SessionSource, holder_session};

/// The widest COMMAND cell.
const COMMAND_WIDTH: usize = 40;
/// The widest NOW cell.
const NOW_WIDTH: usize = 60;

/// One session in `list --json`, which is an array of these sorted by id.
/// Times are absolute unix milliseconds.
#[derive(Debug, Serialize, output::Record)]
pub struct ListRow {
    /// The session id.
    id: String,
    /// The recording process's pid; `null` only for a session in a newer
    /// directory format.
    owner_pid: Option<u32>,
    /// The command line it ran, as recorded.
    argv: Vec<String>,
    /// The workspace it ran in, when it ran in one.
    workspace_root: Option<String>,
    /// The directory it ran from.
    cwd: Option<String>,
    /// Viewer-computed: running, waiting, quiet, ok, failed, lost or unknown.
    state: SessionState,
    /// When it started.
    started_unix_ms: u64,
    /// Its latest build event; `null` before the first.
    last_event_unix_ms: Option<u64>,
    /// Its exit code once it ended; `null` while live, or killed by a signal.
    exit_code: Option<i32>,
    /// Set when the session ended in a panic (its `exit_code` may still be 0).
    panicked: bool,
    /// The PROGRESS column's numbers; `null` before the session wrote its
    /// first `status.json`.
    progress: Option<Progress>,
    /// The NOW column for a live session: the lock or pool it waits on, else
    /// its oldest running target. `null` once it ended, or with nothing on.
    /// Human text, not a stable field: an agent reads `blocked_on` and
    /// `progress`.
    now: Option<String>,
    /// Every wait of a live session, each lock holder mapped to its session.
    blocked_on: Vec<BlockedOn>,
    /// The recording is short of the run (it passed its size cap; `show
    /// --json`'s `incomplete_reason` says more). `null` when this heph cannot
    /// tell: a session in a newer format, or a status row it cannot read.
    incomplete: Option<bool>,
    /// Set only for a session in a newer directory format, which this heph
    /// does not read further.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<u32>,
}

/// How far a session got, as PROGRESS shows it.
#[derive(Debug, Serialize, output::Shape)]
pub(crate) struct Progress {
    /// Matched targets that finished once `total` is known; before that,
    /// every target that finished.
    done: u64,
    /// Matched targets so far; `null` before the matcher reported.
    total: Option<u64>,
    /// Whether `total` is final (the table marks a provisional one `~`).
    total_complete: bool,
    /// Targets with an operation open now (executing, reading or writing a
    /// cache): `status.json`'s `running`, in full. Not `counts.in_flight`,
    /// which also counts targets between their result's start and end that
    /// have no operation open, such as one waiting on a lock or its deps.
    running_ops: u64,
    failed: u64,
}

impl Progress {
    fn of(s: &crate::tui::session::SessionSnapshot) -> Self {
        let c = &s.counts;
        Self {
            done: if c.matched.is_some() {
                c.matched_done
            } else {
                c.finished
            },
            total: c.matched,
            total_complete: c.matched_complete,
            running_ops: s
                .totals
                .running
                .total
                .max(u64::try_from(s.running.len()).unwrap_or(u64::MAX)),
            failed: c.failed,
        }
    }
}

pub(crate) async fn list(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    workspace: Workspace<'_>,
    json: &output::JsonArgs<ListRow>,
    out: Out<'_>,
) -> anyhow::Result<()> {
    let all = list_all(sources).await?;
    let shown: Vec<&SessionEntry> = all
        .iter()
        .map(|(_, e)| e)
        .filter(|e| match (workspace, &e.meta) {
            (Workspace::All, _) => true,
            // Its workspace is unknown: shown everywhere, as the placeholder.
            (Workspace::Only(_), EntryMeta::TooNew { .. }) => true,
            (Workspace::Only(w), EntryMeta::Known(m)) => m.workspace_root.as_deref() == Some(w),
        })
        .collect();
    let every: Vec<&SessionEntry> = all.iter().map(|(_, e)| e).collect();
    if json.enabled() {
        let rows = shown.iter().map(|e| json_row(e, &every, opts));
        json.printer_to(out.w, out.style)
            .list_iter(rows, &output::never_cancelled)
    } else {
        write_table(&shown, &every, workspace, opts, out.w)
    }
}

/// The stderr line saying `list` widened to every workspace on its own: it
/// was not given `--all`, but there is no workspace here to narrow to.
pub(crate) fn scope_note(all: bool, workspace: Option<&str>) -> Option<&'static str> {
    (!all && workspace.is_none())
        .then_some("heph: not in a workspace; listing the sessions of every workspace\n")
}

/// `e`'s element of `list --json`.
fn json_row(e: &SessionEntry, every: &[&SessionEntry], opts: ViewOpts) -> ListRow {
    let state = derive_state(e, opts.now_ms, opts.stall);
    let status = e.status.as_ref();
    let blocked_on = status.map(|s| blocked_on(s, e, every)).unwrap_or_default();
    let end = status.and_then(|s| s.end.as_ref());
    let exit_code = end.and_then(|x| x.exit_code);
    let last_event_unix_ms = status.and_then(|s| s.last_event_unix_ms);
    let now = match state {
        SessionState::Running | SessionState::Waiting | SessionState::Quiet => status
            .map(|s| now_cell(s, e, every, opts))
            .filter(|n| n != "-"),
        _ => None,
    };
    match &e.meta {
        EntryMeta::Known(m) => ListRow {
            id: e.id.encode(),
            owner_pid: Some(m.owner_pid),
            argv: m.argv.clone(),
            workspace_root: m.workspace_root.clone(),
            cwd: m.cwd.clone(),
            state,
            started_unix_ms: m.started_unix_ms,
            last_event_unix_ms,
            exit_code,
            panicked: end.is_some_and(|x| x.panicked),
            progress: status.map(Progress::of),
            now,
            blocked_on,
            // Unknown, not "complete", when the status row is there but this
            // heph cannot read it.
            incomplete: (!e.status_unreadable).then(|| status.is_some_and(|s| s.truncated)),
            format: None,
        },
        EntryMeta::TooNew { found, .. } => ListRow {
            id: e.id.encode(),
            owner_pid: None,
            argv: Vec::new(),
            workspace_root: None,
            cwd: None,
            state,
            started_unix_ms: e.id.started_unix_ms(),
            last_event_unix_ms: None,
            exit_code: None,
            panicked: false,
            progress: None,
            now: None,
            blocked_on: Vec::new(),
            incomplete: None,
            format: Some(*found),
        },
    }
}

const HEADERS: [&str; 7] = [
    "ID", "WORKTREE", "COMMAND", "STATE", "EXIT", "PROGRESS", "NOW",
];

fn write_table(
    shown: &[&SessionEntry],
    every: &[&SessionEntry],
    workspace: Workspace<'_>,
    opts: ViewOpts,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if shown.is_empty() {
        let empty = match workspace {
            Workspace::Only(_) => "no sessions in this workspace (--all for every workspace)",
            Workspace::All => "no sessions",
        };
        writeln!(out, "{empty}").context("writing the session list")?;
        return Ok(());
    }
    // Live first, then ended; each by start.
    let mut ordered: Vec<&SessionEntry> = shown.to_vec();
    ordered.sort_by_key(|e| (!e.live, e.id));
    let rows: Vec<[String; 7]> = ordered.iter().map(|e| row(e, every, opts)).collect();
    let mut widths = HEADERS.map(|h| h.chars().count());
    for r in &rows {
        for (w, cell) in widths.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let header = HEADERS.map(str::to_owned);
    for r in std::iter::once(&header).chain(&rows) {
        let mut line = String::new();
        for (i, (cell, w)) in r.iter().zip(widths).enumerate() {
            if i + 1 == r.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.extend(std::iter::repeat_n(' ', w - cell.chars().count() + 2));
            }
        }
        writeln!(out, "{}", line.trim_end()).context("writing the session list")?;
    }
    Ok(())
}

fn row(e: &SessionEntry, every: &[&SessionEntry], opts: ViewOpts) -> [String; 7] {
    let id = e.id.encode();
    let meta = match &e.meta {
        EntryMeta::Known(m) => m,
        EntryMeta::TooNew { found, .. } => {
            return [
                id,
                "-".to_owned(),
                format!("format {found}, needs newer heph"),
                SessionState::Unknown.label().to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            ];
        }
    };
    let state = derive_state(e, opts.now_ms, opts.stall);
    let worktree = meta
        .workspace_root
        .as_deref()
        .and_then(|w| std::path::Path::new(w).file_name())
        .map_or_else(
            || "-".to_owned(),
            |f| escape(&f.to_string_lossy()).into_owned(),
        );
    let status = e.status.as_ref();
    let end = status.and_then(|s| s.end.as_ref());
    let exit = match end {
        Some(end) if end.panicked => "panic".to_owned(),
        Some(end) => end
            .exit_code
            .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
        None => "-".to_owned(),
    };
    let progress = status.map_or_else(
        || "-".to_owned(),
        |s| {
            let c = &s.counts;
            let mut p = match c.matched {
                Some(m) => format!(
                    "{}/{}{m}",
                    c.matched_done,
                    if c.matched_complete { "" } else { "~" }
                ),
                None => format!("{} done", c.finished),
            };
            if c.failed > 0 {
                p.push_str(&format!("  {} fail", c.failed));
            }
            // Past its size cap the recording is short of the run.
            if s.truncated {
                p.push_str("  incomplete");
            }
            p
        },
    );
    let now = match state {
        SessionState::Ok | SessionState::Failed => end.map_or_else(
            || "-".to_owned(),
            |x| format!("{} ago", ago(opts.now_ms, x.at_unix_ms)),
        ),
        SessionState::Lost => status.and_then(|s| s.last_event_unix_ms).map_or_else(
            || "process gone".to_owned(),
            |t| format!("process gone, last event {} ago", ago(opts.now_ms, t)),
        ),
        _ => status.map_or_else(|| "-".to_owned(), |s| now_cell(s, e, every, opts)),
    };
    [
        id,
        worktree,
        command_line(&meta.argv, COMMAND_WIDTH),
        state.label().to_owned(),
        exit,
        progress,
        truncate(&now, NOW_WIDTH),
    ]
}

/// What a live session is on: the lock it waits for and who holds it, else its
/// oldest running target.
fn now_cell(
    s: &crate::tui::session::SessionSnapshot,
    e: &SessionEntry,
    every: &[&SessionEntry],
    opts: ViewOpts,
) -> String {
    if super::every_running_waits(s)
        && let Some(w) = s.lock_waits.first()
    {
        let holder = match w.holder_pid {
            Some(pid) => match holder_session(every, pid, Some(e.id)) {
                Some(h) => format!("held by {}", h.id),
                None => format!("held by pid {pid}"),
            },
            None if w.in_use_by_readers => "in use by readers".to_owned(),
            None => "held".to_owned(),
        };
        return format!("lock on {} {holder}", escape(&w.addr));
    }
    if let Some(w) = s.admission_waits.first() {
        return format!("{} waits for {}", escape(&w.addr), escape(&w.pool));
    }
    match s.running.first() {
        Some(r) => format!(
            "{} ({})",
            escape(&r.addr),
            ago(opts.now_ms, r.since_unix_ms)
        ),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        output::check_keys(&ListRow {
            id: "id".to_owned(),
            owner_pid: Some(1),
            argv: vec!["heph".to_owned()],
            workspace_root: Some("/w".to_owned()),
            cwd: Some("/w".to_owned()),
            state: SessionState::Running,
            started_unix_ms: 1,
            last_event_unix_ms: Some(2),
            exit_code: None,
            panicked: false,
            progress: None,
            now: Some("//a:b (1s)".to_owned()),
            blocked_on: Vec::new(),
            incomplete: Some(true),
            format: Some(2),
        })
    }
}
