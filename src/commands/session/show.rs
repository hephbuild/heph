//! `heph session show`: one frame of a session, printed once, plus what it is
//! stuck on and what to do next (A16).
//!
//! The body comes from folding `events.ndjson` into the same view the TUI
//! draws, up to what was recorded when the command ran (A18); an ended session
//! is drawn at the time of its `End`, so its elapsed times stay what they were.

use std::collections::HashSet;
use std::io::Write;

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::Serialize;

use super::list::holder_session;
use super::{
    SessionState, ViewOpts, ago, command_line, derive_state, escape, list_all, recorded_by_newer,
    require_known, resolve,
};
use crate::tui::session::{
    HeaderKind, SessionEntry, SessionItem, SessionMeta, SessionRecordKind, SessionSnapshot,
    SessionSource,
};
use crate::tui::{self, TUIAppView as _};

/// The width the snapshot is drawn at.
const WIDTH: u16 = 100;
/// How deep a chain of lock holders is followed.
const MAX_CHAIN: usize = 8;
/// The most rows of one kind the stuck-on block prints.
const MAX_ROWS: usize = 10;
/// A running target older than this is listed as long-running.
const LONG_RUNNING_MS: u64 = 30_000;

/// The view a session is drawn with: `gc`'s sweep header, or the build one.
pub(crate) fn new_view(meta: &SessionMeta) -> tui::TuiProgressView {
    match meta.header {
        HeaderKind::Gc => {
            tui::TuiProgressView::with_header(Box::new(tui::GcHeader::new(meta.label.clone())))
        }
        HeaderKind::Build | HeaderKind::Unknown => tui::TuiProgressView::new(meta.label.clone()),
    }
}

/// A session's records folded up to what was there when the fold started.
struct Folded {
    view: tui::TuiProgressView,
    /// Records that carried a build event.
    events: u64,
    /// The `End` record's time and exit code.
    end: Option<(u64, Option<i32>)>,
    /// The latest record's time.
    last_at: Option<u64>,
    skipped: u64,
    truncated: bool,
}

async fn fold(
    source: &dyn SessionSource,
    entry: &SessionEntry,
    meta: &SessionMeta,
) -> anyhow::Result<Folded> {
    let mut stream = source
        .subscribe(entry.id)
        .await
        .with_context(|| format!("reading session {}", entry.id))?;
    let mut folded = Folded {
        view: new_view(meta),
        events: 0,
        end: None,
        last_at: None,
        skipped: 0,
        truncated: false,
    };
    while let Some(item) = stream.next().await {
        match item.with_context(|| format!("reading session {}", entry.id))? {
            SessionItem::Record(r) => {
                folded.last_at = Some(r.at_unix_ms);
                if let Some(ev) = r.build_event() {
                    folded.view.apply(&ev);
                    folded.events += 1;
                }
                match r.session {
                    Some(SessionRecordKind::End { exit_code, .. }) => {
                        folded.end = Some((r.at_unix_ms, exit_code));
                    }
                    Some(SessionRecordKind::Truncated) => folded.truncated = true,
                    _ => {}
                }
            }
            SessionItem::CaughtUp { skipped } => {
                folded.skipped = skipped;
                break;
            }
            SessionItem::Ended { .. } => break,
        }
    }
    Ok(folded)
}

#[derive(Serialize)]
struct ShowJson<'a> {
    id: String,
    /// Viewer-computed; see [`SessionState`].
    state: SessionState,
    #[serde(flatten)]
    snapshot: &'a SessionSnapshot,
}

pub(crate) async fn show(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    arg: &str,
    json: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (i, entry) = resolve(sources, arg, opts).await?;
    let meta = require_known(&entry)?;
    let source = *sources
        .get(i)
        .context("the session's source went missing")?;
    let state = derive_state(&entry, opts.now_ms, opts.stall);

    if json {
        // `status.json` is the snapshot; a session that has not written one yet
        // is folded instead.
        let folded_snapshot;
        let snapshot = match &entry.status {
            Some(s) => s,
            None => {
                let f = fold(source, &entry, meta).await?;
                let mut s = f.view.state().snapshot();
                s.truncated = f.truncated;
                folded_snapshot = s;
                &folded_snapshot
            }
        };
        serde_json::to_writer_pretty(
            &mut *out,
            &ShowJson {
                id: entry.id.encode(),
                state,
                snapshot,
            },
        )
        .context("writing the session")?;
        writeln!(out).context("writing the session")?;
        return Ok(());
    }

    let folded = fold(source, &entry, meta).await?;
    let mut text = String::new();
    header(&mut text, &entry, meta, opts.now_ms);
    if recorded_by_newer(&meta.heph_version, opts.version) {
        text.push_str(&format!(
            "recorded by a newer heph ({}), may be incomplete\n",
            escape(&meta.heph_version)
        ));
    }
    if state == SessionState::Lost {
        text.push_str(
            "the process went away without recording its end: the last events may be missing\n",
        );
    }
    if folded.truncated || entry.status.as_ref().is_some_and(|s| s.truncated) {
        text.push_str("the event log reached its size cap: the progress below stops there\n");
    }
    if folded.skipped > 0 {
        text.push_str(&format!(
            "{} recorded line(s) could not be read and were skipped\n",
            folded.skipped
        ));
    }
    // Drawn at the time of the `End` for an ended session, of the last record
    // for a lost one, of now for a live one.
    let drawn_at = match (folded.end, state) {
        (Some((at, _)), _) => at,
        (None, SessionState::Lost) => folded.last_at.unwrap_or(opts.now_ms),
        _ => opts.now_ms,
    };
    if folded.events == 0 {
        text.push_str("no events recorded\n");
    } else {
        for line in folded.view.snapshot_lines(drawn_at, WIDTH) {
            text.push_str(escape(&line.to_string()).trim_end());
            text.push('\n');
        }
    }

    let every = list_all(sources).await?;
    let every: Vec<&SessionEntry> = every.iter().map(|(_, e)| e).collect();
    let snapshot = folded.view.state().snapshot();
    let mut chain_root = None;
    if matches!(
        state,
        SessionState::Running | SessionState::Waiting | SessionState::Stalled
    ) {
        chain_root = stuck_on(&mut text, &entry, &snapshot, &every, opts.now_ms);
    }
    text.push_str(&next_action(&entry, meta, state, &snapshot, chain_root));
    out.write_all(text.as_bytes())
        .context("writing the session")?;
    Ok(())
}

/// `heph run //go/... (session k7q2r8bn3f, pid 48377, pacu, /Users/…/pacu, started 41s ago, heph v0.48.1)`
fn header(text: &mut String, entry: &SessionEntry, meta: &SessionMeta, now_ms: u64) {
    let worktree = meta
        .workspace_root
        .as_deref()
        .and_then(|w| std::path::Path::new(w).file_name())
        .map_or_else(
            || "-".to_owned(),
            |f| escape(&f.to_string_lossy()).into_owned(),
        );
    let cwd = meta
        .cwd
        .as_deref()
        .map_or_else(|| "-".to_owned(), |c| escape(c).into_owned());
    text.push_str(&format!(
        "{} (session {}, pid {}, {worktree}, {cwd}, started {} ago, heph {})\n",
        command_line(&meta.argv, usize::MAX),
        entry.id,
        meta.owner_pid,
        ago(now_ms, meta.started_unix_ms),
        escape(&meta.heph_version),
    ));
}

/// A session by its command and pid, for a holder line.
fn describe(e: &SessionEntry) -> String {
    match e.known_meta() {
        Some(m) => format!(
            "session {} ({}, pid {})",
            e.id,
            command_line(&m.argv, 40),
            m.owner_pid
        ),
        None => format!("session {}", e.id),
    }
}

/// The stuck-on block. Returns the root of the first lock chain followed, the
/// session the next action should point at.
fn stuck_on<'a>(
    text: &mut String,
    entry: &SessionEntry,
    snapshot: &SessionSnapshot,
    every: &[&'a SessionEntry],
    now_ms: u64,
) -> Option<&'a SessionEntry> {
    let mut block = String::new();
    let mut root = None;
    for w in snapshot.lock_waits.iter().take(MAX_ROWS) {
        block.push_str(&format!(
            "  {}   waiting {} for its result lock, ",
            escape(&w.addr),
            ago(now_ms, w.since_unix_ms)
        ));
        let Some(pid) = w.holder_pid else {
            block.push_str(if w.in_use_by_readers {
                "in use by readers\n"
            } else {
                "holder unknown\n"
            });
            continue;
        };
        let Some(holder) = holder_session(every, pid, entry) else {
            block.push_str(&format!("held by pid {pid} (no session recorded)\n"));
            continue;
        };
        block.push_str(&format!("held by {}\n", describe(holder)));
        let found = follow_chain(&mut block, entry, holder, every);
        root = root.or(Some(found));
    }
    more(&mut block, snapshot.lock_waits.len());
    for w in snapshot.admission_waits.iter().take(MAX_ROWS) {
        block.push_str(&format!(
            "  {}   waiting {} for {} {} ({} free, {} ahead)\n",
            escape(&w.addr),
            ago(now_ms, w.since_unix_ms),
            w.need,
            escape(&w.pool),
            w.free,
            w.ahead
        ));
    }
    more(&mut block, snapshot.admission_waits.len());
    let long: Vec<_> = snapshot
        .running
        .iter()
        .filter(|r| {
            !snapshot.waiting.contains(&r.addr)
                && now_ms.saturating_sub(r.since_unix_ms) >= LONG_RUNNING_MS
        })
        .collect();
    for r in long.iter().take(MAX_ROWS) {
        block.push_str(&format!(
            "  {}   {} for {}\n",
            escape(&r.addr),
            escape(&r.op),
            ago(now_ms, r.since_unix_ms)
        ));
    }
    more(&mut block, long.len());
    if !block.is_empty() {
        text.push_str("\nStuck on\n");
        text.push_str(&block);
    }
    root
}

fn more(block: &mut String, total: usize) {
    if total > MAX_ROWS {
        block.push_str(&format!("  … and {} more\n", total - MAX_ROWS));
    }
}

/// From `holder`, follow each holder's own first lock wait to the session at
/// the root of the chain. Stops at a cycle, a holder that is not a session,
/// and after [`MAX_CHAIN`] hops.
fn follow_chain<'a>(
    block: &mut String,
    start: &SessionEntry,
    holder: &'a SessionEntry,
    every: &[&'a SessionEntry],
) -> &'a SessionEntry {
    let mut seen: HashSet<_> = [start.id, holder.id].into_iter().collect();
    let mut at = holder;
    for _ in 0..MAX_CHAIN {
        let Some(w) = at
            .status
            .as_ref()
            .and_then(|s| s.lock_waits.iter().find(|w| w.holder_pid.is_some()))
        else {
            break;
        };
        let Some(pid) = w.holder_pid else { break };
        block.push_str(&format!("    which waits for {}, ", escape(&w.addr)));
        match holder_session(every, pid, at) {
            Some(next) if seen.insert(next.id) => {
                block.push_str(&format!("held by {}\n", describe(next)));
                at = next;
            }
            Some(next) => {
                block.push_str(&format!("held by {} — a cycle\n", describe(next)));
                break;
            }
            None => {
                block.push_str(&format!("held by pid {pid} (no session recorded)\n"));
                break;
            }
        }
    }
    at
}

/// One line saying what to do next, for every state.
fn next_action(
    entry: &SessionEntry,
    meta: &SessionMeta,
    state: SessionState,
    snapshot: &SessionSnapshot,
    chain_root: Option<&SessionEntry>,
) -> String {
    let id = entry.id;
    let line = match state {
        SessionState::Running => format!("heph session attach {id}   (follow it live)"),
        SessionState::Waiting => match chain_root {
            Some(root) => format!(
                "heph session show {}   (what the session holding the lock is doing)",
                root.id
            ),
            None => format!("heph session attach {id}   (follow it until the wait ends)"),
        },
        SessionState::Stalled => {
            format!("heph session inflight {id}   (ask it what it is doing)")
        }
        SessionState::Failed => match snapshot.failed.first() {
            Some(addr) => format!(
                "heph inspect failure {}   (the failed target's log and outputs)",
                escape(addr)
            ),
            None => format!(
                "re-run `{}`   (it failed before any target did)",
                command_line(&meta.argv, usize::MAX)
            ),
        },
        SessionState::Lost => format!(
            "re-run `{}`   (the process is gone)",
            command_line(&meta.argv, usize::MAX)
        ),
        SessionState::Ok => "nothing   (it succeeded)".to_owned(),
        SessionState::Unknown => {
            format!("heph session list   (session {id} is in an unknown state)")
        }
    };
    format!("\nNext: {line}\n")
}
