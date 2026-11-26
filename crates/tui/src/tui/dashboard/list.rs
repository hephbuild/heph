//! The dashboard's list: one full-width table, TARGET / PHASE / TIME /
//! LAST LINE, for the active tab. A frame reads the fold and the last-line
//! cache, and does no IO: the cache is filled off the render task from the
//! rows the last frame showed.
//!
//! The tab counts do not sum to All's, by design. A pending approval request
//! (`✦`) leads All and makes the Approvals tab, while its target also stays
//! in its own state's tab (Pending or Running): it is counted once in All
//! and twice across the other tabs. A stopped target (`○`) is the other way
//! round, in All only.

use std::borrow::Cow;
use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::intern::Id;
use super::model::{Model, Record, RowState, Tab};
use crate::tui::progress::CountScope;
use crate::tui::text::{
    clean_inline, contains_ignore_ascii_case, cut_left, cut_right, expand_tabs, sanitize_log, width,
};

/// What a blocked row's PHASE and LAST LINE say, in the words of
/// `hcore::events::admission_wait_text` for a pool and of the line's
/// spotlight for a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitWords {
    /// `waits cpu 4`, `waits sandbox`, `locked: d1v9q`.
    pub phase: String,
    /// The rest: `(2 free, 41s)`, `held by session d1v9q (pid 4121)`.
    pub detail: String,
    /// `detail` without the waited time, for the list row whose TIME column
    /// already shows it: `(2 free)`, `(3 free, yielding to a cpu 8 run)`.
    pub row_detail: String,
    pub waited_ms: u64,
}

/// Where a blocked row's words come from: the view's wait maps, which hold
/// what each wait event carried.
pub trait Waits {
    fn wait_words(&self, addr: &str, now_ms: u64) -> Option<WaitWords>;
}

/// Reads a target's log for the LAST LINE column. Called off the render
/// task only ([`read_tails`]), never by a frame.
pub trait LogReader: Send + Sync {
    /// The log's current length; `None` when the target has none. A stat.
    fn size(&self, addr: &str) -> Option<u64>;
    /// At most the last `max` bytes of the log.
    fn tail(&self, addr: &str, max: usize) -> Option<Vec<u8>>;
}

/// How much of a log's end is read for its last line.
pub const TAIL_BYTES: usize = 4096;

/// How wide a kept last line may be, in columns: wider than any column.
const LINE_KEEP_COLS: usize = 512;

/// A row whose last line the background reader should (re)read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailRequest {
    pub id: Id,
    pub addr: String,
    /// The target has ended: once read, its line is final.
    pub finished: bool,
    /// The log's size when last read.
    pub known: Option<u64>,
}

/// What the reader found for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailUpdate {
    pub id: Id,
    pub finished: bool,
    pub size: Option<u64>,
    /// `None`: not read, because the log did not grow.
    pub line: Option<Box<str>>,
}

/// The background half: read what `requests` ask for. A running row's log
/// is read again only when it grew.
pub fn read_tails(reader: &dyn LogReader, requests: &[TailRequest]) -> Vec<TailUpdate> {
    requests
        .iter()
        .map(|r| {
            let size = reader.size(&r.addr);
            let line = if size.is_some() && size == r.known {
                None
            } else {
                let tail = size.and_then(|_| reader.tail(&r.addr, TAIL_BYTES));
                Some(tail.map(|t| last_line(&t)).unwrap_or_default())
            };
            TailUpdate {
                id: r.id,
                finished: r.finished,
                size,
                line,
            }
        })
        .collect()
}

/// The last non-blank line of a log's tail, sanitized (I13) and with its
/// tabs expanded.
pub fn last_line(tail: &[u8]) -> Box<str> {
    let text = sanitize_log(tail);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let line = expand_tabs(line);
    crate::tui::text::take_width(&line, LINE_KEEP_COLS).into()
}

#[derive(Debug)]
struct Kept {
    size: Option<u64>,
    line: Box<str>,
    finished: bool,
}

/// One short line per visible row, as the background reader last found it.
#[derive(Debug, Default)]
pub struct LastLines {
    lines: HashMap<Id, Kept>,
}

impl LastLines {
    /// What to read for the rows `visible` shows: a row never read, a
    /// running row (the reader skips it when its log did not grow), and a
    /// finished row last read while it ran. A finished row read after it
    /// ended is never asked for again.
    pub fn requests(&self, model: &Model, visible: &[Id]) -> Vec<TailRequest> {
        visible
            .iter()
            .filter_map(|&id| {
                let r = model.record(id)?;
                if !wants_log(r.state) {
                    return None;
                }
                let finished = r.state != RowState::Running;
                let kept = self.lines.get(&id);
                if kept.is_some_and(|k| k.finished) {
                    return None;
                }
                Some(TailRequest {
                    id,
                    addr: model.addr(id).to_owned(),
                    finished,
                    known: kept.and_then(|k| k.size),
                })
            })
            .collect()
    }

    /// Take the reader's results, each line with `redact`'s paths taken out
    /// (I9) once here rather than on every frame; forget rows no longer on
    /// screen.
    pub fn apply(
        &mut self,
        updates: Vec<TailUpdate>,
        visible: &[Id],
        redact: &crate::tui::text::Redact,
    ) {
        for u in updates {
            match (self.lines.get_mut(&u.id), u.line) {
                (Some(k), None) => {
                    k.finished = u.finished;
                }
                (_, Some(line)) => {
                    let line = match redact.apply(&line) {
                        Cow::Borrowed(_) => line,
                        Cow::Owned(redacted) => redacted.into_boxed_str(),
                    };
                    self.lines.insert(
                        u.id,
                        Kept {
                            size: u.size,
                            line,
                            finished: u.finished,
                        },
                    );
                }
                (None, None) => {}
            }
        }
        if self.lines.len() > visible.len() {
            self.lines.retain(|id, _| visible.contains(id));
        }
    }

    pub fn get(&self, id: Id) -> Option<&str> {
        self.lines.get(&id).map(|k| &*k.line)
    }
}

/// Rows whose LAST LINE is their log's.
fn wants_log(s: RowState) -> bool {
    matches!(
        s,
        RowState::Running | RowState::Done | RowState::Cached | RowState::Failed
    )
}

/// The `/` filter's matches, cached per filter string: one bit per address,
/// extended as addresses arrive, so a frame never compares an address twice.
#[derive(Debug, Default)]
struct FilterCache {
    query: String,
    bits: Vec<u64>,
    scanned: usize,
}

impl FilterCache {
    fn sync(&mut self, query: &str, model: &Model) {
        if self.query != query {
            query.clone_into(&mut self.query);
            self.bits.clear();
            self.scanned = 0;
        }
        let n = model.id_count();
        self.bits.resize(n.div_ceil(64), 0);
        for i in self.scanned..n {
            let id = Id::try_from(i).unwrap_or(Id::MAX);
            if contains_ignore_ascii_case(model.addr(id), &self.query)
                && let Some(word) = self.bits.get_mut(i / 64)
            {
                *word |= 1 << (i % 64);
            }
        }
        self.scanned = n;
    }

    fn hit(&self, id: Id) -> bool {
        let i = id as usize;
        self.bits
            .get(i / 64)
            .is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

/// The list's selection, scroll and filter.
#[derive(Debug)]
pub struct ListState {
    tab: Tab,
    pub scope: CountScope,
    selected: Option<Id>,
    offset: usize,
    filter: String,
    editing: bool,
    cache: FilterCache,
    /// The rows the filter keeps, in order; rebuilt each frame while a
    /// filter is set, its buffer reused.
    matches: Vec<Id>,
    /// The rows the last frame drew, for the last-line reader.
    visible: Vec<Id>,
    /// Rows the last frame had room for.
    page: usize,
    /// Where the last frame drew the tabs and the rows, for a click.
    pub(crate) drawn: Drawn,
    /// The pending approval requests, oldest first, as the view last set
    /// them: the Approvals tab's rows, and the rows that lead All.
    approvals: Vec<ApprovalRow>,
    /// Where each of `approvals`' targets sits in the model's All, sorted:
    /// All skips them there, since they lead it.
    skips: Vec<usize>,
    /// The approval glyph this frame (`✦`/`❖`).
    glyph: &'static str,
}

/// A pending approval request's row: its target, its number, how long it
/// has waited and what notices it declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    pub id: Id,
    pub seq: u64,
    pub waited_ms: u64,
    /// `notices: plan, changelog`, or empty.
    pub notices: String,
}

/// Where a dashboard frame put what a click can hit, in the frame's rows.
#[derive(Debug, Default)]
pub(crate) struct Drawn {
    /// The tab row, when the frame had room for it, and its tabs' columns.
    pub tabs_row: Option<u16>,
    pub tabs: super::header::TabHits,
    /// The row of the first entry of [`ListState::visible`]; the rest
    /// follow one per row.
    pub rows_top: u16,
}

/// What a click on the dashboard landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListHit {
    Tab(Tab),
    Row(Id),
}

impl Default for ListState {
    fn default() -> Self {
        Self {
            tab: Tab::All,
            scope: CountScope::Matched,
            selected: None,
            offset: 0,
            filter: String::new(),
            editing: false,
            cache: FilterCache::default(),
            matches: Vec::new(),
            visible: Vec::new(),
            page: 1,
            drawn: Drawn::default(),
            approvals: Vec::new(),
            skips: Vec::new(),
            glyph: super::model::APPROVAL_GLYPH,
        }
    }
}

impl ListState {
    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if tab == Tab::Approvals && self.approvals.is_empty() {
            return;
        }
        if tab != self.tab {
            self.tab = tab;
            self.offset = 0;
        }
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        self.set_tab(self.tab.cycle(forward, !self.approvals.is_empty()));
    }

    /// The pending approval requests, oldest first, and this frame's
    /// approval glyph. Set before every frame and key that reads the list:
    /// the requests lead All and make the Approvals tab. With none left the
    /// Approvals tab goes, back to All.
    pub fn set_approvals(&mut self, model: &Model, rows: Vec<ApprovalRow>, glyph: &'static str) {
        self.glyph = glyph;
        self.approvals = rows;
        if self.approvals.is_empty() && self.tab == Tab::Approvals {
            self.tab = Tab::All;
            self.offset = 0;
        }
        self.skips.clear();
        if self.tab == Tab::All {
            let (approvals, skips) = (&self.approvals, &mut self.skips);
            skips.extend(
                approvals
                    .iter()
                    .filter_map(|a| model.index_of(Tab::All, self.scope, a.id)),
            );
            skips.sort_unstable();
        }
    }

    /// The pending requests, oldest first.
    pub fn approvals(&self) -> &[ApprovalRow] {
        &self.approvals
    }

    /// This frame's approval glyph.
    pub fn approval_glyph(&self) -> &'static str {
        self.glyph
    }

    /// The approval request `id`'s row shows, when it is one.
    pub fn approval_of(&self, id: Id) -> Option<&ApprovalRow> {
        self.approvals.iter().find(|a| a.id == id)
    }

    /// The rows that lead this tab: every pending request on All and on
    /// Approvals, none elsewhere.
    fn lead(&self) -> &[ApprovalRow] {
        match self.tab {
            Tab::All | Tab::Approvals => &self.approvals,
            _ => &[],
        }
    }

    pub fn selected(&self) -> Option<Id> {
        self.selected
    }

    /// The rows the last frame drew.
    pub fn visible(&self) -> &[Id] {
        &self.visible
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_editing(&self) -> bool {
        self.editing
    }

    /// `/`: start typing a filter.
    pub fn filter_start(&mut self) {
        self.editing = true;
    }

    pub fn filter_input(&mut self, c: char) {
        self.filter.push(c);
        self.offset = 0;
    }

    pub fn filter_backspace(&mut self) {
        self.filter.pop();
    }

    /// Enter: keep the filter, stop typing.
    pub fn filter_confirm(&mut self) {
        self.editing = false;
    }

    /// Esc: clear the filter first. `false` when there was none, so the
    /// caller closes the dashboard.
    pub fn escape(&mut self) -> bool {
        let had = self.editing || !self.filter.is_empty();
        self.editing = false;
        self.filter.clear();
        had
    }

    /// Rows in the list as the last frame filtered it.
    fn len(&self, model: &Model) -> usize {
        if self.filter.is_empty() {
            (model.tab_len(self.tab, self.scope) + self.lead().len())
                .saturating_sub(self.skips.len())
        } else {
            self.matches.len()
        }
    }

    fn row(&self, model: &Model, k: usize) -> Option<Id> {
        if !self.filter.is_empty() {
            return self.matches.get(k).copied();
        }
        let lead = self.lead();
        if let Some(a) = lead.get(k) {
            return Some(a.id);
        }
        // Past the lead, the model's rows with the lead's own skipped.
        let mut at = k - lead.len();
        for &s in &self.skips {
            if s <= at {
                at += 1;
            } else {
                break;
            }
        }
        model.row(self.tab, self.scope, at)
    }

    fn index_of(&self, model: &Model, id: Id) -> Option<usize> {
        if !self.filter.is_empty() {
            return self.matches.iter().position(|&m| m == id);
        }
        let lead = self.lead();
        if let Some(k) = lead.iter().position(|a| a.id == id) {
            return Some(k);
        }
        let at = model.index_of(self.tab, self.scope, id)?;
        let skipped = self.skips.iter().filter(|&&s| s < at).count();
        Some(at - skipped + lead.len())
    }

    /// Move the selection by `delta` rows.
    pub fn select_move(&mut self, model: &Model, delta: isize) {
        let len = self.len(model);
        if len == 0 {
            self.selected = None;
            return;
        }
        let at = self
            .selected
            .and_then(|id| self.index_of(model, id))
            .unwrap_or(0);
        let to = at.saturating_add_signed(delta).min(len - 1);
        self.selected = self.row(model, to);
    }

    /// A click selected `id`.
    pub fn select(&mut self, id: Id) {
        self.selected = Some(id);
    }

    /// The wheel: move the window by `delta` rows, and the selection with
    /// it only as far as keeps it on screen.
    pub fn wheel(&mut self, model: &Model, delta: isize) {
        let len = self.len(model);
        self.offset = self
            .offset
            .saturating_add_signed(delta)
            .min(len.saturating_sub(self.page));
        let Some(at) = self.selected.and_then(|id| self.index_of(model, id)) else {
            return;
        };
        let last = (self.offset + self.page).min(len).saturating_sub(1);
        let keep = at.clamp(self.offset, last.max(self.offset));
        if keep != at {
            self.selected = self.row(model, keep);
        }
    }

    /// What the last frame drew at `(col, row)` of the frame.
    pub fn hit(&self, col: u16, row: u16) -> Option<ListHit> {
        let d = &self.drawn;
        if d.tabs_row == Some(row) {
            return d
                .tabs
                .iter()
                .find(|(cols, _)| cols.contains(&col))
                .map(|&(_, tab)| ListHit::Tab(tab));
        }
        let k = usize::from(row.checked_sub(d.rows_top)?);
        self.visible.get(k).map(|&id| ListHit::Row(id))
    }

    /// Thin the tab by the filter (cached per filter string) and settle the
    /// selection and scroll for a `rows`-tall window. A selection the filter
    /// or a state change took out of the tab moves to the first row.
    fn settle(&mut self, model: &Model, rows: usize) -> usize {
        if !self.filter.is_empty() {
            self.cache.sync(&self.filter, model);
            let Self {
                cache,
                matches,
                approvals,
                tab,
                ..
            } = self;
            matches.clear();
            let tab = *tab;
            let lead = matches!(tab, Tab::All | Tab::Approvals);
            let led = |id: Id| lead && approvals.iter().any(|a| a.id == id);
            if lead {
                matches.extend(approvals.iter().map(|a| a.id).filter(|&id| cache.hit(id)));
            }
            matches.extend(
                model
                    .rows(tab, self.scope)
                    .filter(|&id| cache.hit(id) && !led(id)),
            );
        }
        let len = self.len(model);
        let at = match self.selected.and_then(|id| self.index_of(model, id)) {
            Some(at) => at,
            None => {
                self.selected = self.row(model, 0);
                0
            }
        };
        self.page = rows.max(1);
        if at < self.offset {
            self.offset = at;
        } else if at >= self.offset + self.page {
            self.offset = at + 1 - self.page;
        }
        self.offset = self.offset.min(len.saturating_sub(self.page));
        len
    }
}

/// What a frame of the list reads.
pub struct ListInputs<'a> {
    pub model: &'a Model,
    pub waits: &'a dyn Waits,
    /// The LAST LINE column, heph's home already taken out (I9).
    pub lines: &'a LastLines,
    pub now_ms: u64,
}

/// Column widths for a `cols`-wide list.
#[derive(Debug, Clone, Copy)]
struct Columns {
    target: usize,
    phase: usize,
    time: usize,
    last: usize,
}

/// `▸ ▶ ` before the target.
const LEAD: usize = 5;

/// The narrowest LAST LINE column worth drawing: under it, the column goes
/// and TARGET takes its room.
const MIN_LAST: usize = 12;

impl Columns {
    /// TARGET sized to `longest`, the widest address on screen, and LAST
    /// LINE the remainder. Past 60% of the width an address is cut while
    /// LAST LINE keeps [`MIN_LAST`]; short of that, LAST LINE shrinks and
    /// goes before any address is cut.
    fn fit(cols: usize, longest: usize) -> Self {
        let phase = 18;
        let time = 6;
        let rest = cols.saturating_sub(LEAD + phase + time + 1);
        let want = longest.max(width("TARGET"));
        let cap = cols * 3 / 5;
        let target = if want > cap && rest.saturating_sub(cap) >= MIN_LAST {
            cap
        } else {
            want.min(rest)
        };
        Self {
            target,
            phase,
            time,
            last: rest - target,
        }
    }
}

fn pad(s: &str, w: usize) -> String {
    let cut = cut_right(s, w);
    let gap = w.saturating_sub(width(&cut));
    format!("{cut}{}", " ".repeat(gap))
}

/// A sub-minute duration with tenths (`8.2s`), else `1m05s`.
pub(crate) fn secs_tenths(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        hcore::units::human_duration(std::time::Duration::from_millis(ms))
    }
}

fn whole(ms: u64) -> String {
    hcore::units::human_duration(std::time::Duration::from_millis(ms))
}

/// A LAST LINE cell whose line is unknown.
const UNKNOWN_LINE: &str = "–";

/// One row's PHASE, TIME and LAST LINE, borrowed where the words already
/// exist.
fn row_words<'a>(
    inp: &ListInputs<'a>,
    id: Id,
    r: &Record,
) -> (Cow<'a, str>, Cow<'a, str>, Cow<'a, str>) {
    let m = inp.model;
    // `–` when no line is known: not read yet, no log, or none this viewer
    // can read (an attached viewer reads its own session's logs only). A
    // blank would read as a log that printed nothing.
    let log = || Cow::Borrowed(inp.lines.get(id).unwrap_or(UNKNOWN_LINE));
    let none = || Cow::Borrowed("");
    match r.state {
        RowState::Queued => ("pending".into(), none(), none()),
        RowState::Blocked => match inp.waits.wait_words(m.addr(id), inp.now_ms) {
            Some(w) => (
                w.phase.into(),
                whole(w.waited_ms).into(),
                w.row_detail.into(),
            ),
            None => ("waiting".into(), none(), none()),
        },
        RowState::Running => {
            let phase = r.op().map_or(Cow::Borrowed("running"), |op| {
                format!("{} {}", op.icon(), op.phase()).into()
            });
            let time = whole(inp.now_ms.saturating_sub(r.op_since_ms));
            (phase, time.into(), log())
        }
        RowState::Done => ("done".into(), secs_tenths(r.total_ms()).into(), log()),
        RowState::Cached if r.remote_hit() => {
            let ms = r.op_ms(crate::tui::progress::Op::RemoteCacheRead);
            ("cached ↓ remote".into(), secs_tenths(ms).into(), log())
        }
        RowState::Cached => ("cached local".into(), none(), log()),
        RowState::Failed => {
            let time = match r.total_ms() {
                0 => none(),
                ms => secs_tenths(ms).into(),
            };
            (m.reason(id).unwrap_or("failed").into(), time, log())
        }
        RowState::DepFailed => {
            let root = m.root_of(id).map_or(none(), |a| format!("root {a}").into());
            ("dep failed".into(), none(), root)
        }
        RowState::Stopped => ("stopped".into(), none(), none()),
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A pending approval's row: `✦ //x  awaits approval  12s notices: plan`,
/// its glyph the frame's.
fn approval_row(
    inp: &ListInputs<'_>,
    a: &ApprovalRow,
    glyph: &'static str,
    selected: bool,
    c: Columns,
) -> Line<'static> {
    let addr = clean_inline(inp.model.addr(a.id));
    let target = cut_left(&addr, c.target);
    let target_gap = c.target.saturating_sub(width(&target));
    let yellow = Style::default().fg(Color::Yellow);
    let row_style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let last = cut_right(&clean_inline(&a.notices), c.last.saturating_sub(1)).into_owned();
    Line::from(vec![
        Span::styled(if selected { " ▸ " } else { "   " }, row_style),
        Span::styled(glyph, yellow.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(format!("{target}{} ", " ".repeat(target_gap)), row_style),
        Span::styled(pad("awaits approval", c.phase), yellow),
        Span::raw(format!("{:>tw$} ", whole(a.waited_ms), tw = c.time - 1)),
        Span::styled(last, dim()),
    ])
}

/// The list for `st`'s tab, `cols` wide and `rows` tall: the column titles,
/// the rows, a blank and the position (or the filter's count).
pub fn list_lines(
    inp: &ListInputs<'_>,
    st: &mut ListState,
    cols: u16,
    rows: u16,
) -> Vec<Line<'static>> {
    let cols = usize::from(cols);
    let rows = usize::from(rows);
    let body = rows.saturating_sub(3).max(usize::from(rows > 0));
    let len = st.settle(inp.model, body);
    st.visible.clear();
    for k in st.offset..(st.offset + body).min(len) {
        let Some(id) = st.row(inp.model, k) else {
            break;
        };
        if inp.model.record(id).is_some() {
            st.visible.push(id);
        }
    }
    let longest = st
        .visible
        .iter()
        .map(|&id| width(&clean_inline(inp.model.addr(id))))
        .max()
        .unwrap_or(0);
    let c = Columns::fit(cols, longest);
    let mut out = Vec::with_capacity(rows);
    if rows > 1 {
        out.push(Line::styled(
            format!(
                "{}{} {}{:>tw$} {}",
                " ".repeat(LEAD),
                pad("TARGET", c.target),
                pad("PHASE", c.phase),
                "TIME",
                cut_right("LAST LINE", c.last.saturating_sub(1)),
                tw = c.time - 1,
            ),
            dim(),
        ));
    }
    for &id in &st.visible {
        let Some(r) = inp.model.record(id) else {
            continue;
        };
        let selected = st.selected == Some(id);
        if let Some(a) = st.approval_of(id) {
            out.push(approval_row(inp, a, st.glyph, selected, c));
            continue;
        }
        let (phase, time, last) = row_words(inp, id, r);
        let addr = clean_inline(inp.model.addr(id));
        let target = cut_left(&addr, c.target);
        let target_gap = c.target.saturating_sub(width(&target));
        let phase = pad(&clean_inline(&phase), c.phase);
        let last = cut_right(&clean_inline(&last), c.last.saturating_sub(1)).into_owned();
        let row_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let phase_style = match r.state {
            RowState::Failed | RowState::DepFailed => Style::default().fg(Color::Red),
            RowState::Blocked => Style::default().fg(Color::Yellow),
            _ => Style::default(),
        };
        out.push(Line::from(vec![
            Span::styled(if selected { " ▸ " } else { "   " }, row_style),
            Span::styled(r.state.glyph(), r.state.style()),
            Span::raw(" "),
            Span::styled(format!("{target}{} ", " ".repeat(target_gap)), row_style),
            Span::styled(phase, phase_style),
            Span::raw(format!("{time:>tw$} ", tw = c.time - 1)),
            Span::styled(last, dim()),
        ]));
    }
    // Pad the body so the position line stays at the bottom.
    while out.len() < body + usize::from(rows > 1) {
        out.push(Line::default());
    }
    if rows >= 3 {
        out.push(Line::default());
        let shown = st.visible.len();
        let total =
            (inp.model.tab_len(st.tab, st.scope) + st.lead().len()).saturating_sub(st.skips.len());
        let text = if !st.filter.is_empty() {
            format!("{len} of {total} match")
        } else if shown == 0 {
            "nothing here".to_owned()
        } else {
            let up = if st.offset > 0 { "↑ " } else { "  " };
            let down = if st.offset + shown < len {
                " ↓"
            } else {
                "  "
            };
            format!("{up}{}–{} of {len}{down}", st.offset + 1, st.offset + shown)
        };
        let gap = cols.saturating_sub(width(&text)) / 2;
        out.push(Line::styled(format!("{}{text}", " ".repeat(gap)), dim()));
    }
    out.truncate(rows);
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hcore::events::{BuildEvent, BuildEventKind};

    use super::*;
    use crate::tui::text::Redact;

    struct NoWaits;

    impl Waits for NoWaits {
        fn wait_words(&self, _: &str, _: u64) -> Option<WaitWords> {
            None
        }
    }

    fn apply(m: &mut Model, kind: BuildEventKind) {
        m.apply(
            &BuildEvent {
                at_unix_ms: 1_000,
                kind,
            },
            &Redact::default(),
        );
    }

    fn model(addrs: &[&str]) -> Model {
        let mut m = Model::default();
        for a in addrs {
            apply(&mut m, BuildEventKind::ResultStart { addr: (*a).into() });
        }
        m
    }

    fn frame(m: &Model, lines: &LastLines, st: &mut ListState, cols: u16) -> Vec<String> {
        let inp = ListInputs {
            model: m,
            waits: &NoWaits,
            lines,
            now_ms: 2_000,
        };
        list_lines(&inp, st, cols, 20)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The addresses a frame shows, as drawn (cut).
    fn shown(rows: &[String]) -> Vec<String> {
        rows.iter()
            .skip(1)
            .filter_map(|r| {
                r.split_whitespace()
                    .find(|a| a.starts_with("//") || a.starts_with('…'))
                    .map(str::to_owned)
            })
            .collect()
    }

    /// I13: the log-text table, as the last-line column keeps it. What
    /// `last_line` returns is all the column draws (cut further to it).
    #[test]
    fn control_bytes_never_reach_terminal_last_line() {
        let long = "z".repeat(1 << 20);
        // Name, the tail, the line kept (`None`: only checked for control).
        let cases: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
            ("empty", b"".to_vec(), Some("")),
            ("blank lines", b"\n\n\n".to_vec(), Some("")),
            ("no newline", b"a\nlast".to_vec(), Some("last")),
            ("crlf", b"one\r\n".to_vec(), Some("one")),
            ("cr progress", b"10%\r20%\r30%\n".to_vec(), Some("30%")),
            (
                "modes",
                b"a\x1b[?1049lb\x1b[?1003hc\x1b[2Jd\n".to_vec(),
                Some("abcd"),
            ),
            (
                "osc",
                b"\x1b]8;;http://x\x07link\x1b]8;;\x07 \x1b]52;c;Zm9v\x07ok\n".to_vec(),
                Some("link ok"),
            ),
            ("lone csi", b"x\x9by\n".to_vec(), None),
            ("invalid utf8", b"a\xffb\n".to_vec(), Some("a\u{fffd}b")),
            ("nul", b"a\0b\n".to_vec(), Some("ab")),
            ("tabs", b"a\tb\n".to_vec(), Some("a       b")),
            ("long line", long.clone().into_bytes(), None),
            ("cut mid utf8 at the start", b"\xa9 tail\n".to_vec(), None),
            ("cut mid utf8 at the end", b"caf\xc3".to_vec(), None),
            (
                "spliced",
                b"out: hal".iter().chain(b"err\nf\n").copied().collect(),
                Some("f"),
            ),
            (
                "wide",
                "日本語 e\u{301}\n".as_bytes().to_vec(),
                Some("日本語 e\u{301}"),
            ),
        ];
        for (name, tail, want) in cases {
            let line = last_line(&tail);
            assert!(
                !line.chars().any(char::is_control),
                "{name}: control kept: {line:?}"
            );
            assert!(
                crate::tui::text::width(&line) <= LINE_KEEP_COLS,
                "{name}: {} wide",
                crate::tui::text::width(&line)
            );
            if let Some(want) = want {
                assert_eq!(&*line, want, "{name}");
            }
        }
        assert!(last_line(long.as_bytes()).chars().all(|c| c == 'z'));
        assert!(last_line(b"\xa9 tail\n").ends_with(" tail"));
        assert!(last_line(b"caf\xc3").starts_with("caf"));
    }

    /// Cut from the left on character boundaries, marked `…`, keeping the
    /// `:name` while the width allows; two addresses that cut alike still
    /// select separately.
    #[test]
    fn address_left_cut() {
        let addr = "//very/long/package/path:name";
        assert_eq!(cut_left(addr, 0), "");
        assert_eq!(cut_left(addr, 1), "…");
        assert_eq!(cut_left(addr, 2), "…e");
        assert_eq!(cut_left(addr, width(addr)), addr, "exactly the width");
        let over = cut_left(addr, width(addr) - 1);
        assert_eq!(over, "…very/long/package/path:name", "one char over");
        for w in ":name".len() + 1..width(addr) {
            let cut = cut_left(addr, w);
            assert!(cut.starts_with('…') && cut.ends_with(":name"), "{w}: {cut}");
            assert_eq!(width(&cut), w, "{cut}");
        }
        for multi in ["//café/ü:näme", "//日本/語:名前", "//p:e\u{301}e\u{301}"] {
            for w in 0..=width(multi) + 1 {
                let cut = cut_left(multi, w);
                assert!(width(&cut) <= w, "{multi} at {w}: {cut}");
                assert!(
                    multi.ends_with(cut.trim_start_matches('…')),
                    "{multi} at {w}: {cut}"
                );
            }
        }

        // Two addresses that cut to the same text in a 10-column TARGET.
        let m = model(&["//aaaa/x/same/pkg:name", "//bbbb/y/same/pkg:name"]);
        let lines = LastLines::default();
        let mut st = ListState::default();
        let rows = frame(&m, &lines, &mut st, 40);
        assert_eq!(shown(&rows), ["…/pkg:name", "…/pkg:name"], "{rows:#?}");
        let first = st.selected().expect("a selection");
        assert!(
            rows.get(1).is_some_and(|r| r.starts_with(" ▸ ")),
            "{rows:#?}"
        );
        st.select_move(&m, 1);
        let second = st.selected().expect("a selection");
        assert_ne!(first, second);
        assert_eq!(m.addr(second), "//bbbb/y/same/pkg:name");
        let rows = frame(&m, &lines, &mut st, 40);
        assert!(
            rows.get(2).is_some_and(|r| r.starts_with(" ▸ ")),
            "{rows:#?}"
        );

        // TARGET has priority over LAST LINE: as the width narrows, LAST
        // LINE shrinks, then goes, and only then is an address cut.
        let long = "//crates/plugin-buildfile/src/lsp:analysis_test";
        let m = model(&[long, "//a:b"]);
        let lines = LastLines::default();
        let mut st = ListState::default();
        let chrome = LEAD + 18 + 6 + 1;
        let full = chrome + width(long);
        for (cols, header) in [
            (full + MIN_LAST + 20, true),
            (full + 3, false),
            (full, false),
        ] {
            let rows = frame(&m, &lines, &mut st, u16::try_from(cols).expect("cols"));
            assert_eq!(shown(&rows), [long, "//a:b"], "{cols} cols: {rows:#?}");
            assert_eq!(
                rows.first().is_some_and(|h| h.ends_with("LAST LINE")),
                header,
                "{cols} cols: {rows:#?}"
            );
        }
        let rows = frame(&m, &lines, &mut st, u16::try_from(full - 1).expect("cols"));
        assert!(shown(&rows)[0].starts_with('…'), "{rows:#?}");
        // Past 60% of a wide window, the address is cut and LAST LINE kept.
        let huge = format!("//{}:x", "p/".repeat(60));
        let m = model(&[&huge]);
        let rows = frame(&m, &lines, &mut st, 120);
        assert_eq!(width(&shown(&rows)[0]), 120 * 3 / 5, "{rows:#?}");
        assert!(rows[0].ends_with("LAST LINE"), "{rows:#?}");
    }

    /// `/` is a literal, ASCII-case-insensitive substring; empty shows all;
    /// a selection filtered out moves to the first match; Esc clears the
    /// filter before it closes anything; the matches are cached per filter
    /// string, extended as targets arrive.
    #[test]
    fn filter_matches_literally() {
        let all = [
            "//p:foo",
            "//p:FOO_bar",
            "//p:a.b",
            "//p:a*b",
            "//p:x[1]",
            "//p:café",
            "//q:other",
        ];
        let mut m = model(&all);
        let lines = LastLines::default();
        let mut st = ListState::default();
        let filtered = |st: &mut ListState, m: &Model, q: &str| {
            st.escape();
            st.filter_start();
            for c in q.chars() {
                st.filter_input(c);
            }
            shown(&frame(m, &lines, st, 100))
        };
        assert_eq!(
            filtered(&mut st, &m, "").len(),
            all.len(),
            "empty: every row"
        );
        assert_eq!(filtered(&mut st, &m, "a.b"), ["//p:a.b"], "`.` is literal");
        assert_eq!(filtered(&mut st, &m, "*"), ["//p:a*b"]);
        assert_eq!(filtered(&mut st, &m, "x[1"), ["//p:x[1]"]);
        assert!(filtered(&mut st, &m, ".*[").is_empty());
        assert_eq!(filtered(&mut st, &m, "foo"), ["//p:foo", "//p:FOO_bar"]);
        assert_eq!(filtered(&mut st, &m, "FOO"), ["//p:foo", "//p:FOO_bar"]);
        assert_eq!(filtered(&mut st, &m, "CAFé"), ["//p:café"], "ASCII folds");
        assert_eq!(filtered(&mut st, &m, "é"), ["//p:café"]);

        // The selected row filtered out: the selection moves to the first match.
        filtered(&mut st, &m, "");
        st.select_move(&m, 6);
        assert_eq!(st.selected().map(|id| m.addr(id)), Some("//q:other"));
        filtered(&mut st, &m, "foo");
        assert_eq!(st.selected().map(|id| m.addr(id)), Some("//p:foo"));

        // Cached per filter string: a new target is the only one scanned.
        assert_eq!(st.cache.query, "foo");
        assert_eq!(st.cache.scanned, all.len());
        apply(
            &mut m,
            BuildEventKind::ResultStart {
                addr: "//r:food".into(),
            },
        );
        let rows = shown(&frame(&m, &lines, &mut st, 100));
        assert_eq!(
            rows,
            ["//p:foo", "//p:FOO_bar", "//r:food"],
            "queued oldest first"
        );
        assert_eq!(st.cache.scanned, all.len() + 1);

        // Esc clears the filter first, then lets the dashboard close.
        assert!(st.escape(), "a filter to clear");
        assert!(st.filter().is_empty() && !st.is_editing());
        assert!(!st.escape(), "nothing left: close");
        assert_eq!(shown(&frame(&m, &lines, &mut st, 100)).len(), all.len() + 1);
    }

    /// Counts what the list asks of the logs.
    #[derive(Default)]
    struct CountingReader {
        logs: Mutex<HashMap<String, Vec<u8>>>,
        sizes: AtomicUsize,
        tails: AtomicUsize,
    }

    impl CountingReader {
        fn write(&self, addr: &str, text: &str) {
            let mut logs = self.logs.lock().expect("logs");
            logs.entry(addr.to_owned())
                .or_default()
                .extend_from_slice(text.as_bytes());
        }

        fn calls(&self) -> (usize, usize) {
            (
                self.sizes.load(Ordering::Relaxed),
                self.tails.load(Ordering::Relaxed),
            )
        }
    }

    impl LogReader for CountingReader {
        fn size(&self, addr: &str) -> Option<u64> {
            self.sizes.fetch_add(1, Ordering::Relaxed);
            let logs = self.logs.lock().expect("logs");
            logs.get(addr).map(|l| l.len() as u64)
        }

        fn tail(&self, addr: &str, max: usize) -> Option<Vec<u8>> {
            self.tails.fetch_add(1, Ordering::Relaxed);
            let logs = self.logs.lock().expect("logs");
            let log = logs.get(addr)?;
            Some(log.get(log.len().saturating_sub(max)..)?.to_vec())
        }
    }

    /// A frame never reads a log: the tails come from the background half,
    /// a finished row's once, a running row's again only when it grew.
    /// That `frame` itself reads nothing is structural (it is handed no
    /// reader); `last_line_column_reads_off_the_frame_thread` in
    /// `progress.rs` proves it end to end, by the reading thread's id.
    #[test]
    fn frame_does_no_io() {
        let mut m = model(&["//p:run", "//p:done"]);
        let exec = |addr: &str| BuildEventKind::ExecuteStart {
            output_groups: Vec::new(),
            log_not_streamed: false,
            addr: addr.into(),
            driver: "exec".into(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
        };
        apply(&mut m, exec("//p:run"));
        apply(&mut m, exec("//p:done"));
        apply(
            &mut m,
            BuildEventKind::ExecuteEnd {
                addr: "//p:done".into(),
                error: None,
            },
        );
        apply(
            &mut m,
            BuildEventKind::ResultEnd {
                spans: Vec::new(),
                left_behind: None,
                cancelled: false,
                addr: "//p:done".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                exit_code: None,
                log_tail: None,
                timed_out: None,
            },
        );
        let reader = CountingReader::default();
        reader.write("//p:run", "Compiling a\n");
        reader.write("//p:done", "built\nFinished ok\n\n");
        let mut lines = LastLines::default();
        let mut st = ListState::default();

        frame(&m, &lines, &mut st, 100);
        assert_eq!(st.visible().len(), 2);

        let background = |lines: &mut LastLines, st: &ListState| {
            let requests = lines.requests(&m, st.visible());
            let updates = read_tails(&reader, &requests);
            lines.apply(updates, st.visible(), &Redact::default());
            requests.len()
        };
        assert_eq!(background(&mut lines, &st), 2);
        assert_eq!(reader.calls(), (2, 2));
        let rows = frame(&m, &lines, &mut st, 100);
        assert!(
            rows.iter()
                .any(|r| r.contains("//p:run") && r.ends_with("Compiling a")),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("//p:done") && r.ends_with("Finished ok")),
            "{rows:#?}"
        );

        // The finished row is never asked for again; the running one is
        // stat'ed, and read only once its log grew.
        assert_eq!(background(&mut lines, &st), 1);
        assert_eq!(reader.calls(), (3, 2), "unchanged: not read");
        reader.write("//p:run", "Compiling b\n");
        assert_eq!(background(&mut lines, &st), 1);
        assert_eq!(reader.calls(), (4, 3));
        let rows = frame(&m, &lines, &mut st, 100);
        assert!(rows.iter().any(|r| r.ends_with("Compiling b")), "{rows:#?}");
    }
}
