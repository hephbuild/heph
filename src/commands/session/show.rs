//! `heph session show`: one frame of a session, printed once, plus what it is
//! stuck on and what to do next (A16).
//!
//! The body comes from folding the recording's `records` into the same view
//! the TUI draws, up to what was recorded when the command ran (A18); an ended
//! session is drawn at the time of its `End`, so its elapsed times stay what
//! they were. It is the line, then the dashboard's All list capped at 15 rows,
//! then one line per bounded pool (D30, from the session-wide
//! `heph.session.pool.*` rows of an ended session), then what the run is stuck
//! on and what to do next.

use std::borrow::Cow;
use std::collections::HashSet;

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::Serialize;

use super::reference::{Opener, Resolved, open_ref};
use super::{
    BlockedOn, Out, SessionState, ViewOpts, ago, blocked_on, command_line, derive_state, escape,
    escape_text, list_all, recorded_by_newer, require_known,
};
use crate::tui::session::recording::{ForeignSource, RecordingState};
use crate::tui::session::{
    ApprovalStanding, HeaderKind, SessionEntry, SessionItem, SessionMeta, SessionRecordKind,
    SessionSnapshot, SessionSource,
    format::{
        AdmissionWaitEntry, ListTotals, LockWaitEntry, RunningTarget, SessionEnd, SnapshotCounts,
    },
    holder_session, recorder,
    store::PoolSummary,
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
    /// When the recording passed its full cap (the first `Truncated`), and
    /// when it reached its ceiling and stopped (the second).
    cut_at: [Option<u64>; 2],
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
        cut_at: [None; 2],
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
                    Some(SessionRecordKind::Truncated) => {
                        folded.truncated = true;
                        if let Some(slot) = folded.cut_at.iter_mut().find(|t| t.is_none()) {
                            *slot = Some(r.at_unix_ms);
                        }
                    }
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

/// `heph session show --json`: the viewer's verdict, then the session's
/// snapshot field by field. The snapshot's own `version` is left out: it
/// versions `status.json` on disk, not this output.
#[derive(Serialize, output::Record)]
pub struct ShowView<'a> {
    /// The session id.
    id: String,
    /// Viewer-computed: running, waiting, quiet, ok, failed, lost or unknown;
    /// for a recording opened by path or URL that is not whole, snapshot or
    /// incomplete. An open enum.
    state: StateJson,
    /// Every wait, each holder mapped to its session: `list --json`'s shape.
    blocked_on: Vec<BlockedOn>,
    /// The session at the root of the first lock chain, the one the run is
    /// really waiting for; `null` when no chain leads to a session.
    chain_root: Option<String>,
    /// What to do next: `kind`, `command`, `argv`, `cwd`, `reason`.
    next: Next,
    /// The standing approval decision (`approve` after Y, `reject` after N)
    /// acting on the running build; absent when none is.
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_standing: Option<ApprovalStanding>,
    /// The first build event's time.
    first_event_unix_ms: Option<u64>,
    /// The latest build event's time.
    last_event_unix_ms: Option<u64>,
    /// How many targets matched, finished, failed and are in flight.
    counts: &'a SnapshotCounts,
    /// Targets with an operation open, oldest first.
    running: &'a [RunningTarget],
    /// Targets blocked on a result lock or on admission, sorted.
    waiting: &'a [String],
    /// Failed targets, in failure order.
    failed: &'a [String],
    /// Result locks waited on, with their holders.
    lock_waits: &'a [LockWaitEntry],
    /// Resource-pool admissions waited on.
    admission_waits: &'a [AdmissionWaitEntry],
    /// Each list's full length, and whether it was cut.
    totals: &'a ListTotals,
    /// The event log hit its cap; the fold is short of the session.
    truncated: bool,
    /// How the session ended; `null` while it runs.
    end: Option<&'a SessionEnd>,
    /// Records that could not be decoded and were skipped by the fold.
    skipped: u64,
    /// The recording is short of the run; `incomplete_reason` says why.
    incomplete: bool,
    /// Why the recording is incomplete, `null` when it is not. An open enum.
    incomplete_reason: Option<IncompleteReason>,
    /// One entry per bounded pool the session admitted from, by name; empty
    /// while it runs (the summaries are written at its end). PR 5 adds the
    /// `resources_vs_machine` totals to each entry.
    pools: &'a [PoolSummary],
    /// A recording opened by path or URL: where from, whether it is whole,
    /// and what its export carried. Absent for a local session.
    #[serde(skip_serializing_if = "Option::is_none")]
    foreign: Option<ForeignJson<'a>>,
}

impl<'a> ShowView<'a> {
    /// Every field of `snapshot` but its `version`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per field of the output that is not the snapshot's"
    )]
    fn new(
        id: String,
        state: StateJson,
        blocked_on: Vec<BlockedOn>,
        chain_root: Option<String>,
        next: Next,
        approval_standing: Option<ApprovalStanding>,
        skipped: u64,
        incomplete: bool,
        pools: &'a [PoolSummary],
        foreign: Option<ForeignJson<'a>>,
        snapshot: &'a SessionSnapshot,
    ) -> Self {
        // Destructured, so a field added to the snapshot is a compile error
        // here rather than silently missing from `show --json`.
        let SessionSnapshot {
            version: _,
            first_event_unix_ms,
            last_event_unix_ms,
            counts,
            running,
            waiting,
            failed,
            lock_waits,
            admission_waits,
            totals,
            truncated,
            end,
        } = snapshot;
        Self {
            id,
            state,
            blocked_on,
            chain_root,
            next,
            approval_standing,
            first_event_unix_ms: *first_event_unix_ms,
            last_event_unix_ms: *last_event_unix_ms,
            counts,
            running,
            waiting,
            failed,
            lock_waits,
            admission_waits,
            totals,
            truncated: *truncated,
            end: end.as_ref(),
            skipped,
            incomplete,
            incomplete_reason: incomplete.then_some(IncompleteReason::SizeCap),
            pools,
            foreign,
        }
    }
}

/// What to do next, for a person (`Next:`) and an agent (`next`).
#[derive(Debug, Serialize, output::Shape)]
pub(crate) struct Next {
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

/// Why a recording is short of its run: `show --json`'s `incomplete_reason`.
/// An open enum: a consumer reads a value it does not know as "incomplete",
/// and the text says why.
#[derive(Debug, Clone, Copy, Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IncompleteReason {
    /// The recording passed its size cap: output and log lines past it were
    /// dropped, and past the ceiling nothing but the end was recorded.
    SizeCap,
}

/// [`Next`]'s kind. A consumer treats one it does not know as "read the
/// reason".
#[derive(Debug, Clone, Copy, Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NextKind {
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

/// What `show` adds for a recording opened by path or URL (spec I7): where
/// it came from, whether it is whole, and the notices and root failure logs
/// its export carried. Absent for a local session.
#[derive(Serialize, output::Shape)]
struct ForeignJson<'a> {
    /// The path, or the URL without its credentials.
    origin: &'a str,
    /// `snapshot` (exported while it ran) or `incomplete` (no end
    /// recorded); `null` for a whole recording. An open enum.
    partial: Option<Partial>,
    notices: Vec<NoticeJson<'a>>,
    failure_logs: Vec<FailureLogJson<'a>>,
}

#[derive(Debug, Clone, Copy, Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
enum Partial {
    Snapshot,
    Incomplete,
}

/// `show --json`'s `state`: a [`SessionState`], or, for a recording opened by
/// path or URL that is not whole, its [`Partial`] (`snapshot`,
/// `incomplete`). Derived from a local session's liveness, those would read
/// `lost`, which says something about a process this viewer cannot see. An
/// open enum. Flat rather than `untagged` over the two, which the output
/// layer's field table cannot list; the values are the same strings.
#[derive(Debug, Clone, Copy, Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
enum StateJson {
    Running,
    Waiting,
    Quiet,
    Ok,
    Failed,
    Lost,
    Unknown,
    Snapshot,
    Incomplete,
}

impl StateJson {
    fn new(state: SessionState, partial: Option<Partial>) -> Self {
        match (partial, state) {
            (Some(Partial::Snapshot), _) => Self::Snapshot,
            (Some(Partial::Incomplete), _) => Self::Incomplete,
            (None, SessionState::Running) => Self::Running,
            (None, SessionState::Waiting) => Self::Waiting,
            (None, SessionState::Quiet) => Self::Quiet,
            (None, SessionState::Ok) => Self::Ok,
            (None, SessionState::Failed) => Self::Failed,
            (None, SessionState::Lost) => Self::Lost,
            (None, SessionState::Unknown) => Self::Unknown,
        }
    }
}

#[derive(Serialize, output::Shape)]
struct NoticeJson<'a> {
    addr: &'a str,
    seq: u64,
    name: &'a str,
    text: &'a str,
}

#[derive(Serialize, output::Shape)]
struct FailureLogJson<'a> {
    addr: &'a str,
    /// The log's last bytes (at most 1 MiB), as UTF-8 with invalid bytes
    /// replaced.
    log: Cow<'a, str>,
    /// What came before them and was left out.
    cut_bytes: u64,
}

/// `heph session show <ref>`: a local session, or a recording opened by path
/// or URL.
pub(crate) async fn show(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    arg: &str,
    json: &output::JsonArgs<ShowView<'static>>,
    out: Out<'_>,
    opener: &Opener,
) -> anyhow::Result<()> {
    match open_ref(sources, arg, opts, opener).await? {
        Resolved::Local { index, entry } => {
            let source = *sources
                .get(index)
                .context("the session's source went missing")?;
            render(source, sources, entry, None, opts, json, out).await
        }
        Resolved::Foreign(rec) => {
            // Reads the whole recording's session-wide rows: blocking work.
            let foreign = hcore::blocking::run(move || rec.into_source()).await?;
            let sources: [&dyn SessionSource; 1] = [&foreign];
            let entry = foreign.entry().clone();
            render(&foreign, &sources, entry, Some(&foreign), opts, json, out).await
        }
    }
}

async fn render(
    source: &dyn SessionSource,
    sources: &[&dyn SessionSource],
    entry: SessionEntry,
    foreign: Option<&ForeignSource>,
    opts: ViewOpts,
    json: &output::JsonArgs<ShowView<'static>>,
    out: Out<'_>,
) -> anyhow::Result<()> {
    let meta = require_known(&entry)?;
    let state = derive_state(&entry, opts.now_ms, opts.stall);

    let mut folded = fold(source, &entry, meta).await?;
    // An ended session's line reads as its finished line did.
    if let Some(end) = &folded.end {
        folded.view.set_finished(end.at_unix_ms);
    }
    let snapshot = current_snapshot(&folded, entry.status.as_ref());
    let pools = source.pools(entry.id).await?;
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
    let next = match foreign {
        // Every other next step reads this machine's sessions or store.
        Some(f) => Next {
            kind: NextKind::None,
            command: None,
            argv: None,
            cwd: None,
            reason: if !f.failure_logs().is_empty() {
                "a recording read from a file: the logs of its root failures are above".to_owned()
            } else if state == SessionState::Failed {
                // `--no-logs`, or a failure that left no log.
                "a recording read from a file, with no failure logs in it: `heph inspect failure \
                 <addr>` prints them on the machine that ran it"
                    .to_owned()
            } else {
                "a recording read from a file: nothing to do from here".to_owned()
            },
        },
        None => next_action(&entry, meta, state, &snapshot, chain_root),
    };
    let partial = foreign.and_then(|f| match f.state() {
        RecordingState::Snapshot => Some(Partial::Snapshot),
        RecordingState::Incomplete => Some(Partial::Incomplete),
        RecordingState::Live | RecordingState::Finished { .. } => None,
    });
    let notices = foreign.map(|f| f.notices());
    let notices: Vec<NoticeJson<'_>> = notices.as_deref().map_or_else(Vec::new, |n| {
        let mut v: Vec<NoticeJson<'_>> = n
            .iter()
            .map(|((addr, seq, name), text)| NoticeJson {
                addr,
                seq: *seq,
                name,
                text,
            })
            .collect();
        v.sort_by(|a, b| (a.seq, a.addr, a.name).cmp(&(b.seq, b.addr, b.name)));
        v
    });
    let failure_logs: Vec<FailureLogJson<'_>> = foreign.map_or_else(Vec::new, |f| {
        f.failure_logs()
            .iter()
            .map(|l| FailureLogJson {
                addr: &l.addr,
                log: String::from_utf8_lossy(&l.log),
                cut_bytes: l.cut_bytes,
            })
            .collect()
    });
    // A Y/N still acting on the build: shown for as long as it acts, never
    // for a run that is over.
    let standing = folded.standing.filter(|_| {
        matches!(
            state,
            SessionState::Running | SessionState::Waiting | SessionState::Quiet
        )
    });
    let incomplete = folded.truncated || entry.status.as_ref().is_some_and(|s| s.truncated);

    if json.enabled() {
        let view = ShowView::new(
            entry.id.encode(),
            StateJson::new(state, partial),
            blocked_on(&snapshot, &entry, &every),
            chain_root.map(|r| r.id.encode()),
            next,
            standing,
            folded.skipped,
            incomplete,
            &pools,
            foreign.map(|f| ForeignJson {
                origin: f.origin(),
                partial,
                notices,
                failure_logs,
            }),
            &snapshot,
        );
        return json
            .printer_to(out.w, out.style)
            .record(&view, &output::never_cancelled);
    }
    let out = out.w;

    let mut text = String::new();
    // A snapshot or an incomplete recording says so on its first line.
    if let Some(f) = foreign {
        text.push_str(&format!("read from {}", escape(f.origin())));
        if let Some(caveat) = f.state().caveat() {
            text.push_str(&format!(": {caveat}"));
        }
        text.push('\n');
    }
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
    // A foreign recording with no end said so on its first line: nothing
    // here knows whether its process went away.
    if state == SessionState::Lost && foreign.is_none() {
        text.push_str(
            "the process went away without recording its end: the last events may be missing\n",
        );
    }
    if incomplete {
        text.push_str(&incomplete_line(&folded, meta.started_unix_ms));
    }
    if folded.skipped > 0 {
        text.push_str(&format!(
            "{} record{} could not be read and {} skipped, so the progress below leaves {} out: \
             damaged, or written in a shape this heph does not know (a newer heph may read {})\n",
            folded.skipped,
            if folded.skipped == 1 { "" } else { "s" },
            if folded.skipped == 1 { "was" } else { "were" },
            if folded.skipped == 1 { "it" } else { "them" },
            if folded.skipped == 1 { "it" } else { "them" },
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

    for pool in &pools {
        text.push_str(&pool_line(pool));
    }
    if let Some(standing) = standing {
        text.push_str(&standing_line(standing));
    }
    text.push_str(&stuck);
    for n in &notices {
        text.push_str(&format!(
            "\napproval notice {} of request {} for {}:\n",
            escape(n.name),
            n.seq,
            escape(n.addr)
        ));
        push_block(&mut text, n.text);
    }
    for l in &failure_logs {
        text.push_str(&format!("\nlog of {}", escape(l.addr)));
        if l.cut_bytes > 0 {
            text.push_str(&format!(
                " (its first {} left out)",
                hcore::units::human_bytes(l.cut_bytes)
            ));
        }
        text.push_str(":\n");
        push_block(&mut text, &l.log);
    }
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

/// Text from a recording (a notice, a log), escaped for the terminal with
/// its lines kept, ending in a newline.
fn push_block(text: &mut String, block: &str) {
    let escaped = escape_text(block);
    text.push_str(&escaped);
    if !escaped.ends_with('\n') {
        text.push('\n');
    }
}

/// Why the recording is short of the run, which cap it hit, how far into the
/// run, and what it dropped.
fn incomplete_line(folded: &Folded, started_ms: u64) -> String {
    let size = hcore::units::human_bytes;
    let into = |t: u64| ago(t, started_ms);
    let full_cap = size(recorder::FULL_CAP);
    match folded.cut_at {
        [Some(cap), Some(ceiling)] => format!(
            "incomplete: the recording passed its {full_cap} size cap {} into the run, and \
             output and log lines were dropped from there; it reached its {} ceiling {} into the \
             run and recorded nothing after that but its end, so the progress below stops there\n",
            into(cap),
            size(recorder::CEILING),
            into(ceiling),
        ),
        [Some(cap), None] => format!(
            "incomplete: the recording passed its {full_cap} size cap {} into the run: output \
             and log lines were dropped from there, and progress-only events with them; results, \
             cache and pool events were kept\n",
            into(cap),
        ),
        // The status row says so, but the fold saw no `Truncated` record.
        _ => format!(
            "incomplete: the recording passed its {full_cap} size cap: output and log lines \
             were dropped from there\n"
        ),
    }
}

/// One bounded pool's line, e.g.
/// `cpu pool (capacity 8): held 7.6 mean while in use (95%), 8 peak; something
/// was waiting for 1m2s`. PR 5 adds the machine comparison after the peak.
fn pool_line(p: &PoolSummary) -> String {
    let amount = |v: f64| {
        if p.unit == "By" {
            hcore::units::human_bytes(u64::try_from(v as i64).unwrap_or(0))
        } else if (v - v.round()).abs() < 0.05 {
            format!("{v:.0}")
        } else {
            format!("{v:.1}")
        }
    };
    let mut line = format!("{} pool", escape(&p.pool));
    let share = match p.capacity {
        Some(cap) => {
            line.push_str(&format!(" (capacity {})", amount(cap)));
            (cap > 0.0).then(|| format!(" ({:.0}%)", p.held_mean / cap * 100.0))
        }
        None => None,
    };
    line.push_str(&format!(
        ": held {} mean while in use{}, {} peak; ",
        amount(p.held_mean),
        share.unwrap_or_default(),
        amount(p.held_peak),
    ));
    if p.waiting_us == 0 {
        line.push_str("nothing waited for it\n");
    } else {
        line.push_str(&format!(
            "something was waiting for {}\n",
            hcore::units::human_duration(std::time::Duration::from_micros(p.waiting_us))
        ));
    }
    line
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        let snapshot = SessionSnapshot::default();
        let pools = [PoolSummary {
            pool: "cpu".to_owned(),
            unit: "{cpu}".to_owned(),
            capacity: Some(8.0),
            held_mean: 1.5,
            held_peak: 4.0,
            waiting_us: 10,
        }];
        let view = ShowView::new(
            "id".to_owned(),
            StateJson::Running,
            Vec::new(),
            None,
            Next {
                kind: NextKind::Attach,
                command: None,
                argv: None,
                cwd: None,
                reason: String::new(),
            },
            Some(ApprovalStanding::Approve),
            1,
            true,
            &pools,
            Some(ForeignJson {
                origin: "origin",
                partial: Some(Partial::Snapshot),
                notices: Vec::new(),
                failure_logs: Vec::new(),
            }),
            &snapshot,
        );
        output::check_keys(&view)?;
        // Every snapshot key but `version` is in the view: what `flatten`
        // printed before, field for field.
        let serde_json::Value::Object(keys) = serde_json::to_value(&snapshot)? else {
            anyhow::bail!("a snapshot serializes as an object");
        };
        let fields = <ShowView<'_> as output::Record>::FIELDS;
        let missing: Vec<&String> = keys
            .keys()
            .filter(|k| *k != "version" && !fields.iter().any(|f| f.name == k.as_str()))
            .collect();
        anyhow::ensure!(missing.is_empty(), "snapshot keys missing: {missing:?}");
        Ok(())
    }
}
