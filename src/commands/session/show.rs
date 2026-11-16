//! `heph session show`: one frame of a session, printed once, plus what it is
//! stuck on and what to do next (A16).
//!
//! The body comes from folding `events.ndjson` into the same view the TUI
//! draws, up to what was recorded when the command ran (A18); an ended session
//! is drawn at the time of its `End`, so its elapsed times stay what they were.
//! It is the line, then the dashboard's All list capped at 15 rows, then what
//! the run is stuck on and what to do next.

use std::collections::HashSet;
use std::io::Write;

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::Serialize;

use super::{
    BlockedOn, SessionState, ViewOpts, ago, blocked_on, command_line, derive_state, escape,
    list_all, recorded_by_newer, require_known, resolve,
};
use crate::tui::session::{
    ApprovalStanding, HeaderKind, SessionEntry, SessionItem, SessionMeta, SessionRecordKind,
    SessionSnapshot, SessionSource, format::SessionEnd, holder_session,
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
    let view = match meta.header {
        HeaderKind::Gc => {
            tui::TuiProgressView::with_header(Box::new(tui::GcHeader::new(meta.label.clone())))
        }
        HeaderKind::Build | HeaderKind::Unknown => tui::TuiProgressView::new(meta.label.clone()),
    };
    // The paths in what the run drew are under the run's home, which may
    // not be the one this terminal resolves: redact the one it recorded (I9).
    match meta.home_dir.as_deref() {
        Some(home) => view.with_redact(tui::text::Redact::heph_home(std::path::Path::new(home))),
        None => view,
    }
}

/// A session's records folded up to what was there when the fold started.
struct Folded {
    view: tui::TuiProgressView,
    /// Records that carried a build event.
    events: u64,
    /// The `End` record.
    end: Option<SessionEnd>,
    /// The latest record's time.
    last_at: Option<u64>,
    skipped: u64,
    truncated: bool,
    /// The standing approval decision (Y/N) in force at the fold's end: the
    /// latest `Approval` record's, cleared when the TUI was torn down.
    standing: Option<ApprovalStanding>,
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
        standing: None,
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
                    Some(SessionRecordKind::End {
                        exit_code,
                        panicked,
                    }) => {
                        folded.end = Some(SessionEnd {
                            at_unix_ms: r.at_unix_ms,
                            exit_code,
                            panicked,
                        });
                    }
                    Some(SessionRecordKind::Truncated) => folded.truncated = true,
                    Some(SessionRecordKind::Approval { standing, .. }) => {
                        folded.standing = standing;
                    }
                    // The build a standing decision covered is over.
                    Some(SessionRecordKind::Finished { stopped }) => {
                        folded.standing = None;
                        // The run's line said "stopped": so does this one.
                        if stopped {
                            folded.view.set_stopped();
                        }
                    }
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
    /// Every wait, each holder mapped to its session: `list --json`'s shape.
    blocked_on: Vec<BlockedOn>,
    /// The session at the root of the first lock chain, the one the run is
    /// really waiting for; `null` when no chain leads to a session.
    chain_root: Option<String>,
    next: Next,
    /// The standing approval decision (`approve` after Y, `reject` after N)
    /// acting on the running build; absent when none is.
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_standing: Option<ApprovalStanding>,
    #[serde(flatten)]
    snapshot: &'a SessionSnapshot,
}

/// What to do next, for a person (`Next:`) and an agent (`next`).
#[derive(Debug, Serialize)]
struct Next {
    kind: NextKind,
    /// Display text for a person: each word shell-quoted when it needs it, so
    /// the line pastes back into a shell, then escaped for the terminal; the
    /// program of a `rerun` is its file name. `null` when there is nothing to
    /// do. An agent runs [`Next::argv`] instead.
    command: Option<String>,
    /// What an agent runs: the exact words, unquoted and unescaped. For
    /// `rerun`, the recorded `argv` with the program as recorded; otherwise
    /// `heph` and its arguments. `null` when there is nothing to do.
    argv: Option<Vec<String>>,
    /// The directory to run `argv` in: the recorded `cwd` for the commands
    /// that depend on it (`rerun`, `inspect_failure`, `list`); `null` for the
    /// ones that take a session id and run from anywhere, or when no `cwd` was
    /// recorded.
    cwd: Option<String>,
    /// Why, for a person.
    reason: String,
}

/// [`Next`]'s kind. A consumer treats one it does not know as "read the
/// reason".
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum NextKind {
    /// Follow the session: `heph session attach`.
    Attach,
    /// Look at another session, the root of the lock chain: `heph session show`.
    ShowHolder,
    /// Ask a quiet session what it is doing: `heph session inflight`.
    Inflight,
    /// Read the first failed target's log: `heph inspect failure`.
    InspectFailure,
    /// Run the command again.
    Rerun,
    /// List the sessions.
    List,
    /// Nothing; it succeeded.
    None,
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

    let mut folded = fold(source, &entry, meta).await?;
    // An ended session's line reads as its finished line did.
    if let Some(end) = &folded.end {
        folded.view.set_finished(end.at_unix_ms);
    }
    let snapshot = current_snapshot(&folded, entry.status.as_ref());
    let every = list_all(sources).await?;
    let every: Vec<&SessionEntry> = every.iter().map(|(_, e)| e).collect();
    // The stuck-on block is drawn whatever the output, for the chain root.
    let mut stuck = String::new();
    let chain_root = if matches!(
        state,
        SessionState::Running | SessionState::Waiting | SessionState::Quiet
    ) {
        stuck_on(&mut stuck, &entry, &snapshot, &every, opts.now_ms)
    } else {
        None
    };
    let next = next_action(&entry, meta, state, &snapshot, chain_root);
    // A Y/N still acting on the build: shown for as long as it acts, never
    // for a run that is over.
    let standing = folded.standing.filter(|_| {
        matches!(
            state,
            SessionState::Running | SessionState::Waiting | SessionState::Quiet
        )
    });

    if json {
        serde_json::to_writer_pretty(
            &mut *out,
            &ShowJson {
                id: entry.id.encode(),
                state,
                blocked_on: blocked_on(&snapshot, &entry, &every),
                chain_root: chain_root.map(|r| r.id.encode()),
                next,
                approval_standing: standing,
                snapshot: &snapshot,
            },
        )
        .context("writing the session")?;
        writeln!(out).context("writing the session")?;
        return Ok(());
    }

    let mut text = String::new();
    header(&mut text, &entry, meta, opts.now_ms);
    if recorded_by_newer(&meta.heph_version, opts.version) {
        text.push_str(&format!(
            "recorded by a newer heph ({}), may be incomplete\n",
            escape(&meta.heph_version)
        ));
    }
    if state == SessionState::Quiet {
        let last = entry
            .status
            .as_ref()
            .and_then(|s| s.last_event_unix_ms)
            .unwrap_or(meta.started_unix_ms)
            .max(meta.started_unix_ms);
        let running = snapshot
            .totals
            .running
            .total
            .max(u64::try_from(snapshot.running.len()).unwrap_or(u64::MAX));
        text.push_str(&format!(
            "no events for {}; {running} target{} still running\n",
            ago(opts.now_ms, last),
            if running == 1 { "" } else { "s" }
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
    let drawn_at = match (&folded.end, state) {
        (Some(end), _) => end.at_unix_ms,
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

    if let Some(standing) = standing {
        text.push_str(&standing_line(standing));
    }
    text.push_str(&stuck);
    // The command alone on its line, so it can be copied; the reason below.
    text.push_str(&format!(
        "\nNext: {}\n  {}\n",
        next.command.as_deref().unwrap_or("nothing"),
        next.reason
    ));
    out.write_all(text.as_bytes())
        .context("writing the session")?;
    Ok(())
}

/// The snapshot `show` reasons from and `show --json` prints: the fold of the
/// event log, which has every list in full where `status.json` keeps only
/// their first entries (U3). Past the event log's cap the fold is short of the
/// run, so `status.json`, capped but current, is used instead; its `totals`
/// say what was cut.
fn current_snapshot(folded: &Folded, status: Option<&SessionSnapshot>) -> SessionSnapshot {
    if folded.truncated
        && let Some(s) = status
    {
        return s.clone();
    }
    let mut s = folded.view.state().snapshot();
    s.truncated = folded.truncated;
    s.end = status
        .and_then(|st| st.end.clone())
        .or_else(|| folded.end.clone());
    s
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
        let Some(holder) = holder_session(every, pid, Some(entry.id)) else {
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
            "  {}   waiting {} for {} {} ({} free)\n",
            escape(&w.addr),
            ago(now_ms, w.since_unix_ms),
            w.need,
            escape(&w.pool),
            w.free
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

/// The standing decision's line: the indicator the run's line and title row
/// show, and what it does.
fn standing_line(standing: ApprovalStanding) -> String {
    let does = match standing {
        ApprovalStanding::Approve => "approval requests are approved without a prompt",
        ApprovalStanding::Reject => "approval requests are rejected (their targets fail)",
    };
    format!("{} · {does} until the build ends\n", standing.indicator())
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
        match holder_session(every, pid, Some(at.id)) {
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

/// What to do next, for every state.
fn next_action(
    entry: &SessionEntry,
    meta: &SessionMeta,
    state: SessionState,
    snapshot: &SessionSnapshot,
    chain_root: Option<&SessionEntry>,
) -> Next {
    let id = entry.id.encode();
    // A `heph` command; `in_cwd` when it depends on the directory it runs in.
    let heph = |kind, args: &[&str], in_cwd: bool, reason: &str| {
        let argv: Vec<String> = std::iter::once("heph")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        Next {
            kind,
            command: Some(shell_line(&argv)),
            argv: Some(argv),
            cwd: in_cwd.then(|| meta.cwd.clone()).flatten(),
            reason: reason.to_owned(),
        }
    };
    let rerun = |reason: &str| Next {
        kind: NextKind::Rerun,
        command: Some(shell_line(&meta.argv)),
        argv: Some(meta.argv.clone()),
        cwd: meta.cwd.clone(),
        reason: reason.to_owned(),
    };
    match state {
        SessionState::Running => heph(
            NextKind::Attach,
            &["session", "attach", &id],
            false,
            "follow it live",
        ),
        SessionState::Waiting => match chain_root {
            Some(root) => heph(
                NextKind::ShowHolder,
                &["session", "show", &root.id.encode()],
                false,
                "what the session holding the lock is doing",
            ),
            None => heph(
                NextKind::Attach,
                &["session", "attach", &id],
                false,
                "follow it until the wait ends",
            ),
        },
        SessionState::Quiet => heph(
            NextKind::Inflight,
            &["session", "inflight", &id],
            false,
            "ask it what it is doing",
        ),
        SessionState::Failed => match snapshot.failed.first() {
            Some(addr) => heph(
                NextKind::InspectFailure,
                &["inspect", "failure", addr],
                true,
                "the failed target's log and outputs",
            ),
            None => rerun("run it again: it failed before any target did"),
        },
        SessionState::Lost => rerun("run it again: the process is gone"),
        SessionState::Ok => Next {
            kind: NextKind::None,
            command: None,
            argv: None,
            cwd: None,
            reason: "it succeeded".to_owned(),
        },
        SessionState::Unknown => heph(
            NextKind::List,
            &["session", "list"],
            true,
            &format!("session {id} is in an unknown state"),
        ),
    }
}

/// `argv` as a line a shell reads back into the same words: the program by its
/// file name, each word quoted when it needs it, then escaped for the terminal
/// (a word with a NUL, which no shell can take, is left unquoted).
fn shell_line(argv: &[String]) -> String {
    let mut words = argv.iter();
    let mut line = String::new();
    if let Some(program) = words.next() {
        let name = std::path::Path::new(program)
            .file_name()
            .map_or_else(|| program.clone(), |f| f.to_string_lossy().into_owned());
        line.push_str(&shlex::try_quote(&name).unwrap_or(std::borrow::Cow::Borrowed(&name)));
    }
    for w in words {
        line.push(' ');
        line.push_str(&shlex::try_quote(w).unwrap_or(std::borrow::Cow::Borrowed(w)));
    }
    escape(&line).into_owned()
}
