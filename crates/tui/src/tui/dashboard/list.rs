//! The dashboard's list: one full-width table, TARGET / PHASE / TIME /
//! LAST LINE, for the active tab. A frame reads the fold and the last-line
//! cache, and does no IO: the cache is filled off the render task from the
//! rows the last frame showed.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::intern::Id;
use super::model::{Model, Record, RowState, Tab};
use crate::tui::progress::CountScope;
use crate::tui::text::{clean_inline, cut_left, cut_right, expand_tabs, sanitize_log, width};

/// What a blocked row's PHASE and LAST LINE say, in the words of
/// `hcore::events::admission_wait_text` for a pool and of the line's
/// spotlight for a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitWords {
    /// `waits cpu 4`, `waits sandbox`, `locked: d1v9q`.
    pub phase: String,
    /// The rest: `(2 free, 6 ahead, 41s)`, `held by session d1v9q (pid 4121)`.
    pub detail: String,
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

    /// Take the reader's results; forget rows no longer on screen.
    pub fn apply(&mut self, updates: Vec<TailUpdate>, visible: &[Id]) {
        for u in updates {
            match (self.lines.get_mut(&u.id), u.line) {
                (Some(k), None) => {
                    k.finished = u.finished;
                }
                (_, Some(line)) => {
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
            if crate::tui::progress::addr_matches(model.addr(id), &self.query)
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
        }
    }
}

impl ListState {
    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if tab != self.tab {
            self.tab = tab;
            self.offset = 0;
        }
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        self.set_tab(self.tab.cycle(forward));
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
            model.tab_len(self.tab, self.scope)
        } else {
            self.matches.len()
        }
    }

    fn row(&self, model: &Model, k: usize) -> Option<Id> {
        if self.filter.is_empty() {
            model.row(self.tab, self.scope, k)
        } else {
            self.matches.get(k).copied()
        }
    }

    fn index_of(&self, model: &Model, id: Id) -> Option<usize> {
        if self.filter.is_empty() {
            model.index_of(self.tab, self.scope, id)
        } else {
            self.matches.iter().position(|&m| m == id)
        }
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

    /// Thin the tab by the filter (cached per filter string) and settle the
    /// selection and scroll for a `rows`-tall window. A selection the filter
    /// or a state change took out of the tab moves to the first row.
    fn settle(&mut self, model: &Model, rows: usize) -> usize {
        if !self.filter.is_empty() {
            self.cache.sync(&self.filter, model);
            let Self { cache, matches, .. } = self;
            matches.clear();
            matches.extend(model.rows(self.tab, self.scope).filter(|&id| cache.hit(id)));
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

impl Columns {
    fn of(cols: usize) -> Self {
        let phase = 18;
        let time = 6;
        let rest = cols.saturating_sub(LEAD + phase + time + 1);
        let target = (rest / 2 + 4).clamp(rest.min(10), 45).min(rest);
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

/// One row's PHASE, TIME and LAST LINE.
fn row_words(inp: &ListInputs<'_>, id: Id, r: &Record) -> (String, String, String) {
    let m = inp.model;
    let log = || inp.lines.get(id).unwrap_or("").to_owned();
    match r.state {
        RowState::Queued => ("pending".into(), String::new(), String::new()),
        RowState::Blocked => match inp.waits.wait_words(m.addr(id), inp.now_ms) {
            Some(w) => (w.phase, whole(w.waited_ms), w.detail),
            None => ("waiting".into(), String::new(), String::new()),
        },
        RowState::Running => {
            let phase = r
                .op()
                .map(|op| format!("{} {}", op.icon(), op.phase()))
                .unwrap_or_else(|| "running".into());
            let time = whole(inp.now_ms.saturating_sub(r.op_since_ms));
            (phase, time, log())
        }
        RowState::Done => ("done".into(), secs_tenths(r.total_ms()), log()),
        RowState::Cached if r.remote_hit() => {
            let ms = r.op_ms(crate::tui::progress::Op::RemoteCacheRead);
            ("cached ↓ remote".into(), secs_tenths(ms), log())
        }
        RowState::Cached => ("cached local".into(), String::new(), log()),
        RowState::Failed => {
            let time = match r.total_ms() {
                0 => String::new(),
                ms => secs_tenths(ms),
            };
            (m.reason(id).unwrap_or("failed").to_owned(), time, log())
        }
        RowState::DepFailed => {
            let root = m
                .root_of(id)
                .map(|a| format!("root {a}"))
                .unwrap_or_default();
            ("dep failed".into(), String::new(), root)
        }
        RowState::Stopped => ("stopped".into(), String::new(), String::new()),
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
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
    let c = Columns::of(cols);
    let mut out = Vec::with_capacity(rows);
    if rows > 1 {
        out.push(Line::styled(
            format!(
                "{}{} {}{:>tw$} LAST LINE",
                " ".repeat(LEAD),
                pad("TARGET", c.target),
                pad("PHASE", c.phase),
                "TIME",
                tw = c.time - 1,
            ),
            dim(),
        ));
    }
    st.visible.clear();
    for k in st.offset..(st.offset + body).min(len) {
        let Some(id) = st.row(inp.model, k) else {
            break;
        };
        let Some(r) = inp.model.record(id) else {
            continue;
        };
        st.visible.push(id);
        let selected = st.selected == Some(id);
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
        let total = inp.model.tab_len(st.tab, st.scope);
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
    #[test]
    fn frame_does_no_io() {
        let mut m = model(&["//p:run", "//p:done"]);
        let exec = |addr: &str| BuildEventKind::ExecuteStart {
            output_groups: Vec::new(),
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
                cancelled: false,
                addr: "//p:done".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: None,
            },
        );
        let reader = CountingReader::default();
        reader.write("//p:run", "Compiling a\n");
        reader.write("//p:done", "built\nFinished ok\n\n");
        let mut lines = LastLines::default();
        let mut st = ListState::default();

        for _ in 0..5 {
            frame(&m, &lines, &mut st, 100);
        }
        assert_eq!(reader.calls(), (0, 0), "frames read nothing");
        assert_eq!(st.visible().len(), 2);

        let background = |lines: &mut LastLines, st: &ListState| {
            let requests = lines.requests(&m, st.visible());
            let updates = read_tails(&reader, &requests);
            lines.apply(updates, st.visible());
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
        for _ in 0..5 {
            frame(&m, &lines, &mut st, 100);
        }
        assert_eq!(reader.calls(), (4, 3), "frames read nothing");
        let rows = frame(&m, &lines, &mut st, 100);
        assert!(rows.iter().any(|r| r.ends_with("Compiling b")), "{rows:#?}");
    }
}
