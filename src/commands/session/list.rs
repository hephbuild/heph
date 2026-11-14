//! `heph session list`: one row per session, from `meta.json` and
//! `status.json` alone — never the event log (I3: listing costs a few small
//! reads per session, however long each one ran).

use std::io::Write;

use anyhow::Context as _;
use serde::Serialize;

use super::{
    SessionState, ViewOpts, Workspace, ago, command_line, derive_state, escape, list_all, truncate,
};
use crate::tui::session::{EntryMeta, SessionEntry, SessionSource};

/// `list --json`'s own version, bumped only on an incompatible change.
pub const LIST_JSON_VERSION: u32 = 1;

/// The widest COMMAND cell.
const COMMAND_WIDTH: usize = 40;
/// The widest NOW cell.
const NOW_WIDTH: usize = 60;

#[derive(Debug, Serialize)]
struct ListJson {
    version: u32,
    sessions: Vec<ListJsonEntry>,
}

/// One session in `list --json`. Times are absolute unix milliseconds.
#[derive(Debug, Serialize)]
struct ListJsonEntry {
    id: String,
    owner_pid: Option<u32>,
    argv: Vec<String>,
    workspace_root: Option<String>,
    cwd: Option<String>,
    /// Viewer-computed; see [`SessionState`].
    state: SessionState,
    started_unix_ms: u64,
    last_event_unix_ms: Option<u64>,
    exit_code: Option<i32>,
    blocked_on: Vec<BlockedOn>,
    /// Set only for a session in a newer directory format, which this heph
    /// does not read further.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<u32>,
}

/// What one waiting target waits for.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BlockedOn {
    /// A result lock another process holds.
    Lock {
        addr: String,
        holder_pid: Option<u32>,
        /// The session of that process, when it records one.
        holder_session: Option<String>,
    },
    /// Admission into a resource pool.
    Admission { addr: String, pool: String },
}

pub(crate) async fn list(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    workspace: Workspace<'_>,
    json: bool,
    out: &mut dyn Write,
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
    if json {
        write_json(&shown, &every, opts, out)
    } else {
        write_table(&shown, &every, opts, out)
    }
}

fn write_json(
    shown: &[&SessionEntry],
    every: &[&SessionEntry],
    opts: ViewOpts,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let sessions = shown
        .iter()
        .map(|e| {
            let state = derive_state(e, opts.now_ms, opts.stall);
            let status = e.status.as_ref();
            let blocked_on = status
                .map(|s| {
                    s.lock_waits
                        .iter()
                        .map(|w| BlockedOn::Lock {
                            addr: w.addr.clone(),
                            holder_pid: w.holder_pid,
                            holder_session: w
                                .holder_pid
                                .and_then(|pid| holder_session(every, pid, e))
                                .map(|h| h.id.encode()),
                        })
                        .chain(s.admission_waits.iter().map(|w| BlockedOn::Admission {
                            addr: w.addr.clone(),
                            pool: w.pool.clone(),
                        }))
                        .collect()
                })
                .unwrap_or_default();
            let exit_code = status
                .and_then(|s| s.end.as_ref())
                .and_then(|x| x.exit_code);
            let last_event_unix_ms = status.and_then(|s| s.last_event_unix_ms);
            match &e.meta {
                EntryMeta::Known(m) => ListJsonEntry {
                    id: e.id.encode(),
                    owner_pid: Some(m.owner_pid),
                    argv: m.argv.clone(),
                    workspace_root: m.workspace_root.clone(),
                    cwd: m.cwd.clone(),
                    state,
                    started_unix_ms: m.started_unix_ms,
                    last_event_unix_ms,
                    exit_code,
                    blocked_on,
                    format: None,
                },
                EntryMeta::TooNew { found, .. } => ListJsonEntry {
                    id: e.id.encode(),
                    owner_pid: None,
                    argv: Vec::new(),
                    workspace_root: None,
                    cwd: None,
                    state,
                    started_unix_ms: e.id.started_unix_ms(),
                    last_event_unix_ms: None,
                    exit_code: None,
                    blocked_on: Vec::new(),
                    format: Some(*found),
                },
            }
        })
        .collect();
    serde_json::to_writer_pretty(
        &mut *out,
        &ListJson {
            version: LIST_JSON_VERSION,
            sessions,
        },
    )
    .context("writing the session list")?;
    writeln!(out).context("writing the session list")?;
    Ok(())
}

const HEADERS: [&str; 7] = [
    "ID", "WORKTREE", "COMMAND", "STATE", "EXIT", "PROGRESS", "NOW",
];

fn write_table(
    shown: &[&SessionEntry],
    every: &[&SessionEntry],
    opts: ViewOpts,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if shown.is_empty() {
        writeln!(out, "no sessions").context("writing the session list")?;
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
    if s.running.iter().all(|r| s.waiting.contains(&r.addr))
        && let Some(w) = s.lock_waits.first()
    {
        let holder = match w.holder_pid {
            Some(pid) => match holder_session(every, pid, e) {
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

/// The live session, other than `of`, whose process is `pid`.
pub(crate) fn holder_session<'a>(
    every: &[&'a SessionEntry],
    pid: u32,
    of: &SessionEntry,
) -> Option<&'a SessionEntry> {
    every
        .iter()
        .copied()
        .find(|h| h.live && h.id != of.id && h.known_meta().is_some_and(|m| m.owner_pid == pid))
}
