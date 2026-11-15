//! The target screen (spec section 3): one target full screen, with tabs
//! Overview · Log · Outputs under the dashboard's header. Overview reads
//! only the model; Log and Outputs read through a [`TargetFeed`], off the
//! render task (I7). Nothing drawn names a path into heph's home (I9).

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

use hcore::events::{AdmittedKind, display_amount, unit_of};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::detail::{Detail, Phase};
use super::feed::{
    FeedMsg, FileKind, LogOpened, OutputFile, OutputsFound, TargetFeed, TargetReader,
};
use super::header::{Title, header_lines};
use super::intern::Id;
use super::list::{Waits, secs_tenths};
use super::logpane::{LogPane, PaneState};
use super::model::{Model, Record, RowState, Tab};
use crate::tui::app::ScreenKey;
use crate::tui::progress::CountScope;
use crate::tui::text::{Redact, clean_inline, cut_left, cut_right, width};

pub const OVERVIEW_HINTS: &str = "tab/⇧tab switch tab · esc back to the list";
pub const LOG_HINTS: &str = "↑/↓ scroll (stops following) · f follow · / search · n/N next/prev · g/G top/bottom · w wrap · esc back";
pub const OUTPUTS_HINTS: &str =
    "↑/↓ scroll · enter expand a folded group · tab/⇧tab switch tab · esc back to the list";

/// Files a group lists before folding the rest into "… N more".
const FOLD_AT: usize = 3;

/// Rows besides the body: two of header, the tab row, its rule, a blank
/// and the hint row.
const CHROME_ROWS: u16 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetTab {
    Overview,
    Log,
    Outputs,
}

impl TargetTab {
    const ALL: [TargetTab; 3] = [TargetTab::Overview, TargetTab::Log, TargetTab::Outputs];

    fn name(self) -> &'static str {
        match self {
            TargetTab::Overview => "Overview",
            TargetTab::Log => "Log",
            TargetTab::Outputs => "Outputs",
        }
    }

    fn cycle(self, forward: bool) -> Self {
        match (self, forward) {
            (TargetTab::Overview, true) | (TargetTab::Outputs, false) => TargetTab::Log,
            (TargetTab::Log, true) | (TargetTab::Overview, false) => TargetTab::Outputs,
            (TargetTab::Outputs, true) | (TargetTab::Log, false) => TargetTab::Overview,
        }
    }
}

/// What a target frame reads besides the screen's own state.
pub struct TargetInputs<'a> {
    pub model: &'a Model,
    pub waits: &'a dyn Waits,
    pub redact: &'a Redact,
    pub now_ms: u64,
    /// The dashboard list's count scope, for the header's counts.
    pub scope: CountScope,
    /// Drawn for `heph session attach`: the hints end in `q detach`, as the
    /// list's do there (I11).
    pub detach: bool,
}

/// One open target screen. Dropping it ends its reader thread and closes
/// the log it followed (I4).
#[derive(Debug)]
pub struct TargetScreen {
    id: Id,
    tab: TargetTab,
    feed: Option<TargetFeed>,
    pane: LogPane,
    /// The state the log was last asked for in: a log that was not there
    /// is asked for again once the target moves on.
    log_asked: Option<RowState>,
    outputs: Option<OutputsFound>,
    /// The end (`end_ms`) the outputs were read for.
    outputs_asked: Option<u64>,
    /// Overview's and Outputs' first body row.
    scroll: usize,
    /// Outputs groups shown whole rather than folded.
    expanded: BTreeSet<String>,
    /// The last Outputs frame's folded groups, by body row, and how many
    /// body rows it showed: what Enter expands.
    folds: Vec<(usize, String)>,
    body_rows: usize,
    /// Where the last frame drew what a click can hit.
    drawn: Drawn,
}

/// The breadcrumb that starts the tab row; a click on it goes back.
const BACK: &str = " ◂ esc  ";
/// The tab row's place in the frame, and the first body row's.
const TABS_ROW: u16 = 2;
const BODY_TOP: u16 = 4;
/// Rows one wheel step moves.
pub const WHEEL_ROWS: isize = 3;

/// Where a target frame put the tab row's labels, how tall it was, and how
/// many body rows it filled.
#[derive(Debug)]
struct Drawn {
    tabs: [(Range<u16>, TargetTab); 3],
    rows: u16,
    body_shown: usize,
}

impl Default for Drawn {
    fn default() -> Self {
        Self {
            tabs: TargetTab::ALL.map(|t| (0..0, t)),
            rows: 0,
            body_shown: 0,
        }
    }
}

/// What a click on a target screen did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClick {
    /// The breadcrumb: back to the list, like esc.
    Back,
    /// A tab switched, or a folded group expanded.
    Done,
    /// Nothing there.
    Nothing,
}

impl TargetScreen {
    /// `id`'s screen: on Log when `log` asks for it or the target failed,
    /// else on Overview. `reader` serves its log and outputs.
    pub fn open(model: &Model, id: Id, log: bool, reader: Option<Arc<dyn TargetReader>>) -> Self {
        let failed = model
            .record(id)
            .is_some_and(|r| r.state == RowState::Failed);
        let feed = reader.and_then(|r| TargetFeed::spawn(r, model.addr(id).to_owned()));
        Self {
            id,
            tab: if log || failed {
                TargetTab::Log
            } else {
                TargetTab::Overview
            },
            feed,
            pane: LogPane::default(),
            log_asked: None,
            outputs: None,
            outputs_asked: None,
            scroll: 0,
            expanded: BTreeSet::new(),
            folds: Vec::new(),
            body_rows: 0,
            drawn: Drawn::default(),
        }
    }

    /// A click at `(col, row)` of the last frame: the breadcrumb, a tab, or
    /// a folded group's "… N more files" row on Outputs.
    pub fn click(&mut self, col: u16, row: u16) -> TargetClick {
        let d = &self.drawn;
        // The tab row is there while the hint row still fits under it.
        if row == TABS_ROW && d.rows > TABS_ROW + 1 {
            if usize::from(col) < width(BACK.trim_end()) {
                return TargetClick::Back;
            }
            let Some(&(_, tab)) = d.tabs.iter().find(|(cols, _)| cols.contains(&col)) else {
                return TargetClick::Nothing;
            };
            if tab != self.tab {
                self.pane.search_cancel();
                self.tab = tab;
                self.scroll = 0;
            }
            return TargetClick::Done;
        }
        if self.tab != TargetTab::Outputs {
            return TargetClick::Nothing;
        }
        let Some(k) = row.checked_sub(BODY_TOP).map(usize::from) else {
            return TargetClick::Nothing;
        };
        if k >= d.body_shown {
            return TargetClick::Nothing;
        }
        let at = self.scroll + k;
        match self.folds.iter().find(|(r, _)| *r == at) {
            Some((_, group)) => {
                self.expanded.insert(group.clone());
                TargetClick::Done
            }
            None => TargetClick::Nothing,
        }
    }

    /// One wheel step, `down` or up: the Log pane (up stops following, as
    /// ↑ does; down while following stays at the end), else the body.
    pub fn wheel(&mut self, down: bool) {
        if down && self.tab == TargetTab::Log && self.pane.following() {
            return;
        }
        self.scroll(if down { WHEEL_ROWS } else { -WHEEL_ROWS });
    }

    /// Enter on Outputs: expand the first folded group in view; with none
    /// in view, fold every group again. `false` on another tab.
    pub fn enter(&mut self) -> bool {
        if self.tab != TargetTab::Outputs {
            return false;
        }
        let view = self.scroll..self.scroll.saturating_add(self.body_rows);
        match self.folds.iter().find(|(row, _)| view.contains(row)) {
            Some((_, group)) => {
                self.expanded.insert(group.clone());
            }
            None => self.expanded.clear(),
        }
        true
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn tab(&self) -> TargetTab {
        self.tab
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        self.pane.search_cancel();
        self.tab = self.tab.cycle(forward);
        self.scroll = 0;
    }

    /// ↑/↓: the Log pane (which stops following), else the body.
    pub fn scroll(&mut self, delta: isize) {
        match self.tab {
            TargetTab::Log => self.pane.scroll(delta),
            _ => self.scroll = self.scroll.saturating_add_signed(delta),
        }
    }

    /// The Log tab's keys; `false` when `key` means nothing here.
    pub fn key(&mut self, key: ScreenKey, redact: &Redact) -> bool {
        if self.tab != TargetTab::Log {
            return false;
        }
        match key {
            ScreenKey::Follow | ScreenKey::Bottom => self.pane.follow_end(),
            ScreenKey::Top => self.pane.to_top(),
            ScreenKey::Wrap => self.pane.toggle_wrap(),
            ScreenKey::Next => self.pane.search_next(true, redact),
            ScreenKey::Prev => self.pane.search_next(false, redact),
            ScreenKey::PageUp => self.pane.page(-1),
            ScreenKey::PageDown => self.pane.page(1),
        }
        true
    }

    pub fn is_searching(&self) -> bool {
        self.pane.is_editing()
    }

    pub fn has_search(&self) -> bool {
        self.pane.has_search()
    }

    /// `/`: search the log, on the Log tab only.
    pub fn search_start(&mut self) {
        if self.tab == TargetTab::Log {
            self.pane.search_start();
        }
    }

    pub fn search_input(&mut self, c: char) {
        self.pane.search_input(c);
    }

    pub fn search_backspace(&mut self) {
        self.pane.search_backspace();
    }

    pub fn search_confirm(&mut self, redact: &Redact) {
        self.pane.search_confirm(redact);
    }

    pub fn search_cancel(&mut self) {
        self.pane.search_cancel();
    }

    /// Take what the reader found, and ask it for what this tab needs.
    /// Neither does IO: both are channel operations.
    fn pump(&mut self, m: &Model) {
        let Some(r) = m.record(self.id) else {
            return;
        };
        if let Some(feed) = &self.feed {
            for msg in feed.drain() {
                match msg {
                    FeedMsg::Following(about) => self.pane.start_live(about),
                    FeedMsg::Log(LogOpened::Tail { bytes, len, about }) => {
                        self.pane.start_tail(&bytes, len, about);
                    }
                    FeedMsg::Log(LogOpened::Nothing(why)) => {
                        self.pane.set_state(PaneState::Nothing(why));
                    }
                    FeedMsg::Log(LogOpened::Unavailable(why)) => {
                        self.pane.set_state(PaneState::Unavailable(why));
                    }
                    // Never sent: a follow arrives as `Following`.
                    FeedMsg::Log(LogOpened::Follow { .. }) => {}
                    FeedMsg::Bytes(b) => self.pane.append(&b),
                    FeedMsg::End => self.pane.ended(),
                    FeedMsg::Failed(why) => self.pane.set_state(PaneState::Unavailable(why)),
                    FeedMsg::Outputs(found) => self.outputs = Some(found),
                }
            }
        }
        // The log: asked for the first time the Log tab shows (a remote
        // hit's is downloaded then, never before), and again when it was
        // missing and the target has moved on.
        let missing = matches!(
            self.pane.state,
            PaneState::Closed | PaneState::Nothing(_) | PaneState::Unavailable(_)
        );
        if self.tab == TargetTab::Log && missing && self.log_asked != Some(r.state) {
            self.log_asked = Some(r.state);
            match &self.feed {
                Some(feed) => {
                    feed.want_log();
                    if self.pane.state == PaneState::Closed {
                        self.pane.set_state(PaneState::Opening);
                    }
                }
                None => self.pane.set_state(PaneState::Nothing(
                    "no log reader in this view: heph log reads it".into(),
                )),
            }
        }
        let ended = matches!(
            r.state,
            RowState::Done | RowState::Cached | RowState::Failed
        );
        if self.tab == TargetTab::Outputs && ended && self.outputs_asked != Some(r.end_ms) {
            self.outputs_asked = Some(r.end_ms);
            self.outputs = None;
            if let Some(feed) = &self.feed {
                feed.want_outputs(r.state == RowState::Failed);
            }
        }
    }

    /// The screen at `cols` × `rows`: the dashboard header's first two rows,
    /// the tab row and its rule, the tab's body, a blank and the hints.
    pub fn frame(
        &mut self,
        title: &Title<'_>,
        inp: &TargetInputs<'_>,
        cols: u16,
        rows: u16,
    ) -> Vec<Line<'static>> {
        self.pump(inp.model);
        let m = inp.model;
        let gauges = m.gauges();
        let counts = m.counts(inp.scope);
        let (header, _) = header_lines(title, &gauges, &counts, Tab::All, cols);
        let mut out: Vec<Line<'static>> = header.into_iter().take(2).collect();
        let c = usize::from(cols);
        let body_rows = usize::from(rows.saturating_sub(CHROME_ROWS));
        let addr = m.addr(self.id);
        let rec = m.record(self.id).copied();
        let (status, body, hints) = match (self.tab, rec) {
            (_, None) => (String::new(), Vec::new(), OVERVIEW_HINTS.to_owned()),
            (TargetTab::Overview, Some(r)) => {
                let body = overview(m, self.id, &r, inp, &self.pane, c);
                (
                    String::new(),
                    self.window(body, body_rows),
                    OVERVIEW_HINTS.to_owned(),
                )
            }
            (TargetTab::Log, Some(r)) => {
                let mut body = Vec::new();
                if r.state == RowState::Failed {
                    let (mut words, style) = state_words(m, self.id, &r, inp);
                    if let Some(took) = took_down_text(m, self.id) {
                        words = format!("{words} · {took}");
                    }
                    let words = inp.redact.apply(&clean_inline(&words)).into_owned();
                    body.push(Line::styled(
                        cut_right(&format!(" {words}"), c).into_owned(),
                        style,
                    ));
                    body.push(Line::default());
                }
                let left = body_rows.saturating_sub(body.len());
                body.extend(self.pane.body(c, left, inp.redact));
                let hints = self
                    .pane
                    .search_hint()
                    .unwrap_or_else(|| LOG_HINTS.to_owned());
                (self.pane.status(), body, hints)
            }
            (TargetTab::Outputs, Some(r)) => {
                let mut folds = std::mem::take(&mut self.folds);
                folds.clear();
                let (summary, body) = outputs(
                    m,
                    self.id,
                    &r,
                    self.outputs.as_ref(),
                    Fold {
                        expanded: &self.expanded,
                        folds: &mut folds,
                    },
                    inp,
                    c,
                );
                self.folds = folds;
                self.body_rows = body_rows;
                (
                    summary,
                    self.window(body, body_rows),
                    OUTPUTS_HINTS.to_owned(),
                )
            }
        };
        // A search's own hints say what its keys do; `q` types into it.
        let searching = self.tab == TargetTab::Log && self.pane.search_hint().is_some();
        let hints = if inp.detach && !searching {
            format!("{hints} · q detach")
        } else {
            hints
        };
        let (tabs, rule) = tab_rows(addr, self.tab, &status, c, &mut self.drawn.tabs);
        out.push(tabs);
        out.push(rule);
        self.drawn.rows = rows;
        self.drawn.body_shown = body.len().min(body_rows);
        out.extend(body.into_iter().take(body_rows));
        while out.len() < usize::from(rows.saturating_sub(2)) {
            out.push(Line::default());
        }
        out.push(Line::default());
        let hint = cut_right(&clean_inline(&hints), c.saturating_sub(2)).into_owned();
        out.push(Line::styled(format!("  {hint}"), dim()));
        let rows = usize::from(rows);
        if out.len() > rows {
            let hint = out.pop();
            out.truncate(rows.saturating_sub(1));
            out.extend(hint.filter(|_| rows > 0));
        }
        out
    }

    /// `body` from the scroll offset, kept within it.
    fn window(&mut self, body: Vec<Line<'static>>, rows: usize) -> Vec<Line<'static>> {
        self.scroll = self.scroll.min(body.len().saturating_sub(rows));
        body.into_iter().skip(self.scroll).take(rows).collect()
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// ` ◂ esc  //addr  │  Overview    Log    Outputs      <status>` and the
/// rule under it, heavy under the active tab.
fn tab_rows(
    addr: &str,
    active: TargetTab,
    status: &str,
    cols: usize,
    hits: &mut [(Range<u16>, TargetTab); 3],
) -> (Line<'static>, Line<'static>) {
    let addr = cut_left(&clean_inline(addr), cols / 2).into_owned();
    let mut spans = vec![
        Span::styled(BACK, dim()),
        Span::styled(addr, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("  │  ", dim()),
    ];
    let mut at: usize = spans.iter().map(|s| width(&s.content)).sum();
    let mut mark = (0, 0);
    for (i, t) in TargetTab::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("    "));
            at += 4;
        }
        let name = t.name();
        if let Some(h) = hits.get_mut(i) {
            let col = |c: usize| u16::try_from(c.min(cols)).unwrap_or(u16::MAX);
            *h = (col(at)..col(at + width(name)), t);
        }
        if t == active {
            mark = (at, width(name));
            spans.push(Span::styled(
                name,
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(name, dim()));
        }
        at += width(name);
    }
    let room = cols.saturating_sub(at + 2);
    if !status.is_empty() && room > 4 {
        let status = cut_right(status, room).into_owned();
        let gap = cols.saturating_sub(at + width(&status) + 1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(status, dim()));
    }
    let mut line = Line::from(spans);
    if line.width() > cols {
        line = Line::raw(cut_right(&line.to_string(), cols).into_owned());
    }
    let (start, w) = mark;
    let rule: String = (0..cols)
        .map(|i| {
            if i >= start && i < start + w {
                '━'
            } else {
                '─'
            }
        })
        .collect();
    (line, Line::styled(rule, dim()))
}

/// The state's words and colour: the Overview's title and a failed Log's
/// summary.
fn state_words(m: &Model, id: Id, r: &Record, inp: &TargetInputs<'_>) -> (String, Style) {
    let took = secs_tenths(r.total_ms());
    let red = Style::default().fg(Color::Red);
    match r.state {
        RowState::Queued => ("· pending".into(), dim()),
        RowState::Blocked => match inp.waits.wait_words(m.addr(id), inp.now_ms) {
            Some(w) => (format!("⧗ {}", w.phase), Style::default().fg(Color::Yellow)),
            None => ("⧗ waiting".into(), Style::default().fg(Color::Yellow)),
        },
        RowState::Running => {
            // From the first op: a wait for admission or the lock is not
            // running.
            let first = m
                .phases(id, inp.now_ms)
                .iter()
                .find(|p| !matches!(p.label, "queued" | "lock wait"))
                .map(|p| p.start_ms);
            let so_far = first.map_or(0, |s| inp.now_ms.saturating_sub(s));
            (
                format!("▶ running · {} so far", secs_tenths(so_far)),
                Style::default().fg(Color::Cyan),
            )
        }
        RowState::Done => (
            format!("✓ done in {took}"),
            Style::default().fg(Color::Green),
        ),
        RowState::Cached => (
            format!(
                "● cached · {}",
                if r.remote_hit() { "remote" } else { "local" }
            ),
            Style::default().fg(Color::Blue),
        ),
        RowState::Failed => {
            let reason = m.reason(id).unwrap_or("failed");
            (format!("✗ failed after {took} · {reason}"), red)
        }
        RowState::DepFailed => match m.root_of(id) {
            Some(root) => (format!("⊘ not built: {root} failed"), red),
            None => ("⊘ not built: a dependency failed".into(), red),
        },
        RowState::Stopped => ("○ stopped".into(), dim()),
    }
}

/// Names shown in "took down …" before the rest is counted.
const TOOK_DOWN_NAMED: usize = 3;

/// `took down //a, //b and 4 more`: what a root failure stopped from
/// building; `None` when it took nothing down.
fn took_down_text(m: &Model, id: Id) -> Option<String> {
    let (names, n) = m.took_down(id, TOOK_DOWN_NAMED);
    if n == 0 {
        return None;
    }
    let named = names.join(", ");
    Some(match n - names.len() {
        0 => format!("took down {named}"),
        rest => format!("took down {named} and {rest} more"),
    })
}

/// A label column and its value.
fn row(label: &str, value: &str, cols: usize) -> Line<'static> {
    let value = cut_right(&clean_inline(value), cols.saturating_sub(13)).into_owned();
    Line::from(vec![
        Span::styled(format!(" {label:<11} "), dim()),
        Span::raw(value),
    ])
}

/// The Overview: the facts, the timeline of phases on one time scale, and
/// the log's state. No def or input hash (decided).
fn overview(
    m: &Model,
    id: Id,
    r: &Record,
    inp: &TargetInputs<'_>,
    pane: &LogPane,
    cols: usize,
) -> Vec<Line<'static>> {
    let addr = clean_inline(m.addr(id)).into_owned();
    let (words, style) = state_words(m, id, r, inp);
    let words = cut_right(&words, cols.saturating_sub(4) / 2).into_owned();
    let addr = cut_left(&addr, cols.saturating_sub(width(&words) + 4)).into_owned();
    let gap = cols.saturating_sub(width(&addr) + width(&words) + 2);
    let mut out = vec![
        Line::from(vec![
            Span::styled(
                format!(" {addr}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(words, style),
        ]),
        Line::default(),
    ];
    let d = m.detail(id);
    let shape = d.and_then(|d| m.shape(d));
    if let Some(s) = shape {
        out.push(row("driver", &s.driver, cols));
    }
    let package = m.addr(id).rsplit_once(':').map_or(m.addr(id), |(p, _)| p);
    out.push(row("package", package, cols));
    if let Some(words) = d.and_then(|d| d.cache_words()) {
        out.push(row("cache", &words, cols));
    }
    let resources = match shape {
        Some(s) => resources(s.declared.as_ref(), s.reserved.as_ref(), m),
        None if r.state == RowState::Cached => "none reserved (no run)".to_owned(),
        None => String::new(),
    };
    if !resources.is_empty() {
        out.push(row("resources", &resources, cols));
    }
    // The timeout is known once the run hit it: `ExecuteStart` does not
    // carry it, so a run going on shows no time left.
    if let Some(abandoned) = d.and_then(Detail::timed_out) {
        let reason = m.reason(id).unwrap_or("timed out");
        let text = if abandoned {
            format!(
                "{reason} · abandoned: heph stopped waiting after the grace, its process may still be running"
            )
        } else {
            format!("{reason} · stopped")
        };
        out.push(row("timeout", &text, cols));
    }
    // Waiting now: the shared formatter's words, as the Pending row and
    // the spotlight say them. Admitted: how long and how.
    if r.state == RowState::Blocked
        && let Some(w) = inp.waits.wait_words(m.addr(id), inp.now_ms)
    {
        let text = if w.detail.is_empty() {
            w.phase
        } else {
            format!("{} {}", w.phase, w.detail)
        };
        out.push(row("waiting", &inp.redact.apply(&text), cols));
    } else if let Some(d) = d.filter(|d| d.ran()) {
        let how = match d.admitted() {
            Some(AdmittedKind::Arrival) => Some("at once"),
            Some(AdmittedKind::Head) => Some("as the oldest waiter"),
            Some(AdmittedKind::Handoff) => Some("by hand-off from a finished run of its size"),
            Some(AdmittedKind::Pass) => Some("by fitting what was free"),
            Some(AdmittedKind::Unknown) | None => None,
        };
        let text = match (d.queued_ms(), how) {
            (0, Some(how)) => how.to_owned(),
            (0, None) => String::new(),
            (q, Some(how)) => format!("waited {}, {how}", secs_tenths(q)),
            (q, None) => format!("waited {}", secs_tenths(q)),
        };
        if !text.is_empty() {
            out.push(row("admission", &text, cols));
        }
    }
    if let Some(s) = shape {
        let names: Vec<&str> = s
            .outputs
            .iter()
            .map(|g| {
                if g.is_empty() {
                    "(unnamed)"
                } else {
                    g.as_str()
                }
            })
            .collect();
        let text = match names.len() {
            0 => "none declared".to_owned(),
            n => format!("{n} declared: {}", names.join(", ")),
        };
        out.push(row("outputs", &text, cols));
    }
    if r.state == RowState::DepFailed
        && let Some(root) = m.root_of(id)
    {
        out.push(row("root", &format!("{root} (heph log {root})"), cols));
    }
    if r.state == RowState::Failed
        && let Some(took) = took_down_text(m, id)
    {
        let names = took.strip_prefix("took down ").unwrap_or(&took);
        out.push(row("took down", names, cols));
    }
    out.push(Line::default());
    out.extend(timeline_lines(&m.phases(id, inp.now_ms), cols));
    out.push(Line::default());
    let log = match &pane.state {
        PaneState::Closed | PaneState::Opening => "log · tab to open".to_owned(),
        PaneState::Live(_) => format!("log · live · {} · tab to open", pane.status()),
        PaneState::Ended(_) | PaneState::Read(_) => {
            format!("log · {} · tab to open", pane.status())
        }
        PaneState::Nothing(why) | PaneState::Unavailable(why) => {
            format!("log · {}", inp.redact.apply(&clean_inline(why)))
        }
    };
    out.push(Line::styled(
        cut_right(&format!(" {log}"), cols).into_owned(),
        dim(),
    ));
    for l in pane.last_lines(2, inp.redact) {
        out.push(Line::raw(cut_right(&format!(" │ {l}"), cols).into_owned()));
    }
    out
}

/// `cpu 2 of 2 · memory 1.0 GB of 1.0 GB`: granted of declared, by pool.
fn resources(
    declared: Option<&std::collections::BTreeMap<String, u64>>,
    reserved: Option<&std::collections::BTreeMap<String, u64>>,
    m: &Model,
) -> String {
    let Some(reserved) = reserved.filter(|r| !r.is_empty()) else {
        return "none reserved".to_owned();
    };
    reserved
        .iter()
        .map(|(pool, &got)| {
            let unit = unit_of(m.units(), pool);
            let got = display_amount(unit, got);
            match declared.and_then(|d| d.get(pool)) {
                Some(&want) => format!("{pool} {got} of {}", display_amount(unit, want)),
                None => format!("{pool} {got}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Label column of a timeline row: ` ⧗ queued       1.2s  `.
const TIMELINE_LABEL: usize = 24;

/// The timeline: a scale row, then one bar per phase on it.
pub fn timeline_lines(phases: &[Phase], cols: usize) -> Vec<Line<'static>> {
    let Some(t0) = phases.iter().map(|p| p.start_ms).min() else {
        return vec![Line::styled(" timeline    no phase has run yet", dim())];
    };
    let t1 = phases
        .iter()
        .map(|p| p.end_ms)
        .max()
        .unwrap_or(t0)
        .max(t0 + 1);
    let bar_w = cols.saturating_sub(TIMELINE_LABEL + 1).max(1);
    let span = t1 - t0;
    let col = |t: u64| -> usize {
        let x = u128::from(t.saturating_sub(t0)) * bar_w as u128 / u128::from(span);
        usize::try_from(x).unwrap_or(bar_w).min(bar_w)
    };
    let end = secs_tenths(span);
    let scale_gap = bar_w.saturating_sub(2 + width(&end));
    let mut out = vec![Line::styled(
        format!(
            " {:<w$}0s{}{end}",
            "timeline",
            " ".repeat(scale_gap),
            w = TIMELINE_LABEL - 1
        ),
        dim(),
    )];
    for p in phases {
        let a = col(p.start_ms).min(bar_w - 1);
        let b = col(p.end_ms).max(a + 1);
        let mut bar = " ".repeat(a);
        bar.extend(std::iter::repeat_n('█', b - a));
        if p.open {
            bar.pop();
            bar.push('▶');
        }
        let label = format!("   {} {:<12} {:>6}  ", p.glyph, p.label, secs_tenths(p.ms));
        let label = cut_right(&label, TIMELINE_LABEL + 1).into_owned();
        out.push(Line::from(vec![
            Span::raw(label),
            Span::styled(bar, Style::default().fg(Color::Cyan)),
        ]));
    }
    out
}

/// Which groups show whole, and where the folded ones' "… N more" rows
/// landed.
struct Fold<'a> {
    expanded: &'a BTreeSet<String>,
    folds: &'a mut Vec<(usize, String)>,
}

/// The Outputs tab's summary (the tab row's right side) and body.
fn outputs(
    m: &Model,
    id: Id,
    r: &Record,
    found: Option<&OutputsFound>,
    mut fold: Fold<'_>,
    inp: &TargetInputs<'_>,
    cols: usize,
) -> (String, Vec<Line<'static>>) {
    let addr = m.addr(id);
    let note = |s: &str| {
        vec![Line::styled(
            cut_right(&format!(" {s}"), cols).into_owned(),
            dim(),
        )]
    };
    let declared = m
        .detail(id)
        .and_then(|d| m.shape(d))
        .map(|s| s.outputs.clone());
    match r.state {
        RowState::Queued | RowState::Blocked | RowState::Running => match declared {
            None => (
                String::new(),
                note("not run yet: outputs show once it has finished"),
            ),
            Some(names) if names.is_empty() => (String::new(), note("no outputs declared")),
            Some(names) => {
                let mut out = vec![header_row(cols)];
                for n in names {
                    let name = if n.is_empty() {
                        "(unnamed)".to_owned()
                    } else {
                        n
                    };
                    out.push(file_row(&name, "not written yet", None, None, cols));
                }
                (format!("{} declared", out.len() - 1), out)
            }
        },
        RowState::DepFailed => (String::new(), note("no outputs: a dependency failed")),
        RowState::Stopped => (String::new(), note("no outputs: the run was stopped")),
        RowState::Failed => match found {
            None => (String::new(), note("reading what the failed run kept…")),
            Some(OutputsFound::Failure { kept, fixes })
                if !kept.is_empty() || !fixes.is_empty() =>
            {
                let mut out = vec![header_row(cols)];
                list_files("failure_out", kept, &mut out, cols, &mut fold);
                for (i, f) in fixes.iter().enumerate() {
                    let label = if i == 0 { "fixes" } else { "" };
                    out.push(file_row(
                        label,
                        &format!("{f} (written back)"),
                        None,
                        None,
                        cols,
                    ));
                }
                out.push(Line::default());
                out.push(row("paths", &format!("heph inspect failure {addr}"), cols));
                (format!("✗ failed · {} kept", kept.len()), out)
            }
            Some(OutputsFound::Unavailable(why)) => (String::new(), note(&inp.redact.apply(why))),
            Some(_) => (String::new(), note("no outputs: the run failed")),
        },
        RowState::Done | RowState::Cached => match found {
            None => (String::new(), note("reading the outputs…")),
            Some(OutputsFound::Listing(groups)) => {
                let mut out = vec![header_row(cols)];
                let mut files = 0;
                let mut bytes = 0;
                for g in groups {
                    let name = if g.name.is_empty() {
                        "(unnamed)"
                    } else {
                        &g.name
                    };
                    files += g.files.len();
                    bytes += g.files.iter().map(|f| f.size).sum::<u64>();
                    match &g.unavailable {
                        Some(why) => out.push(file_row(
                            name,
                            &inp.redact.apply(why),
                            Some(g.size),
                            g.hash.as_deref(),
                            cols,
                        )),
                        None if g.files.is_empty() => {
                            out.push(file_row(name, "(nothing written)", None, None, cols));
                        }
                        None => list_files(name, &g.files, &mut out, cols, &mut fold),
                    }
                }
                out.push(Line::default());
                out.push(row("paths", &format!("heph inspect outputs {addr}"), cols));
                let head = match r.state {
                    RowState::Cached => "● cached".to_owned(),
                    _ => format!("✓ done {}", secs_tenths(r.total_ms())),
                };
                let summary = format!(
                    "{head} · {} groups · {files} files · {}",
                    groups.len(),
                    hcore::units::human_bytes(bytes)
                );
                (summary, out)
            }
            Some(OutputsFound::Unavailable(why)) => (String::new(), note(&inp.redact.apply(why))),
            Some(_) => (String::new(), note("no outputs held in the local cache")),
        },
    }
}

/// Size column, hash column.
const SIZE_W: usize = 9;
const HASH_W: usize = 12;
const GROUP_W: usize = 12;

fn header_row(cols: usize) -> Line<'static> {
    file_row_styled(
        "GROUP",
        "PATH",
        Some("SIZE".into()),
        Some("HASH"),
        cols,
        dim(),
    )
}

fn file_row(
    group: &str,
    path: &str,
    size: Option<u64>,
    hash: Option<&str>,
    cols: usize,
) -> Line<'static> {
    file_row_styled(
        group,
        path,
        size.map(hcore::units::human_bytes),
        hash,
        cols,
        Style::default(),
    )
}

fn file_row_styled(
    group: &str,
    path: &str,
    size: Option<String>,
    hash: Option<&str>,
    cols: usize,
    style: Style,
) -> Line<'static> {
    // Indent, group, space, path, space, size, two spaces, hash.
    let path_w = cols.saturating_sub(3 + GROUP_W + 1 + 1 + SIZE_W + 2 + HASH_W);
    let group = cut_right(&clean_inline(group), GROUP_W).into_owned();
    let path = cut_left(&clean_inline(path), path_w).into_owned();
    let hash: String = hash.unwrap_or("").chars().take(HASH_W).collect();
    let pad = |s: &str, w: usize| " ".repeat(w.saturating_sub(width(s)));
    let size = size.unwrap_or_default();
    let text = format!(
        "   {group}{} {path}{} {}{size}  {hash}",
        pad(&group, GROUP_W),
        pad(&path, path_w),
        pad(&size, SIZE_W),
    );
    Line::styled(cut_right(&text, cols).into_owned(), style)
}

/// A group's files: the first [`FOLD_AT`] then "… N more files", or all of
/// them once Enter expanded the group.
fn list_files(
    group: &str,
    files: &[OutputFile],
    out: &mut Vec<Line<'static>>,
    cols: usize,
    fold: &mut Fold<'_>,
) {
    let shown = if fold.expanded.contains(group) {
        files.len()
    } else {
        FOLD_AT
    };
    for (i, f) in files.iter().take(shown).enumerate() {
        let label = if i == 0 { group } else { "" };
        let path = match &f.kind {
            FileKind::File => f.path.clone(),
            FileKind::Symlink(to) => format!("{} → {to}", f.path),
            FileKind::Dir => format!("{}/", f.path.trim_end_matches('/')),
        };
        let size = (f.kind == FileKind::File).then_some(f.size);
        out.push(file_row(label, &path, size, f.hash.as_deref(), cols));
    }
    if let Some(rest) = files.get(shown..).filter(|r| !r.is_empty()) {
        let size = rest.iter().map(|f| f.size).sum();
        // `out` is the body from its first row: the fold row's index.
        fold.folds.push((out.len(), group.to_owned()));
        out.push(file_row(
            "",
            &format!("… {} more files", rest.len()),
            Some(size),
            None,
            cols,
        ));
    }
}
