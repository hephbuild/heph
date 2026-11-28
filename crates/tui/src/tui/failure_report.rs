//! One failure, drawn from its [`FailureRecord`]: the box every text
//! renderer prints (the report after the TUI, the line backend's live block,
//! the CI end-of-run box), and the facts `--json` adds from the same record
//! (error-reporting D5). Written once here, so no two renderers can say a
//! failure differently.
//!
//! ```text
//! ✗ //p:t · exit status 1
//!   unknown option `dep` for driver exec
//!     ╭─[p/BUILD:4:5]
//!   4 │     dep = [],
//!     │     ─┬─
//!     │      ╰── did you mean `deps`?
//!     ╰────
//!   valid options: deps, tools
//!   reached by: //a:root → //p:t
//!      ╭─[log · 12 lines]
//!      │ … 10 earlier lines
//!   11 │ compiling
//!   12 │ error: boom
//!      ╰────
//!   log: heph log //p:t
//! ```
//!
//! Pure but for two reads, both bounded and both of files the record names:
//! the source file a location points into (P9: the excerpt is re-framed
//! from the file as it is now, never from the producer's layout), and the
//! failure slot's log, through [`hcore::failure_log::open_tail`], which says
//! whether the log is still this failure's.
//!
//! Every text heph wrote (the message, a cause, a hint, a location) goes
//! through [`message`]: heph's home redacted, workspace paths relative,
//! controls escaped. A log line keeps its paths (D2) and goes through
//! [`escape_log_tail`]; heph's home is still redacted from it (I9).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crossterm::style::Stylize;
use hcore::diagnostic::{Hint, HintKind, SNIPPET_LINE_CAP, SNIPPET_LINES_CAP, SourceLoc, Stage};
use hcore::events::{FailedIn, FailureRecord, LogRef};
use hcore::failure_log::{self, TAIL_LINES_CAP, TAIL_MAX_BYTES, TailRead};
use serde::Serialize;
use unicode_width::UnicodeWidthChar;

use crate::tui::failure_rows::{
    TailRows, left_behind_rows_in, log_line_count, log_readable, reached_by_rows, shell_quote,
    tail_rows, took_down_row,
};
use crate::tui::text::{
    ELLIPSIS, Redact, WorkspaceRel, clean_inline, cut_right, escape_control, escape_log_tail,
    expand_tabs, message, sgr_len, width,
};

/// How many failures the report draws as full boxes; each further one gets
/// one line (D7b, U7: confirmed by the user, 2026-11-27).
pub const BOX_CAP: usize = 50;

/// How many culprits the totals line names before `+N more` (D6.1c).
pub const TOTALS_NAMED: usize = 5;

/// The largest source file an excerpt is read from. A hand-written source
/// file is far smaller; past this, the location is named without an excerpt.
const SOURCE_READ_CAP: u64 = 4 * 1024 * 1024;

/// Which screen a box is drawn for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RenderMode {
    /// As the failure happens or at the end of a run without the TUI: the
    /// line backend's live block and the CI box. Paths into heph's home
    /// (the slot's log, its `out/`) are named, for a CI log to point at.
    #[default]
    Line,
    /// The report after the TUI is torn down: no path into heph's home
    /// (I9); the log and the kept outputs are named by the command that
    /// reads them.
    Report,
}

/// What a box is drawn with, beyond its record. Build it before the render:
/// [`WorkspaceRel::workspace`] and [`Redact::heph_home`] read the filesystem.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    /// The workspace, which heph's text names paths relative to.
    pub rel: WorkspaceRel,
    /// heph's home, which no text shows.
    pub redact: Redact,
    /// Where a location's file is read from to frame its excerpt; `None`
    /// names the location without one.
    pub source_root: Option<PathBuf>,
    /// The columns rows wrap at, read when the box is drawn (the report
    /// wraps at the width the terminal has after teardown); `None` never
    /// wraps (a CI log).
    pub width: Option<usize>,
    /// Paint the box, and keep the log's own SGR colours.
    pub color: bool,
    /// The most log lines a box shows, within [`TAIL_LINES_CAP`].
    pub log_lines: usize,
    pub mode: RenderMode,
}

impl Default for RenderCtx {
    fn default() -> Self {
        Self {
            rel: WorkspaceRel::default(),
            redact: Redact::default(),
            source_root: None,
            width: None,
            color: false,
            log_lines: TAIL_LINES_CAP,
            mode: RenderMode::default(),
        }
    }
}

/// The facts `run --json` (contract 2) adds to a failure's entry, from the
/// same record the boxes are drawn from (D5, PR 4). Every field is written,
/// `location` as `null` when there is none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contract2Fields {
    pub stage: Stage,
    pub location: Option<SourceLoc>,
    pub hints: Vec<Hint>,
    /// A cap cut something from the record.
    pub truncated: bool,
    /// The failure slot was filled again by a newer failed run since this
    /// failure: its log is no longer this one's.
    pub log_replaced: bool,
    /// The run's output reached a terminal while it ran.
    pub streamed: bool,
}

/// `rec`'s [`Contract2Fields`]. Reads the slot's manifest, as the box does,
/// to tell whether its log was replaced.
pub fn json_facts(rec: &FailureRecord) -> Contract2Fields {
    let d = &rec.diagnostic;
    Contract2Fields {
        stage: d.stage,
        location: d.location.clone(),
        hints: d.hints.clone(),
        truncated: d.truncated,
        log_replaced: rec.log.as_ref().is_some_and(|log| {
            log.memory_tail.is_none()
                && matches!(failure_log::open_tail(log, 0, 0), TailRead::Replaced)
        }),
        streamed: rec.streamed,
    }
}

/// What a box names a failure by: its target, or the file that did not
/// evaluate.
fn culprit_name<'a>(rec: &'a FailureRecord, ctx: &RenderCtx) -> Cow<'a, str> {
    match (&rec.addr, &rec.file) {
        (Some(addr), _) => clean_inline(addr),
        (None, Some(file)) => message(file, &ctx.redact, &ctx.rel),
        (None, None) => clean_inline(&rec.entry),
    }
}

/// The address `heph log` reads the failure's log by.
fn log_addr(rec: &FailureRecord) -> &str {
    rec.addr.as_deref().unwrap_or(&rec.entry)
}

/// How the run ended, for the header: its timeout, its exit, or a
/// `--frozen` difference. `None` when the message says what went wrong.
fn status(rec: &FailureRecord) -> Option<String> {
    if let Some(end) = rec.process_end() {
        return Some(match clean_inline(&end) {
            Cow::Borrowed(_) => end,
            Cow::Owned(cleaned) => cleaned,
        });
    }
    rec.frozen_diff
        .as_ref()
        .map(|_| "generated output differs from tree".to_owned())
}

/// The message is said by the header's [`status`], so a renderer leaves it
/// out: compact replaced it with how the process ended
/// ([`FailureRecord::message_is_status`]), or it is a `--frozen` check's,
/// whose diff the box draws. A driver's own diagnostic beside an exit is
/// not, and is shown.
fn message_in_header(rec: &FailureRecord) -> bool {
    match rec.process_end() {
        Some(_) => rec.message_is_status(),
        None => rec.frozen_diff.is_some(),
    }
}

/// A hint as a row: a near-miss name as the question it answers, any other
/// kind as its own sentence.
fn hint_text(h: &Hint, ctx: &RenderCtx) -> String {
    let text = message(&h.text, &ctx.redact, &ctx.rel);
    match h.kind {
        HintKind::DidYouMean => format!("did you mean `{text}`?"),
        _ => text.into_owned(),
    }
}

/// One failure's box, ending in a newline. See the module docs for the
/// layout:
/// 1. the header, `✗ //t · exit status 1` (or `✗ d/BUILD`);
/// 2. the message, wrapped, and an aggregate's causes; left out when the
///    header's status says it (a process's exit or timeout with no
///    diagnostic of its driver's, a diff);
/// 3. the location's excerpt (`╭─[p/BUILD:4:5]`) with a marker under the
///    span, the near-miss hint at its end; `at p/BUILD:4` when the file
///    cannot frame it;
/// 4. the other hints, then a `--frozen` diff;
/// 5. `reached by:`;
/// 6. the log, as [`tail_rows`] decides, or why it is not this failure's;
/// 7. what the run left behind.
pub fn render_box(rec: &FailureRecord, ctx: &RenderCtx) -> String {
    let mut b = BoxWriter::new(ctx);
    let d = &rec.diagnostic;
    let status = status(rec);
    b.header(&culprit_name(rec, ctx), status.as_deref());

    if !message_in_header(rec) {
        let msg = message(&d.message, &ctx.redact, &ctx.rel);
        for line in msg.lines() {
            b.row(line);
        }
    }
    for cause in &rec.causes {
        b.row(&format!("- {}", message(cause, &ctx.redact, &ctx.rel)));
    }
    if rec.more_causes > 0 {
        b.row(&format!("+{} more", rec.more_causes));
    }
    if rec.timed_out.as_ref().is_some_and(|t| t.abandoned) {
        b.row(hcore::failure::ABANDONED);
    }

    let mut attached = None;
    if let Some(loc) = &d.location {
        let near_miss = d.hints.iter().position(|h| h.kind == HintKind::DidYouMean);
        let label = near_miss
            .and_then(|i| d.hints.get(i))
            .map(|h| hint_text(h, ctx));
        let source = ctx.source_root.as_deref().and_then(|r| read_source(r, loc));
        match reframe(
            source.as_deref(),
            d.snippet.as_deref(),
            loc,
            label.as_deref(),
        ) {
            Some(rows) => {
                b.excerpt(&message(&loc.to_string(), &ctx.redact, &ctx.rel), &rows);
                attached = near_miss;
            }
            None => b.row(&format!(
                "at {}:{}",
                message(&loc.file, &ctx.redact, &ctx.rel),
                loc.line
            )),
        }
    }
    for (i, h) in d.hints.iter().enumerate() {
        if Some(i) != attached {
            b.row(&hint_text(h, ctx));
        }
    }
    if let Some(diff) = &rec.frozen_diff {
        b.diff(diff);
    }
    for row in reached_by_rows(&rec.path, rec.file.is_some()) {
        b.row(&row);
    }
    if let Some(row) = rec.took_down.as_ref().and_then(took_down_row) {
        b.row(&row);
    }
    if let Some(log) = &rec.log {
        b.log(rec, log);
    }
    if let Some(left) = &rec.left_behind {
        // A failure before the run never had a log: no `log:` row, not even
        // `none kept`.
        let log_row = rec.log.is_some() || rec.failed_in != FailedIn::BeforeRun;
        b.out.push_str(&left_behind_rows_in(
            log_addr(rec),
            left,
            ctx.mode == RenderMode::Line,
            log_row,
        ));
    }
    b.out
}

/// The report's boxes: the first [`BOX_CAP`] failures in full, a blank line
/// between two, then one line for each further failure and
/// `N more: heph inspect failure --json`. Failures only: a stop is not one,
/// and the caller leaves it out.
pub fn render_report<'a>(
    recs: impl IntoIterator<Item = &'a FailureRecord>,
    ctx: &RenderCtx,
) -> String {
    let mut out = String::new();
    let mut more = 0usize;
    for (i, rec) in recs.into_iter().enumerate() {
        if i < BOX_CAP {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&render_box(rec, ctx));
            continue;
        }
        if i == BOX_CAP {
            out.push('\n');
        }
        out.push_str(&one_line(rec, ctx));
        out.push('\n');
        more += 1;
    }
    if more > 0 {
        // Writing to a `String` cannot fail.
        writeln!(out, "{more} more: heph inspect failure --json").unwrap_or_default();
    }
    out
}

/// A failure past the report's cap: its header, and its message's first
/// line unless the header's status says it, cut to the width.
pub fn one_line(rec: &FailureRecord, ctx: &RenderCtx) -> String {
    let mut line = format!("✗ {}", culprit_name(rec, ctx));
    if let Some(status) = status(rec) {
        line.push_str(" · ");
        line.push_str(&status);
    }
    if !message_in_header(rec) {
        let msg = message(&rec.diagnostic.message, &ctx.redact, &ctx.rel);
        if let Some(first) = msg.lines().next().filter(|l| !l.is_empty()) {
            line.push_str(" · ");
            line.push_str(first);
        }
    }
    let line = match ctx.width {
        Some(w) => cut_right(&line, w).into_owned(),
        None => line,
    };
    paint(&line, Tone::RedBold, ctx.color)
}

/// The one line a target whose output streamed to the terminal gets when
/// its process fails (D6.1c, U6): `✗ //t · exit status 1 · heph log //t`,
/// right after its output; the header's red bold under `color`. A driver's
/// own diagnostic is never hidden by it: its message's lines, then its
/// hints, each follow on a line of its own, indented, as the box would show
/// them (CQ-1). No newline after the last.
pub fn streamed_exec_line(rec: &FailureRecord, color: bool) -> String {
    let addr = log_addr(rec);
    let mut line = format!("✗ {}", clean_inline(addr));
    if let Some(status) = status(rec) {
        line.push_str(" · ");
        line.push_str(&status);
    }
    let mut line = paint(&line, Tone::RedBold, color);
    let pointer = format!(" · heph log {}", shell_quote(addr));
    line.push_str(&paint(&pointer, Tone::Dim, color));
    // As the CI box words it: heph's home and the workspace as it does.
    let ctx = RenderCtx::default();
    let d = &rec.diagnostic;
    if !message_in_header(rec) {
        let msg = message(&d.message, &ctx.redact, &ctx.rel);
        for l in msg.lines().filter(|l| !l.trim().is_empty()) {
            line.push_str("\n  ");
            line.push_str(l);
        }
    }
    for h in &d.hints {
        line.push_str("\n  ");
        line.push_str(&hint_text(h, &ctx));
    }
    line
}

/// The non-TUI run's last line (D6.1c): `2 failed: //c:y, d/BUILD ·
/// 3 skipped`, naming at most [`TOTALS_NAMED`] culprits, then `+N more`.
/// Empty when nothing failed or was skipped. No newline.
pub fn totals_line<S: AsRef<str>>(failed: &[S], skipped: usize) -> String {
    let mut parts = Vec::new();
    if !failed.is_empty() {
        let named: Vec<Cow<'_, str>> = failed
            .iter()
            .take(TOTALS_NAMED)
            .map(|f| clean_inline(f.as_ref()))
            .collect();
        let more = failed.len().saturating_sub(TOTALS_NAMED);
        let mut names = named.join(", ");
        if more > 0 {
            names.push_str(&format!(" +{more} more"));
        }
        parts.push(format!(
            "{} failed: {names}",
            hcore::units::thousands(failed.len())
        ));
    }
    if skipped > 0 {
        parts.push(format!("{} skipped", hcore::units::thousands(skipped)));
    }
    parts.join(" · ")
}

/// The line a requested target that did not run gets as it ends (D6.1c):
/// `– //a:root skipped: //c:y failed`. No newline.
pub fn skipped_line(addr: &str, root: &str) -> String {
    format!(
        "– {} skipped: {} failed",
        clean_inline(addr),
        clean_inline(root)
    )
}

/// What `--fail-fast` stopped (D5, RUN-24): a line for each target that was
/// running, `stopped by --fail-fast: //p:slow (was running)`, then how many
/// queued targets never started. Each line ends in a newline.
pub fn fail_fast_lines<S: AsRef<str>>(running: &[S], queued: usize) -> String {
    let mut out = String::new();
    for addr in running {
        out.push_str(&format!(
            "stopped by --fail-fast: {} (was running)\n",
            clean_inline(addr.as_ref())
        ));
    }
    if queued > 0 {
        out.push_str(&format!(
            "stopped by --fail-fast: {} queued {} never started\n",
            hcore::units::thousands(queued),
            if queued == 1 { "target" } else { "targets" }
        ));
    }
    out
}

/// A line of a framed excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameRow {
    /// A source line, numbered as in its file.
    Source { line: usize, text: String },
    /// A marker under the line above, or the label at the marker's end: no
    /// number.
    Marker(String),
}

/// The source file `loc` names, read from under `root`; `None` when there
/// is none there (a provider's builtin source, with no file on disk) or it
/// is over [`SOURCE_READ_CAP`].
/// Invalid UTF-8 reads as U+FFFD.
fn read_source(root: &Path, loc: &SourceLoc) -> Option<String> {
    let path = root.join(&loc.file);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > SOURCE_READ_CAP {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The excerpt of `source` (the located file as it is now) at `loc`, framed
/// as a log is (P9, U4): the span's lines numbered, each marked under the
/// span, `label` (the near-miss hint) at the end of the last marker.
///
/// - `snippet` (the producer's lines, as it read them at eval) only says how
///   many lines the span covers: as many as it has, when they are still the
///   file's lines there; one otherwise. Its layout is never reused.
/// - Columns count chars, not bytes. A tab is laid out as spaces and the
///   marker follows it. A leading BOM and each line's CR are dropped.
/// - On one line the span is the token at `loc`: an identifier, a quoted
///   string, else one char. Over several lines it runs from `loc` to the
///   end of its last line.
/// - A line over [`SNIPPET_LINE_CAP`] chars is cut around the span, `…`
///   marking each cut, the whole within the cap.
/// - Control characters are shown escaped.
///
/// `None` (the caller names `file:line` with no excerpt) when there is no
/// source (a provider's builtin source, with no file on disk) or the span is not in it
/// (the file was shortened since eval).
pub fn reframe(
    source: Option<&str>,
    snippet: Option<&str>,
    loc: &SourceLoc,
    label: Option<&str>,
) -> Option<Vec<FrameRow>> {
    let source = source?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let first = usize::try_from(loc.line).ok()?.checked_sub(1)?;
    let col = usize::try_from(loc.col).ok()?.saturating_sub(1);
    let lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .skip(first)
        .take(SNIPPET_LINES_CAP)
        .collect();
    let at = lines.first()?;
    if col > at.chars().count() {
        return None;
    }
    let span_lines = match snippet {
        Some(s) => {
            let given: Vec<&str> = s
                .lines()
                .map(|l| l.strip_suffix('\r').unwrap_or(l))
                .collect();
            let matches = given.len() > 1
                && given.len() <= lines.len()
                && given.iter().zip(&lines).all(|(a, b)| a == b);
            if matches { given.len() } else { 1 }
        }
        None => 1,
    };

    let mut rows = Vec::new();
    for (i, line) in lines.iter().take(span_lines).enumerate() {
        let trimmed = line.trim_end().chars().count();
        let (start, end) = if span_lines == 1 {
            (col, token_end(line, col))
        } else if i == 0 {
            (col, trimmed.max(col + 1))
        } else {
            let indent = line.chars().take_while(|c| c.is_whitespace()).count();
            (indent, trimmed)
        };
        let last = i + 1 == span_lines;
        let Shown {
            text,
            offset,
            width: span_w,
        } = show_line(line, start, end);
        rows.push(FrameRow::Source {
            line: first + i + 1,
            text,
        });
        if end <= start && span_lines > 1 {
            // A blank line inside the span: nothing to mark.
            continue;
        }
        let pad = " ".repeat(offset);
        match label.filter(|_| last) {
            Some(label) => {
                let tee = span_w / 2;
                rows.push(FrameRow::Marker(format!(
                    "{pad}{}┬{}",
                    "─".repeat(tee),
                    "─".repeat(span_w - tee - 1)
                )));
                rows.push(FrameRow::Marker(format!(
                    "{pad}{}╰── {label}",
                    " ".repeat(tee)
                )));
            }
            None => rows.push(FrameRow::Marker(format!("{pad}{}", "─".repeat(span_w)))),
        }
    }
    Some(rows)
}

/// The end (exclusive, in chars) of the token at char `start` of `line`: an
/// identifier, a quoted string to its closing quote, else one char.
fn token_end(line: &str, start: usize) -> usize {
    let mut chars = line.chars().skip(start).peekable();
    let Some(&c0) = chars.peek() else {
        return start + 1;
    };
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    if ident(c0) {
        return start + chars.take_while(|&c| ident(c)).count();
    }
    if c0 == '"' || c0 == '\'' {
        let mut escaped = false;
        for (n, c) in chars.enumerate().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                c if c == c0 => return start + n + 1,
                _ => {}
            }
        }
        return line.chars().count();
    }
    start + 1
}

/// A source line as an excerpt shows it, and where its span is.
struct Shown {
    text: String,
    /// The span's first column.
    offset: usize,
    /// The span's width in columns, at least 1.
    width: usize,
}

/// `line` with chars `start..end` the span: cut around it to
/// [`SNIPPET_LINE_CAP`] chars when longer, controls escaped, tabs laid out.
fn show_line(line: &str, start: usize, end: usize) -> Shown {
    let len = line.chars().count();
    let (from, to) = if len <= SNIPPET_LINE_CAP {
        (0, len)
    } else {
        // Two chars of the cap go to the ellipses.
        let budget = SNIPPET_LINE_CAP - 2;
        let from = start.saturating_sub(budget / 4).min(len - budget);
        (from, from + budget)
    };
    let start = start.clamp(from, to);
    let end = end.clamp(start, to);
    let range = |a: usize, b: usize| -> &str {
        let byte = |n: usize| line.char_indices().nth(n).map_or(line.len(), |(i, _)| i);
        line.get(byte(a)..byte(b)).unwrap_or("")
    };
    let lead = if from > 0 { ELLIPSIS } else { "" };
    let tail = if to < len { ELLIPSIS } else { "" };
    let before = format!("{lead}{}", range(from, start));
    let through = format!("{before}{}", range(start, end));
    let lay = |s: &str| expand_tabs(&escape_control(s)).into_owned();
    let offset = width(&lay(&before));
    let span = width(&lay(&through)).saturating_sub(offset).max(1);
    Shown {
        text: lay(&format!("{through}{}{tail}", range(end, to))),
        offset,
        width: span,
    }
}

/// `line` broken into rows at most `width` columns wide, by display width
/// (a CJK char takes two columns, a combining mark none).
///
/// - Breaks at a space, which the break consumes; a word wider than the
///   width breaks at a char boundary, never between a char and its
///   combining marks.
/// - A width of 0 is read as 1. A char wider than the width gets a row of
///   its own, so every char is shown and the wrap always ends.
/// - An SGR sequence (`ESC [ … m`, which only a colour-keeping log line
///   holds) takes no column and is never split.
/// - Leading spaces are kept. At least one row, empty for an empty line.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    wrap_with(line, width, width)
}

/// [`wrap`], the first row `first` columns wide and the rest `rest`.
fn wrap_with(line: &str, first: usize, rest: usize) -> Vec<String> {
    let limit = |rows: &Vec<String>| if rows.is_empty() { first } else { rest }.max(1);
    let body = line.trim_start_matches(' ');
    let indent = line.len() - body.len();
    let mut rows = Vec::new();
    let mut cur = " ".repeat(indent);
    let mut used = indent;
    // Whether the row holds a word yet, and the spaces before the next one:
    // the spaces at a break start no row.
    let mut has_word = false;
    let mut spaces = 0usize;
    for word in body.split(' ') {
        if word.is_empty() {
            spaces += 1;
            continue;
        }
        let w = styled_width(word);
        let gap = if has_word { spaces + 1 } else { 0 };
        spaces = 0;
        if has_word {
            if used + gap + w <= limit(&rows) {
                cur.extend(std::iter::repeat_n(' ', gap));
                cur.push_str(word);
                used += gap + w;
                continue;
            }
            rows.push(std::mem::take(&mut cur));
            used = 0;
        }
        has_word = true;
        if used + w <= limit(&rows) {
            cur.push_str(word);
            used += w;
            continue;
        }
        for (unit, uw) in units(word) {
            if uw > 0 && used > 0 && used + uw > limit(&rows) {
                rows.push(std::mem::take(&mut cur));
                used = 0;
            }
            cur.push_str(unit);
            used += uw;
        }
    }
    rows.push(cur);
    rows
}

/// `s`'s display units: each char with its width, an SGR sequence whole
/// with none.
fn units(s: &str) -> impl Iterator<Item = (&str, usize)> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        let n = if c == '\x1b' {
            sgr_len(rest).unwrap_or(c.len_utf8())
        } else {
            c.len_utf8()
        };
        let (unit, after) = rest.split_at(n);
        rest = after;
        let w = if n > c.len_utf8() {
            0
        } else {
            c.width().unwrap_or(0)
        };
        Some((unit, w))
    })
}

/// `s`'s width in columns, an SGR sequence taking none.
fn styled_width(s: &str) -> usize {
    units(s).map(|(_, w)| w).sum()
}

#[derive(Debug, Clone, Copy)]
enum Tone {
    Plain,
    Dim,
    Red,
    RedBold,
    White,
}

fn paint(s: &str, tone: Tone, color: bool) -> String {
    if !color || s.is_empty() {
        return s.to_owned();
    }
    match tone {
        Tone::Plain => s.to_owned(),
        Tone::Dim => s.dim().to_string(),
        Tone::Red => s.red().to_string(),
        Tone::RedBold => s.red().bold().to_string(),
        Tone::White => s.white().to_string(),
    }
}

/// The indent of a box's rows, and of a wrapped row's continuation.
const ROW_INDENT: &str = "  ";
const CONT_INDENT: &str = "    ";

/// A box being written.
struct BoxWriter<'a> {
    out: String,
    ctx: &'a RenderCtx,
}

impl<'a> BoxWriter<'a> {
    fn new(ctx: &'a RenderCtx) -> Self {
        Self {
            out: String::new(),
            ctx,
        }
    }

    /// `✗ name · status`, wrapped like a row.
    fn header(&mut self, name: &str, status: Option<&str>) {
        let color = self.ctx.color;
        let plain = match status {
            Some(s) => format!("✗ {name} · {s}"),
            None => format!("✗ {name}"),
        };
        let rows = match self.ctx.width {
            Some(w) => wrap_with(&plain, w, w.saturating_sub(ROW_INDENT.len())),
            None => vec![plain],
        };
        if let ([_], Some(s)) = (rows.as_slice(), status) {
            // One row: the mark and name red, the status plain.
            self.out
                .push_str(&paint(&format!("✗ {name}"), Tone::RedBold, color));
            self.out.push_str(&paint(" · ", Tone::Dim, color));
            self.out.push_str(s);
            self.out.push('\n');
            return;
        }
        for (i, row) in rows.iter().enumerate() {
            let indent = if i == 0 { "" } else { ROW_INDENT };
            self.out.push_str(indent);
            self.out.push_str(&paint(row, Tone::RedBold, color));
            self.out.push('\n');
        }
    }

    /// One row of heph's text, wrapped, continuations indented further.
    fn row(&mut self, text: &str) {
        let text = expand_tabs(text);
        let rows = match self.ctx.width {
            Some(w) => wrap_with(
                &text,
                w.saturating_sub(ROW_INDENT.len()),
                w.saturating_sub(CONT_INDENT.len()),
            ),
            None => vec![text.into_owned()],
        };
        for (i, row) in rows.iter().enumerate() {
            self.out
                .push_str(if i == 0 { ROW_INDENT } else { CONT_INDENT });
            self.out.push_str(row);
            self.out.push('\n');
        }
    }

    /// A located excerpt: numbered source lines and their markers.
    fn excerpt(&mut self, label: &str, rows: &[FrameRow]) {
        let last = rows
            .iter()
            .filter_map(|r| match r {
                FrameRow::Source { line, .. } => Some(*line),
                FrameRow::Marker(_) => None,
            })
            .max()
            .unwrap_or(1);
        let frame = Frame::numbered(last, self.ctx.color);
        frame.open(&mut self.out, label);
        for row in rows {
            match row {
                FrameRow::Source { line, text } => {
                    frame.line(&mut self.out, Some(*line), text, Tone::Plain, None);
                }
                FrameRow::Marker(text) => {
                    frame.line(&mut self.out, None, text, Tone::Red, None);
                }
            }
        }
        frame.close(&mut self.out);
    }

    /// A `--frozen` check's diff, additions green and deletions red.
    fn diff(&mut self, diff: &str) {
        let color = self.ctx.color;
        let frame = Frame {
            numbers: None,
            color,
        };
        frame.open(&mut self.out, "diff");
        for line in diff.lines() {
            let line = escape_control(line);
            let header = line.starts_with("+++") || line.starts_with("---");
            let styled = if !color || header {
                line.into_owned()
            } else if line.starts_with('+') {
                line.green().to_string()
            } else if line.starts_with('-') {
                line.red().to_string()
            } else {
                line.into_owned()
            };
            frame.line(&mut self.out, None, &styled, Tone::Plain, None);
        }
        frame.close(&mut self.out);
    }

    /// The log `log` names, or why it is not shown: replaced by a newer
    /// failed run, cleared, or unreadable. A streamed log is the pointer, when
    /// `heph log` can open it.
    fn log(&mut self, rec: &FailureRecord, log: &LogRef) {
        let addr = log_addr(rec);
        let readable = rec
            .left_behind
            .as_ref()
            .map_or(log.path.is_some(), log_readable);
        let counted = usize::try_from(log.lines).unwrap_or(usize::MAX);
        if let Some(tail) = &log.memory_tail {
            let start = counted.saturating_sub(tail.lines().count()) + 1;
            self.tail(addr, tail, start, rec.streamed, readable);
            return;
        }
        let open = match failure_log::open_tail(log, self.ctx.log_lines, TAIL_MAX_BYTES) {
            TailRead::Stream(open) | TailRead::CannotTell(open) => open,
            TailRead::Replaced => {
                self.row(&format!(
                    "log replaced by a newer failed run: heph log {}",
                    shell_quote(addr)
                ));
                return;
            }
            TailRead::Cleared => {
                self.row("log cleared");
                return;
            }
            TailRead::Unreadable(e) => {
                self.unreadable(&e);
                return;
            }
        };
        if rec.streamed && readable {
            // The user saw it: no line of it, only how long it is. A log
            // `heph log` cannot open gets its tail instead (U26).
            let total = match counted {
                0 => failure_log::count_lines(&open.file)
                    .ok()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0),
                n => n,
            };
            self.tail(addr, "", total.saturating_add(1), true, readable);
            return;
        }
        let mut bytes = Vec::new();
        match open.stream(&mut bytes) {
            Ok(stats) => {
                let start = usize::try_from(stats.start_line).unwrap_or(usize::MAX);
                self.tail(
                    addr,
                    &String::from_utf8_lossy(&bytes),
                    start,
                    false,
                    readable,
                );
            }
            Err(e) => self.unreadable(&e),
        }
    }

    fn unreadable(&mut self, e: &std::io::Error) {
        let e = e.to_string();
        self.row(&format!(
            "log could not be read: {}",
            message(&e, &self.ctx.redact, &self.ctx.rel)
        ));
    }

    /// The tail `text`, whose first line is line `start` of the log, as
    /// [`tail_rows`] decides, in a `log · N lines` frame.
    fn tail(&mut self, addr: &str, text: &str, start: usize, streamed: bool, readable: bool) {
        let text = self.ctx.redact.apply(text);
        let rows = tail_rows(addr, &text, start, streamed, readable, self.ctx.log_lines);
        let total = log_line_count(&text, start);
        let label = format!(
            "log · {} {}",
            hcore::units::thousands(total),
            if total == 1 { "line" } else { "lines" }
        );
        render_log_box(&mut self.out, &rows, &label, self.ctx.width, self.ctx.color);
    }
}

/// A frame's gutter: line numbers this wide, or none.
struct Frame {
    numbers: Option<usize>,
    color: bool,
}

impl Frame {
    /// A frame numbering lines up to `last`.
    fn numbered(last: usize, color: bool) -> Self {
        Self {
            numbers: Some(last.max(1).to_string().len()),
            color,
        }
    }

    /// The gutter before the `╭`/`│`/`╰` column: a line number's field and
    /// the space after it, so the corners line up with the bar.
    fn pad(&self) -> String {
        " ".repeat(self.numbers.map_or(0, |w| w + 1))
    }

    fn open(&self, out: &mut String, label: &str) {
        let label = format!("╭─[{}]", clean_inline(label));
        out.push_str(&format!(
            "  {}{}\n",
            self.pad(),
            paint(&label, Tone::White, self.color)
        ));
    }

    fn close(&self, out: &mut String) {
        out.push_str(&format!(
            "  {}{}\n",
            self.pad(),
            paint("╰────", Tone::White, self.color)
        ));
    }

    /// One line, numbered `num` (blank for a marker or a note), wrapped to
    /// `width` columns with blank-numbered continuations.
    fn line(
        &self,
        out: &mut String,
        num: Option<usize>,
        text: &str,
        tone: Tone,
        width: Option<usize>,
    ) {
        let gutter = self.numbers.map_or(0, |w| w + 1);
        let rows = match width {
            Some(w) => wrap(text, w.saturating_sub(2 + gutter + 2)),
            None => vec![text.to_owned()],
        };
        let bar = paint("│", Tone::White, self.color);
        for (i, row) in rows.iter().enumerate() {
            let field = match (self.numbers, num.filter(|_| i == 0)) {
                (Some(w), Some(n)) => {
                    format!("{} ", paint(&format!("{n:>w$}"), Tone::Dim, self.color))
                }
                _ => self.pad(),
            };
            out.push_str(&format!(
                "  {field}{bar} {}\n",
                paint(row, tone, self.color)
            ));
        }
    }
}

/// A log tail in its frame, indented two spaces, the gutter's line numbers
/// those of the whole log (the last 10 lines of a 100-line log read 91–100).
/// What is shown is `rows`' ([`tail_rows`]): a note with no line (a
/// streamed tail's pointer, `(no output)`) is one dim row and no frame; the
/// earlier-lines note opens the frame. The lines go through
/// [`escape_log_tail`] (D6.1b), keeping their SGR colours with `color`, and
/// wrap at `width` when given.
///
/// ```text
///      ╭─[log]
///      │ … 90 earlier lines
///   91 │ line one
///   92 │ line two
///      ╰────
/// ```
pub fn render_log_box(
    out: &mut String,
    rows: &TailRows<'_>,
    label: &str,
    width: Option<usize>,
    color: bool,
) {
    let note = rows.note.as_deref().map(clean_inline);
    if rows.shown.is_empty() {
        if let Some(note) = note {
            out.push_str(&format!("  {}\n", paint(&note, Tone::Dim, color)));
        }
        return;
    }
    let escaped = escape_log_tail(rows.shown, color);
    let lines: Vec<&str> = escaped.lines().collect();
    let frame = Frame::numbered(rows.first_line + lines.len().saturating_sub(1), color);
    frame.open(out, label);
    if let Some(note) = note {
        frame.line(out, None, &note, Tone::Dim, width);
    }
    for (i, line) in lines.iter().enumerate() {
        frame.line(out, Some(rows.first_line + i), line, Tone::Plain, width);
    }
    frame.close(out);
}

#[cfg(test)]
mod tests;
