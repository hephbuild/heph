//! `heph session list`: one row per session, from the catalog (who ran
//! what, when, for how long) and each session directory's `meta.json` and
//! `status.json` (its state now) — never the event log (I3: listing costs a
//! few small reads per session, however long each one ran).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::Context as _;
use serde::Serialize;

use super::{
    ActorKind, BlockedOn, Out, SessionState, ViewOpts, Workspace, ago, blocked_on, command_line,
    derive_state, escape, list_all, truncate,
};
use crate::tui::session::catalog::{self, ListQuery, Listed};
use crate::tui::session::{EntryMeta, SessionEntry, SessionSource, holder_session};

/// The widest COMMAND cell.
const COMMAND_WIDTH: usize = 40;
/// The widest NOW cell.
const NOW_WIDTH: usize = 60;

/// What `list` selects, besides the workspace. The default selects every
/// session.
#[derive(Debug, Default, Clone)]
pub(crate) struct ListFilter {
    /// Sessions started at or after this (unix ms).
    pub since_ms: Option<u64>,
    /// Sessions started at or before this (unix ms).
    pub until_ms: Option<u64>,
    /// Only the sessions that resolved this target, with its outcome.
    pub addr: Option<String>,
    /// Only the sessions that failed; with `addr`, only those in which that
    /// target failed.
    pub failed: bool,
    pub actor: Option<ActorKind>,
    /// The heph subcommand (`run`, `query`, ...).
    pub command: Option<String>,
    pub user: Option<String>,
}

/// Parse `--since`/`--until`: a duration back from `now_ms` (`7d`, `12h`,
/// `0d`) or an RFC 3339 time, seconds optional (`2026-11-20T10:00+02:00`).
pub(crate) fn parse_time(flag: &str, s: &str, now_ms: u64) -> anyhow::Result<u64> {
    if let Ok(d) = hcore::units::parse_duration(s) {
        return Ok(now_ms.saturating_sub(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)));
    }
    let t = s.trim();
    // `Z` as an offset, for the forms without seconds that `%:z` reads.
    let zoned = match t.strip_suffix(['Z', 'z']) {
        Some(rest) => format!("{rest}+00:00"),
        None => t.to_owned(),
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(t)
        .or_else(|_| chrono::DateTime::parse_from_str(&zoned, "%Y-%m-%dT%H:%M%:z"))
        .or_else(|_| chrono::DateTime::parse_from_str(&zoned, "%Y-%m-%dT%H:%M:%S%.f%:z"));
    match parsed {
        Ok(at) => Ok(u64::try_from(at.timestamp_millis()).unwrap_or(0)),
        Err(_) => anyhow::bail!(
            "{flag} {s:?} is neither a duration (7d, 12h, 30m) nor an RFC 3339 time \
             (2026-11-20T10:00+02:00)"
        ),
    }
}

/// The window `--since` and `--until` select, refusing one that ends before
/// it starts.
pub(crate) fn window(
    since: &str,
    until: Option<&str>,
    now_ms: u64,
) -> anyhow::Result<(u64, Option<u64>)> {
    let since_ms = parse_time("--since", since, now_ms)?;
    let until_ms = until
        .map(|u| parse_time("--until", u, now_ms))
        .transpose()?;
    if let Some(u) = until_ms
        && u < since_ms
    {
        anyhow::bail!(
            "--until {:?} is earlier than --since {since:?}",
            until.unwrap_or_default()
        );
    }
    Ok((since_ms, until_ms))
}

/// The catalog's answer, read before `list` runs.
#[derive(Debug, Default)]
pub(crate) struct CatalogRows {
    /// The catalog's sessions in the window, with the target under `--addr`.
    pub rows: Vec<Listed>,
    /// When its oldest session of the workspace in scope started (of any
    /// workspace, unscoped): how far back it answers for that workspace.
    pub oldest_in_scope_ms: Option<u64>,
}

/// Open (and so repair, D14) the catalog in `root` and read what `filter`
/// asks of it, and how far back it reaches for `workspace`. Its notes go to
/// `note`, each a full line. `None` when there is no sessions directory yet,
/// or the catalog cannot be read at all: `list` then answers from the
/// session directories alone.
pub(crate) fn read_catalog(
    root: &Path,
    now_ms: u64,
    filter: &ListFilter,
    workspace: Workspace<'_>,
    note: &mut dyn FnMut(&str),
) -> Option<CatalogRows> {
    if !root.is_dir() {
        return None;
    }
    let mut read = || -> anyhow::Result<CatalogRows> {
        let cat = catalog::open(root, now_ms)?;
        for n in &cat.notes {
            note(n);
        }
        let q = ListQuery {
            since_ms: filter.since_ms,
            until_ms: filter.until_ms,
            addr: filter.addr.as_deref(),
        };
        let oldest_in_scope_ms = match workspace {
            Workspace::All => catalog::oldest_started_ms(&cat.conn)?,
            Workspace::Only(w) => catalog::oldest_started_ms_in(&cat.conn, w)?,
        };
        Ok(CatalogRows {
            rows: catalog::list_sessions(&cat.conn, &q)?,
            oldest_in_scope_ms,
        })
    };
    match read() {
        Ok(rows) => Some(rows),
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "listing without the session catalog");
            None
        }
    }
}

/// One session in `list --json`, which is an array of these in the table's
/// order: live sessions first, then ended ones, each newest first. Times are
/// absolute unix milliseconds.
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
    /// `null` while it runs, and for a session that never recorded its end.
    ended_unix_ms: Option<u64>,
    last_event_unix_ms: Option<u64>,
    /// Its exit code once it ended; `null` while live, or killed by a signal.
    exit_code: Option<i32>,
    /// Set when the session ended in a panic (its `exit_code` may still be 0).
    panicked: bool,
    /// Who ran it; `null` when the catalog has no row for it.
    actor: Option<ActorJson>,
    /// `$USER` of the run; `null` when unknown.
    user: Option<String>,
    /// The heph subcommand (`run`, `query`, ...).
    command: Option<String>,
    /// The session wrote a recording. False only for a private session
    /// (`HEPH_NO_SESSION`), which leaves just its catalog row. Stays true
    /// once cleanup removed the recording: see `recording_kept`.
    recorded: bool,
    /// The recording's directory is still kept, so `show`, `attach`,
    /// `export` and `query --from` can open it. False for a private session,
    /// and for one the catalog still lists after cleanup removed it. True
    /// for a recording in a newer heph's format, which this one cannot open.
    recording_kept: bool,
    /// With `--addr`: that target in this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<TargetJson>,
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

/// Who ran a session, in `list --json`.
#[derive(Debug, Serialize, output::Shape)]
pub(crate) struct ActorJson {
    /// `human`, `agent` or `ci`; an open set.
    kind: String,
    /// The agent or CI system, when known.
    name: Option<String>,
}

/// The `--addr` target in one session, in `list --json`.
#[derive(Debug, Serialize, output::Shape)]
pub(crate) struct TargetJson {
    addr: String,
    /// `ran`, `hit`, `failed` or `cancelled`; an open set.
    outcome: String,
    duration_ms: Option<u64>,
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

/// One listed session: its directory, its catalog row, or both.
#[derive(Debug, Clone, Copy)]
struct Row<'a> {
    entry: Option<&'a SessionEntry>,
    cat: Option<&'a Listed>,
}

impl Row<'_> {
    fn id(&self) -> String {
        match (self.entry, self.cat) {
            (Some(e), _) => e.id.encode(),
            (None, Some(c)) => c.session_id.clone(),
            (None, None) => String::new(),
        }
    }

    fn live(&self) -> bool {
        self.entry.is_some_and(|e| e.live)
    }

    fn started_ms(&self) -> u64 {
        match (self.entry, self.cat) {
            (_, Some(c)) => c.started_ms,
            (Some(e), None) => match &e.meta {
                EntryMeta::Known(m) => m.started_unix_ms,
                EntryMeta::TooNew { .. } => e.id.started_unix_ms(),
            },
            (None, None) => 0,
        }
    }

    fn ended_ms(&self) -> Option<u64> {
        self.entry
            .and_then(|e| e.status.as_ref())
            .and_then(|s| s.end.as_ref())
            .map(|end| end.at_unix_ms)
            .or_else(|| self.cat.and_then(|c| c.ended_ms))
    }

    fn workspace_root(&self) -> Option<&str> {
        match (self.entry.map(|e| &e.meta), self.cat) {
            (Some(EntryMeta::Known(m)), _) => m.workspace_root.as_deref(),
            (_, Some(c)) => c.workspace_root.as_deref(),
            _ => None,
        }
    }

    fn too_new(&self) -> bool {
        self.entry
            .is_some_and(|e| matches!(e.meta, EntryMeta::TooNew { .. }))
    }

    fn command(&self) -> Option<&str> {
        if let Some(c) = self.cat.and_then(|c| c.command.as_deref()) {
            return Some(c);
        }
        match self.entry.map(|e| &e.meta) {
            Some(EntryMeta::Known(m)) => m.argv.get(1).map(String::as_str),
            _ => None,
        }
    }

    fn state(&self, opts: ViewOpts) -> SessionState {
        match (self.entry, self.cat) {
            (Some(e), _) => derive_state(e, opts.now_ms, opts.stall),
            // Its directory is gone (cleaned up, or a private session's).
            (None, Some(c)) => match c.ended_ms {
                Some(_) if c.exit_code == Some(0) && !c.panicked => SessionState::Ok,
                Some(_) => SessionState::Failed,
                None => SessionState::Lost,
            },
            (None, None) => SessionState::Unknown,
        }
    }

    /// The ACTOR cell: `human`, `ci`, `agent (<name>)` or bare `agent`.
    fn actor_cell(&self) -> String {
        let Some(c) = self.cat else {
            return "-".to_owned();
        };
        match (c.actor_kind.as_deref(), c.actor_name.as_deref()) {
            (Some("agent"), Some(name)) => format!("agent ({})", escape(name)),
            (Some(kind), _) => escape(kind).into_owned(),
            (None, _) => "-".to_owned(),
        }
    }
}

/// Every session to list: the directories (live first, then ended newest
/// first), joined with the catalog's rows, filtered. Under `--addr`, only the
/// sessions whose catalog rows resolved that target.
fn rows<'a>(
    every: &[&'a SessionEntry],
    catalog: Option<&'a CatalogRows>,
    workspace: Workspace<'_>,
    filter: &ListFilter,
    opts: ViewOpts,
) -> Vec<Row<'a>> {
    let listed: &[Listed] = catalog.map_or(&[], |c| &c.rows);
    let by_id: HashMap<&str, &Listed> = listed.iter().map(|l| (l.session_id.as_str(), l)).collect();
    let mut rows: Vec<Row<'a>> = Vec::new();
    if filter.addr.is_some() {
        let entries: HashMap<String, &SessionEntry> =
            every.iter().map(|e| (e.id.encode(), *e)).collect();
        rows.extend(listed.iter().map(|l| Row {
            entry: entries.get(&l.session_id).copied(),
            cat: Some(l),
        }));
    } else {
        let mut seen: HashSet<String> = HashSet::with_capacity(every.len());
        for e in every {
            let id = e.id.encode();
            rows.push(Row {
                entry: Some(e),
                cat: by_id.get(id.as_str()).copied(),
            });
            seen.insert(id);
        }
        rows.extend(
            listed
                .iter()
                .filter(|l| !seen.contains(&l.session_id))
                .map(|l| Row {
                    entry: None,
                    cat: Some(l),
                }),
        );
    }
    rows.retain(|r| keep(r, workspace, filter, opts));
    rows.sort_by_cached_key(|r| {
        (
            !r.live(),
            std::cmp::Reverse(r.started_ms()),
            std::cmp::Reverse(r.id()),
        )
    });
    rows
}

fn keep(r: &Row<'_>, workspace: Workspace<'_>, filter: &ListFilter, opts: ViewOpts) -> bool {
    let started = r.started_ms();
    if filter.since_ms.is_some_and(|s| started < s) || filter.until_ms.is_some_and(|u| started > u)
    {
        return false;
    }
    let in_workspace = match workspace {
        Workspace::All => true,
        // Its workspace is unknown: shown everywhere, as the placeholder.
        Workspace::Only(_) if r.too_new() => true,
        Workspace::Only(w) => r.workspace_root() == Some(w),
    };
    if !in_workspace {
        return false;
    }
    if filter.failed {
        // With `--addr`, the target failed in this session ("when did this
        // target break"); without it, the session failed.
        let failed = if filter.addr.is_some() {
            r.cat
                .and_then(|c| c.target.as_ref())
                .is_some_and(|t| t.outcome == "failed")
        } else {
            r.state(opts) == SessionState::Failed
        };
        if !failed {
            return false;
        }
    }
    if let Some(kind) = filter.actor
        && r.cat.and_then(|c| c.actor_kind.as_deref()) != Some(kind.as_str())
    {
        return false;
    }
    if let Some(cmd) = &filter.command
        && r.command() != Some(cmd.as_str())
    {
        return false;
    }
    if let Some(user) = &filter.user
        && r.cat.and_then(|c| c.user.as_deref()) != Some(user.as_str())
    {
        return false;
    }
    true
}

pub(crate) async fn list(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    workspace: Workspace<'_>,
    filter: &ListFilter,
    catalog: Option<&CatalogRows>,
    json: &output::JsonArgs<ListRow>,
    out: Out<'_>,
) -> anyhow::Result<Shown> {
    let all = list_all(sources).await?;
    let every: Vec<&SessionEntry> = all.iter().map(|(_, e)| e).collect();
    let shown = rows(&every, catalog, workspace, filter, opts);
    // `--addr` reads the catalog's target rows, which a session writes when
    // it ends: the live ones in the window are not searched.
    let live_unsearched = if filter.addr.is_some() {
        let window = ListFilter {
            since_ms: filter.since_ms,
            until_ms: filter.until_ms,
            ..ListFilter::default()
        };
        every
            .iter()
            .copied()
            .filter(|e| e.live)
            .filter(|e| {
                let r = Row {
                    entry: Some(*e),
                    cat: None,
                };
                keep(&r, workspace, &window, opts)
            })
            .count()
    } else {
        0
    };
    if json.enabled() {
        let rows = shown.iter().map(|r| json_row(r, &every, opts));
        json.printer_to(out.w, out.style)
            .list_iter(rows, &output::never_cancelled)?;
    } else {
        write_table(&shown, &every, workspace, filter, opts, out.w)?;
    }
    Ok(Shown {
        rows: shown.len(),
        live_unsearched,
    })
}

/// What `list` printed, for [`hints`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Shown {
    pub rows: usize,
    /// Live sessions in the window `--addr` did not search.
    pub live_unsearched: usize,
}

/// The stderr lines that say what `list` did not look at: under `--addr`,
/// the live sessions (their targets reach the catalog when they end); and,
/// when nothing matched, how much further back the catalog reaches than the
/// window `since` (the `--since` text) asked for. That one reaches the
/// oldest session of the workspace in scope, and is said only when no other
/// filter narrowed the list (`--addr` aside, whose row the catalog reaches
/// back as far): that oldest session is of any actor, user and command, so
/// under one of those the suggested `--since` may still match nothing.
pub(crate) fn hints(
    shown: Shown,
    filter: &ListFilter,
    catalog: Option<&CatalogRows>,
    since: &str,
    now_ms: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    if shown.live_unsearched > 0 {
        let n = shown.live_unsearched;
        out.push(format!(
            "heph: --addr searches ended sessions only; {n} live session{} not searched \
             until {} ends\n",
            if n == 1 { " is" } else { "s are" },
            if n == 1 { "it" } else { "each" },
        ));
    }
    let window_only = !filter.failed
        && filter.actor.is_none()
        && filter.command.is_none()
        && filter.user.is_none();
    if shown.rows == 0
        && window_only
        && let Some(since_ms) = filter.since_ms
        && let Some(oldest) = catalog.and_then(|c| c.oldest_in_scope_ms)
        && oldest < since_ms
    {
        let days = now_ms.saturating_sub(oldest).div_ceil(DAY_MS).max(1);
        let keep = catalog::KEEP.as_secs() / (24 * 60 * 60);
        out.push(format!(
            "heph: no session matched since {since}; the catalog holds sessions from up to \
             {days} day{} ago (it keeps {keep} days): --since {days}d reads them\n",
            if days == 1 { "" } else { "s" },
        ));
    }
    out
}

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The stderr line saying `list` widened to every workspace on its own: it
/// was not given `--all`, but there is no workspace here to narrow to.
pub(crate) fn scope_note(all: bool, workspace: Option<&str>) -> Option<&'static str> {
    (!all && workspace.is_none())
        .then_some("heph: not in a workspace; listing the sessions of every workspace\n")
}

/// `r`'s element of `list --json`.
fn json_row(r: &Row<'_>, every: &[&SessionEntry], opts: ViewOpts) -> ListRow {
    let state = r.state(opts);
    let cat = r.cat;
    let actor = cat.and_then(|c| {
        c.actor_kind.as_ref().map(|kind| ActorJson {
            kind: kind.clone(),
            name: c.actor_name.clone(),
        })
    });
    let target = cat.and_then(|c| c.target.as_ref()).map(|t| TargetJson {
        addr: t.addr.clone(),
        outcome: t.outcome.clone(),
        duration_ms: t.duration_us.map(|d| d / 1000),
    });
    let mut j = ListRow {
        id: r.id(),
        owner_pid: cat.and_then(|c| c.owner_pid),
        argv: cat.map(|c| c.argv.clone()).unwrap_or_default(),
        workspace_root: cat.and_then(|c| c.workspace_root.clone()),
        cwd: cat.and_then(|c| c.cwd.clone()),
        state,
        started_unix_ms: r.started_ms(),
        ended_unix_ms: r.ended_ms(),
        last_event_unix_ms: None,
        exit_code: cat.and_then(|c| c.exit_code),
        panicked: cat.is_some_and(|c| c.panicked),
        actor,
        user: cat.and_then(|c| c.user.clone()),
        command: r.command().map(str::to_owned),
        recorded: r.entry.is_some() || cat.is_none_or(|c| c.recorded),
        recording_kept: r.entry.is_some(),
        target,
        progress: None,
        now: None,
        blocked_on: Vec::new(),
        incomplete: cat.map(|c| c.truncated),
        format: None,
    };
    let Some(e) = r.entry else {
        return j;
    };
    let status = e.status.as_ref();
    let end = status.and_then(|s| s.end.as_ref());
    j.blocked_on = status.map(|s| blocked_on(s, e, every)).unwrap_or_default();
    j.last_event_unix_ms = status.and_then(|s| s.last_event_unix_ms);
    j.progress = status.map(Progress::of);
    j.now = match state {
        SessionState::Running | SessionState::Waiting | SessionState::Quiet => status
            .map(|s| now_cell(s, e, every, opts))
            .filter(|n| n != "-"),
        _ => None,
    };
    match &e.meta {
        EntryMeta::Known(m) => {
            j.owner_pid = Some(m.owner_pid);
            j.argv.clone_from(&m.argv);
            j.workspace_root.clone_from(&m.workspace_root);
            j.cwd.clone_from(&m.cwd);
            j.started_unix_ms = m.started_unix_ms;
            j.exit_code = end.and_then(|x| x.exit_code);
            j.panicked = end.is_some_and(|x| x.panicked);
            // Unknown, not "complete", when the status row is there but this
            // heph cannot read it.
            j.incomplete = (!e.status_unreadable).then(|| status.is_some_and(|s| s.truncated));
        }
        EntryMeta::TooNew { found, .. } => {
            j.owner_pid = None;
            j.argv = Vec::new();
            j.started_unix_ms = e.id.started_unix_ms();
            j.blocked_on = Vec::new();
            j.last_event_unix_ms = None;
            j.progress = None;
            j.now = None;
            j.incomplete = None;
            j.format = Some(*found);
        }
    }
    j
}

const HEADERS: [&str; 10] = [
    "ID", "ACTOR", "STARTED", "TOOK", "WORKTREE", "COMMAND", "STATE", "EXIT", "PROGRESS", "NOW",
];
/// Where `--addr`'s OUTCOME column goes: before STATE.
const OUTCOME_AT: usize = 6;

fn write_table(
    shown: &[Row<'_>],
    every: &[&SessionEntry],
    workspace: Workspace<'_>,
    filter: &ListFilter,
    opts: ViewOpts,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if shown.is_empty() {
        let empty = match (&filter.addr, workspace) {
            (Some(addr), _) => format!("no sessions resolved {}", escape(addr)),
            (None, Workspace::Only(_)) => {
                "no sessions in this workspace (--all for every workspace)".to_owned()
            }
            (None, Workspace::All) => "no sessions".to_owned(),
        };
        writeln!(out, "{empty}").context("writing the session list")?;
        return Ok(());
    }
    let with_outcome = filter.addr.is_some();
    let mut header: Vec<String> = HEADERS.iter().map(|h| (*h).to_owned()).collect();
    if with_outcome {
        header.insert(OUTCOME_AT, "OUTCOME".to_owned());
    }
    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|r| {
            let mut cells = row(r, every, opts);
            if with_outcome {
                let outcome = r
                    .cat
                    .and_then(|c| c.target.as_ref())
                    .map_or_else(|| "-".to_owned(), |t| escape(&t.outcome).into_owned());
                cells.insert(OUTCOME_AT, outcome);
            }
            cells
        })
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in &rows {
        for (w, cell) in widths.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    for r in std::iter::once(&header).chain(&rows) {
        let mut line = String::new();
        for (i, (cell, w)) in r.iter().zip(&widths).enumerate() {
            line.push_str(cell);
            if i + 1 < r.len() {
                line.extend(std::iter::repeat_n(' ', w - cell.chars().count() + 2));
            }
        }
        writeln!(out, "{}", line.trim_end()).context("writing the session list")?;
    }
    Ok(())
}

fn row(r: &Row<'_>, every: &[&SessionEntry], opts: ViewOpts) -> Vec<String> {
    let id = r.id();
    let actor = r.actor_cell();
    let started_ms = r.started_ms();
    let started = format!("{} ago", ago(opts.now_ms, started_ms));
    let took = r
        .ended_ms()
        .map_or_else(|| "-".to_owned(), |end| ago(end, started_ms));
    let state = r.state(opts);
    let worktree = r
        .workspace_root()
        .and_then(|w| Path::new(w).file_name())
        .map_or_else(
            || "-".to_owned(),
            |f| escape(&f.to_string_lossy()).into_owned(),
        );
    let e = match (r.entry, r.cat) {
        (Some(e), _) => e,
        // Only the catalog row is left: its directory was cleaned up, or it
        // was a private session that kept none.
        (None, Some(c)) => {
            return catalog_row(c, [id, actor, started, took, worktree], state, opts);
        }
        (None, None) => return vec![id],
    };
    let meta = match &e.meta {
        EntryMeta::Known(m) => m,
        EntryMeta::TooNew { found, .. } => {
            return vec![
                id,
                "-".to_owned(),
                started,
                "-".to_owned(),
                "-".to_owned(),
                format!("format {found}, needs newer heph"),
                SessionState::Unknown.label().to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            ];
        }
    };
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
    vec![
        id,
        actor,
        started,
        took,
        worktree,
        command_line(&meta.argv, COMMAND_WIDTH),
        state.label().to_owned(),
        exit,
        progress,
        truncate(&now, NOW_WIDTH),
    ]
}

/// The row of a session only the catalog still holds; `lead` is its ID,
/// ACTOR, STARTED, TOOK and WORKTREE cells.
fn catalog_row(c: &Listed, lead: [String; 5], state: SessionState, opts: ViewOpts) -> Vec<String> {
    let exit = match (c.ended_ms, c.exit_code) {
        (None, _) => "-".to_owned(),
        (Some(_), _) if c.panicked => "panic".to_owned(),
        (Some(_), Some(code)) => code.to_string(),
        (Some(_), None) => "signal".to_owned(),
    };
    // `incomplete` as a recording's status says it, from the catalog's
    // `truncated` once the recording is gone (as `--json` does).
    let progress = match (c.recorded, c.truncated) {
        (true, false) => "-",
        (true, true) => "incomplete",
        (false, false) => "not recorded",
        (false, true) => "not recorded  incomplete",
    };
    let now = c.ended_ms.map_or_else(
        || "-".to_owned(),
        |end| format!("{} ago", ago(opts.now_ms, end)),
    );
    let mut cells = Vec::from(lead);
    cells.extend([
        command_line(&c.argv, COMMAND_WIDTH),
        state.label().to_owned(),
        exit,
        progress.to_owned(),
        now,
    ]);
    cells
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
            ended_unix_ms: Some(3),
            last_event_unix_ms: Some(2),
            exit_code: None,
            panicked: false,
            actor: Some(ActorJson {
                kind: "agent".to_owned(),
                name: Some("claude-code".to_owned()),
            }),
            user: Some("bob".to_owned()),
            command: Some("run".to_owned()),
            recorded: true,
            recording_kept: true,
            target: Some(TargetJson {
                addr: "//a:b".to_owned(),
                outcome: "ran".to_owned(),
                duration_ms: Some(4),
            }),
            progress: None,
            now: Some("//a:b (1s)".to_owned()),
            blocked_on: Vec::new(),
            incomplete: Some(true),
            format: Some(2),
        })
    }
}
