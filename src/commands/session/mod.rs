//! `heph session`: list, show, attach to and query the sessions other heph
//! processes record under `~/.heph/sessions` (see `htui::tui::session`).
//!
//! Every command here is a viewer. A viewer never records a session of its own
//! (its apps say `records_session() == false`), ignores `HEPH_NO_SESSION`, and
//! writes nothing — `heph session inflight` dropping `inflight.request` is the
//! one exception the design names, and the source owns it.
//!
//! The commands run against `&[&dyn SessionSource]` and merge what the sources
//! say, so a test (and later a daemon) is just another source.
//!
//! Exit codes: 125 for this command's own failures (no such session, an
//! ambiguous id, a format this heph cannot read, a lost session on attach, an
//! in-flight request that went unanswered); `attach` otherwise exits with the
//! session's exit code, and 0 on detach. A session can exit 125 too, so
//! `attach` names a mirrored 125 on stderr. See [`SessionExit`].

mod attach;
mod list;
mod show;
#[cfg(test)]
mod tests;

use crate::tui::session::SessionSlot;
use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use super::GlobalOptions;
use super::bootstrap;
use crate::tui::LogSink;
use crate::tui::session::{EntryMeta, FileSource, SessionEntry, SessionId, SessionSource};

/// How long `heph session inflight` waits for the session to answer.
const INFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// The quiet time after which a live session reads as `quiet` when
/// `--stall-notice` is off.
const DEFAULT_STALL: Duration = Duration::from_secs(60);

#[derive(Args, Clone, Debug)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommands,
}

#[derive(Subcommand, Clone, Debug)]
pub enum SessionCommands {
    /// One line per session, live first
    ///
    /// Only this workspace's sessions unless `--all`. Reads each session's
    /// `meta.json` and `status.json`, never its event log.
    ///
    /// STATE is computed by this viewer:
    ///
    /// - `running`: live, with a target working.
    /// - `waiting`: live, every open target in a lock or admission wait.
    /// - `quiet`: live, no event for longer than `--stall-notice` (60s when
    ///   it is off); not a verdict, a long compile is quiet too.
    /// - `ok` / `failed`: ended, by its exit code.
    /// - `lost`: the process went away without recording its end.
    /// - `unknown`: a session this heph cannot read.
    ///
    /// The other commands take an id's last 4–5 characters, or a live
    /// session's pid, in place of the full id. The leading characters are
    /// the start time, so sessions started minutes apart share them.
    ///
    /// Examples:
    ///
    /// `heph session list`
    ///
    /// `heph session list --all --json`
    List(ListArgs),
    /// Print a session's progress once, and what it is stuck on
    ///
    /// `<id>` is a full session id, the pid of a live session, or a unique
    /// prefix or suffix of an id. Give the suffix (the last 4–5 characters)
    /// or the pid: the leading characters are the start time, so sessions
    /// started minutes apart share them.
    ///
    /// Examples:
    ///
    /// `heph session show 02kf5`
    ///
    /// `heph session show 48213 --json`
    Show(ShowArgs),
    /// Follow a session live, until it ends or you detach
    ///
    /// `q` (or Ctrl-C) detaches and leaves the session running. Exits with the
    /// session's exit code once it ends, 0 on detach, and 125 when heph cannot
    /// follow it. A 125 that is the session's own is named on stderr.
    ///
    /// Examples:
    ///
    /// `heph session attach 02kf5`
    ///
    /// `heph session attach 48213`
    Attach(AttachArgs),
    /// Ask a running session for its in-flight report
    ///
    /// The same report `SIGQUIT` writes, without sending any signal: the
    /// session answers through its own directory within 5s, or this exits 125.
    ///
    /// Examples:
    ///
    /// `heph session inflight 02kf5`
    Inflight(InflightArgs),
}

#[derive(Args, Clone, Debug)]
pub struct ListArgs {
    /// Every workspace's sessions, not only this one's.
    #[arg(long)]
    pub all: bool,
    /// Machine-readable output: `{version, sessions: [...]}`, sorted by id.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone, Debug)]
pub struct ShowArgs {
    /// Session id, its last 4–5 characters, or a live session's pid (any
    /// unique prefix or suffix of an id works).
    pub id: String,
    /// The session's snapshot as JSON, with the viewer-computed `state`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone, Debug)]
pub struct AttachArgs {
    /// Session id, its last 4–5 characters, or a live session's pid (any
    /// unique prefix or suffix of an id works).
    pub id: String,
}

#[derive(Args, Clone, Debug)]
pub struct InflightArgs {
    /// Session id, its last 4–5 characters, or a live session's pid (any
    /// unique prefix or suffix of an id works).
    pub id: String,
    /// `{id, report}`.
    #[arg(long)]
    pub json: bool,
}

impl SessionArgs {
    pub fn execute(
        &self,
        sink: LogSink,
        session: &Arc<SessionSlot>,
        global: &GlobalOptions,
    ) -> anyhow::Result<()> {
        wrap_exit(execute(&self.command, sink, session, global))
    }
}

/// Every error that is not already a [`SessionExit`] is this command's own:
/// exit 125, so it is never read as an exit code `attach` mirrored.
pub(crate) fn wrap_exit(res: anyhow::Result<()>) -> anyhow::Result<()> {
    res.map_err(|e| {
        if e.downcast_ref::<SessionExit>().is_some() {
            e
        } else {
            SessionExit::own(e).into()
        }
    })
}

fn execute(
    cmd: &SessionCommands,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    // A viewer reads HOME and nothing else: `HEPH_NO_SESSION` turns recording
    // off, not looking.
    let root = hcore::userdir::heph_user_dir_from(std::env::var_os("HOME").as_deref())
        .context("locating ~/.heph/sessions")?
        .join(crate::tui::session::SESSIONS_DIR);
    let file = FileSource::new(root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let opts = ViewOpts {
        now_ms: hcore::events::now_unix_ms(),
        stall: stall_threshold(global.stall_notice),
        version: hcore::version::current(),
    };
    let mut stdout = std::io::stdout().lock();
    match cmd {
        SessionCommands::List(a) => {
            // Outside a workspace there is no "this one": list them all.
            let workspace = if a.all {
                None
            } else {
                crate::engine::get_root()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            };
            let filter = match &workspace {
                Some(w) => Workspace::Only(w),
                None => Workspace::All,
            };
            if let Some(note) = list::scope_note(a.all, workspace.as_deref()) {
                sink.write_diagnostic(note);
            }
            bootstrap::block_on(list::list(&sources, opts, filter, a.json, &mut stdout))?
        }
        SessionCommands::Show(a) => {
            bootstrap::block_on(show::show(&sources, opts, &a.id, a.json, &mut stdout))?
        }
        SessionCommands::Inflight(a) => {
            bootstrap::block_on(inflight(&sources, opts, &a.id, a.json, &mut stdout))?
        }
        SessionCommands::Attach(a) => {
            drop(stdout);
            bootstrap::block_on(attach::attach(
                &sources,
                opts,
                &a.id,
                sink,
                session,
                global.no_tui,
            ))?
        }
    }
}

/// How a `heph session` command ended, when not with 0. `main.rs` exits with
/// `code`, rendering `error` first when there is one.
#[derive(Debug)]
pub struct SessionExit {
    pub code: u8,
    pub error: Option<anyhow::Error>,
}

impl SessionExit {
    /// The command's own failure (A14). A session can exit 125 as well;
    /// `attach` says so on stderr when it mirrors one (`attach::exit_note`).
    pub const OWN_FAILURE: u8 = 125;

    pub fn own(error: anyhow::Error) -> Self {
        Self {
            code: Self::OWN_FAILURE,
            error: Some(error),
        }
    }

    /// `attach` mirroring the session's exit code; nothing to render.
    pub fn mirrored(code: i32) -> Self {
        Self {
            // An exit status is 0–255; anything else was not one.
            code: u8::try_from(code).unwrap_or(Self::OWN_FAILURE),
            error: None,
        }
    }
}

impl std::fmt::Display for SessionExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            Some(e) => write!(f, "{e:#}"),
            None => write!(f, "exit {}", self.code),
        }
    }
}

impl std::error::Error for SessionExit {}

/// The exit code a `heph session` error carries, `None` for any other error.
pub fn exit_code(e: &anyhow::Error) -> Option<u8> {
    e.downcast_ref::<SessionExit>().map(|x| x.code)
}

/// The viewer's clock and thresholds, injected so a test is deterministic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewOpts {
    pub now_ms: u64,
    /// Quiet for longer than this, a live session reads as `quiet`.
    pub stall: Duration,
    /// This heph's version, against which a session's recorder is "newer".
    pub version: &'static str,
}

pub(crate) fn stall_threshold(stall_notice: Duration) -> Duration {
    if stall_notice.is_zero() {
        DEFAULT_STALL
    } else {
        stall_notice
    }
}

/// Which workspace's sessions `list` shows.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Workspace<'a> {
    All,
    Only(&'a str),
}

/// What a session is doing, as a viewer derives it — never stored.
/// `Unknown` is a session in a format this heph cannot read, and whatever a
/// newer heph adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Running,
    Waiting,
    /// No event for longer than the quiet threshold (`--stall-notice`, 60s
    /// when it is off). Not a verdict: a long compile is quiet too. It moves
    /// with the viewer's `--stall-notice`; `last_event_unix_ms` is the stable
    /// signal.
    Quiet,
    Ok,
    Failed,
    Lost,
    #[serde(other)]
    Unknown,
}

impl SessionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Quiet => "quiet",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Lost => "lost",
            Self::Unknown => "unknown",
        }
    }
}

/// Derive a session's state from what its listing says.
///
/// Ended: `ok` / `failed` from `status.json`'s `end`, `lost` without one,
/// `unknown` when `status.json` is there but unreadable (newer). Live:
/// `waiting` when something waits and every running target is among the
/// waiters, else `quiet` when no event came for longer than `stall`, else
/// `running`.
pub(crate) fn derive_state(entry: &SessionEntry, now_ms: u64, stall: Duration) -> SessionState {
    let Some(meta) = entry.known_meta() else {
        return SessionState::Unknown;
    };
    let status = entry.status.as_ref();
    if !entry.live {
        return match status.and_then(|s| s.end.as_ref()) {
            Some(end) if end.exit_code == Some(0) && !end.panicked => SessionState::Ok,
            Some(_) => SessionState::Failed,
            // A snapshot this heph cannot read may hold the `End`: say so
            // rather than call the session lost.
            None if entry.status_unreadable => SessionState::Unknown,
            None => SessionState::Lost,
        };
    }
    let Some(status) = status else {
        return SessionState::Running;
    };
    if all_waiting(status) {
        return SessionState::Waiting;
    }
    let last = status
        .last_event_unix_ms
        .unwrap_or(meta.started_unix_ms)
        .max(meta.started_unix_ms);
    let stall_ms = u64::try_from(stall.as_millis()).unwrap_or(u64::MAX);
    if now_ms.saturating_sub(last) > stall_ms {
        SessionState::Quiet
    } else {
        SessionState::Running
    }
}

/// Something waits, and every running target is among the waiters. From the
/// counts, which cover the full lists, never from the lists themselves: in
/// `status.json` `running` and `waiting` are each cut to their first entries.
pub(crate) fn all_waiting(status: &crate::tui::session::SessionSnapshot) -> bool {
    (!status.waiting.is_empty() || status.totals.waiting.total > 0) && every_running_waits(status)
}

/// No running target is outside a wait: from `running_unblocked`, or, from a
/// writer that predates the count, by comparing the lists (exact when neither
/// was cut).
pub(crate) fn every_running_waits(status: &crate::tui::session::SessionSnapshot) -> bool {
    match status.counts.running_unblocked {
        Some(n) => n == 0,
        None => status
            .running
            .iter()
            .all(|r| status.waiting.contains(&r.addr)),
    }
}

/// What one waiting target waits for, in `list --json` and `show --json`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum BlockedOn {
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

/// Every wait in `status`, lock waits first, each holder pid mapped to the
/// session that records it.
pub(crate) fn blocked_on(
    status: &crate::tui::session::SessionSnapshot,
    entry: &SessionEntry,
    every: &[&SessionEntry],
) -> Vec<BlockedOn> {
    status
        .lock_waits
        .iter()
        .map(|w| BlockedOn::Lock {
            addr: w.addr.clone(),
            holder_pid: w.holder_pid,
            holder_session: w
                .holder_pid
                .and_then(|pid| crate::tui::session::holder_session(every, pid, Some(entry.id)))
                .map(|h| h.id.encode()),
        })
        .chain(status.admission_waits.iter().map(|w| BlockedOn::Admission {
            addr: w.addr.clone(),
            pool: w.pool.clone(),
        }))
        .collect()
}

/// Every session the sources know, merged: the first source to list an id
/// wins it. Each entry keeps the index of the source it came from.
pub(crate) async fn list_all(
    sources: &[&dyn SessionSource],
) -> anyhow::Result<Vec<(usize, SessionEntry)>> {
    let mut out: Vec<(usize, SessionEntry)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, source) in sources.iter().enumerate() {
        for entry in source.list().await.context("listing sessions")? {
            if seen.insert(entry.id) {
                out.push((i, entry));
            }
        }
    }
    out.sort_by_key(|(_, e)| e.id);
    Ok(out)
}

/// Find the session `arg` names (A12): an exact id; an all-digit argument as
/// the pid of a *live* session; then a unique prefix or a unique suffix of an
/// id. Several matches list the candidates.
pub(crate) async fn resolve(
    sources: &[&dyn SessionSource],
    arg: &str,
    opts: ViewOpts,
) -> anyhow::Result<(usize, SessionEntry)> {
    let arg = arg.trim();
    if arg.is_empty() {
        anyhow::bail!("no session id given");
    }
    if let Ok(id) = SessionId::parse(arg) {
        for (i, source) in sources.iter().enumerate() {
            if let Some(entry) = source
                .snapshot(id)
                .await
                .with_context(|| format!("reading session {id}"))?
            {
                return Ok((i, entry));
            }
        }
    }
    let all = list_all(sources).await?;
    if arg.bytes().all(|b| b.is_ascii_digit())
        && let Ok(pid) = arg.parse::<u32>()
    {
        let mut by_pid: Vec<&(usize, SessionEntry)> = all
            .iter()
            .filter(|(_, e)| e.live && e.known_meta().is_some_and(|m| m.owner_pid == pid))
            .collect();
        if by_pid.len() == 1
            && let Some(found) = by_pid.pop()
        {
            return Ok(found.clone());
        }
        if by_pid.len() > 1 {
            return Err(ambiguous(arg, &by_pid, opts));
        }
    }
    let Some(needle) = crate::tui::session::id::normalize(arg) else {
        anyhow::bail!("no session {arg}: not a session id, a live session's pid, or part of an id");
    };
    let matches: Vec<&(usize, SessionEntry)> = all
        .iter()
        .filter(|(_, e)| {
            let id = e.id.encode();
            id.starts_with(&needle) || id.ends_with(&needle)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!("no session {arg} (`heph session list --all` lists them)"),
        _ => Err(ambiguous(arg, &matches, opts)),
    }
}

fn ambiguous(arg: &str, candidates: &[&(usize, SessionEntry)], opts: ViewOpts) -> anyhow::Error {
    let mut msg = format!("{arg} matches {} sessions:", candidates.len());
    for (_, e) in candidates {
        let state = derive_state(e, opts.now_ms, opts.stall);
        let command = e
            .known_meta()
            .map_or_else(|| "-".to_owned(), |m| command_line(&m.argv, usize::MAX));
        msg.push_str(&format!("\n  {}  {:<8} {command}", e.id, state.label()));
    }
    msg.push_str("\nGive more of the id (or its last characters).");
    anyhow::anyhow!(msg)
}

/// Refuse a session whose directory format this heph does not read (A6).
pub(crate) fn require_known(
    entry: &SessionEntry,
) -> anyhow::Result<&crate::tui::session::SessionMeta> {
    match &entry.meta {
        EntryMeta::Known(m) => Ok(m),
        EntryMeta::TooNew { found, supported } => anyhow::bail!(
            "session {} was recorded in session format {found}; this heph reads format {supported} \
             and older — use a newer heph to view it",
            entry.id
        ),
    }
}

async fn inflight(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    arg: &str,
    json: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (i, entry) = resolve(sources, arg, opts).await?;
    require_known(&entry)?;
    let source = sources
        .get(i)
        .context("the session's source went missing")?;
    let report = source.inflight(entry.id, INFLIGHT_TIMEOUT).await?;
    if json {
        #[derive(Serialize)]
        struct InflightJson<'a> {
            id: String,
            report: &'a str,
        }
        serde_json::to_writer_pretty(
            &mut *out,
            &InflightJson {
                id: entry.id.encode(),
                report: &report,
            },
        )
        .context("writing the report")?;
        writeln!(out).context("writing the report")?;
    } else {
        write!(out, "{}", escape_text(&report)).context("writing the report")?;
        if !report.ends_with('\n') {
            writeln!(out).context("writing the report")?;
        }
    }
    Ok(())
}

/// `argv` as one line: the program by its file name, the arguments as given,
/// escaped, cut to `max` characters.
pub(crate) fn command_line(argv: &[String], max: usize) -> String {
    let mut parts = argv.iter();
    let mut line = parts
        .next()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map_or_else(|| p.clone(), |f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    for p in parts {
        line.push(' ');
        line.push_str(p);
    }
    truncate(&escape(&line), max)
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The longest SGR parameter list (the bytes between `ESC[` and `m`) kept as
/// colour. Real ones are far shorter: bold, italic, underline, a truecolour
/// foreground and background and an underline colour come to under 60 bytes.
/// A longer list is escaped like a malformed sequence, and the scan for the
/// `m` stops one byte past this, so a megabyte of parameters costs a bounded
/// look-ahead and is never kept.
const MAX_SGR_PARAMS: usize = 128;

/// The bidirectional controls: the embedding, override and isolate controls
/// (`U+202A`–`U+202E`, `U+2066`–`U+2069`) and the implicit marks (`U+200E`
/// LRM, `U+200F` RLM, `U+061C` ALM). They are not control characters to
/// `char::is_control` (they are format characters), but they reorder how the
/// rest of a line is displayed, so recorded text could make a viewer show
/// something it does not contain. Escaped like a control character.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}'
    )
}

/// One line of recorded text, made safe for a terminal: SGR sequences
/// (`ESC[…m`) are dropped, every other control character — newlines included —
/// and every bidi control is escaped (`\u{1b}`, `\n`, `\u{202e}`). `attach`
/// keeps SGR: see [`escape_replayed`].
pub(crate) fn escape(s: &str) -> Cow<'_, str> {
    escape_with(s, false, false)
}

/// [`escape`] for multi-line text: newlines and tabs are kept.
pub(crate) fn escape_text(s: &str) -> Cow<'_, str> {
    escape_with(s, true, false)
}

/// [`escape_text`] for what a run printed to its own terminal, replayed by
/// `attach`: a well-formed SGR sequence (colour and attributes, `ESC[` digits
/// `;` `:` then `m`, at most [`MAX_SGR_PARAMS`] parameter bytes) is kept when
/// `color` (the viewer's stream takes colour) and dropped otherwise.
/// Everything else is escaped as [`escape_text`] does: cursor moves, clears,
/// OSC titles, any other CSI (an intermediate byte, a private marker), C1
/// controls, bidi controls, and a truncated, malformed or oversized sequence.
pub(crate) fn escape_replayed(s: &str, color: bool) -> Cow<'_, str> {
    escape_with(s, true, color)
}

fn escape_with(s: &str, keep_lines: bool, keep_sgr: bool) -> Cow<'_, str> {
    let kept = |c: char| keep_lines && (c == '\n' || c == '\t');
    let escaped = |c: char| (c.is_control() || is_bidi_control(c)) && !kept(c);
    if !s.chars().any(escaped) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            // An SGR sequence: `ESC [` parameters `m`. Anything else after
            // `ESC [` is escaped like any control character. A scan never
            // reads past `MAX_SGR_PARAMS + 1` parameter bytes, and the bytes
            // it read are not control characters, so no later scan starts
            // inside them: linear in the input.
            let params: String = chars
                .clone()
                .skip(1)
                .take_while(|c| c.is_ascii_digit() || *c == ';' || *c == ':')
                .take(MAX_SGR_PARAMS + 1)
                .collect();
            let mut probe = chars.clone().skip(1 + params.len());
            if params.len() <= MAX_SGR_PARAMS && probe.next() == Some('m') {
                for _ in 0..params.len() + 2 {
                    chars.next();
                }
                if keep_sgr {
                    out.push_str("\u{1b}[");
                    out.push_str(&params);
                    out.push('m');
                }
                continue;
            }
        }
        if escaped(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `vX.Y.Z` at the start of a heph version, `None` for a dev build or anything
/// else that does not start with three numbers.
fn release_triple(v: &str) -> Option<(u64, u64, u64)> {
    if v == hcore::version::DEV_VERSION {
        return None;
    }
    let v = v.strip_prefix('v').unwrap_or(v);
    let end = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let mut nums = v.get(..end)?.split('.');
    let triple = (
        nums.next()?.parse().ok()?,
        nums.next()?.parse().ok()?,
        nums.next()?.parse().ok()?,
    );
    nums.next().is_none().then_some(triple)
}

/// Whether `recorded` is a newer heph than `viewer` (A6). A dev build on either
/// side, or a version that does not parse, never warns.
pub(crate) fn recorded_by_newer(recorded: &str, viewer: &str) -> bool {
    match (release_triple(recorded), release_triple(viewer)) {
        (Some(r), Some(v)) => r > v,
        _ => false,
    }
}

/// A duration between two unix-ms instants, for a person.
pub(crate) fn ago(now_ms: u64, then_ms: u64) -> String {
    hcore::units::human_duration(Duration::from_millis(now_ms.saturating_sub(then_ms)))
}
