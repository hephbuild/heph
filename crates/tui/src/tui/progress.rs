//! Client-side aggregator for the engine's build-progress event stream.
//!
//! [`BuildState`] folds [`BuildEvent`]s into running/completed/errored counts,
//! cache hit/miss statistics, and a list of targets that are currently executing.
//! It uses only the server-stamped `at_unix_ms` off each event — never a local
//! receipt clock — so elapsed times stay correct across a future client/server
//! process split. Rendering callers pass their own `now_ms` wall clock.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::tui::app::{CIAppView, TUIAppView};
use hcore::events::{BuildEvent, BuildEventKind};

/// Floor on the pinned viewport row count. The viewport targets one third of the
/// terminal height (see [`rows_for_height`]) but never shrinks below this, which
/// is the minimum that fits the box: top border + 1 body row + bottom border +
/// help row.
pub const MIN_PROGRESS_ROWS: u16 = 4;

/// Pinned viewport rows for a given terminal height: one third of the terminal,
/// clamped up to [`MIN_PROGRESS_ROWS`]. The box grows the body (slow/lock rows)
/// to fill whatever rows it is given.
pub fn rows_for_height(term_height: u16) -> u16 {
    (term_height / 3).max(MIN_PROGRESS_ROWS)
}

/// Slice `lines` to a `rows`-tall window starting at `scroll`, returning the
/// window and the clamped scroll offset. Scroll is clamped so the window never
/// runs off the end. The scroll indicator in the bottom border conveys overflow.
fn windowed(
    mut lines: Vec<Line<'static>>,
    rows: usize,
    scroll: usize,
) -> (Vec<Line<'static>>, usize) {
    let total = lines.len();
    if rows == 0 {
        return (Vec::new(), 0);
    }
    if total <= rows {
        return (lines, 0);
    }
    let max_scroll = total - rows;
    let scroll = scroll.min(max_scroll);
    let window: Vec<Line<'static>> = lines.drain(scroll..scroll + rows).collect();
    (window, scroll)
}

/// One row of a list-view body before it is rendered: the addr, the colour it
/// paints in, and an optional dim trailing detail (a failure message).
///
/// Rows borrow out of [`BuildState`], so a list can be counted, measured and
/// windowed without building a `Line` for every entry. The viewport shows ~20
/// rows; at 100k targets the other 99,980 `format!`s were pure waste.
#[derive(Debug, Clone, Copy)]
struct BodyRow<'a> {
    addr: &'a str,
    color: Color,
    /// Rendered dim after the addr. `None` for the addr-only lists.
    detail: Option<&'a str>,
}

impl BodyRow<'_> {
    /// Visible column count of this row's line, without rendering it. Must track
    /// [`BodyRow::render`] exactly: the horizontal pan clamps against the widest
    /// row in the whole list, so a mismatch would shift the viewport.
    fn width(&self) -> usize {
        // The leading indent, plus the same two-space gap before any detail.
        2 + self.addr.chars().count() + self.detail.map_or(0, |d| 2 + d.chars().count())
    }

    fn render(&self) -> Line<'static> {
        let addr = Span::styled(format!("  {}", self.addr), Style::default().fg(self.color));
        match self.detail {
            None => Line::from(addr),
            Some(detail) => Line::from(vec![
                addr,
                Span::styled(
                    format!("  {detail}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]),
        }
    }
}

/// The rows of one list view, plus whether they still need ordering. Building
/// one costs a single allocation whatever the list length; only the rows the
/// viewport actually shows become `Line`s.
struct BodyRows<'a> {
    rows: Vec<BodyRow<'a>>,
    /// Set when the rows came out of a `HashSet` and must be ordered by addr
    /// before windowing. The arrival-ordered lists leave it clear — they already
    /// render in the order they were folded.
    sort_by_addr: bool,
}

impl<'a> BodyRows<'a> {
    /// Rows that render in the order given.
    fn in_order(rows: Vec<BodyRow<'a>>) -> Self {
        Self {
            rows,
            sort_by_addr: false,
        }
    }

    /// Rows that render ordered by addr. Only the visible window is ever
    /// actually ordered — see [`BodyRows::window`].
    fn by_addr(rows: Vec<BodyRow<'a>>) -> Self {
        Self {
            rows,
            sort_by_addr: true,
        }
    }

    fn len(&self) -> usize {
        self.rows.len()
    }

    /// The widest row in visible columns — the clamp for the horizontal pan.
    fn max_width(&self) -> usize {
        self.rows.iter().map(BodyRow::width).max().unwrap_or(0)
    }

    /// Render the `rows`-tall window at `scroll`, returning it and the clamped
    /// offset. Clamps like [`windowed`]: the window never runs off the end. An
    /// addr-ordered list is placed by [`place_window`], never sorted whole.
    fn window(mut self, rows: usize, scroll: usize) -> (Vec<Line<'static>>, usize) {
        let total = self.rows.len();
        if rows == 0 {
            return (Vec::new(), 0);
        }
        let start = if total <= rows {
            0
        } else {
            scroll.min(total - rows)
        };
        let end = start.saturating_add(rows).min(total);
        if self.sort_by_addr {
            // Addrs are unique within every list `by_addr` backs — they come out
            // of `BuildState`'s `matched` / `cache_hit`, both keyed by addr — so
            // the order is total and the unstable partition is unambiguous.
            place_window(&mut self.rows, start, end, |a, b| a.addr.cmp(b.addr));
        }
        let window = self
            .rows
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(BodyRow::render)
            .collect();
        (window, start)
    }
}

/// A row buffer sized for a list of `len` rows that `filter` will thin out.
///
/// Unfiltered, the whole list lands in it, so one reservation beats `collect`'s
/// geometric growth and its repeated copies. Filtered, the match count is not
/// known ahead and can be a handful out of 100k, so reserving the full length
/// every frame would ask the allocator for megabytes to hold three rows.
fn row_buffer<T>(len: usize, filter: &str) -> Vec<T> {
    if filter.is_empty() {
        Vec::with_capacity(len)
    } else {
        Vec::new()
    }
}

/// Move the elements that belong at `[start, end)` in `cmp` order into that
/// range, leaving the rest of `rows` in no particular order.
///
/// This is the sort the viewport does *not* do. Two `select_nth_unstable_by`
/// partitions place the range in `O(n)` — first splitting off everything that
/// sorts after the window, then everything before it — and only the window
/// itself, `end - start` long, is sorted. A full sort would be `O(n log n)`
/// to then throw all but a screenful away.
///
/// `start <= end <= rows.len()`. Callers must order on a key that is unique
/// across `rows`: the partitions are unstable, so with duplicate keys the
/// window could hold a different pick than a stable full sort would.
fn place_window<T>(
    rows: &mut [T],
    start: usize,
    end: usize,
    mut cmp: impl FnMut(&T, &T) -> std::cmp::Ordering,
) {
    if end < rows.len() {
        // Ranks `[0, end)` move to the front; the tail is left unordered.
        rows.select_nth_unstable_by(end, &mut cmp);
    }
    // `end <= rows.len()`, so the split is in bounds.
    let (head, _) = rows.split_at_mut(end);
    if start > 0 {
        // Ranks `[0, start)` move to the front of that prefix, leaving exactly
        // the window's elements behind them.
        head.select_nth_unstable_by(start, &mut cmp);
    }
    // `start <= end == head.len()`, so this split is in bounds too.
    let (_, window) = head.split_at_mut(start);
    window.sort_unstable_by(&mut cmp);
}

/// One frame's body: the live view's already-rendered lines, or a list tab's
/// deferred rows. The live body is bounded by what is in flight; a list tab is
/// bounded only by the run, which is why it defers.
enum Body<'a> {
    Lines(Vec<Line<'static>>),
    Rows(BodyRows<'a>),
}

impl Body<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Lines(lines) => lines.len(),
            Self::Rows(rows) => rows.len(),
        }
    }

    /// The widest row in visible columns — the clamp for the horizontal pan.
    fn max_width(&self) -> usize {
        match self {
            Self::Lines(lines) => lines
                .iter()
                .map(|l| spans_width(&l.spans))
                .max()
                .unwrap_or(0),
            Self::Rows(rows) => rows.max_width(),
        }
    }

    /// The `rows`-tall window at `scroll`, plus the clamped offset.
    fn window(self, rows: usize, scroll: usize) -> (Vec<Line<'static>>, usize) {
        match self {
            Self::Lines(lines) => windowed(lines, rows, scroll),
            Self::Rows(r) => r.window(rows, scroll),
        }
    }
}

/// Columns shifted per Left/Right key press when panning a wide body.
pub const HSCROLL_STEP: usize = 4;

/// Minimum columns the bottom border must reserve when the scroll indicator is
/// active. The shortest possible indicator `↑ 1–1 of 1 ↓` is 14 columns; this
/// leaves room for the label to remain visible on narrow-but-not-tiny terminals.
const SCROLL_INDICATOR_MIN_WIDTH: usize = 20;

/// Drop the first `offset` visible columns from a line, preserving each span's
/// styling. Every glyph this module emits is single-width, so a char count is an
/// exact column count. `offset == 0` returns the line untouched.
fn hscroll_line(line: Line<'static>, offset: usize) -> Line<'static> {
    if offset == 0 {
        return line;
    }
    let mut remaining = offset;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let chars = span.content.chars().count();
        if remaining >= chars {
            remaining -= chars;
            continue;
        }
        if remaining > 0 {
            let kept: String = span.content.chars().skip(remaining).collect();
            spans.push(Span::styled(kept, span.style));
            remaining = 0;
        } else {
            spans.push(span);
        }
    }
    Line::from(spans)
}

/// A target is considered "taking long" once its execute span exceeds this.
pub const LONG_RUNNING_THRESHOLD_MS: u64 = 5_000;

/// Display width the elapsed clock pads to once it leaves the seconds band.
/// The seconds band renders natural (2–3 chars) so a fresh run starts compact;
/// from a minute on it pads to this so the field only ever grows, never shrinks
/// (e.g. `59m59s` → `1h00m` would otherwise lose a column and jitter the counts).
const ELAPSED_MIN_WIDTH: usize = 6;

/// One braille cell holds up to this many worker dots (8-dot braille).
const WORKERS_PER_CELL: usize = 8;

/// Glyph for a cell with N busy workers, indexed by N (0..=8). Index 0 is the
/// empty/idle cell (the caller paints it grey); the lit progression follows the
/// pinned UI rule `⠁ ⠃ ⠇ ⠧ ⠷ ⠿ … ⣿` with the 7-dot step filled in.
const BRAILLE_FILL: [char; WORKERS_PER_CELL + 1] = ['⣿', '⠁', '⠃', '⠇', '⡇', '⣇', '⣧', '⣷', '⣿'];

/// Banner scroll cadence: advance one column every this many milliseconds.
const SCROLL_MS: u64 = 150;

/// Below this width the box-drawing math has no room; we clamp up to it.
const MIN_BOX_WIDTH: usize = 16;

/// Visible column count of a span run. Every glyph we emit (box-drawing,
/// braille, ASCII, `·`) is single-width, so a char count is exact.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Paint a header span as the active tab: a blue background with white text so
/// the selected view reads as highlighted in the status line.
fn highlight_span(s: &Span<'static>) -> Span<'static> {
    Span::styled(s.content.clone(), s.style.bg(Color::Blue).fg(Color::White))
}

/// One braille cell per group of [`WORKERS_PER_CELL`] worker slots. Busy slots
/// fill left-to-right across cells; an all-idle cell is dim grey, any-busy cell
/// is blue at the glyph matching its busy count.
fn worker_spans(max_workers: usize, busy: usize) -> Vec<Span<'static>> {
    if max_workers == 0 {
        return Vec::new();
    }
    let cells = max_workers.div_ceil(WORKERS_PER_CELL);
    let mut spans = Vec::with_capacity(cells);
    for c in 0..cells {
        let cell_start = c * WORKERS_PER_CELL;
        let cap = (max_workers - cell_start).min(WORKERS_PER_CELL);
        let busy_here = busy.saturating_sub(cell_start).min(cap);
        let (glyph, style) = if busy_here == 0 {
            // Idle cell: grey outline of the *available* slots in this cell, so a
            // partial trailing cell (e.g. 2 of 8) shows only its real worker count
            // (⠃) instead of a full ⣿ that overstates capacity. A full cell uses
            // cap == 8 ⇒ ⣿, unchanged.
            (
                BRAILLE_FILL.get(cap).copied().unwrap_or('⣿'),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )
        } else {
            (
                BRAILLE_FILL.get(busy_here).copied().unwrap_or('⣿'),
                Style::default().fg(Color::Blue),
            )
        };
        spans.push(Span::styled(glyph.to_string(), style));
    }
    spans
}

/// Compact, human-friendly elapsed time. Precision is indicative only — the
/// coarsest two units are shown: `12s`, `1m05s`, `1h05m`, `2d03h`. The seconds
/// band is natural width (compact start); everything past a minute is padded to
/// [`ELAPSED_MIN_WIDTH`] so the field grows monotonically and never flickers.
fn human_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let s = if secs < 3_600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3_600)
    };
    format!("{s:>ELAPSED_MIN_WIDTH$}")
}

/// Like [`human_elapsed`] but the sub-minute band keeps millisecond precision
/// (`12.345s`). Used by the final report where the live flicker concern that
/// drives [`human_elapsed`]'s coarse seconds band does not apply.
fn human_elapsed_ms(ms: u64) -> String {
    if ms / 1000 < 60 {
        return format!("{}.{:03}s", ms / 1000, ms % 1000);
    }
    human_elapsed(ms)
}

/// A `window`-wide view of `label` scrolled like a banner: the text plus a
/// 3-space gap, cycled, sampled at a time-derived offset. Wraps seamlessly.
fn banner_slice(label: &str, window: usize, now_ms: u64) -> String {
    let cycle: Vec<char> = label.chars().chain("   ".chars()).collect();
    let cyc_len = cycle.len().max(1);
    let offset = (now_ms / SCROLL_MS) as usize % cyc_len;
    (0..window)
        .map(|i| cycle.get((offset + i) % cyc_len).copied().unwrap_or(' '))
        .collect()
}

/// Wall-clock → animation phase divisor (ms). Larger = slower drift.
const ART_PERIOD_MS: f64 = 1400.0;

/// Plasma density ramp, low → high. Classic ASCII-plasma glyphs; rendered dim so
/// the whole field stays discreet despite full coverage.
const ART_RAMP: [char; 8] = ['.', ':', '-', '=', '+', '*', '#', '%'];

/// `rows` body lines of a dim, slowly-drifting plasma field: overlaid sine waves
/// (including a radial term) sampled into [`ART_RAMP`]. Pure function of `now_ms`
/// and cell position; one uniform dim style per line keeps spans cheap. A 1-space
/// gutter is kept on the left and right so the field never touches the box edges.
fn art_lines(now_ms: u64, width: usize, rows: usize) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let t = now_ms as f64 / ART_PERIOD_MS;
    let inner = width.saturating_sub(2);
    let cx = inner as f64 / 2.0;
    let cy = rows as f64 / 2.0;
    let n = ART_RAMP.len();
    let mut lines = Vec::with_capacity(rows);
    for y in 0..rows {
        let fy = y as f64;
        let mut s = String::with_capacity(width);
        s.push(' ');
        for x in 0..inner {
            let fx = x as f64;
            let dx = fx - cx;
            let dy = (fy - cy) * 2.0; // cells are ~2× taller than wide
            let v = (fx * 0.16 + t).sin()
                + (fy * 0.55 - t * 0.7).sin()
                + ((fx + fy) * 0.11 + t * 0.5).sin()
                + (dx.hypot(dy) * 0.14 - t).sin();
            // v ∈ [-4, 4] → [0, n] via threshold count (avoids a float→int cast).
            let level = (v + 4.0) / 8.0 * n as f64;
            let idx = (1..n).filter(|&i| level >= i as f64).count();
            s.push(ART_RAMP.get(idx).copied().unwrap_or(' '));
        }
        s.push(' ');
        lines.push(Line::from(Span::styled(s, style)));
    }
    lines
}

/// Which body the TUI viewport is showing. The default view is the live
/// slow-target / lock-wait breakdown; the list views each mirror one header
/// counter — [`ViewMode::Done`] the completed count, [`ViewMode::Matched`] the
/// matched total, [`ViewMode::Cached`] the cached count, [`ViewMode::Failed`]
/// the failed count. `Tab` cycles through `[Default]` plus one entry per
/// tab-bound header segment the header model exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The live breakdown: slow targets + lock waits (or idle art when empty).
    Default,
    /// The list of completed (successfully finished) targets, scoped by the
    /// active [`CountScope`] (matched set vs every observed target). Mirrors the
    /// `X` of the header's `X / Y done`.
    Done,
    /// The list of matched top-level targets. Mirrors the `Y` of the header's
    /// `X / Y done`.
    Matched,
    /// The list of targets that hit cache. Mirrors the header's cached count.
    Cached,
    /// The list of failed targets.
    Failed,
}

/// Case-insensitive substring test for the `/` filter on the `Done`/`Failed`
/// bodies. An empty filter matches everything (the unfiltered list).
///
/// Compares in place. Lower-casing both sides allocated two `String`s per row,
/// and the filter has to be tried against *every* row to know how many matched —
/// so at 100k targets that was 200k allocations on every frame.
fn addr_matches(addr: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    // ASCII-only folding, matching what the two `to_ascii_lowercase` calls did.
    // `windows` yields nothing when the filter is longer than the addr, and the
    // empty filter is handled above, so the width is always non-zero.
    addr.as_bytes()
        .windows(filter.len())
        .any(|w| w.eq_ignore_ascii_case(filter.as_bytes()))
}

/// Which target set the header counters are scoped to. `Matched` (the default)
/// counts only the matched top-level targets; `All` counts every target the view
/// has observed, including transitive deps. The `a` key toggles between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountScope {
    /// Counters scoped to the matched top-level target set.
    #[default]
    Matched,
    /// Counters across every target the view has seen (incl. transitive deps).
    All,
}

/// One freestanding segment of the header status line. The view joins items with
/// ` · ` itself — items must NOT bake in separators. A [`HeaderItem::Tab`] binds a
/// segment to a body [`ViewMode`]; the view highlights it (background colour)
/// while that mode is the active view.
pub enum HeaderItem {
    /// A plain segment that is always rendered as-is.
    Text(Vec<Span<'static>>),
    /// A segment bound to a body view. Selectable via `Tab`; highlighted while
    /// its `mode` is active.
    Tab {
        mode: ViewMode,
        spans: Vec<Span<'static>>,
    },
    /// A single header segment composed of several parts rendered with NO
    /// internal ` · ` separator (the parts bake in their own connective text,
    /// e.g. ` / ` and ` done`). Each part may bind to its own body view and is
    /// highlighted independently while that view is active — this is how the
    /// `X / Y done` segment highlights `X` (Done) and `Y` (Matched) separately.
    Split(Vec<HeaderPart>),
}

/// One part of a [`HeaderItem::Split`] segment: its spans plus an optional body
/// view. `Some(mode)` makes the part a tab — selectable via `Tab` and
/// highlighted while `mode` is the active view; `None` is inert connective text.
pub struct HeaderPart {
    mode: Option<ViewMode>,
    spans: Vec<Span<'static>>,
}

impl HeaderPart {
    /// An inert (non-selectable) text part.
    fn text(s: impl Into<String>) -> Self {
        HeaderPart {
            mode: None,
            spans: vec![Span::raw(s.into())],
        }
    }

    /// A text part bound to a body view.
    fn tab(mode: ViewMode, s: impl Into<String>) -> Self {
        HeaderPart {
            mode: Some(mode),
            spans: vec![Span::raw(s.into())],
        }
    }
}

impl HeaderItem {
    /// A plain-text segment.
    pub fn text(s: impl Into<String>) -> Self {
        HeaderItem::Text(vec![Span::raw(s.into())])
    }

    /// A plain-text segment bound to a body view.
    pub fn tab(mode: ViewMode, s: impl Into<String>) -> Self {
        HeaderItem::Tab {
            mode,
            spans: vec![Span::raw(s.into())],
        }
    }

    /// The item's spans, regardless of variant (used by tests).
    #[cfg(test)]
    fn spans(&self) -> Vec<Span<'static>> {
        match self {
            HeaderItem::Text(spans) | HeaderItem::Tab { spans, .. } => spans.clone(),
            HeaderItem::Split(parts) => {
                parts.iter().flat_map(|p| p.spans.iter().cloned()).collect()
            }
        }
    }
}

/// A target-scoped operation, as the client groups it for display. This is a
/// purely client-side (rendering) concept: the engine emits individually typed
/// events (`ExecuteStart/End`, `LocalCacheWriteStart/End`, …) and
/// [`event_op_boundary`] collapses those into this timeline. Add a variant here
/// (plus a mapping arm) when a new typed span event should appear in the
/// breakdown — e.g. remote read/write once their events land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Op {
    RemoteCacheRead,
    Execute,
    LocalCacheWrite,
    RemoteCacheWrite,
}

impl Op {
    /// Single glyph shown before the op's elapsed time. Must stay **BMP
    /// single-width** (plain-text rows; a double-width glyph would clip). Remote
    /// ops use `↓`/`↑` (U+2193/2191), never the emoji `⬇`/`⬆`.
    fn icon(self) -> char {
        match self {
            Op::RemoteCacheRead => '↓',
            Op::Execute => '▶',
            Op::LocalCacheWrite => '⊕',
            Op::RemoteCacheWrite => '↑',
        }
    }

    /// Pipeline ordinal for stable left-to-right ordering of the breakdown:
    /// remote download → execute → local-cache write → remote upload.
    fn order(self) -> u8 {
        match self {
            Op::RemoteCacheRead => 0,
            Op::Execute => 1,
            Op::LocalCacheWrite => 2,
            Op::RemoteCacheWrite => 3,
        }
    }
}

/// A slow target as surfaced by [`BuildState::long_running`]: its address, the
/// elapsed of its currently-active op, and the per-op breakdown `(op, elapsed_ms)`
/// ordered by [`Op::order`].
type SlowTarget = (String, u64, Vec<(Op, u64)>);

/// Which edge of an [`Op`] span an event represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Start,
    End,
}

/// The per-target operation timeline: how long each [`Op`] has taken (completed
/// runs summed) plus the currently-open op and its start timestamp. This is the
/// paved road — every op a target passes through folds in here, and the slow-row
/// renderer reads the breakdown straight off it.
#[derive(Debug, Default)]
struct OpTimeline {
    /// op → summed elapsed (ms) of its finished runs on this target.
    completed: HashMap<Op, u64>,
    /// the currently-open op and its server start timestamp, if any.
    active: Option<(Op, u64)>,
}

/// Map an individually-typed engine event to the op-timeline boundary it
/// represents, if any. This is where the well-defined engine events are collapsed
/// into the shared per-target timeline; a new op needs one arm here.
fn event_op_boundary(kind: &BuildEventKind) -> Option<(&str, Op, Boundary)> {
    match kind {
        BuildEventKind::ExecuteStart { addr, .. } => Some((addr, Op::Execute, Boundary::Start)),
        BuildEventKind::ExecuteEnd { addr, .. } => Some((addr, Op::Execute, Boundary::End)),
        BuildEventKind::LocalCacheWriteStart { addr } => {
            Some((addr, Op::LocalCacheWrite, Boundary::Start))
        }
        BuildEventKind::LocalCacheWriteEnd { addr, .. } => {
            Some((addr, Op::LocalCacheWrite, Boundary::End))
        }
        BuildEventKind::RemoteCacheWriteStart { addr } => {
            Some((addr, Op::RemoteCacheWrite, Boundary::Start))
        }
        BuildEventKind::RemoteCacheWriteEnd { addr, .. } => {
            Some((addr, Op::RemoteCacheWrite, Boundary::End))
        }
        BuildEventKind::RemoteCacheReadStart { addr } => {
            Some((addr, Op::RemoteCacheRead, Boundary::Start))
        }
        BuildEventKind::RemoteCacheReadEnd { addr, .. } => {
            Some((addr, Op::RemoteCacheRead, Boundary::End))
        }
        _ => None,
    }
}

/// Which cache answered for an addr — kept so a hit can be taken back off the
/// right counter when the target turns out to need building after all. See the
/// `ExecuteStart` arm of [`BuildState::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheHitKind {
    Local,
    Remote,
}

/// Folds the engine's build-progress event stream into renderable state.
#[derive(Debug, Default)]
pub struct BuildState {
    /// Addrs between `ResultStart` and `ResultEnd` — the "running" set.
    in_flight_results: HashSet<String>,
    /// addr → per-target operation timeline. Drives the worker braille (count of
    /// targets whose active op is `Execute`) and the slow-target breakdown rows.
    /// An entry persists only while its target can still be "slow": completed
    /// durations are kept alongside a later active op so the breakdown stays
    /// right, but the whole entry is dropped once the target's `ResultEnd`
    /// arrives (see the `ResultEnd` arm of [`BuildState::apply`]) — by then every
    /// op has closed and nothing will ever read it again, so keeping it would be
    /// a per-target allocation for the rest of the request at 100k-target scale.
    ops: HashMap<String, OpTimeline>,
    /// The addrs in `ops` whose timeline has an op open right now — the live
    /// subset the render path cares about.
    ///
    /// Walking all of `ops` to find the few with an open op cost ~2 ms in the
    /// worker braille and ~3 ms in the slow rows on *every* frame at 100k
    /// targets. This set is bounded by what is in flight instead. Maintained at
    /// the one place `OpTimeline::active` changes, so it cannot drift from it.
    open_ops: HashSet<String>,
    /// The matched top-level target set, accumulated as the matcher streams.
    matched: HashSet<String>,
    /// Whether any `Matched` event has been seen (gates display of the line).
    matched_seen: bool,
    /// Whether the matched set is final (matcher fully resolved). While false
    /// the total is provisional and rendered with a `~` prefix.
    matched_complete: bool,
    /// Every addr that reached `ResultEnd` (deduped). Matched progress is
    /// `matched ∩ finished` — order-independent, since `Matched` events can
    /// arrive after some matched results already finished.
    finished: HashSet<String>,
    /// `|matched ∩ finished|`, folded incrementally by [`BuildState::apply`].
    ///
    /// The header reads this on every frame, and rescanning `matched` there cost
    /// ~21-24 ms at 100k targets. Maintained at **both** edges of the
    /// intersection — a `Matched` event that names an already-finished addr, and
    /// a `ResultEnd` for an already-matched addr — because neither event is
    /// guaranteed to arrive first. Each edge fires only when its `HashSet::insert`
    /// reports the addr was new, so duplicate events cannot double-count.
    matched_finished: usize,
    /// `|matched ∩ cache_hit|`, folded on the same two-edge rule as
    /// [`BuildState::matched_finished`].
    matched_cached: usize,
    completed: usize,
    errored: usize,
    /// Failed targets in failure order, each with its error message (if the
    /// `ResultEnd` carried one). Drives the [`ViewMode::Failed`] body.
    failed: Vec<(String, Option<String>)>,
    /// Successfully completed targets in completion order (cache hits included —
    /// any `ResultEnd` with no error). Drives the [`ViewMode::Done`] body; the
    /// `Matched` scope filters this against `matched`.
    done: Vec<String>,
    /// Targets whose driver actually ran to success (`ExecuteEnd` with no error).
    /// Distinct from `completed`, which includes cache hits that never executed.
    executed: usize,
    local_hits: usize,
    local_misses: usize,
    remote_hits: usize,
    remote_misses: usize,
    /// Addrs that had a cache hit, and which kind. The header's "cached" count is
    /// `matched ∩ cache_hit` so it tracks matched targets only, not the transitive
    /// deps that also hit cache.
    ///
    /// The kind is kept because a hit can be **retracted**: the engine decides a
    /// hit from the revision's manifest, and a manifest can outlive its blobs (a
    /// local GC, an object-store lifecycle rule, or simply a revision whose blobs
    /// were never pulled on a run that is now offline). The engine then rebuilds
    /// the target, which arrives here as an `ExecuteStart` for an addr already
    /// counted as cached. Without the kind there is no way to know which counter
    /// to take it back out of.
    cache_hit: HashMap<String, CacheHitKind>,
    /// Worker capacity announced by the engine via `RequestConfig`. `None` until
    /// the event lands (no worker indicator rendered before then).
    max_workers: Option<usize>,
    /// Server timestamp of the first event seen — the run's start anchor for the
    /// header's elapsed clock. `None` until any event lands.
    started_at_ms: Option<u64>,
    /// Addrs blocked on the result lock past the notice threshold, mapped to the
    /// holder's pid (`None` if unknown). Added on `ResultLockWaitStart`, removed
    /// on `ResultLockWaitEnd`, so it reflects only currently-blocked waits.
    lock_waits: HashMap<String, Option<u32>>,
    /// Consumers blocked on a scratch slot, keyed by the *consumer* addr so a
    /// wait can be removed when its own `ScratchLockWaitEnd` lands.
    ///
    /// Rendered collapsed by scratch declaration — see [`scratch_wait_lines`].
    /// One `exclusive` cache shared by hundreds of targets produces that many
    /// simultaneous waiters, and that many identical rows is a flooded terminal,
    /// not a diagnostic.
    ///
    /// [`scratch_wait_lines`]: BuildState::scratch_wait_lines
    scratch_waits: HashMap<String, ScratchWait>,
    /// Finished scratch waits, aggregated per cache as `(waiters, total ms)`.
    ///
    /// Kept after the wait ends because the *total* is the number worth
    /// reporting: one target blocked for two seconds is noise, but 47 targets
    /// blocked for three minutes between them is the whole reason the build was
    /// slow, and by the time anyone reads the summary every wait has ended.
    scratch_wait_totals: HashMap<String, (u64, u64)>,
}

/// Every blocked wait on one cache, collapsed for a single row.
#[derive(Debug, Clone, Copy)]
struct ScratchWaitGroup<'a> {
    waiters: usize,
    /// The *oldest* wait on this cache — how long the contention has run, not
    /// how long the most recent arrival has been queued.
    since_ms: u64,
    holder_pid: Option<u32>,
    access: &'a str,
}

/// One consumer's blocked wait on a scratch slot.
#[derive(Debug, Clone)]
struct ScratchWait {
    /// The scratch declaration's addr — the subject of the rendered row, since
    /// the cache is what the user would change, not the target that wanted it.
    scratch: String,
    /// `"exclusive"` or `"shared"`, echoed from the declaration.
    access: String,
    /// A process holding the slot, when one could be named.
    holder_pid: Option<u32>,
    /// When this consumer started waiting, for the row's elapsed clock.
    since_ms: u64,
}

/// The header's count fields for one frame, as rendered strings.
///
/// Every consumer — the split header, the plain-text summary — goes through
/// [`BuildState::counts`]. The done count is kept split from its denominator
/// because the header binds each half to a different view tab, while the
/// plain-text summary wants them joined; carrying both shapes in one value is
/// what lets the two renderings share a single computation and stay in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    /// The finished count, bound to the [`ViewMode::Done`] tab.
    pub done: String,
    /// The denominator, bound to the [`ViewMode::Matched`] tab and `~`-prefixed
    /// while the matcher is still streaming. `None` before any `Matched` event,
    /// where the segment reads `{done} done` with no denominator (and `done` is
    /// the executed count, not a matched count).
    pub total: Option<String>,
    /// Cache hits in scope, e.g. `"3 cached"`.
    pub cached: String,
    /// Failures, e.g. `"0 failed"`.
    pub failed: String,
}

impl Counts {
    /// The done segment as one string: `"{done} / {total} done"`, or
    /// `"{done} done"` when there is no denominator.
    pub fn done_segment(&self) -> String {
        match &self.total {
            Some(total) => format!("{} / {total} done", self.done),
            None => format!("{} done", self.done),
        }
    }
}

impl BuildState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a single event. Reads `ev.at_unix_ms` + `ev.kind`; no local clock.
    ///
    /// Idempotent against the rare same-addr/different-`OutputMatcher`
    /// double-fire: `ResultStart` inserts only if absent, and a second
    /// `ResultEnd` for an already-removed addr is a no-op.
    pub fn apply(&mut self, ev: &BuildEvent) {
        // Anchor the elapsed clock to the earliest event observed.
        self.started_at_ms = Some(match self.started_at_ms {
            Some(t) => t.min(ev.at_unix_ms),
            None => ev.at_unix_ms,
        });
        // Generic op timeline: any event that is an op boundary folds here,
        // independent of the counter side effects handled in the match below.
        if let Some((addr, op, boundary)) = event_op_boundary(&ev.kind) {
            let tl = self.ops.entry(addr.to_string()).or_default();
            match boundary {
                Boundary::Start => {
                    // Overlap guard: if an op is somehow still open (a missing or
                    // reordered end), fold it into completed before opening the new
                    // one so durations stay bounded and `active` reflects reality.
                    if let Some((prev_op, prev_start)) = tl.active.take() {
                        *tl.completed.entry(prev_op).or_insert(0) +=
                            ev.at_unix_ms.saturating_sub(prev_start);
                    }
                    tl.active = Some((op, ev.at_unix_ms));
                }
                Boundary::End => {
                    // Ignore a mismatched/duplicate close: only the currently-open
                    // op can end.
                    if let Some((active_op, start)) = tl.active
                        && active_op == op
                    {
                        *tl.completed.entry(op).or_insert(0) += ev.at_unix_ms.saturating_sub(start);
                        tl.active = None;
                    }
                }
            }
            // Mirror the transition into the live index the frame path walks. Kept
            // here, at the sole writer of `active`, so the two cannot disagree.
            let open = tl.active.is_some();
            if open {
                if !self.open_ops.contains(addr) {
                    self.open_ops.insert(addr.to_string());
                }
            } else {
                self.open_ops.remove(addr);
                // Retention tail: `RemoteCacheWrite` is pushed from a detached
                // background task (`Engine::spawn_remote_upload`), which the
                // engine deliberately does not await inside the `ResultEnd` scope
                // (see its doc comment in `remote_cache.rs`) — so its Start/End
                // can arrive for an addr *after* that addr's `ResultEnd` already
                // fired and removed the entry below. When that happens this Start
                // recreates the entry (`entry().or_default()` above); nothing
                // would ever remove it again unless caught here. Whichever
                // closing edge is actually last — `ResultEnd` or this trailing
                // End — is the one that reclaims it.
                if self.finished.contains(addr) {
                    self.ops.remove(addr);
                }
            }
        }
        match &ev.kind {
            BuildEventKind::RequestConfig {
                max_workers: count, ..
            } => {
                self.max_workers = Some(*count);
            }
            BuildEventKind::Matched { addrs, complete } => {
                self.matched_seen = true;
                // The loop below replaced an `extend`, which reserved off the
                // iterator's size hint; keep that so a wide `Matched` batch does
                // not rehash its way up.
                self.matched.reserve(addrs.len());
                for addr in addrs {
                    // The matched-set edge of both intersections: an addr can be
                    // named here *after* it finished or hit cache, so fold those
                    // in now. `insert` guards against a repeated addr.
                    if self.matched.insert(addr.clone()) {
                        if self.finished.contains(addr) {
                            self.matched_finished += 1;
                        }
                        if self.cache_hit.contains_key(addr) {
                            self.matched_cached += 1;
                        }
                    }
                }
                if *complete {
                    self.matched_complete = true;
                }
            }
            BuildEventKind::ResultStart { addr } => {
                self.in_flight_results.insert(addr.clone());
            }
            BuildEventKind::ResultEnd { addr, error, .. } => {
                if self.in_flight_results.remove(addr) {
                    if let Some(err) = error {
                        self.errored += 1;
                        self.failed.push((addr.clone(), Some(err.clone())));
                    } else {
                        self.completed += 1;
                        self.done.push(addr.clone());
                    }
                }
                // Record every terminal addr. This is the result edge of
                // `matched ∩ finished`; the `Matched` arm covers the other one,
                // so the count is right whichever event arrives first.
                if self.finished.insert(addr.clone()) && self.matched.contains(addr) {
                    self.matched_finished += 1;
                }
                // Retention: `ResultEnd` is emitted by the drop guard wrapping the
                // whole `inner_result_addr` scope, so by the time it fires,
                // Execute/LocalCacheWrite/RemoteCacheRead have all closed for this
                // addr (they run awaited inside that scope) and neither reader
                // walks a closed entry — keeping it around would be unbounded
                // per-target memory for the rest of the run. `open_ops.remove` is
                // normally a no-op here (the entry left it already) but guards
                // against a dangling index entry if `active` were somehow still
                // open at this point.
                //
                // `RemoteCacheWrite` is the one op this does NOT catch:
                // `Engine::spawn_remote_upload` pushes it from a detached
                // background task the engine deliberately does not await inside
                // this scope, so its Start/End can arrive after this removal has
                // already run. The `Boundary::End` arm above closes that tail —
                // it checks `self.finished` and reclaims the (re-created) entry
                // itself once that trailing op closes, so whichever event is
                // actually last is the one that frees it.
                self.ops.remove(addr);
                self.open_ops.remove(addr);
            }
            // The op timeline (folded above) tracks Execute's duration; here we
            // keep only the `executed` counter side effect on a successful end.
            //
            // A target that executes is not a cached target — even if it was
            // announced as a hit first. The engine decides a hit from the
            // revision's manifest, which can outlive its blobs, so the bytes may
            // turn out to be unreadable (GC'd, expired on a shared cache, or
            // simply never pulled and the remote is now unreachable) and the
            // target is rebuilt. Retract the hit here rather than counting the
            // same addr as both cached and executed, which would let "executed + cached"
            // exceed the number of targets — and, on an offline run over a
            // remote-mirrored cache, report a full rebuild as "N cached".
            BuildEventKind::ExecuteStart { addr, .. } => self.retract_cache_hit(addr),
            BuildEventKind::ExecuteEnd { error, .. } => {
                if error.is_none() {
                    self.executed += 1;
                }
            }
            BuildEventKind::LocalCacheHit { addr } => {
                self.local_hits += 1;
                self.note_cache_hit(addr, CacheHitKind::Local);
            }
            BuildEventKind::LocalCacheMiss { .. } => self.local_misses += 1,
            BuildEventKind::RemoteCacheHit { addr } => {
                self.remote_hits += 1;
                self.note_cache_hit(addr, CacheHitKind::Remote);
            }
            BuildEventKind::RemoteCacheMiss { .. } => self.remote_misses += 1,
            BuildEventKind::ResultLockWaitStart { addr, holder_pid } => {
                self.lock_waits.insert(addr.clone(), *holder_pid);
            }
            BuildEventKind::ResultLockWaitEnd { addr } => {
                self.lock_waits.remove(addr);
            }
            BuildEventKind::ScratchLockWaitStart {
                addr,
                scratch,
                access,
                holder_pid,
            } => {
                self.scratch_waits.insert(
                    addr.clone(),
                    ScratchWait {
                        scratch: scratch.clone(),
                        access: access.clone(),
                        holder_pid: *holder_pid,
                        since_ms: ev.at_unix_ms,
                    },
                );
            }
            BuildEventKind::ScratchLockWaitEnd { addr, .. } => {
                if let Some(w) = self.scratch_waits.remove(addr) {
                    let e = self.scratch_wait_totals.entry(w.scratch).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += ev.at_unix_ms.saturating_sub(w.since_ms);
                }
            }
            // Read/Write markers are not aggregated into counters. The local
            // cache-write span is folded into the op timeline above, so the
            // counter match ignores it here.
            BuildEventKind::RemoteCacheReadStart { .. }
            | BuildEventKind::RemoteCacheReadEnd { .. }
            | BuildEventKind::RemoteCacheWriteStart { .. }
            | BuildEventKind::RemoteCacheWriteEnd { .. }
            | BuildEventKind::LocalCacheWriteStart { .. }
            | BuildEventKind::LocalCacheWriteEnd { .. } => {}
            // GC progress is tracked by GcHeader, not the build counters. The
            // elapsed-clock anchor at the top of `apply` still runs, so the
            // clock works during a gc sweep.
            BuildEventKind::GcTargetSwept { .. } => {}
            // An event kind newer than this build knows about. Skipped rather
            // than fatal — see `BuildEventKind::Unknown`.
            BuildEventKind::Unknown => {}
        }
    }

    /// Record a cache hit (local or remote). This is the cache edge of
    /// `matched ∩ cache_hit`; the `Matched` arm of [`BuildState::apply`] covers
    /// the other one, so the count is right whichever event arrives first. A
    /// repeat hit on an addr already in the set does not re-count.
    ///
    /// `kind` is remembered so [`retract_cache_hit`](Self::retract_cache_hit) can
    /// undo this exact bookkeeping if the hit turns out not to stand.
    fn note_cache_hit(&mut self, addr: &str, kind: CacheHitKind) {
        if self.cache_hit.insert(addr.to_string(), kind).is_none() && self.matched.contains(addr) {
            self.matched_cached += 1;
        }
    }

    /// Take a cache hit back off every counter [`note_cache_hit`](Self::note_cache_hit)
    /// put it on, because the target is about to build after all.
    ///
    /// The engine decides a hit from the revision's manifest, which can outlive
    /// its blobs — a local GC, an expiry rule on a shared cache, or blobs that
    /// were never pulled and a remote that is now unreachable. It announces the
    /// hit, discovers the bytes are unreadable, and rebuilds. Dropping the addr
    /// from `cache_hit` alone is not enough: `matched_cached` is folded, not
    /// rescanned, so it has to be decremented in the same breath or the header
    /// keeps reporting a target as cached while it builds.
    ///
    /// A no-op for an addr that never hit — the ordinary cache miss, which is
    /// most `ExecuteStart`s.
    fn retract_cache_hit(&mut self, addr: &str) {
        let Some(kind) = self.cache_hit.remove(addr) else {
            return;
        };
        if self.matched.contains(addr) {
            self.matched_cached = self.matched_cached.saturating_sub(1);
        }
        match kind {
            CacheHitKind::Local => {
                self.local_hits = self.local_hits.saturating_sub(1);
                self.local_misses += 1;
            }
            CacheHitKind::Remote => {
                self.remote_hits = self.remote_hits.saturating_sub(1);
                self.remote_misses += 1;
            }
        }
    }

    /// Targets whose *currently-active* op has run longer than `threshold_ms`,
    /// with the per-op breakdown for that target. Returns
    /// `(addr, active_elapsed_ms, breakdown)` sorted by active elapsed descending
    /// (then addr). The breakdown is the target's completed ops plus the live
    /// active op, each at least one second, ordered by [`Op::order`]. A target
    /// with no active op is never slow (it dropped off when its last op ended).
    fn long_running(&self, now_ms: u64, threshold_ms: u64) -> Vec<SlowTarget> {
        // Walks only the targets with an op open, not every target the run has
        // touched — see [`BuildState::open_ops`]. This runs on every frame of the
        // live view.
        let mut out: Vec<SlowTarget> = self
            .open_ops
            .iter()
            .filter_map(|addr| {
                let tl = self.ops.get(addr)?;
                let (active_op, active_start) = tl.active?;
                let active_elapsed = now_ms.saturating_sub(active_start);
                if active_elapsed <= threshold_ms {
                    return None;
                }
                // Merge completed durations with the live active op, drop sub-second
                // ops, and order the breakdown by pipeline position.
                let mut durs = tl.completed.clone();
                *durs.entry(active_op).or_insert(0) += active_elapsed;
                let mut breakdown: Vec<(Op, u64)> =
                    durs.into_iter().filter(|&(_, ms)| ms >= 1000).collect();
                breakdown.sort_by_key(|&(op, _)| op.order());
                Some((addr.clone(), active_elapsed, breakdown))
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Number of matched top-level targets seen so far (provisional until the
    /// matcher fully resolves). `None` if no `Matched` event has arrived.
    pub fn matched_total(&self) -> Option<usize> {
        self.matched_seen.then_some(self.matched.len())
    }

    /// Matched-target progress as `(done, total, complete)`: how many matched
    /// top-level targets have finished, the total matched so far, and whether the
    /// matcher fully resolved (`false` ⇒ the total is provisional). `None` until
    /// a `Matched` event arrives (e.g. a single-target `result_addr` entry).
    ///
    /// `O(1)` — the intersection is folded in [`BuildState::apply`], not scanned
    /// here. This runs on every frame.
    pub fn matched_progress(&self) -> Option<(usize, usize, bool)> {
        if !self.matched_seen {
            return None;
        }
        // A subset-count that has drifted above the set it counts into is the
        // observable symptom of a missed fold edge. Free in release; turns a
        // desync into a failure across the whole suite, not just one test.
        debug_assert!(
            self.matched_finished <= self.matched.len(),
            "matched_finished {} exceeds matched {}",
            self.matched_finished,
            self.matched.len(),
        );
        Some((
            self.matched_finished,
            self.matched.len(),
            self.matched_complete,
        ))
    }

    /// The count fields for one frame — see [`Counts`]. `O(1)`: every field is
    /// either a `len()` or a counter folded in [`BuildState::apply`], so this is
    /// four reads and the `format!`s, with no walk of any target set.
    pub fn counts(&self, scope: CountScope) -> Counts {
        let (done, total) = match scope {
            // All-targets scope: every observed target (`finished ∪ in_flight`),
            // with the finished count over that running total. No `~` — the total
            // grows as deps stream rather than resolving to a fixed matched set.
            CountScope::All => {
                let done = self.finished.len();
                let total = done + self.in_flight_results.len();
                (done.to_string(), Some(total.to_string()))
            }
            CountScope::Matched => match self.matched_progress() {
                Some((done, total, complete)) => {
                    let tilde = if complete { "" } else { "~" };
                    (done.to_string(), Some(format!("{tilde}{total}")))
                }
                None => (self.executed.to_string(), None),
            },
        };
        Counts {
            done,
            total,
            cached: format!("{} cached", self.cached_count(scope)),
            failed: format!("{} failed", self.errored),
        }
    }

    /// The three freestanding count fields — `(done, cached, failed)` — each
    /// without separators. The plain-text rendering of [`BuildState::counts`],
    /// used by [`BuildState::counts_segment`] for the final summary; the live
    /// header takes [`Counts`] directly so it can split the done segment into
    /// per-view tabs.
    pub fn count_fields(&self, scope: CountScope) -> (String, String, String) {
        let counts = self.counts(scope);
        (counts.done_segment(), counts.cached, counts.failed)
    }

    /// The textual count segment shared by the live header and the final
    /// summary: `D / ~N done · C cached · F failed`. No elapsed clock, no worker
    /// braille — callers prepend the elapsed field themselves.
    pub fn counts_segment(&self, scope: CountScope) -> String {
        let (done, cached, failed) = self.count_fields(scope);
        format!("{done} · {cached} · {failed}")
    }

    /// Body rows for the [`ViewMode::Failed`] view: one row per failed target,
    /// addr in red followed by its error message (when one was reported), in
    /// failure order. Empty when nothing has failed.
    fn failed_rows(&self, filter: &str) -> BodyRows<'_> {
        let mut rows = row_buffer(self.failed.len(), filter);
        rows.extend(
            self.failed
                .iter()
                .filter(|(addr, _)| addr_matches(addr, filter))
                .map(|(addr, err)| BodyRow {
                    addr,
                    color: Color::Red,
                    // Keep the message on one line; the viewport clips overflow.
                    detail: err.as_deref().map(|e| e.lines().next().unwrap_or(e)),
                }),
        );
        BodyRows::in_order(rows)
    }

    /// Body rows for the [`ViewMode::Done`] view: one row per completed target,
    /// addr in green, in completion order. Scoped by `scope`: `Matched` keeps only
    /// addrs in the matched top-level set, `All` lists every completed target
    /// (transitive deps included). Empty when nothing has finished.
    fn done_rows(&self, scope: CountScope, filter: &str) -> BodyRows<'_> {
        let mut rows = row_buffer(self.done.len(), filter);
        rows.extend(
            self.done
                .iter()
                .filter(|addr| match scope {
                    CountScope::All => true,
                    CountScope::Matched => self.matched.contains(*addr),
                })
                .filter(|addr| addr_matches(addr, filter))
                .map(|addr| BodyRow {
                    addr,
                    color: Color::Green,
                    detail: None,
                }),
        );
        BodyRows::in_order(rows)
    }

    /// Body rows for the [`ViewMode::Matched`] view: every matched top-level
    /// target, ordered by addr for a stable list (the matched set is a
    /// `HashSet`). Finished targets render green, still-pending ones
    /// default-white. Empty before any `Matched` event arrives.
    fn matched_rows(&self, filter: &str) -> BodyRows<'_> {
        let mut rows = row_buffer(self.matched.len(), filter);
        rows.extend(
            self.matched
                .iter()
                .filter(|a| addr_matches(a, filter))
                .map(|addr| BodyRow {
                    addr,
                    color: if self.finished.contains(addr) {
                        Color::Green
                    } else {
                        Color::White
                    },
                    detail: None,
                }),
        );
        BodyRows::by_addr(rows)
    }

    /// Body rows for the [`ViewMode::Cached`] view: every target that hit cache
    /// (local or remote), scoped like the header's cached count — `Matched` keeps
    /// only matched top-level targets (all cache hits before any `Matched` event),
    /// `All` lists every cached target. Ordered by addr for a stable list (cache
    /// hits are keyed by addr). Empty when nothing has hit cache.
    fn cached_rows(&self, scope: CountScope, filter: &str) -> BodyRows<'_> {
        let mut rows = row_buffer(self.cache_hit.len(), filter);
        rows.extend(
            self.cache_hit
                .keys()
                .filter(|a| match scope {
                    CountScope::All => true,
                    CountScope::Matched if self.matched_seen => self.matched.contains(*a),
                    CountScope::Matched => true,
                })
                .filter(|a| addr_matches(a, filter))
                .map(|addr| BodyRow {
                    addr,
                    color: Color::Cyan,
                    detail: None,
                }),
        );
        BodyRows::by_addr(rows)
    }

    /// The header's cached count: matched targets that hit cache
    /// (`matched ∩ cache_hit`), falling back to all cache hits before any
    /// `Matched` event arrives, or to every cached addr under
    /// [`CountScope::All`].
    ///
    /// `O(1)` — the intersection is folded in [`BuildState::apply`], not scanned
    /// here. This runs on every frame.
    pub fn cached_count(&self, scope: CountScope) -> usize {
        match scope {
            // All-targets scope: every addr that hit cache (deduped), deps included.
            CountScope::All => self.cache_hit.len(),
            CountScope::Matched if self.matched_seen => {
                debug_assert!(
                    self.matched_cached <= self.matched.len(),
                    "matched_cached {} exceeds matched {}",
                    self.matched_cached,
                    self.matched.len(),
                );
                self.matched_cached
            }
            CountScope::Matched => self.local_hits + self.remote_hits,
        }
    }

    /// Workers currently holding an execute slot (targets whose active op is
    /// `Execute`). This is the semaphore-bound busy count, not `running` (which
    /// includes targets blocked on deps without a permit).
    ///
    /// Bounded by what is in flight, not by the run: it walks [`Self::open_ops`],
    /// not every target that has ever run an op. The header calls this on every
    /// frame in every view.
    pub fn busy_workers(&self) -> usize {
        // A stale entry here would be silently skipped by this filter and by
        // `long_running`'s `?`, under-reporting the worker count rather than
        // failing. `O(in-flight)`, and free in release.
        debug_assert!(
            self.open_ops
                .iter()
                .all(|addr| self.ops.get(addr).is_some_and(|tl| tl.active.is_some())),
            "open_ops holds an addr whose timeline has no op open",
        );
        self.open_ops
            .iter()
            .filter(|addr| {
                self.ops
                    .get(*addr)
                    .is_some_and(|tl| matches!(tl.active, Some((Op::Execute, _))))
            })
            .count()
    }

    /// Announced worker capacity, or `None` before the `RequestConfig` event.
    pub fn max_workers(&self) -> Option<usize> {
        self.max_workers
    }

    /// Milliseconds since the first observed event, given the caller's wall
    /// clock. `0` before any event has been seen.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.started_at_ms
            .map(|start| now_ms.saturating_sub(start))
            .unwrap_or(0)
    }

    /// Whether any build activity was observed. Used to suppress an all-zero
    /// final summary for commands that emit no build events (inspect/query).
    pub fn has_activity(&self) -> bool {
        self.matched_seen
            || self.completed > 0
            || self.errored > 0
            || !self.in_flight_results.is_empty()
            || self.local_hits > 0
            || self.local_misses > 0
            || self.remote_hits > 0
            || self.remote_misses > 0
    }

    /// One-line summary. Prefixed with matched progress once a `Matched` event
    /// has arrived: `matched D / N` (or `D / ~N` while the matcher is still
    /// streaming), then `done M, err K, running R, cache H hit / M miss`.
    pub fn summary(&self) -> String {
        let hits = self.local_hits + self.remote_hits;
        let misses = self.local_misses + self.remote_misses;
        let matched = match self.matched_progress() {
            // `~` marks a provisional total while the matcher is still streaming.
            Some((done, total, complete)) => {
                let tilde = if complete { "" } else { "~" };
                format!("matched {done} / {tilde}{total}, ")
            }
            None => String::new(),
        };
        format!(
            "{matched}done {}, err {}, running {}, cache {} hit / {} miss",
            self.completed,
            self.errored,
            self.in_flight_results.len(),
            hits,
            misses,
        )
    }

    /// The long-running ("slow") target rows, one per slow target and uncollapsed:
    /// `addr (icon Ns)…` — one `(icon Ns)` group per op the target passed through
    /// or is in.
    fn slow_rows(&self, now_ms: u64) -> Vec<Line<'static>> {
        self.long_running(now_ms, LONG_RUNNING_THRESHOLD_MS)
            .into_iter()
            .map(|(addr, _active_elapsed, breakdown)| {
                let groups: String = breakdown
                    .iter()
                    .map(|(op, ms)| format!(" ({} {}s)", op.icon(), ms / 1000))
                    .collect();
                Line::from(format!("  {addr}{groups}"))
            })
            .collect()
    }

    /// The full body: wait notices first (they take priority), then every
    /// slow-target row. Uncollapsed — the caller windows it to the available rows.
    pub fn body_lines(&self, now_ms: u64) -> Vec<Line<'static>> {
        let mut body = self.lock_wait_lines();
        body.extend(self.scratch_wait_lines(now_ms));
        body.extend(self.slow_rows(now_ms));
        body
    }

    /// Every cache that serialized somebody this run, as
    /// `(scratch, waiters, total wait ms)`, ordered by total wait descending so
    /// the worst offender is first. Empty when nothing ever blocked.
    pub fn scratch_wait_totals(&self) -> Vec<(&str, u64, u64)> {
        let mut v: Vec<(&str, u64, u64)> = self
            .scratch_wait_totals
            .iter()
            .map(|(k, (n, ms))| (k.as_str(), *n, *ms))
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// Rows for scratch slots that are currently blocking somebody, one row per
    /// *cache* rather than per waiting target:
    /// `⧗ //build:gocache — 47 waiting (exclusive, 1m12s)`.
    ///
    /// Deliberately not shaped like the result-lock rows above, because the two
    /// name different problems. A blocked result lock means another process is
    /// building this exact target: wait, or kill it. A blocked scratch slot
    /// means the build is serialized on a shared cache — a declaration-level
    /// choice (`access = "exclusive"`) with a declaration-level fix. Rendering
    /// them identically would send a user looking for a rogue process when what
    /// they need to change is a line in a BUILD file.
    ///
    /// The subject is the scratch addr, and the count is the point: "47 waiting"
    /// is what tells someone their cache is the bottleneck. Sorted by addr so
    /// the order is stable across frames.
    pub fn scratch_wait_lines(&self, now_ms: u64) -> Vec<Line<'static>> {
        if self.scratch_waits.is_empty() {
            return Vec::new();
        }
        // Collapse by cache: waiters, the oldest wait, and any named holder.
        let mut by_scratch: HashMap<&str, ScratchWaitGroup<'_>> = HashMap::new();
        for w in self.scratch_waits.values() {
            let e = by_scratch
                .entry(w.scratch.as_str())
                .or_insert(ScratchWaitGroup {
                    waiters: 0,
                    since_ms: w.since_ms,
                    holder_pid: w.holder_pid,
                    access: w.access.as_str(),
                });
            e.waiters += 1;
            e.since_ms = e.since_ms.min(w.since_ms);
            // Any waiter that managed to name a holder speaks for the rest.
            e.holder_pid = e.holder_pid.or(w.holder_pid);
        }

        let mut rows: Vec<(&str, ScratchWaitGroup<'_>)> = by_scratch.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        rows.into_iter()
            .map(|(scratch, g)| {
                let waited = human_elapsed(now_ms.saturating_sub(g.since_ms));
                // A named holder outranks the access mode: "another process has
                // it" and "you declared this exclusive" are different fixes, and
                // only the first can be pinned on a pid.
                let detail = match g.holder_pid {
                    Some(pid) => format!("held by pid {pid}, {waited}"),
                    None => format!("{}, {waited}", g.access),
                };
                Line::from(Span::styled(
                    format!("  ⧗ {scratch} — {} waiting ({detail})", g.waiters),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect()
    }

    /// Rows for addrs currently blocked on the result lock past the notice
    /// threshold, rendered like the slow-target rows but flagged locked:
    /// `🔒 <addr> (locked by pid N)`, or `(locked, holder unknown)` when the pid
    /// could not be determined. Sorted by addr so the order is stable across
    /// frames. Empty when nothing is blocked.
    pub fn lock_wait_lines(&self) -> Vec<Line<'static>> {
        let mut waits: Vec<(&String, &Option<u32>)> = self.lock_waits.iter().collect();
        waits.sort_by(|a, b| a.0.cmp(b.0));
        waits
            .into_iter()
            .map(|(addr, pid)| {
                let holder = match pid {
                    Some(pid) => format!("locked by pid {pid}"),
                    None => "locked, holder unknown".to_string(),
                };
                Line::from(Span::styled(
                    format!("  🔒 {addr} ({holder})"),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect()
    }
}

/// Shared aggregation backing both paved-road views: a label plus the folded
/// [`BuildState`] and the list of errored targets. Both `TuiProgressView` and
/// `CiProgressView` wrap one of these so the fold logic lives in one place.
struct ProgressCore {
    label: String,
    state: BuildState,
}

impl ProgressCore {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: BuildState::new(),
        }
    }

    /// Fold one event into the aggregate counters. Per-target failures are not
    /// collected here — they're rendered richly from the request's failure
    /// registry (see `commands::errors::render_failures`).
    fn fold(&mut self, ev: &BuildEvent) {
        self.state.apply(ev);
    }
}

pub use hcore::units::human_bytes;

/// Per-command header content for [`TuiProgressView`]. The view owns the box
/// chrome, the elapsed clock, and the body (slow rows / lock waits / idle art);
/// the header supplies only the status segment after the clock (and the bottom
/// label + final summary). `run`/`query`/`inspect` use [`BuildHeader`]; `gc`
/// uses [`GcHeader`].
pub trait ProgressHeader: Send {
    /// Fold an event into header-private state. The build header reads the
    /// view's shared [`BuildState`] instead, so its impl is a no-op.
    fn apply(&mut self, _ev: &BuildEvent) {}
    /// Status items shown after the elapsed clock. Each item is freestanding —
    /// the view joins them with ` · ` and highlights any [`HeaderItem::Tab`]
    /// whose mode is the active view — so a model must NOT bake in separators.
    /// `core` is the view's shared build state; headers that track their own
    /// state ignore it. `scope` selects whether the counts cover the matched set
    /// or every observed target (toggled by the `a` key); state-private headers
    /// ignore it.
    fn header(&self, core: &BuildState, scope: CountScope) -> Vec<HeaderItem>;
    /// The bottom-border label.
    fn label(&self) -> String;
    /// Final summary segment printed after the run (after the elapsed clock).
    /// Empty ⇒ nothing is printed. `scope` is the count scope the view was left
    /// on (the `a` toggle), so the printed summary matches the header the user
    /// was reading; state-private headers ignore it.
    fn last_render(&self, core: &BuildState, scope: CountScope) -> String;
}

/// Build/query/inspect header: the elapsed-clock counts segment plus worker
/// braille, all read from the shared [`BuildState`].
pub struct BuildHeader {
    label: String,
}

impl BuildHeader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl ProgressHeader for BuildHeader {
    fn header(&self, core: &BuildState, scope: CountScope) -> Vec<HeaderItem> {
        // One `counts` call per frame, and it is `O(1)` — the matched
        // intersections behind `done` and `cached` are counters folded in
        // `BuildState::apply`, not scans. Keep the whole header on this one call
        // so the fields cannot disagree about the frame they describe.
        let Counts {
            done,
            total,
            cached,
            failed,
        } = core.counts(scope);
        // The done segment `X / Y done` splits into two independent tabs: `X`
        // (the finished count → Done view) and `Y` (the matched total → Matched
        // view). The connective ` / ` and trailing ` done` are inert text. Before
        // any `Matched` event there is no denominator, so it reads `{executed} done`
        // with only the Done tab.
        let mut done_parts = vec![HeaderPart::tab(ViewMode::Done, done)];
        match total {
            Some(total) => {
                done_parts.push(HeaderPart::text(" / "));
                done_parts.push(HeaderPart::tab(ViewMode::Matched, total));
                done_parts.push(HeaderPart::text(" done"));
            }
            None => done_parts.push(HeaderPart::text(" done")),
        }
        let mut items = vec![
            HeaderItem::Split(done_parts),
            // The cached count is a tab into the cached-targets view.
            HeaderItem::tab(ViewMode::Cached, cached),
            // The failed count is a tab into the failed-targets view.
            HeaderItem::tab(ViewMode::Failed, failed),
        ];
        let workers = worker_spans(core.max_workers().unwrap_or(0), core.busy_workers());
        if !workers.is_empty() {
            items.push(HeaderItem::Text(workers));
        }
        items
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn last_render(&self, core: &BuildState, scope: CountScope) -> String {
        if !core.has_activity() {
            return String::new();
        }
        // Report the scope the view was left on: switching to `All` mid-run
        // carries through to the printed exit summary.
        match scope {
            CountScope::Matched => core.counts_segment(scope),
            // The printed line outlives the header (scrollback, copy-paste),
            // where nothing else says which population the counts cover — so
            // the non-default scope labels itself. Matched output stays
            // byte-identical to what it always was.
            CountScope::All => format!("{} · all targets", core.counts_segment(scope)),
        }
    }
}

/// GC sweep header: targets explored, revisions dropped, and bytes freed,
/// folded from [`BuildEventKind::GcTargetSwept`].
pub struct GcHeader {
    label: String,
    targets: usize,
    revisions_removed: usize,
    bytes_removed: u64,
}

impl GcHeader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            targets: 0,
            revisions_removed: 0,
            bytes_removed: 0,
        }
    }

    fn segment(&self) -> String {
        format!(
            "{} targets · {} freed",
            self.targets,
            human_bytes(self.bytes_removed),
        )
    }
}

impl ProgressHeader for GcHeader {
    fn apply(&mut self, ev: &BuildEvent) {
        if let BuildEventKind::GcTargetSwept {
            revisions_removed,
            bytes_removed,
        } = &ev.kind
        {
            self.targets += 1;
            self.revisions_removed += revisions_removed;
            self.bytes_removed = self.bytes_removed.saturating_add(*bytes_removed);
        }
    }

    fn header(&self, _core: &BuildState, _scope: CountScope) -> Vec<HeaderItem> {
        vec![HeaderItem::text(self.segment())]
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn last_render(&self, _core: &BuildState, _scope: CountScope) -> String {
        if self.targets > 0 {
            self.segment()
        } else {
            String::new()
        }
    }
}

/// The paved-road [`TUIAppView`]: an elapsed-clock header (its status segment
/// supplied by a [`ProgressHeader`]) over a shared [`BuildState`] that drives the
/// long-running/lock-wait body, plus a persistent final summary.
pub struct TuiProgressView {
    /// Shared build state: elapsed-clock anchor + body rows. Folds every event.
    state: BuildState,
    /// Per-command header content (build counts vs gc sweep stats).
    model: Box<dyn ProgressHeader>,
    /// Body scroll offset (rows from the top of the combined body list). Held in
    /// a `Cell` so `render` can clamp it against the live row count while staying
    /// `&self`; `scroll()` mutates it from key events.
    scroll: Cell<usize>,
    /// Body horizontal pan offset in columns, for lines wider than the viewport.
    /// Clamped in `render` against the widest body line; `hscroll()` mutates it.
    hscroll: Cell<usize>,
    /// The active body view. `Tab` cycles it through [`ViewMode::Default`] plus
    /// every tab the header model exposes.
    view: Cell<ViewMode>,
    /// Which target set the header counters cover. `a` toggles matched ⇄ all.
    /// Held in a `Cell` so `render`/`header_item_spans` read it through `&self`.
    scope: Cell<CountScope>,
    /// Server-wall timestamp captured when the run finished and the viewport was
    /// held open (user navigated off the main view). `Some` drives the green
    /// "press q to quit" notice and freezes the elapsed clock at this instant.
    finished_at_ms: Option<u64>,
    /// Whether the `/` filter is capturing keystrokes. While `true` printable keys
    /// edit [`Self::search_query`] instead of triggering the normal shortcuts.
    /// Only meaningful on the `Done`/`Failed` tabs.
    search_active: Cell<bool>,
    /// The active filter for the `Done`/`Failed` bodies: a case-insensitive
    /// substring matched against the target addr. Empty means no filtering.
    /// Persists after `Enter` confirms so the filtered list stays scrollable.
    search_query: RefCell<String>,
    /// Pending approval prompts shared with the engine's approval handler. `None`
    /// for commands without an approval gate. When a prompt is active it is
    /// rendered at the top of the live body and `y`/`n`/Enter resolve it.
    approval: Option<crate::tui::approval::ApprovalCenter>,
}

impl TuiProgressView {
    /// Build-counts header (run/query/inspect). The label rides the bottom border.
    pub fn new(label: impl Into<String>) -> Self {
        Self::with_header(Box::new(BuildHeader::new(label)))
    }

    /// A view with a custom header model (e.g. [`GcHeader`] for `heph gc`).
    pub fn with_header(model: Box<dyn ProgressHeader>) -> Self {
        Self {
            state: BuildState::new(),
            model,
            scroll: Cell::new(0),
            hscroll: Cell::new(0),
            view: Cell::new(ViewMode::Default),
            scope: Cell::new(CountScope::Matched),
            finished_at_ms: None,
            search_active: Cell::new(false),
            search_query: RefCell::new(String::new()),
            approval: None,
        }
    }

    /// The final summary segment, scoped to whatever the `a` toggle was left on
    /// — a user who switched the header to `All` reads the same scope after
    /// teardown. Split out from [`TUIAppView::last_render`], which writes it
    /// straight to stderr and so cannot be asserted on.
    fn final_summary_segment(&self) -> String {
        self.model.last_render(&self.state, self.scope.get())
    }

    /// Attach the shared approval queue so this view renders pending prompts and
    /// resolves them from key events. Used by commands that gate execution.
    pub fn with_approval(mut self, center: crate::tui::approval::ApprovalCenter) -> Self {
        self.approval = Some(center);
        self
    }

    /// Body rows for the active approval prompt, or empty when idle. The banner
    /// carries the keys; when the target declares notices, each notice's file
    /// path is shown as an "open in editor" link below it, and `enter` toggles an
    /// inline (scrollable) preview of the contents.
    fn approval_lines(&self) -> Vec<Line<'static>> {
        let Some(view) = self.approval.as_ref().and_then(|c| c.current()) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        // When more than one target is awaiting approval, show the total count so
        // the user knows further prompts follow this one.
        let total_pending = view.queued_behind + 1;
        let queued = if total_pending > 1 {
            format!(" · {total_pending} pending")
        } else {
            String::new()
        };
        // Only offer the notice-view toggle when there is a notice to view.
        let action = if view.notices.is_empty() {
            String::new()
        } else if view.expanded {
            " · enter hide".to_string()
        } else {
            " · enter view".to_string()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  ⚠ approval required: {}  [y] approve · [n] reject{action}{queued}",
                view.addr
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        // Always show the notice file link(s) so the user can open the full text
        // in an editor without expanding the inline preview.
        for notice in &view.notices {
            lines.push(Line::from(Span::styled(
                format!("    → {}: {}", notice.name, notice.path),
                Style::default().fg(Color::Cyan),
            )));
        }
        if view.expanded {
            for notice in &view.notices {
                lines.push(Line::from(Span::styled(
                    format!("  ── {} ──", notice.name),
                    Style::default().fg(Color::Cyan),
                )));
                for raw in notice.content.lines() {
                    lines.push(Line::from(format!("  {raw}")));
                }
            }
        }
        lines
    }

    /// The selectable body views in cycle order: [`ViewMode::Default`] first,
    /// then one entry per [`HeaderItem::Tab`] the header model exposes, in header
    /// order. `Tab` walks this list.
    fn view_modes(&self) -> Vec<ViewMode> {
        let mut modes = vec![ViewMode::Default];
        for item in self.model.header(&self.state, self.scope.get()) {
            match item {
                HeaderItem::Tab { mode, .. } => modes.push(mode),
                // A split segment contributes each of its tab-bound parts, in
                // order — so `X / Y done` yields Done then Matched.
                HeaderItem::Split(parts) => {
                    modes.extend(parts.into_iter().filter_map(|p| p.mode));
                }
                HeaderItem::Text(_) => {}
            }
        }
        modes
    }

    /// The help row pinned under the box. While held open after the run finishes
    /// it turns into a green "press q to quit" notice; otherwise it lists the keys
    /// the viewport responds to.
    fn help_line(&self) -> Line<'static> {
        // While typing a filter the help row becomes the search prompt with a
        // block cursor; Enter keeps it, Esc clears it.
        if self.search_active.get() {
            let query = self.search_query.borrow();
            return Line::from(vec![
                Span::styled(format!("  /{query}▏"), Style::default().fg(Color::Yellow)),
                Span::styled(
                    "  enter keep · esc clear",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]);
        }
        // A confirmed (but still applied) filter: show it with the clear hint.
        {
            let query = self.search_query.borrow();
            if !query.is_empty() {
                return Line::from(vec![
                    Span::styled(
                        format!("  filter: {query}"),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        "  ↑/↓ scroll · / edit · esc clear",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]);
            }
        }
        if self.finished_at_ms.is_some() {
            // Off the main view `q`/esc step back to it (which then auto-exits);
            // on the main view they quit. Reflect that in the notice.
            let notice = if self.view.get() == ViewMode::Default {
                "  ✓ finished — press q or Ctrl-C to quit"
            } else {
                "  ✓ finished — esc/q back to main · Ctrl-C to quit"
            };
            return Line::from(Span::styled(
                notice,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // The `a` hint reads as the *action*: it names the scope you switch to.
        let scope_key = match self.scope.get() {
            CountScope::Matched => "a all",
            CountScope::All => "a matched",
        };
        // `/` filters and `esc` steps back — both only apply off the live view.
        let (search_hint, back_hint) = if self.view.get() == ViewMode::Default {
            ("", "")
        } else {
            (" · / search", " · esc back")
        };
        Line::from(Span::styled(
            format!(
                "  ↑/↓ scroll · ←/→ pan · tab/⇧tab switch view · {scope_key}{search_hint}{back_hint}"
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))
    }

    /// Render the header model's items into a flat span run: items joined by
    /// ` · `, with the [`HeaderItem::Tab`] matching the active view highlighted
    /// (reversed background). This is the one place the ` · ` separator lives.
    fn header_item_spans(&self) -> Vec<Span<'static>> {
        let active = self.view.get();
        let items = self.model.header(&self.state, self.scope.get());
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 2);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" · "));
            }
            match item {
                HeaderItem::Text(item_spans) => spans.extend(item_spans.iter().cloned()),
                HeaderItem::Tab {
                    mode,
                    spans: item_spans,
                } if *mode == active => {
                    // Active tab: paint a background so the selected view reads
                    // as highlighted in the header.
                    spans.extend(item_spans.iter().map(highlight_span));
                }
                HeaderItem::Tab {
                    spans: item_spans, ..
                } => spans.extend(item_spans.iter().cloned()),
                // A split segment renders its parts back-to-back with no ` · `
                // between them; each tab-bound part highlights independently when
                // its own view is active.
                HeaderItem::Split(parts) => {
                    for part in parts {
                        if part.mode == Some(active) {
                            spans.extend(part.spans.iter().map(highlight_span));
                        } else {
                            spans.extend(part.spans.iter().cloned());
                        }
                    }
                }
            }
        }
        spans
    }

    /// The rounded top border:
    /// `╭─ 1m05s · D / N done · N cached · N failed · <workers> ──────╮`.
    /// The leading field is the elapsed-time clock. "done" shows matched progress
    /// `done / total` (total prefixed `~` while the matcher is still resolving),
    /// falling back to the executed count before any match streams. The "running"
    /// count is omitted — the worker braille (flush right) conveys concurrency.
    fn header_line(&self, now_ms: u64, width: u16) -> Line<'static> {
        let width = usize::from(width).max(MIN_BOX_WIDTH);
        // Once finished, the clock is frozen at the finish instant rather than
        // advancing with the live render wall clock.
        let clock = self.finished_at_ms.unwrap_or(now_ms);
        let elapsed = human_elapsed(self.state.elapsed_ms(clock));

        let mut left: Vec<Span<'static>> = Vec::with_capacity(5);
        left.push(Span::raw("╭─ "));
        left.push(Span::from(elapsed).bold());
        // The status segment (counts + worker braille, or gc sweep stats) is
        // supplied by the header model; the view owns only the leading clock.
        left.push(Span::raw(" · "));
        left.extend(self.header_item_spans());
        // Space between the segment and the dash fill.
        left.push(Span::raw(" "));

        let left_w = spans_width(&left);
        // Trailing "─╮" closes the border.
        let fill = width.saturating_sub(left_w + 2);

        let mut spans = left;
        spans.push(Span::raw("─".repeat(fill)));
        spans.push(Span::raw("─╮"));
        Line::from(spans)
    }

    /// The rounded bottom border: `╰─── <label> ────…────╯`. The label is left
    /// after a `─── ` lead-in; if it overruns the available span it scrolls like
    /// a banner. Total visible width always equals `width`. When the body has
    /// more lines than the viewport, a scroll indicator like
    /// `↑ 3–43 of 75 ↓` replaces the trailing dash fill.
    fn bottom_line(
        &self,
        now_ms: u64,
        width: u16,
        scroll: usize,
        total: usize,
        body_rows: usize,
    ) -> Line<'static> {
        let width = usize::from(width).max(MIN_BOX_WIDTH);
        // "╰─── " (5) + window + "─╯" (2) == width  ⇒  window = width - 7.
        let mut window = width.saturating_sub(7);
        let label = self.model.label();
        let label_len = label.chars().count();

        // When the body overflows the viewport, show a scroll position indicator
        // in the bottom-right: `↑ 1–20 of 75 ↓`. The label shrinks to make room.
        let scrollable = total > body_rows && body_rows > 0;
        let show_indicator = scrollable && window > SCROLL_INDICATOR_MIN_WIDTH;
        let indicator = if show_indicator {
            let vis_start = scroll + 1;
            let vis_end = (scroll + body_rows).min(total);
            format!(" ↑ {vis_start}–{vis_end} of {total} ↓")
        } else {
            String::new()
        };
        if show_indicator {
            window = window.saturating_sub(indicator.chars().count());
        }

        let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
        spans.push(Span::raw("╰─── "));
        if label_len <= window {
            spans.push(Span::raw(label.clone()));
            let pad = window - label_len;
            if pad >= 1 {
                spans.push(Span::raw(" "));
                spans.push(Span::raw("─".repeat(pad - 1)));
            }
        } else if window > 0 {
            spans.push(Span::raw(banner_slice(&label, window, now_ms)));
        }
        if show_indicator {
            spans.push(Span::raw(indicator));
        }
        spans.push(Span::raw("─╯"));
        Line::from(spans)
    }
}

impl TUIAppView for TuiProgressView {
    fn apply(&mut self, ev: &BuildEvent) {
        self.state.apply(ev);
        self.model.apply(ev);
    }

    fn rows(&self, term_height: u16) -> u16 {
        rows_for_height(term_height)
    }

    fn scroll(&mut self, delta: i32) {
        let cur = self.scroll.get();
        let mag = delta.unsigned_abs() as usize;
        let next = if delta >= 0 {
            cur.saturating_add(mag)
        } else {
            cur.saturating_sub(mag)
        };
        self.scroll.set(next);
    }

    fn hscroll(&mut self, delta: i32) {
        let cur = self.hscroll.get();
        let mag = delta.unsigned_abs() as usize;
        let next = if delta >= 0 {
            cur.saturating_add(mag)
        } else {
            cur.saturating_sub(mag)
        };
        self.hscroll.set(next);
    }

    fn tab(&mut self, forward: bool) {
        let modes = self.view_modes();
        let len = modes.len().max(1);
        let cur = self.view.get();
        let idx = modes.iter().position(|&m| m == cur).unwrap_or(0);
        // Step forward or backward with wrap. `modes` always holds at least
        // `Default`, so the index is in-bounds; the `.get`/fallback keeps it
        // panic-free regardless.
        let step = if forward { 1 } else { len - 1 };
        let next = modes
            .get((idx + step) % len)
            .copied()
            .unwrap_or(ViewMode::Default);
        self.view.set(next);
        // Switching views resets the scroll so the new body starts at the
        // top-left, and drops any active filter (it was scoped to the old tab).
        self.scroll.set(0);
        self.hscroll.set(0);
        self.search_active.set(false);
        self.search_query.borrow_mut().clear();
    }

    fn toggle_scope(&mut self) {
        let next = match self.scope.get() {
            CountScope::Matched => CountScope::All,
            CountScope::All => CountScope::Matched,
        };
        self.scope.set(next);
    }

    fn is_on_main_view(&self) -> bool {
        self.view.get() == ViewMode::Default
    }

    fn has_active_filter(&self) -> bool {
        self.search_active.get() || !self.search_query.borrow().is_empty()
    }

    fn back_to_main(&mut self) {
        self.view.set(ViewMode::Default);
        // Reset the same per-tab state a `Tab` switch does: scroll, pan, and any
        // filter scoped to the tab we are leaving.
        self.scroll.set(0);
        self.hscroll.set(0);
        self.search_active.set(false);
        self.search_query.borrow_mut().clear();
    }

    fn approval_active(&self) -> bool {
        self.approval.as_ref().is_some_and(|c| c.is_active())
    }

    fn approval_respond(&mut self, approve: bool) {
        if let Some(center) = self.approval.as_ref() {
            center.respond(approve);
        }
        // A resolved prompt shrinks the body; reset scroll so the live rows show.
        self.scroll.set(0);
    }

    fn approval_toggle_notice(&mut self) {
        if let Some(center) = self.approval.as_ref() {
            center.toggle_expanded();
        }
        self.scroll.set(0);
    }

    fn is_searching(&self) -> bool {
        self.search_active.get()
    }

    fn search_start(&mut self) {
        // Search only filters the list tabs; ignore `/` on the live Default view.
        if self.view.get() == ViewMode::Default {
            return;
        }
        // Re-entering search keeps any confirmed query so `/` resumes editing.
        self.search_active.set(true);
        self.scroll.set(0);
        self.hscroll.set(0);
    }

    fn search_input(&mut self, c: char) {
        self.search_query.borrow_mut().push(c);
        // A changed filter reshapes the list; restart from the top.
        self.scroll.set(0);
    }

    fn search_backspace(&mut self) {
        self.search_query.borrow_mut().pop();
        self.scroll.set(0);
    }

    fn search_cancel(&mut self) {
        self.search_active.set(false);
        self.search_query.borrow_mut().clear();
        self.scroll.set(0);
        self.hscroll.set(0);
    }

    fn search_confirm(&mut self) {
        // Leave input mode but keep the filter so the list stays scrollable.
        self.search_active.set(false);
    }

    /// Layout (top to bottom), a rounded box sized to `height` rows (one third of
    /// the terminal, see [`rows_for_height`]), with a dim help row pinned beneath:
    /// ```text
    /// ╭─ 1m05s · D / N done · N cached · N failed ──────────── <workers> ─╮
    ///   <slow rows + lock waits, scrollable>
    /// ╰─── <label> ───────────────────────────────────────────────────────╯
    ///   ↑/↓ scroll
    /// ```
    /// The body grows to fill the available rows; when it overflows it scrolls and
    /// the last visible row collapses the remainder. When nothing is slow the body
    /// shows a dim, slowly-drifting abstract field instead of blank rows. The
    /// `spinner` is unused — liveness is conveyed by the worker braille, the
    /// scrolling label, and the idle art.
    fn render(&self, _spinner: &str, now_ms: u64, width: u16, height: u16) -> Vec<Line<'static>> {
        let height = usize::from(height.max(MIN_PROGRESS_ROWS));
        // height = 1 header + body_rows + 1 bottom border + 1 help row.
        let body_rows = height - 3;
        let mut lines = Vec::with_capacity(height);
        lines.push(self.header_line(now_ms, width));
        let view = self.view.get();
        // The `/` filter only applies to the list tabs; the Default body ignores it.
        let query = self.search_query.borrow();
        let filter: &str = query.as_str();
        let body = match view {
            // Pending approval prompts ride at the very top of the live body so a
            // gated run is impossible to miss; the slow/lock rows follow.
            ViewMode::Default => {
                let mut b = self.approval_lines();
                b.extend(self.state.body_lines(now_ms));
                Body::Lines(b)
            }
            ViewMode::Done => Body::Rows(self.state.done_rows(self.scope.get(), filter)),
            ViewMode::Matched => Body::Rows(self.state.matched_rows(filter)),
            ViewMode::Cached => Body::Rows(self.state.cached_rows(self.scope.get(), filter)),
            ViewMode::Failed => Body::Rows(self.state.failed_rows(filter)),
        };
        let total = body.len();
        let filtering = !filter.is_empty();
        if total == 0 {
            self.scroll.set(0);
            self.hscroll.set(0);
            match view {
                // Default view: the dim drifting idle field.
                ViewMode::Default => {
                    lines.extend(art_lines(now_ms, usize::from(width.max(1)), body_rows))
                }
                // List tabs with an active filter that matched nothing.
                _ if filtering => lines.push(Line::from(Span::styled(
                    format!("  no targets match \"{filter}\""),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ))),
                // Done view with nothing completed: a single dim placeholder.
                ViewMode::Done => lines.push(Line::from(Span::styled(
                    "  no completed targets",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ))),
                // Matched view before any match streamed: a dim placeholder.
                ViewMode::Matched => lines.push(Line::from(Span::styled(
                    "  no matched targets",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ))),
                // Cached view with nothing cached: a dim placeholder.
                ViewMode::Cached => lines.push(Line::from(Span::styled(
                    "  no cached targets",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ))),
                // Failed view with nothing failed: a single dim placeholder.
                ViewMode::Failed => lines.push(Line::from(Span::styled(
                    "  no failed targets",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ))),
            }
        } else {
            // Clamp the horizontal pan against the widest body line so panning
            // stops once the longest line's tail reaches the right edge. Measured
            // over the whole list, not just the window, so the clamp does not
            // shift as you scroll — which means measuring every row. An unpanned
            // body clamps to 0 whatever the widest row is, so skip the walk
            // entirely in that case; it is every frame the user has not panned.
            let hscroll = match self.hscroll.get() {
                0 => 0,
                pan => {
                    let avail = usize::from(width.max(1));
                    pan.min(body.max_width().saturating_sub(avail))
                }
            };
            self.hscroll.set(hscroll);

            let (window, scroll) = body.window(body_rows, self.scroll.get());
            self.scroll.set(scroll);
            lines.extend(window.into_iter().map(|l| hscroll_line(l, hscroll)));
        }
        // Pad the body so the bottom border always pins to the same row.
        while lines.len() < body_rows + 1 {
            lines.push(Line::from(""));
        }
        lines.push(self.bottom_line(now_ms, width, self.scroll.get(), total, body_rows));
        lines.push(self.help_line());
        lines
    }

    fn hold_after_finish(&self) -> bool {
        // Auto-exit only from the main view; if the user is reading another tab
        // (e.g. the failed list), keep the viewport up until they quit.
        self.view.get() != ViewMode::Default
    }

    fn set_finished(&mut self) {
        // Freeze the elapsed clock at the finish instant; the viewport keeps
        // rendering while held but the time must stop counting.
        self.finished_at_ms = Some(hcore::events::now_unix_ms());
    }

    /// Final report — the elapsed clock plus the header model's summary segment,
    /// printed straight to stderr (not the log sink) so it survives the torn-down
    /// inline viewport. Per-target failures are rendered separately (rich
    /// diagnostics from the failure registry). The model returns an empty segment
    /// when there was nothing to report (e.g. inspect/query, or a no-op gc), in
    /// which case nothing is printed.
    fn last_render(&self) {
        let segment = self.final_summary_segment();
        if segment.is_empty() {
            return;
        }
        // Match the live header: a held/finished run freezes the clock at the
        // finish instant rather than the teardown wall clock.
        let clock = self
            .finished_at_ms
            .unwrap_or_else(hcore::events::now_unix_ms);
        let elapsed = human_elapsed_ms(self.state.elapsed_ms(clock));
        let elapsed = elapsed.trim_start();
        use std::io::Write;
        drop(writeln!(std::io::stderr().lock(), "{elapsed} · {segment}"));
    }
}

/// The paved-road [`CIAppView`]: a one-line label header, a concise per-execute
/// line, and a final one-line summary plus per-error lines — all through the
/// log sink. Used by every command's `ci_view`.
pub struct CiProgressView {
    core: ProgressCore,
    /// Caches whose contention has already been announced.
    ///
    /// One `exclusive` cache shared by hundreds of targets produces that many
    /// waits; without this, a GitHub log gets that many identical lines. The
    /// first names the problem, and [`finish`](CIAppView::finish) reports the
    /// total it cost.
    scratch_wait_announced: HashSet<String>,
}

impl CiProgressView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            core: ProgressCore::new(label),
            scratch_wait_announced: HashSet::new(),
        }
    }
}

impl CIAppView for CiProgressView {
    fn begin(&self) {
        tracing::info!("{}", self.core.label);
    }

    /// "Not chatty" policy: one line only for cacheable executes (the 1:1
    /// replacement for the old `tracing::info!(… "run")` — cache hits
    /// short-circuit before execute, so they stay silent).
    fn apply(&mut self, ev: &BuildEvent) {
        match &ev.kind {
            BuildEventKind::ExecuteStart {
                addr,
                driver,
                cache: true,
            } => {
                tracing::info!("running {addr} [{driver}]");
            }
            BuildEventKind::ResultLockWaitStart { addr, holder_pid } => {
                let holder = match holder_pid {
                    Some(pid) => format!("held by pid {pid}"),
                    None => "holder unknown".to_string(),
                };
                tracing::info!("waiting on result lock for {addr} ({holder})");
            }
            // First waiter per cache only — see `scratch_wait_announced`.
            BuildEventKind::ScratchLockWaitStart {
                scratch,
                access,
                holder_pid,
                ..
            } if self.scratch_wait_announced.insert(scratch.clone()) => match holder_pid {
                Some(pid) => tracing::info!("waiting on scratch {scratch}, held by pid {pid}"),
                None => tracing::info!(
                    "waiting on scratch {scratch} ({access}); \
                     targets sharing it run one at a time"
                ),
            },
            _ => {}
        }
        self.core.fold(ev);
    }

    fn finish(&self) {
        if let Some(n) = self.core.state.matched_total() {
            tracing::info!("matched {n} targets");
        }
        // The one number that tells someone their `exclusive` default is what
        // made the build slow. Reported per cache, worst first.
        for (scratch, waiters, total_ms) in self.core.state.scratch_wait_totals() {
            let total = human_elapsed(total_ms);
            tracing::info!("scratch {scratch} serialized {waiters} targets, {total} of total wait");
        }
        tracing::info!("{}", self.core.state.summary());
    }
}

/// CI (non-TUI) view for a cache sweep (`heph tool gc`, `heph tool clean`): folds it
/// into a [`GcHeader`] and logs a single summary line at the end, prefixed with
/// the header's label so two commands sharing this view stay distinguishable in
/// a log. Per-target progress is silent.
pub struct GcCiView {
    header: GcHeader,
}

impl GcCiView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            header: GcHeader::new(label),
        }
    }
}

impl CIAppView for GcCiView {
    fn begin(&self) {
        tracing::info!("{}", self.header.label());
    }

    fn apply(&mut self, ev: &BuildEvent) {
        self.header.apply(ev);
    }

    fn finish(&self) {
        tracing::info!("{}: {}", self.header.label(), self.header.segment());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at_unix_ms: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent { at_unix_ms, kind }
    }

    /// Every row of a list body, rendered in order — what the viewport would
    /// show if it had unlimited rows. The reference the windowed renders are
    /// checked against.
    fn all_lines(rows: BodyRows<'_>) -> Vec<Line<'static>> {
        rows.window(usize::MAX, 0).0
    }

    /// Flatten a header model's items into their concatenated text.
    fn header_text(items: &[HeaderItem]) -> String {
        items
            .iter()
            .flat_map(|i| i.spans())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn lock_wait_shown_with_holder_pid_then_cleared_on_end() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//pkg:a".into(),
                holder_pid: Some(4242),
            },
        ));
        let lines = s.lock_wait_lines();
        assert_eq!(lines.len(), 1);
        let text = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("//pkg:a"), "{text}");
        assert!(text.contains("pid 4242"), "{text}");

        // The notice disappears when the wait ends.
        s.apply(&ev(
            1,
            BuildEventKind::ResultLockWaitEnd {
                addr: "//pkg:a".into(),
            },
        ));
        assert!(s.lock_wait_lines().is_empty());
    }

    #[test]
    fn lock_wait_unknown_holder_renders_unknown() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//pkg:a".into(),
                holder_pid: None,
            },
        ));
        let text = s.lock_wait_lines()[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("holder unknown"), "{text}");
    }

    fn result_start(addr: &str) -> BuildEventKind {
        BuildEventKind::ResultStart { addr: addr.into() }
    }
    fn result_end(addr: &str, error: Option<String>) -> BuildEventKind {
        BuildEventKind::ResultEnd {
            addr: addr.into(),
            error,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
        }
    }
    fn execute_start(addr: &str) -> BuildEventKind {
        BuildEventKind::ExecuteStart {
            addr: addr.into(),
            driver: "exec".into(),
            cache: true,
        }
    }
    fn execute_end(addr: &str) -> BuildEventKind {
        BuildEventKind::ExecuteEnd {
            addr: addr.into(),
            error: None,
        }
    }
    fn local_write_start(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheWriteStart { addr: addr.into() }
    }
    fn local_write_end(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheWriteEnd {
            addr: addr.into(),
            error: None,
        }
    }
    fn remote_write_start(addr: &str) -> BuildEventKind {
        BuildEventKind::RemoteCacheWriteStart { addr: addr.into() }
    }
    fn remote_write_end(addr: &str) -> BuildEventKind {
        BuildEventKind::RemoteCacheWriteEnd {
            addr: addr.into(),
            error: None,
        }
    }

    #[test]
    fn op_timeline_records_execute_then_local_cache_write_breakdown() {
        // Execute runs 0→3s (completed), then LocalCacheWrite opens at 3s and is
        // still live at now=9s (6s active, over the 5s trigger). The breakdown
        // carries both, ordered by pipeline.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(3_000, execute_end("//a:b")));
        s.apply(&ev(3_000, local_write_start("//a:b")));

        let long = s.long_running(9_000, 5_000);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].0, "//a:b");
        assert_eq!(long[0].1, 6_000); // active LocalCacheWrite elapsed
        assert_eq!(
            long[0].2,
            vec![(Op::Execute, 3_000), (Op::LocalCacheWrite, 6_000)]
        );
    }

    #[test]
    fn op_timeline_omits_sub_second_ops() {
        // A 500ms Execute is below the 1s breakdown floor and is dropped; only the
        // live 6s LocalCacheWrite shows.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(500, execute_end("//a:b")));
        s.apply(&ev(500, local_write_start("//a:b")));

        let long = s.long_running(6_500, 5_000);
        assert_eq!(long[0].2, vec![(Op::LocalCacheWrite, 6_000)]);
    }

    #[test]
    fn op_timeline_overlap_folds_dangling_active() {
        // A new op opening while one is still active folds the dangling one into
        // completed (defensive against a missing/reordered end).
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(3_000, local_write_start("//a:b")));

        let tl = s.ops.get("//a:b").expect("timeline");
        assert_eq!(tl.completed.get(&Op::Execute).copied(), Some(3_000));
        assert_eq!(tl.active, Some((Op::LocalCacheWrite, 3_000)));
    }

    #[test]
    fn op_timeline_mismatched_end_ignored() {
        // An end for an op that is not the active one is a no-op.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, local_write_end("//a:b")));

        let tl = s.ops.get("//a:b").expect("timeline");
        assert_eq!(tl.active, Some((Op::Execute, 0)));
        assert!(tl.completed.is_empty());
    }

    #[test]
    fn op_timeline_dropped_from_ops_once_result_ends() {
        // `ops` must not retain a finished target's timeline forever — that is
        // unbounded per-target memory over the life of a run. By the time
        // `ResultEnd` fires, Execute (and any local-cache-write) has already
        // closed for the addr, so the entry is dead weight: neither
        // `long_running` nor `busy_workers` reads a closed-op entry (both walk
        // `open_ops`, and `long_running`'s `?` already skips an addr with no
        // active op). Retention should reclaim it here.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, execute_end("//a:b")));
        assert!(s.ops.contains_key("//a:b"));

        s.apply(&ev(2_000, result_end("//a:b", None)));
        assert!(
            !s.ops.contains_key("//a:b"),
            "ops must drop a target's timeline once its ResultEnd arrives"
        );
        assert!(
            !s.open_ops.contains("//a:b"),
            "open_ops must not disagree with ops once the entry is gone"
        );
    }

    #[test]
    fn op_timeline_reclaimed_when_remote_cache_write_trails_result_end() {
        // `RemoteCacheWrite` is pushed from a detached background task
        // (`Engine::spawn_remote_upload`) that the engine does not await inside
        // the `ResultEnd` scope, so its Start/End can legitimately arrive after
        // `ResultEnd` already removed the entry. The Start recreates it
        // (`ops.entry(..).or_default()`); if nothing then reclaimed it, every
        // remote-cache-uploading target would leak one `OpTimeline` forever —
        // the exact unbounded growth this retention is meant to fix, just gated
        // on remote caching instead of run size. The trailing End must be the
        // one that frees it this time.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, execute_end("//a:b")));
        s.apply(&ev(2_000, result_end("//a:b", None)));
        assert!(!s.ops.contains_key("//a:b"), "gone after ResultEnd");

        // The background upload starts after ResultEnd already fired.
        s.apply(&ev(3_000, remote_write_start("//a:b")));
        assert!(
            s.ops.contains_key("//a:b"),
            "the trailing op legitimately recreates the entry"
        );
        assert!(s.open_ops.contains("//a:b"));

        s.apply(&ev(4_000, remote_write_end("//a:b")));
        assert!(
            !s.ops.contains_key("//a:b"),
            "the trailing RemoteCacheWriteEnd must reclaim it since ResultEnd already fired"
        );
        assert!(!s.open_ops.contains("//a:b"));
    }

    #[test]
    fn op_timeline_dropped_from_ops_on_a_failing_result_end_too() {
        // The removal in the `ResultEnd` arm runs unconditionally, before the
        // success/error branch is even inspected — a failed target must not keep
        // its timeline around any more than a successful one does.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, execute_end("//a:b")));

        s.apply(&ev(2_000, result_end("//a:b", Some("boom".into()))));
        assert!(
            !s.ops.contains_key("//a:b"),
            "a failing ResultEnd must drop the timeline too"
        );
    }

    #[test]
    fn ops_map_does_not_grow_unbounded_over_a_large_run() {
        // The retention regression guard at the scale the field doc calls out:
        // before this fix `ops` gained one `OpTimeline` per target for the life
        // of the request, so a 100k-target run held 100k never-freed entries
        // (each carrying its own `completed: HashMap<Op, u64>`). A wall-clock
        // measurement of that is noisy under load on this box; the retained
        // entry count is not — every target below fully completes, so the
        // post-run count is the deterministic before/after number: unbounded
        // (100_000) without the fix, 0 with it.
        let mut s = BuildState::new();
        for i in 0..100_000 {
            let addr = format!("//pkg{i}:t");
            s.apply(&ev(0, result_start(&addr)));
            s.apply(&ev(0, execute_start(&addr)));
            s.apply(&ev(1, execute_end(&addr)));
            s.apply(&ev(1, result_end(&addr, None)));
        }
        assert_eq!(s.completed, 100_000);
        assert_eq!(
            s.ops.len(),
            0,
            "ops retained every finished target instead of freeing them"
        );
    }

    #[test]
    fn busy_workers_counts_only_active_execute() {
        // One target mid-Execute, one mid-LocalCacheWrite: only Execute counts as
        // a busy worker (the semaphore-bound slot).
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:exec")));
        s.apply(&ev(0, local_write_start("//a:write")));
        assert_eq!(s.busy_workers(), 1);
    }

    #[test]
    fn op_timeline_non_execute_op_alone_surfaces_target() {
        // A non-Execute op (cache write) with no Execute event still surfaces the
        // target as slow, proving the timeline is not Execute-specific.
        let mut s = BuildState::new();
        s.apply(&ev(0, local_write_start("//a:b")));
        let long = s.long_running(6_000, 5_000);
        assert_eq!(long.len(), 1);
        assert_eq!(long[0].2, vec![(Op::LocalCacheWrite, 6_000)]);
    }

    #[test]
    fn result_start_then_ok_end_completes_and_clears_in_flight() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        assert_eq!(s.in_flight_results.len(), 1);
        s.apply(&ev(2, result_end("//a:b", None)));
        assert_eq!(s.completed, 1);
        assert_eq!(s.errored, 0);
        assert_eq!(s.in_flight_results.len(), 0);
    }

    #[test]
    fn result_end_with_error_increments_errored() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, result_end("//a:b", Some("boom".into()))));
        assert_eq!(s.errored, 1);
        assert_eq!(s.completed, 0);
        assert_eq!(s.in_flight_results.len(), 0);
    }

    #[test]
    fn duplicate_result_start_is_idempotent_and_second_end_noop() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(1, result_start("//a:b")));
        assert_eq!(s.in_flight_results.len(), 1);
        s.apply(&ev(2, result_end("//a:b", None)));
        // Second ResultEnd for an already-removed addr must not double-count.
        s.apply(&ev(3, result_end("//a:b", Some("late".into()))));
        assert_eq!(s.completed, 1);
        assert_eq!(s.errored, 0);
        assert_eq!(s.in_flight_results.len(), 0);
    }

    #[test]
    fn local_cache_hit_miss_counters() {
        let mut s = BuildState::new();
        s.apply(&ev(
            1,
            BuildEventKind::LocalCacheHit {
                addr: "//a:b".into(),
            },
        ));
        s.apply(&ev(
            2,
            BuildEventKind::LocalCacheMiss {
                addr: "//c:d".into(),
            },
        ));
        s.apply(&ev(
            3,
            BuildEventKind::LocalCacheMiss {
                addr: "//e:f".into(),
            },
        ));
        assert_eq!(s.local_hits, 1);
        assert_eq!(s.local_misses, 2);
        let summary = s.summary();
        assert!(summary.contains("1 hit"), "summary: {summary}");
        assert!(summary.contains("2 miss"), "summary: {summary}");
    }

    /// A cache hit the engine later has to rebuild must stop counting as a hit.
    ///
    /// The engine decides a hit from the revision's manifest, which can outlive
    /// its blobs — a local GC, an expiry rule on a shared cache, or blobs that
    /// were never pulled and a remote that is now unreachable. It announces the
    /// hit, discovers the bytes are unreadable, and rebuilds. Counting both would
    /// let "executed + cached" exceed the number of targets, and would report a
    /// fully-offline rebuild of the whole graph as "N cached" — the header would
    /// say the opposite of what happened.
    #[test]
    fn a_hit_that_rebuilds_stops_counting_as_a_hit() {
        let mut s = BuildState::new();
        // `matched_cached` is folded at both edges — a hit can be announced
        // before or after its addr is matched — so retraction has to survive
        // both orders. `//a:local` and `//a:stands` are matched first and hit
        // after; `//a:remote` hits first and is matched after.
        s.apply(&ev(1, matched(&["//a:local", "//a:stands"], false)));
        s.apply(&ev(2, local_cache_hit("//a:local")));
        s.apply(&ev(3, remote_cache_hit("//a:remote")));
        s.apply(&ev(4, local_cache_hit("//a:stands")));
        s.apply(&ev(5, matched(&["//a:remote"], true)));
        assert_eq!((s.local_hits, s.remote_hits), (2, 1));
        assert_eq!(
            s.cached_count(CountScope::Matched),
            3,
            "all three matched targets are announced cached to begin with"
        );

        // Two of the three revisions turn out to be unreadable and get rebuilt.
        s.apply(&ev(6, execute_start("//a:local")));
        s.apply(&ev(7, execute_start("//a:remote")));
        s.apply(&ev(8, execute_end("//a:local")));
        s.apply(&ev(9, execute_end("//a:remote")));

        assert_eq!(
            (s.local_hits, s.remote_hits),
            (1, 0),
            "a rebuilt target must be taken back off the cache counter it was put on"
        );
        assert_eq!(
            (s.local_misses, s.remote_misses),
            (1, 1),
            "and counted as the miss it turned out to be"
        );
        assert_eq!(s.executed, 2);
        // Through the header's own accessors, not the raw counters: these are the
        // numbers the user reads, and the point is that `executed + cached` (2 + 1)
        // does not exceed the three targets seen.
        assert_eq!(
            s.cached_count(CountScope::All),
            1,
            "only the hit that stood may still be reported as cached"
        );
        assert_eq!(
            s.cached_count(CountScope::Matched),
            1,
            "the folded `matched ∩ cache_hit` counter must be decremented too — \
             dropping the addr from the set alone leaves the count stale"
        );
        assert_eq!(
            s.cached_count(CountScope::Matched),
            scan_matched(&s).1,
            "the folded counter must still agree with the set it stands in for"
        );
        assert!(
            !s.cache_hit.contains_key("//a:local") && s.cache_hit.contains_key("//a:stands"),
            "only the rebuilt addrs are retracted: {:?}",
            s.cache_hit
        );
    }

    /// A target that executes without ever being announced as a hit — the plain
    /// miss — must not underflow the hit counters.
    #[test]
    fn executing_a_target_that_never_hit_leaves_the_counters_alone() {
        let mut s = BuildState::new();
        s.apply(&ev(1, execute_start("//a:fresh")));
        s.apply(&ev(2, execute_end("//a:fresh")));
        assert_eq!((s.local_hits, s.remote_hits), (0, 0));
        assert_eq!((s.local_misses, s.remote_misses), (0, 0));
        assert_eq!(s.executed, 1);
    }

    #[test]
    fn long_running_filters_by_threshold_and_sorts_desc() {
        let mut s = BuildState::new();
        // Started at 0, 1000, 4000.
        s.apply(&ev(0, execute_start("//slow:a")));
        s.apply(&ev(1_000, execute_start("//mid:b")));
        s.apply(&ev(4_000, execute_start("//fresh:c")));

        // now = 7000, threshold = 5000:
        //   //slow:a   elapsed 7000 > 5000  ✓
        //   //mid:b    elapsed 6000 > 5000  ✓
        //   //fresh:c  elapsed 3000         ✗
        let long = s.long_running(7_000, 5_000);
        assert_eq!(long.len(), 2);
        // Sorted descending by active elapsed: slow (7000) before mid (6000).
        assert_eq!(long[0].0, "//slow:a");
        assert_eq!(long[0].1, 7_000);
        assert_eq!(long[0].2, vec![(Op::Execute, 7_000)]);
        assert_eq!(long[1].0, "//mid:b");
        assert_eq!(long[1].1, 6_000);
        assert_eq!(long[1].2, vec![(Op::Execute, 6_000)]);
    }

    #[test]
    fn execute_end_removes_from_long_running() {
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//slow:a")));
        s.apply(&ev(100, execute_end("//slow:a")));
        assert!(s.long_running(10_000, 5_000).is_empty());
    }

    #[test]
    fn long_running_lines_render_icons_per_op() {
        // A slow target with a finished Execute and a live LocalCacheWrite renders
        // one `(icon Ns)` group per op.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//slow:x")));
        s.apply(&ev(3_000, execute_end("//slow:x")));
        s.apply(&ev(3_000, local_write_start("//slow:x")));

        let line = format!("{}", s.slow_rows(9_000)[0]);
        assert!(line.contains("//slow:x"), "{line}");
        assert!(
            line.contains(&format!("({} ", Op::Execute.icon())),
            "{line}"
        );
        assert!(
            line.contains(&format!("({} ", Op::LocalCacheWrite.icon())),
            "{line}"
        );
    }

    fn scratch_wait(addr: &str, scratch: &str, access: &str, pid: Option<u32>) -> BuildEventKind {
        BuildEventKind::ScratchLockWaitStart {
            addr: addr.into(),
            scratch: scratch.into(),
            access: access.into(),
            holder_pid: pid,
        }
    }

    fn scratch_wait_end(addr: &str, scratch: &str) -> BuildEventKind {
        BuildEventKind::ScratchLockWaitEnd {
            addr: addr.into(),
            scratch: scratch.into(),
        }
    }

    /// The row is about the *cache*, and many blocked targets collapse into one
    /// line. A shared `exclusive` cache with hundreds of consumers produces that
    /// many simultaneous waits; a row each would bury every other diagnostic.
    #[test]
    fn scratch_waits_collapse_to_one_row_per_cache() {
        let mut st = BuildState::new();
        for i in 0..40 {
            st.apply(&ev(
                1_000,
                scratch_wait(
                    &format!("//go/pkg{i}:build"),
                    "//build:gocache",
                    "exclusive",
                    None,
                ),
            ));
        }
        st.apply(&ev(
            1_000,
            scratch_wait("//other:x", "//build:gomodcache", "shared", Some(88214)),
        ));

        let lines = st.scratch_wait_lines(61_000);
        assert_eq!(lines.len(), 2, "one row per cache, not per waiter");

        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            text[0].contains("//build:gocache") && text[0].contains("40 waiting"),
            "the cache and the count are the diagnostic: {:?}",
            text[0],
        );
        assert!(
            text[0].contains("exclusive"),
            "an unheld slot names the access mode that caused it: {:?}",
            text[0],
        );
        assert!(
            text[1].contains("held by pid 88214"),
            "a named holder outranks the access mode: {:?}",
            text[1],
        );
    }

    /// A scratch wait must not render as a result-lock wait. They are different
    /// problems: one is "another process holds this target", the other is "your
    /// own build is serialized on a cache you declared `exclusive`".
    #[test]
    fn a_scratch_wait_is_not_rendered_as_a_result_lock_wait() {
        let mut st = BuildState::new();
        st.apply(&ev(
            0,
            scratch_wait("//a:x", "//build:gocache", "exclusive", None),
        ));
        assert!(
            st.lock_wait_lines().is_empty(),
            "a scratch wait must not appear among the result-lock rows",
        );
        assert_eq!(st.scratch_wait_lines(1_000).len(), 1);
    }

    /// The row disappears the moment the slot is acquired — it reflects only
    /// currently-blocked waits, and a stale row would send someone chasing a
    /// bottleneck that has already cleared.
    #[test]
    fn a_scratch_wait_row_clears_when_the_slot_is_acquired() {
        let mut st = BuildState::new();
        st.apply(&ev(
            1_000,
            scratch_wait("//a:x", "//build:gocache", "exclusive", None),
        ));
        assert_eq!(st.scratch_wait_lines(5_000).len(), 1);
        st.apply(&ev(9_000, scratch_wait_end("//a:x", "//build:gocache")));
        assert!(st.scratch_wait_lines(9_000).is_empty());

        // …and is remembered as a total, which is what the summary reports.
        assert_eq!(
            st.scratch_wait_totals(),
            vec![("//build:gocache", 1, 8_000)]
        );
    }

    /// Totals accumulate across every consumer, so the summary can say what the
    /// contention actually cost rather than what one target saw.
    #[test]
    fn scratch_wait_totals_sum_every_waiter() {
        let mut st = BuildState::new();
        for (i, since) in [1_000u64, 2_000, 3_000].iter().enumerate() {
            let addr = format!("//a:x{i}");
            st.apply(&ev(
                *since,
                scratch_wait(&addr, "//build:gocache", "exclusive", None),
            ));
            st.apply(&ev(
                since + 10_000,
                scratch_wait_end(&addr, "//build:gocache"),
            ));
        }
        assert_eq!(
            st.scratch_wait_totals(),
            vec![("//build:gocache", 3, 30_000)]
        );
    }

    /// One line per contended cache, however many targets are queued behind it.
    /// Without the dedup a large build writes one identical line per target.
    #[test]
    fn the_ci_view_announces_each_contended_cache_once() {
        let mut v = CiProgressView::new("Running");
        for i in 0..50 {
            v.apply(&ev(
                0,
                scratch_wait(&format!("//a:x{i}"), "//build:gocache", "exclusive", None),
            ));
        }
        assert_eq!(
            v.scratch_wait_announced.len(),
            1,
            "one cache, one announcement",
        );

        v.apply(&ev(
            0,
            scratch_wait("//a:y", "//build:gomodcache", "shared", None),
        ));
        assert_eq!(
            v.scratch_wait_announced.len(),
            2,
            "a second cache is its own problem and gets its own line",
        );
    }

    fn max_workers(count: usize) -> BuildEventKind {
        BuildEventKind::RequestConfig {
            max_workers: count,
            fail_fast: false,
            scratch_disabled: false,
        }
    }

    #[test]
    fn tui_view_box_layout_header_body_label() {
        let mut v = TuiProgressView::new("Running //a:b");
        v.apply(&ev(0, execute_start("//slow:x")));
        let height = 8u16;
        let lines = v.render("⠋", 10_000, 80, height);

        // The box fills exactly the rows it was given.
        assert_eq!(lines.len(), usize::from(height));

        // Top border: rounded corners + title, not the label.
        let header = format!("{}", lines.first().expect("header line"));
        assert!(header.starts_with("╭─"), "header: {header}");
        assert!(header.ends_with('╮'), "header: {header}");
        // Leading field is the elapsed clock (10s after the start anchor at t=0).
        assert!(header.contains("10s"), "header: {header}");
        assert!(!header.contains("Running //a:b"), "header: {header}");

        // Help row is pinned last, below the box.
        let help = format!("{}", lines.last().expect("help line"));
        assert!(help.contains("scroll"), "help: {help}");

        // Bottom border (second-to-last row): rounded corners + the label.
        let footer = format!("{}", lines[lines.len() - 2]);
        assert!(footer.starts_with("╰─"), "footer: {footer}");
        assert!(footer.ends_with('╯'), "footer: {footer}");
        assert!(footer.contains("Running //a:b"), "footer: {footer}");

        // The slow row sits in the body between the header and the bottom border.
        assert!(
            lines[1..lines.len() - 2]
                .iter()
                .any(|l| format!("{l}").contains("//slow:x")),
            "expected slow row in body, got {lines:?}"
        );
    }

    #[test]
    fn header_shows_built_cached_failed_counts_no_running() {
        let mut v = TuiProgressView::new("L");
        // One real build (execute end ok), one cache hit, one running, one failed.
        v.apply(&ev(0, execute_start("//a:built")));
        v.apply(&ev(1, execute_end("//a:built")));
        v.apply(&ev(
            2,
            BuildEventKind::LocalCacheHit {
                addr: "//a:cached".into(),
            },
        ));
        v.apply(&ev(3, result_start("//a:running")));
        v.apply(&ev(4, result_start("//a:failed")));
        v.apply(&ev(5, result_end("//a:failed", Some("boom".into()))));

        let header = format!("{}", v.render("⠋", 100, 120, 8).first().expect("header"));
        assert!(header.contains("1 done"), "{header}");
        assert!(header.contains("1 cached"), "{header}");
        assert!(header.contains("1 failed"), "{header}");
        // The "running" count was dropped in favour of the worker braille.
        assert!(!header.contains("running"), "{header}");
        // A space separates the counts from the dash fill.
        assert!(header.contains("failed "), "{header}");
    }

    #[test]
    fn header_done_shows_matched_progress_with_provisional_marker() {
        let mut v = TuiProgressView::new("L");
        // Matcher still streaming: total is provisional (`~`).
        v.apply(&ev(0, matched(&["//a:x", "//a:y", "//a:z"], false)));
        v.apply(&ev(1, result_start("//a:x")));
        v.apply(&ev(2, result_end("//a:x", None)));
        let header = format!("{}", v.render("⠋", 100, 120, 8).first().expect("header"));
        assert!(header.contains("1 / ~3 done"), "{header}");

        // Matcher resolves: the `~` drops.
        v.apply(&ev(3, matched(&[], true)));
        let header = format!("{}", v.render("⠋", 100, 120, 8).first().expect("header"));
        assert!(header.contains("1 / 3 done"), "{header}");
        assert!(!header.contains('~'), "{header}");
    }

    #[test]
    fn header_braille_sits_after_failed_count() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, max_workers(8)));
        v.apply(&ev(1, execute_start("//a:b")));
        let header = format!("{}", v.render("⠋", 100, 120, 8).first().expect("header"));
        // " · ⠁" appears after "failed".
        let failed_at = header.find("failed").expect("failed in header");
        let braille_at = header.find('⠁').expect("braille in header");
        assert!(
            braille_at > failed_at,
            "braille must follow failed: {header}"
        );
        assert!(
            header.contains("failed · "),
            "separator before braille: {header}"
        );
    }

    #[test]
    fn max_workers_event_drives_worker_braille() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, max_workers(8)));
        // One busy worker → first cell shows the 1-dot glyph, in the header.
        v.apply(&ev(1, execute_start("//a:b")));
        let header = format!("{}", v.render("⠋", 120, 120, 8).first().expect("header"));
        assert!(header.contains('⠁'), "expected 1-busy braille: {header}");
    }

    #[test]
    fn no_worker_braille_before_max_workers_event() {
        let v = TuiProgressView::new("L");
        let header = format!("{}", v.render("⠋", 0, 120, 8).first().expect("header"));
        for g in ['⠁', '⠃', '⠇', '⠧', '⠷', '⠿', '⡿', '⣿'] {
            assert!(!header.contains(g), "no braille expected: {header}");
        }
    }

    #[test]
    fn ci_view_event_folds_error_count() {
        let mut v = CiProgressView::new("Running //a:b");
        v.apply(&ev(1, result_start("//a:b")));
        v.apply(&ev(2, result_end("//a:b", Some("boom".into()))));
        // The failing ResultEnd bumps the errored counter; the message itself is
        // not retained — rich diagnostics come from the failure registry.
        assert_eq!(v.core.state.errored, 1);
    }

    #[test]
    fn human_bytes_formats_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024u64), "3.0 GiB");
    }

    fn gc_swept(revisions_removed: usize, bytes_removed: u64) -> BuildEventKind {
        BuildEventKind::GcTargetSwept {
            revisions_removed,
            bytes_removed,
        }
    }

    #[test]
    fn gc_header_folds_targets_revisions_bytes() {
        let core = BuildState::new();
        let mut h = GcHeader::new("GC");

        h.apply(&ev(0, gc_swept(2, 1024)));
        h.apply(&ev(1, gc_swept(0, 0))); // zero-removal target still counts as explored

        let seg = header_text(&h.header(&core, CountScope::Matched));
        assert!(seg.contains("2 targets"), "{seg}");
        assert!(seg.contains("1.0 KiB"), "{seg}");
        assert_eq!(h.label(), "GC");
        assert!(!h.last_render(&core, CountScope::Matched).is_empty());
    }

    #[test]
    fn build_header_segment_has_counts_and_braille_and_ignores_gc() {
        let mut core = BuildState::new();
        core.apply(&ev(0, max_workers(8)));
        core.apply(&ev(1, execute_start("//a:b")));
        // A gc event must not perturb the build counters.
        core.apply(&ev(2, gc_swept(9, 9)));

        let h = BuildHeader::new("L");
        let text = header_text(&h.header(&core, CountScope::Matched));
        assert!(text.contains("done"), "{text}");
        assert!(text.contains('⠁'), "expected worker braille: {text}");
    }

    #[test]
    fn has_activity_gates_final_summary() {
        // Empty view (e.g. an inspect/query run): no summary should print.
        let empty = TuiProgressView::new("Spec //a:b");
        assert!(!empty.state.has_activity());

        // Any observed event flips it on.
        let mut active = TuiProgressView::new("Running //a:b");
        active.apply(&ev(1, result_start("//a:b")));
        active.apply(&ev(2, result_end("//a:b", None)));
        assert!(active.state.has_activity());
    }

    fn matched(addrs: &[&str], complete: bool) -> BuildEventKind {
        BuildEventKind::Matched {
            addrs: addrs.iter().map(|s| (*s).to_string()).collect(),
            complete,
        }
    }

    /// The exit summary follows the `a` scope toggle: a user who switched the
    /// header to `All` reads the all-targets counts after teardown, not a
    /// hardcoded matched-scope line that disagrees with the header they were
    /// just looking at.
    #[test]
    fn final_summary_follows_scope_toggle() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:b"], true)));
        v.apply(&ev(1, result_start("//a:b")));
        v.apply(&ev(2, result_end("//a:b", None)));
        // A transitive dep: outside the matched set, counted only under `All`.
        v.apply(&ev(3, result_start("//dep:x")));
        v.apply(&ev(4, result_end("//dep:x", None)));

        // The production seam itself — `last_render` prints exactly this.
        let segment = |v: &TuiProgressView| v.final_summary_segment();
        assert!(segment(&v).contains("1 / 1 done"), "{}", segment(&v));
        assert!(
            !segment(&v).contains("all targets"),
            "matched scope must not be labeled: {}",
            segment(&v)
        );
        v.toggle_scope();
        assert!(segment(&v).contains("2 / 2 done"), "{}", segment(&v));
        // The line outlives the header, so the non-default scope names itself.
        assert!(segment(&v).ends_with("· all targets"), "{}", segment(&v));
        // Toggling back restores the matched-scope summary.
        v.toggle_scope();
        assert!(segment(&v).contains("1 / 1 done"), "{}", segment(&v));
    }

    #[test]
    fn matched_tracks_done_against_matched_set_only() {
        let mut s = BuildState::new();
        s.apply(&ev(0, matched(&["//a:b", "//c:d"], true)));
        assert_eq!(s.matched_total(), Some(2));
        assert!(s.has_activity(), "Matched alone counts as activity");

        // A transitive dep (not in the matched set) does not advance progress.
        s.apply(&ev(1, result_start("//dep:x")));
        s.apply(&ev(2, result_end("//dep:x", None)));
        assert!(s.summary().contains("matched 0 / 2"), "{}", s.summary());

        // A matched target completing advances it.
        s.apply(&ev(3, result_start("//a:b")));
        s.apply(&ev(4, result_end("//a:b", None)));
        assert!(s.summary().contains("matched 1 / 2"), "{}", s.summary());

        // A duplicate ResultEnd for the same matched addr is idempotent.
        s.apply(&ev(5, result_end("//a:b", None)));
        assert!(s.summary().contains("matched 1 / 2"), "{}", s.summary());
    }

    fn local_cache_hit(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheHit { addr: addr.into() }
    }

    fn remote_cache_hit(addr: &str) -> BuildEventKind {
        BuildEventKind::RemoteCacheHit { addr: addr.into() }
    }

    /// The two matched intersections computed the slow way, straight off the
    /// sets. The oracle for the counters folded in `apply`.
    fn scan_matched(s: &BuildState) -> (usize, usize) {
        (
            s.matched.iter().filter(|a| s.finished.contains(*a)).count(),
            s.matched
                .iter()
                .filter(|a| s.cache_hit.contains_key(*a))
                .count(),
        )
    }

    /// The same two intersections as the render path reports them.
    fn rendered_matched(s: &BuildState) -> (usize, usize) {
        let (done, _, _) = s.matched_progress().expect("a Matched event has landed");
        (done, s.cached_count(CountScope::Matched))
    }

    #[test]
    fn matched_counters_track_the_full_scan_under_out_of_order_events() {
        // `matched ∩ finished` and `matched ∩ cache_hit` are folded incrementally
        // rather than rescanned every frame, so they have to be maintained at
        // *both* edges of each intersection. `Engine::result` really does emit a
        // target's `ResultEnd` before the `Matched` event that names it, so a
        // counter folded only on the result edge undercounts. Pin both against a
        // full scan of the same sets, after every interleaving.
        let mut s = BuildState::new();
        let mut clock = 0u64;
        let mut apply = |s: &mut BuildState, kind| {
            clock += 1;
            s.apply(&ev(clock, kind));
        };

        // Edge 1: these finish (and one hits cache) BEFORE anything is matched.
        apply(&mut s, result_start("//early:a"));
        apply(&mut s, local_cache_hit("//early:a"));
        apply(&mut s, result_end("//early:a", None));
        apply(&mut s, result_start("//early:b"));
        apply(&mut s, result_end("//early:b", None));
        // A transitive dep that is never matched — proves these are
        // intersections, not running totals.
        apply(&mut s, result_start("//dep:x"));
        apply(&mut s, remote_cache_hit("//dep:x"));
        apply(&mut s, result_end("//dep:x", None));

        apply(
            &mut s,
            matched(
                &["//early:a", "//early:b", "//late:c", "//late:d", "//late:e"],
                false,
            ),
        );
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "matched arrives last"
        );
        assert_eq!(rendered_matched(&s), (2, 1));

        // Edge 2: these finish AFTER the Matched event that named them.
        apply(&mut s, result_start("//late:c"));
        apply(&mut s, remote_cache_hit("//late:c"));
        apply(&mut s, result_end("//late:c", None));
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "matched arrives first"
        );
        assert_eq!(rendered_matched(&s), (3, 2));

        // A cache hit on a matched target that has not finished: the cached
        // counter moves, the done counter does not.
        apply(&mut s, local_cache_hit("//late:d"));
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "cache hit, no result"
        );
        assert_eq!(rendered_matched(&s), (3, 3));

        // A matched target that *fails* still advances the done count — the
        // header reads `4 / 5 done · … · 1 failed`, and `finished` is recorded
        // outside the success branch precisely so it does. The scan oracle cannot
        // see a regression here (it reads the same `finished` set), so this one
        // needs the literal.
        apply(&mut s, result_start("//late:e"));
        apply(&mut s, result_end("//late:e", Some("boom".into())));
        assert_eq!(rendered_matched(&s), scan_matched(&s), "matched failure");
        assert_eq!(rendered_matched(&s), (4, 3));

        // Repeats on every path are idempotent: a duplicate `ResultEnd`, a second
        // cache hit on an already-cached addr, and a `Matched` event re-naming an
        // addr already in the set.
        apply(&mut s, result_end("//early:a", None));
        apply(&mut s, remote_cache_hit("//early:a"));
        apply(&mut s, matched(&["//early:a", "//late:c"], true));
        assert_eq!(rendered_matched(&s), scan_matched(&s), "duplicate events");
        assert_eq!(rendered_matched(&s), (4, 3));
    }

    #[test]
    fn count_scope_toggles_done_and_cached_between_matched_and_all() {
        let mut s = BuildState::new();
        // One matched target plus a transitive dep, both finishing with a cache
        // hit, and a third target still in flight.
        s.apply(&ev(0, matched(&["//a:b"], true)));
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, local_cache_hit("//a:b")));
        s.apply(&ev(3, result_end("//a:b", None)));
        s.apply(&ev(4, result_start("//dep:x")));
        s.apply(&ev(5, local_cache_hit("//dep:x")));
        s.apply(&ev(6, result_end("//dep:x", None)));
        s.apply(&ev(7, result_start("//run:y")));

        // Matched scope: only the matched target is counted.
        let (done, cached, _) = s.count_fields(CountScope::Matched);
        assert_eq!(done, "1 / 1 done", "matched done");
        assert_eq!(cached, "1 cached", "matched cached");

        // All scope: every observed target — both finished deps over the running
        // total (incl. the in-flight one), and both cache hits.
        let (done, cached, _) = s.count_fields(CountScope::All);
        assert_eq!(done, "2 / 3 done", "all done");
        assert_eq!(cached, "2 cached", "all cached");
    }

    /// The three count fields as the split header renders them, in header order.
    fn header_count_fields(s: &BuildState, scope: CountScope) -> (String, String, String) {
        let items = BuildHeader::new("L").header(s, scope);
        let text = |i: usize| header_text(&items[i..=i]);
        (text(0), text(1), text(2))
    }

    /// Shorthand for a `(done, cached, failed)` golden.
    fn fields(done: &str, cached: &str, failed: &str) -> (String, String, String) {
        (done.to_string(), cached.to_string(), failed.to_string())
    }

    #[test]
    fn split_header_count_fields_match_the_plain_count_fields() {
        // The header destructures `Counts` and splits the done segment into
        // tabs; `count_fields` renders the same `Counts` as flat strings for the
        // final summary. The two must stay character-identical in every scope and
        // in every branch of the done segment — the counts are computed once per
        // frame precisely so the header does not re-derive them.
        let mut s = BuildState::new();

        // Before any Matched event: the done segment falls back to `{executed} done`
        // with no denominator, and cached falls back to raw hit counts.
        s.apply(&ev(0, execute_start("//a:built")));
        s.apply(&ev(1, execute_end("//a:built")));
        s.apply(&ev(2, local_cache_hit("//a:cached")));
        s.apply(&ev(3, result_start("//a:failed")));
        s.apply(&ev(4, result_end("//a:failed", Some("boom".into()))));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(header_count_fields(&s, scope), s.count_fields(scope));
        }
        // The no-denominator branch is the one the done segment re-sources, so
        // pin it to a literal rather than only cross-checking it.
        assert_eq!(
            header_count_fields(&s, CountScope::Matched),
            fields("1 done", "1 cached", "1 failed"),
        );
        assert_eq!(
            header_count_fields(&s, CountScope::All),
            fields("1 / 1 done", "1 cached", "1 failed"),
        );

        // Provisional matched total (`~`): three matched, one finished, two of
        // them cached. The distinct cached/failed counts keep a transposition of
        // the two fields visible.
        s.apply(&ev(5, matched(&["//m:x", "//m:y", "//m:z"], false)));
        s.apply(&ev(6, result_start("//m:x")));
        s.apply(&ev(7, local_cache_hit("//m:x")));
        s.apply(&ev(8, local_cache_hit("//m:y")));
        s.apply(&ev(9, result_end("//m:x", None)));
        s.apply(&ev(10, result_start("//m:y")));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(header_count_fields(&s, scope), s.count_fields(scope));
        }
        assert_eq!(
            header_count_fields(&s, CountScope::Matched),
            fields("1 / ~3 done", "2 cached", "1 failed"),
        );

        // Matcher resolves: the `~` drops on both paths.
        s.apply(&ev(11, matched(&[], true)));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(header_count_fields(&s, scope), s.count_fields(scope));
        }
        assert_eq!(
            header_count_fields(&s, CountScope::Matched),
            fields("1 / 3 done", "2 cached", "1 failed"),
        );
    }

    #[test]
    fn toggle_scope_flips_view_count_scope() {
        let mut v = TuiProgressView::new("L");
        assert_eq!(v.scope.get(), CountScope::Matched);
        v.toggle_scope();
        assert_eq!(v.scope.get(), CountScope::All);
        v.toggle_scope();
        assert_eq!(v.scope.get(), CountScope::Matched);
    }

    #[test]
    fn matched_progress_is_order_independent() {
        // Regression: in `Engine::result` the spawned target tasks emit their
        // ResultEnd before the `Matched` event lands. Matched progress must
        // still reconcile against results that already finished.
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, result_end("//a:b", None)));
        s.apply(&ev(3, result_start("//c:d")));
        s.apply(&ev(4, result_end("//c:d", None)));
        // Matched arrives last, after both matched targets already finished.
        s.apply(&ev(5, matched(&["//a:b", "//c:d"], true)));
        assert!(s.summary().contains("matched 2 / 2"), "{}", s.summary());
    }

    #[test]
    fn matched_accumulates_incrementally_with_provisional_marker() {
        let mut s = BuildState::new();
        // Stream matches one at a time; total is provisional (`~`) meanwhile.
        s.apply(&ev(1, matched(&["//a:b"], false)));
        assert!(s.summary().contains("matched 0 / ~1"), "{}", s.summary());
        s.apply(&ev(2, matched(&["//c:d"], false)));
        assert_eq!(s.matched_total(), Some(2));
        assert!(s.summary().contains("matched 0 / ~2"), "{}", s.summary());

        // Final marker drops the `~` without adding addrs.
        s.apply(&ev(3, matched(&[], true)));
        assert!(s.summary().contains("matched 0 / 2"), "{}", s.summary());
        assert!(!s.summary().contains('~'), "{}", s.summary());
    }

    #[test]
    fn render_fits_within_progress_rows_and_shows_scroll_indicator() {
        let mut v = TuiProgressView::new("Running //x:y");
        // 20 long-running targets, all started at 0.
        for i in 0..20 {
            v.apply(&ev(0, execute_start(&format!("//pkg:t{i}"))));
        }
        let height = 8u16;
        let lines = v.render("⠋", 10_000, 100, height);
        // The box fills exactly the rows it was given.
        assert_eq!(lines.len(), usize::from(height));
        // The bottom border (second-to-last row) carries the label and scroll indicator.
        let footer = format!("{}", lines[lines.len() - 2]);
        assert!(footer.contains("Running //x:y"), "{footer}");
        // 20 slow rows can't fit the small body, so the scroll indicator appears.
        assert!(
            footer.contains("↑ 1–5 of 20 ↓"),
            "expected scroll indicator in footer, got {footer}"
        );
    }

    #[test]
    fn human_elapsed_starts_compact_then_grows_monotonically() {
        // Seconds band is natural width (compact start).
        assert_eq!(human_elapsed(9_000), "9s");
        assert_eq!(human_elapsed(59_000), "59s");
        // Past a minute, width is padded so the field never shrinks.
        for ms in [65_000, 3_600_000, 90_000_000] {
            assert!(human_elapsed(ms).chars().count() >= ELAPSED_MIN_WIDTH);
        }
        // Width is non-decreasing as time advances across band boundaries.
        let mut prev = 0usize;
        for secs in [0u64, 30, 59, 60, 600, 3_599, 3_600, 86_399, 86_400] {
            let w = human_elapsed(secs * 1_000).chars().count();
            assert!(w >= prev, "width shrank at {secs}s: {prev} → {w}");
            prev = w;
        }
        assert!(human_elapsed(65_000).contains("1m05s"));
        assert!(human_elapsed(3_725_000).contains("1h02m"));
        assert!(human_elapsed(90_000_000).contains("1d01h"));
    }

    #[test]
    fn human_elapsed_ms_keeps_millis_under_a_minute() {
        assert_eq!(human_elapsed_ms(9_123), "9.123s");
        assert_eq!(human_elapsed_ms(59_007), "59.007s");
        assert_eq!(human_elapsed_ms(500), "0.500s");
        // Past a minute it falls back to the coarse seconds band.
        assert_eq!(human_elapsed_ms(65_000), human_elapsed(65_000));
    }

    #[test]
    fn elapsed_anchors_to_first_event() {
        let mut s = BuildState::new();
        assert_eq!(s.elapsed_ms(5_000), 0, "no anchor before any event");
        s.apply(&ev(2_000, result_start("//a:b")));
        // Later events do not move the anchor backward or forward.
        s.apply(&ev(4_000, result_end("//a:b", None)));
        assert_eq!(s.elapsed_ms(9_000), 7_000);
    }

    #[test]
    fn slow_targets_replace_idle_field_in_body() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//slow:x")));
        let lines = v.render("⠋", 10_000, 80, 8);
        let body: String = lines[1..lines.len() - 2]
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("//slow:x"), "{body}");
    }

    #[test]
    fn worker_spans_map_busy_count_to_braille() {
        // 8 workers = one cell; busy count selects the glyph.
        let glyph = |busy: usize| -> String {
            worker_spans(8, busy)
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(glyph(0), "⣿"); // idle (painted grey by caller)
        assert_eq!(glyph(1), "⠁");
        assert_eq!(glyph(6), "⣧");
        assert_eq!(glyph(8), "⣿");

        // Idle cell is dim dark grey, busy cell is blue.
        assert_eq!(worker_spans(8, 0)[0].style.fg, Some(Color::DarkGray));
        assert_eq!(worker_spans(8, 3)[0].style.fg, Some(Color::Blue));

        // 10 workers spread over two cells; busy fills left-to-right, so 10 busy
        // fills the first cell (8) and the second (2 → ⠃).
        let cells = worker_spans(10, 10);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].content, "⣿");
        assert_eq!(cells[1].content, "⠃");
        // With only 8 busy, the first cell is full (blue) and the second is idle.
        // The idle trailing cell shows only its real capacity (2 → ⠃ grey), not a
        // full ⣿ that would misread as 8 more workers.
        let partial = worker_spans(10, 8);
        assert_eq!(partial[0].content, "⣿");
        assert_eq!(partial[1].content, "⠃");
        assert_eq!(partial[1].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn worker_spans_render_full_capacity_when_all_idle() {
        // 10 workers, none busy: two grey cells totalling 10 slots (8 + 2), not
        // two full ⣿ cells (which would read as 16).
        let idle = worker_spans(10, 0);
        assert_eq!(idle.len(), 2);
        assert_eq!(idle[0].content, "⣿"); // full cell: 8 slots
        assert_eq!(idle[1].content, "⠃"); // partial cell: 2 slots
        assert!(idle.iter().all(|c| c.style.fg == Some(Color::DarkGray)));

        // 3 busy: first cell shows 3 blue dots, trailing cell stays 2 grey.
        let active = worker_spans(10, 3);
        assert_eq!(active[0].content, "⠇");
        assert_eq!(active[0].style.fg, Some(Color::Blue));
        assert_eq!(active[1].content, "⠃");
        assert_eq!(active[1].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn banner_scrolls_when_label_overflows_window() {
        let label = "a-really-long-label-that-will-not-fit-in-a-narrow-window";
        // Narrow box forces scrolling; window = width - 7.
        let window = 80usize - 7;
        let a = banner_slice(label, window, 0);
        let b = banner_slice(label, window, SCROLL_MS); // one column later
        assert_eq!(a.chars().count(), window);
        assert_eq!(b.chars().count(), window);
        assert_ne!(a, b, "banner must advance over time");
    }

    #[test]
    fn bottom_line_total_width_matches_terminal_width() {
        // Short label (padded with dashes) and long label (scrolled) both fill W.
        let short = TuiProgressView::new("ok");
        let long =
            TuiProgressView::new("a-really-long-label-that-overflows-the-available-window-area");
        for w in [40u16, 80, 120] {
            // No scroll: total=0, body_rows=0 → indicator never triggers.
            let s = format!("{}", short.bottom_line(0, w, 0, 0, 0));
            let l = format!("{}", long.bottom_line(0, w, 0, 0, 0));
            assert_eq!(s.chars().count(), usize::from(w), "short @ {w}: {s}");
            assert_eq!(l.chars().count(), usize::from(w), "long @ {w}: {l}");
        }
        // With scroll active: the indicator replaces trailing dashes and the
        // total width still matches.
        for w in [40u16, 80, 120] {
            let s = format!("{}", short.bottom_line(0, w, 0, 75, 20));
            let l = format!("{}", long.bottom_line(0, w, 55, 100, 20));
            assert_eq!(s.chars().count(), usize::from(w), "short scroll @ {w}: {s}");
            assert_eq!(l.chars().count(), usize::from(w), "long scroll @ {w}: {l}");
        }
    }

    #[test]
    fn rows_for_height_is_one_third_clamped_to_min() {
        assert_eq!(rows_for_height(30), 10);
        assert_eq!(rows_for_height(24), 8);
        // Tiny terminals clamp up to the minimum box height.
        assert_eq!(rows_for_height(3), MIN_PROGRESS_ROWS);
        assert_eq!(rows_for_height(0), MIN_PROGRESS_ROWS);
    }

    #[test]
    fn render_fills_exactly_the_given_height() {
        let v = TuiProgressView::new("L");
        for h in [MIN_PROGRESS_ROWS, 8, 20] {
            let lines = v.render("⠋", 0, 80, h);
            assert_eq!(lines.len(), usize::from(h), "height {h}");
        }
        // Below the minimum the height is clamped up, never fewer rows.
        let lines = v.render("⠋", 0, 80, 1);
        assert_eq!(lines.len(), usize::from(MIN_PROGRESS_ROWS));
    }

    #[test]
    fn help_row_is_pinned_last_and_dim() {
        let v = TuiProgressView::new("L");
        let lines = v.render("⠋", 0, 80, 8);
        let help = lines.last().expect("help line");
        let text: String = help.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("scroll"), "{text}");
        assert_eq!(help.spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn help_advertises_search_only_on_list_tabs() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:ok"], true)));
        v.apply(&ev(1, result_start("//a:ok")));
        v.apply(&ev(2, result_end("//a:ok", None)));

        let help = |v: &TuiProgressView| -> String {
            format!("{}", v.render("⠋", 10_000, 80, 8).last().expect("help"))
        };

        // Default (live) view: `/` has no list to filter, so it isn't advertised.
        assert_eq!(v.view.get(), ViewMode::Default);
        assert!(!help(&v).contains("/ search"), "{}", help(&v));

        // Done tab: search is available, so the hint appears.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Done);
        assert!(help(&v).contains("/ search"), "{}", help(&v));
    }

    fn lock_wait_start(addr: &str, pid: u32) -> BuildEventKind {
        BuildEventKind::ResultLockWaitStart {
            addr: addr.into(),
            holder_pid: Some(pid),
        }
    }

    #[test]
    fn scroll_indicator_covers_locks_and_slow_together() {
        // 2 lock waits + 4 slow targets = 6 body rows. The scroll indicator
        // must reflect the combined total, not just slow overflow.
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, lock_wait_start("//l:1", 1)));
        v.apply(&ev(0, lock_wait_start("//l:2", 2)));
        for i in 0..4 {
            v.apply(&ev(0, execute_start(&format!("//s:{i}"))));
        }
        // height 7 → body_rows = 4. 6 items > 4 ⇒ scroll indicator shows 1–4 of 6.
        let lines = v.render("⠋", 10_000, 80, 7);
        let footer = format!("{}", lines[lines.len() - 2]);
        assert!(
            footer.contains("↑ 1–4 of 6 ↓"),
            "expected scroll indicator, got {footer}"
        );
    }

    #[test]
    fn failed_lines_list_failed_targets_with_error() {
        let mut s = BuildState::new();
        s.apply(&ev(0, result_start("//a:ok")));
        s.apply(&ev(1, result_end("//a:ok", None)));
        s.apply(&ev(2, result_start("//a:bad")));
        s.apply(&ev(3, result_end("//a:bad", Some("boom".into()))));
        s.apply(&ev(4, result_start("//a:bad2")));
        s.apply(&ev(5, result_end("//a:bad2", Some("kaput".into()))));

        let lines = all_lines(s.failed_rows(""));
        // Only the two errored targets appear, in failure order.
        assert_eq!(lines.len(), 2);
        let l0 = format!("{}", lines[0]);
        assert!(l0.contains("//a:bad"), "{l0}");
        assert!(l0.contains("boom"), "{l0}");
        let l1 = format!("{}", lines[1]);
        assert!(l1.contains("//a:bad2"), "{l1}");
        assert!(!format!("{}{}", l0, l1).contains("//a:ok"));
    }

    #[test]
    fn header_items_are_freestanding_without_separators() {
        // Each header item must carry no ` · ` — the view owns the joins.
        let mut core = BuildState::new();
        core.apply(&ev(0, max_workers(8)));
        core.apply(&ev(1, execute_start("//a:b")));
        let h = BuildHeader::new("L");
        for item in h.header(&core, CountScope::Matched) {
            let text: String = item.spans().iter().map(|s| s.content.to_string()).collect();
            assert!(!text.contains('·'), "item baked in a separator: {text}");
        }
    }

    #[test]
    fn tab_cycles_default_done_matched_cached_failed_and_back() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//slow:x")));
        v.apply(&ev(1, matched(&["//a:ok", "//a:pending"], true)));
        v.apply(&ev(2, result_start("//a:ok")));
        v.apply(&ev(3, local_cache_hit("//a:ok")));
        v.apply(&ev(4, result_end("//a:ok", None)));
        v.apply(&ev(5, result_start("//a:bad")));
        v.apply(&ev(6, result_end("//a:bad", Some("boom".into()))));

        // Default view shows the slow row, not the list targets.
        let body = |v: &TuiProgressView| -> String {
            v.render("⠋", 10_000, 80, 8)
                .iter()
                .map(|l| format!("{l}"))
                .collect()
        };
        assert_eq!(v.view.get(), ViewMode::Default);
        assert!(body(&v).contains("//slow:x"));

        // Tab → done view: shows the completed target.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Done);
        let done_body = body(&v);
        assert!(done_body.contains("//a:ok"), "{done_body}");
        assert!(!done_body.contains("//slow:x"), "{done_body}");

        // Tab → matched view: shows every matched target (done + pending).
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Matched);
        let matched_body = body(&v);
        assert!(matched_body.contains("//a:ok"), "{matched_body}");
        assert!(matched_body.contains("//a:pending"), "{matched_body}");

        // Tab → cached view: shows the cache-hit target only.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Cached);
        let cached_body = body(&v);
        assert!(cached_body.contains("//a:ok"), "{cached_body}");
        assert!(!cached_body.contains("//a:pending"), "{cached_body}");

        // Tab → failed view: shows the failed target.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Failed);
        let failed_body = body(&v);
        assert!(failed_body.contains("//a:bad"), "{failed_body}");
        assert!(!failed_body.contains("//slow:x"), "{failed_body}");

        // Tab again wraps back to default.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Default);
        assert!(body(&v).contains("//slow:x"));
    }

    #[test]
    fn done_and_failed_lines_filter_by_addr_substring() {
        let mut s = BuildState::new();
        s.apply(&ev(0, result_start("//web:server")));
        s.apply(&ev(1, result_end("//web:server", None)));
        s.apply(&ev(2, result_start("//api:server")));
        s.apply(&ev(3, result_end("//api:server", None)));
        s.apply(&ev(4, result_start("//web:bad")));
        s.apply(&ev(5, result_end("//web:bad", Some("boom".into()))));
        s.apply(&ev(6, result_start("//api:bad")));
        s.apply(&ev(7, result_end("//api:bad", Some("kaput".into()))));

        let join =
            |ls: Vec<Line<'static>>| -> String { ls.iter().map(|l| format!("{l}")).collect() };

        // Empty filter keeps everything.
        assert_eq!(all_lines(s.done_rows(CountScope::All, "")).len(), 2);
        assert_eq!(all_lines(s.failed_rows("")).len(), 2);

        // Substring filter, case-insensitive, matches only the package prefix.
        let done = join(all_lines(s.done_rows(CountScope::All, "WEB")));
        assert!(done.contains("//web:server"), "{done}");
        assert!(!done.contains("//api:server"), "{done}");

        let failed = join(all_lines(s.failed_rows("api")));
        assert!(failed.contains("//api:bad"), "{failed}");
        assert!(!failed.contains("//web:bad"), "{failed}");
    }

    #[test]
    fn slash_filters_done_tab_enter_keeps_esc_clears() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//web:server", "//api:server"], true)));
        v.apply(&ev(1, result_start("//web:server")));
        v.apply(&ev(2, result_end("//web:server", None)));
        v.apply(&ev(3, result_start("//api:server")));
        v.apply(&ev(4, result_end("//api:server", None)));

        let body = |v: &TuiProgressView| -> String {
            v.render("⠋", 10_000, 80, 8)
                .iter()
                .map(|l| format!("{l}"))
                .collect()
        };

        // `/` on the Default view is a no-op (search has no list to filter).
        v.search_start();
        assert!(!v.is_searching());

        // Move to Done, then open the filter and type "web".
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Done);
        v.search_start();
        assert!(v.is_searching());
        v.search_input('w');
        v.search_input('e');
        v.search_input('b');
        let filtered = body(&v);
        assert!(filtered.contains("//web:server"), "{filtered}");
        assert!(!filtered.contains("//api:server"), "{filtered}");

        // Enter confirms: input mode ends but the filter stays applied.
        v.search_confirm();
        assert!(!v.is_searching());
        let still = body(&v);
        assert!(still.contains("//web:server"), "{still}");
        assert!(!still.contains("//api:server"), "{still}");

        // Esc clears the filter; the full list returns.
        v.search_cancel();
        let cleared = body(&v);
        assert!(cleared.contains("//web:server"), "{cleared}");
        assert!(cleared.contains("//api:server"), "{cleared}");
    }

    #[test]
    fn backspace_edits_query_and_tab_switch_drops_filter() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//web:server", "//api:server"], true)));
        v.apply(&ev(1, result_start("//web:server")));
        v.apply(&ev(2, result_end("//web:server", None)));
        v.apply(&ev(3, result_start("//api:server")));
        v.apply(&ev(4, result_end("//api:server", None)));

        let body = |v: &TuiProgressView| -> String {
            v.render("⠋", 10_000, 80, 8)
                .iter()
                .map(|l| format!("{l}"))
                .collect()
        };

        v.tab(true); // Done
        v.search_start();
        v.search_input('a');
        v.search_input('p');
        v.search_input('z'); // typo: matches nothing
        assert!(body(&v).contains("no targets match"));
        v.search_backspace(); // drop the 'z' → "ap"
        let fixed = body(&v);
        assert!(fixed.contains("//api:server"), "{fixed}");
        assert!(!fixed.contains("//web:server"), "{fixed}");

        // Switching tabs drops the filter and exits search input.
        v.tab(true); // → Matched
        assert!(!v.is_searching());
        // Return to the Done tab (Default → Done) and confirm the full list.
        v.back_to_main();
        v.tab(true); // Done again — full list, no filter
        let full = body(&v);
        assert!(full.contains("//web:server"), "{full}");
        assert!(full.contains("//api:server"), "{full}");
    }

    #[test]
    fn hscroll_pans_wide_body_lines_and_clamps() {
        let mut v = TuiProgressView::new("L");
        // A slow target whose addr is far wider than the viewport.
        let long = format!("//pkg:{}", "x".repeat(200));
        v.apply(&ev(0, execute_start(&long)));

        let body_at = |v: &TuiProgressView, w: u16| -> String {
            v.render("⠋", 10_000, w, 8)[1..]
                .iter()
                .take(1)
                .map(|l| format!("{l}"))
                .collect()
        };

        let width = 40u16;
        // At pan 0 the row starts with the indent + addr head.
        let row0 = body_at(&v, width);
        assert!(row0.contains("//pkg:xxx"), "{row0}");

        // Pan right: the head columns are dropped.
        v.hscroll(20);
        let row1 = body_at(&v, width);
        assert!(!row1.contains("//pkg:xxx"), "{row1}");
        assert!(row1.contains('x'), "{row1}");

        // Pan back left to the origin restores the head.
        v.hscroll(-1000);
        assert!(body_at(&v, width).contains("//pkg:xxx"));

        // Pan far right clamps so the tail never scrolls off the right edge:
        // the rendered row stays non-empty (the addr's tail is still visible).
        v.hscroll(100_000);
        let clamped = body_at(&v, width);
        // The row's tail is the op breakdown group, e.g. `(▶ 10s)`.
        assert!(clamped.trim().ends_with(')'), "{clamped}");
        assert!(!clamped.trim().is_empty(), "{clamped}");
    }

    #[test]
    fn switching_view_resets_horizontal_pan() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, result_start("//a:bad")));
        v.apply(&ev(1, result_end("//a:bad", Some("boom".into()))));
        v.hscroll(50);
        v.tab(true);
        assert_eq!(v.hscroll.get(), 0);
    }

    #[test]
    fn hold_after_finish_only_off_main_view() {
        let mut v = TuiProgressView::new("L");
        // On the default (main) view: auto-exit, no hold.
        assert!(!v.hold_after_finish());
        // On another tab: hold the viewport open.
        v.tab(true);
        assert!(v.hold_after_finish());
    }

    #[test]
    fn finished_shows_green_quit_notice() {
        use crate::tui::app::TUIAppView;
        let mut v = TuiProgressView::new("L");
        // Before finishing, the help row lists the key bindings.
        let help = format!("{}", v.render("⠋", 0, 80, 8).last().expect("help"));
        assert!(help.contains("scroll"), "{help}");
        assert!(!help.contains("quit"), "{help}");

        v.set_finished();
        let lines = v.render("⠋", 0, 80, 8);
        let help = lines.last().expect("help");
        let text: String = help.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("finished"), "{text}");
        assert!(text.contains('q'), "{text}");
        // The notice is green so it stands out.
        assert_eq!(help.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn elapsed_clock_freezes_after_finish() {
        use crate::tui::app::TUIAppView;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//a:b"))); // anchor the clock at t=0

        // Live: two renders at different wall clocks show different elapsed.
        let h = |v: &TuiProgressView, now: u64| format!("{}", v.render("⠋", now, 80, 8)[0]);
        assert_ne!(h(&v, 5_000), h(&v, 9_000));

        // Frozen: after finish the header is identical regardless of `now_ms`.
        v.set_finished();
        assert_eq!(h(&v, 100_000), h(&v, 999_000));
    }

    #[test]
    fn shift_tab_cycles_backwards() {
        let mut v = TuiProgressView::new("L");
        // A matched event gives the `X / Y done` denominator, so the Matched tab
        // is present and all five modes exist.
        v.apply(&ev(0, matched(&["//a:ok"], true)));
        v.apply(&ev(1, result_start("//a:bad")));
        v.apply(&ev(2, result_end("//a:bad", Some("boom".into()))));

        // Five modes (Default, Done, Matched, Cached, Failed): backward from
        // Default wraps to the last (Failed), then steps back to Default.
        assert_eq!(v.view.get(), ViewMode::Default);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Failed);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Cached);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Matched);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Done);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Default);
    }

    #[test]
    fn failed_tab_is_highlighted_when_active() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, result_start("//a:bad")));
        v.apply(&ev(1, result_end("//a:bad", Some("boom".into()))));

        // Inactive: the "failed" segment carries no background.
        let header = v.render("⠋", 100, 120, 8);
        let plain = header[0]
            .spans
            .iter()
            .find(|s| s.content.contains("failed"))
            .expect("failed span");
        assert_eq!(plain.style.bg, None);

        // Active (failed view): the segment is highlighted with a background.
        // Failed is the last tab; a single shift-tab lands on it regardless of
        // how many middle tabs the current state exposes.
        v.tab(false); // → Failed
        let header = v.render("⠋", 100, 120, 8);
        let hl = header[0]
            .spans
            .iter()
            .find(|s| s.content.contains("failed"))
            .expect("failed span");
        assert_eq!(hl.style.bg, Some(Color::Blue));
    }

    #[test]
    fn failed_view_with_no_failures_shows_placeholder() {
        let mut v = TuiProgressView::new("L");
        v.tab(false); // → Failed (last tab), but nothing has failed
        let body: String = v
            .render("⠋", 10_000, 80, 8)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("no failed targets"), "{body}");
    }

    #[test]
    fn done_view_with_nothing_done_shows_placeholder() {
        let mut v = TuiProgressView::new("L");
        v.tab(true); // → Done, but nothing has completed
        assert_eq!(v.view.get(), ViewMode::Done);
        let body: String = v
            .render("⠋", 10_000, 80, 8)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("no completed targets"), "{body}");
    }

    #[test]
    fn done_lines_respect_matched_and_all_scope() {
        let mut s = BuildState::new();
        // Two matched top-level targets; one transitive dep finishes too.
        s.apply(&ev(0, matched(&["//a:top"], false)));
        s.apply(&ev(1, result_start("//a:top")));
        s.apply(&ev(2, result_end("//a:top", None)));
        s.apply(&ev(3, result_start("//dep:lib")));
        s.apply(&ev(4, result_end("//dep:lib", None)));
        // A failed target must never appear in the done list.
        s.apply(&ev(5, result_start("//a:bad")));
        s.apply(&ev(6, result_end("//a:bad", Some("boom".into()))));

        // Matched scope: only the matched top-level target.
        let matched_scope: String = all_lines(s.done_rows(CountScope::Matched, ""))
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(matched_scope.contains("//a:top"), "{matched_scope}");
        assert!(!matched_scope.contains("//dep:lib"), "{matched_scope}");
        assert!(!matched_scope.contains("//a:bad"), "{matched_scope}");

        // All scope: every completed target, deps included, still no failures.
        let all_scope: String = all_lines(s.done_rows(CountScope::All, ""))
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(all_scope.contains("//a:top"), "{all_scope}");
        assert!(all_scope.contains("//dep:lib"), "{all_scope}");
        assert!(!all_scope.contains("//a:bad"), "{all_scope}");
    }

    #[test]
    fn scroll_advances_the_body_window_and_clamps() {
        let mut v = TuiProgressView::new("L");
        for i in 0..6 {
            v.apply(&ev(0, execute_start(&format!("//s:{i}"))));
        }
        // body_rows = 4, 6 slow rows, max_scroll = 2.
        // Scroll past the end; render clamps to the bottom ⇒ indicator shows 3–6.
        v.scroll(10);
        let lines = v.render("⠋", 10_000, 80, 7);
        let bottom = format!("{}", lines[lines.len() - 2]);
        assert!(
            bottom.contains("↑ 3–6 of 6 ↓"),
            "indicator at bottom: {bottom}"
        );
        // Back to the top: indicator shows 1–4.
        v.scroll(-10);
        let lines = v.render("⠋", 10_000, 80, 7);
        let bottom = format!("{}", lines[lines.len() - 2]);
        assert!(
            bottom.contains("↑ 1–4 of 6 ↓"),
            "indicator at top: {bottom}"
        );
    }

    #[test]
    fn approval_banner_shows_pending_count() {
        let center = crate::tui::approval::ApprovalCenter::new();
        // Two targets awaiting approval: the banner shows the active one plus the
        // total pending count.
        let _r1 = center.request("//a:1".to_string(), vec![]);
        let _r2 = center.request("//a:2".to_string(), vec![]);
        let view = TuiProgressView::new("run").with_approval(center);
        let lines = view.approval_lines();
        let banner: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(banner.contains("//a:1"), "active prompt: {banner}");
        assert!(banner.contains("2 pending"), "count: {banner}");
    }

    #[test]
    fn approval_banner_single_has_no_pending_count() {
        let center = crate::tui::approval::ApprovalCenter::new();
        let _r = center.request("//a:1".to_string(), vec![]);
        let view = TuiProgressView::new("run").with_approval(center);
        let lines = view.approval_lines();
        let banner: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!banner.contains("pending"), "no count for one: {banner}");
    }

    /// The `X / Y done` header splits into two independent tabs: the Done view
    /// highlights `X` (finished count), the Matched view highlights `Y` (total).
    #[test]
    fn done_segment_highlights_x_on_done_and_y_on_matched() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:x", "//a:y", "//a:z"], true)));
        v.apply(&ev(1, result_start("//a:x")));
        v.apply(&ev(2, result_end("//a:x", None)));

        // Background colour of the header span whose content is exactly `content`.
        let bg_of = |v: &TuiProgressView, content: &str| -> Option<Color> {
            v.render("⠋", 100, 120, 8)[0]
                .spans
                .iter()
                .find(|s| s.content == content)
                .and_then(|s| s.style.bg)
        };

        // Default view: neither the count nor the total is highlighted.
        assert_eq!(bg_of(&v, "1"), None);
        assert_eq!(bg_of(&v, "3"), None);

        // Done view highlights `X` (the "1"), leaving `Y` plain.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Done);
        assert_eq!(bg_of(&v, "1"), Some(Color::Blue));
        assert_eq!(bg_of(&v, "3"), None);

        // Matched view highlights `Y` (the "3"), leaving `X` plain.
        v.tab(true);
        assert_eq!(v.view.get(), ViewMode::Matched);
        assert_eq!(bg_of(&v, "3"), Some(Color::Blue));
        assert_eq!(bg_of(&v, "1"), None);
    }

    #[test]
    fn matched_view_lists_all_matched_targets_and_filters() {
        let mut s = BuildState::new();
        s.apply(&ev(0, matched(&["//a:done", "//a:pending"], true)));
        s.apply(&ev(1, result_start("//a:done")));
        s.apply(&ev(2, result_end("//a:done", None)));

        // Both the finished and the still-pending matched target are listed.
        let lines = all_lines(s.matched_rows(""));
        assert_eq!(lines.len(), 2);
        let joined: String = lines.iter().map(|l| format!("{l}")).collect();
        assert!(joined.contains("//a:done"), "{joined}");
        assert!(joined.contains("//a:pending"), "{joined}");

        // Filter narrows to the matching addr.
        let filtered = all_lines(s.matched_rows("pending"));
        assert_eq!(filtered.len(), 1);
        assert!(format!("{}", filtered[0]).contains("//a:pending"));
    }

    #[test]
    fn cached_tab_highlights_and_lists_cache_hits_by_scope() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:hit"], true)));
        v.apply(&ev(1, local_cache_hit("//a:hit")));
        v.apply(&ev(2, result_end("//a:hit", None)));
        // A transitive dep also hits cache — only in the `All` scope.
        v.apply(&ev(3, local_cache_hit("//dep:hit")));

        v.tab(true); // Done
        v.tab(true); // Matched
        v.tab(true); // Cached
        assert_eq!(v.view.get(), ViewMode::Cached);

        // The cached segment is highlighted while its view is active.
        let header = v.render("⠋", 100, 120, 8);
        let hl = header[0]
            .spans
            .iter()
            .find(|s| s.content.contains("cached"))
            .expect("cached span");
        assert_eq!(hl.style.bg, Some(Color::Blue));

        // Matched scope (default) lists only the matched hit.
        let body: String = v
            .render("⠋", 10_000, 80, 8)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("//a:hit"), "{body}");
        assert!(!body.contains("//dep:hit"), "{body}");
    }

    #[test]
    fn cached_lines_respect_matched_and_all_scope() {
        let mut s = BuildState::new();
        s.apply(&ev(0, matched(&["//a:top"], true)));
        s.apply(&ev(1, local_cache_hit("//a:top")));
        s.apply(&ev(2, local_cache_hit("//dep:lib")));

        let m: String = all_lines(s.cached_rows(CountScope::Matched, ""))
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(m.contains("//a:top"), "{m}");
        assert!(!m.contains("//dep:lib"), "{m}");

        let a: String = all_lines(s.cached_rows(CountScope::All, ""))
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(a.contains("//a:top"), "{a}");
        assert!(a.contains("//dep:lib"), "{a}");
    }

    #[test]
    fn back_to_main_returns_from_list_tab_and_resets_filter() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//web:server", "//api:server"], true)));
        v.apply(&ev(1, result_start("//web:server")));
        v.apply(&ev(2, result_end("//web:server", None)));

        v.tab(true); // Done
        assert!(!v.is_on_main_view());
        v.search_start();
        v.search_input('w');
        assert!(v.has_active_filter());

        // Esc/q off the main view route here: back to Default, filter cleared.
        v.back_to_main();
        assert!(v.is_on_main_view());
        assert_eq!(v.view.get(), ViewMode::Default);
        assert!(!v.has_active_filter());
        assert!(!v.is_searching());
        assert_eq!(v.scroll.get(), 0);
    }

    #[test]
    fn addr_filter_folds_ascii_case_without_allocating() {
        assert!(addr_matches("//WEB:Server", "web"));
        assert!(addr_matches("//web:server", "WEB:SERVER"));
        assert!(addr_matches("//web:server", "//web:server"));
        assert!(!addr_matches("//web:server", "api"));
        // A filter longer than the addr can never match.
        assert!(!addr_matches("//a:b", "//a:b:c"));
        // The empty filter is the unfiltered list.
        assert!(addr_matches("//a:b", ""));
        // Non-ASCII bytes compare as-is — exactly what ASCII lower-casing did.
        assert!(addr_matches("//pkg/ünï:t", "ünï"));
        assert!(!addr_matches("//pkg/ünï:t", "ÜNÏ"));
    }

    #[test]
    fn body_row_width_matches_the_rendered_line() {
        // `width` drives the horizontal-pan clamp, `render` drives the pixels.
        // They are computed two different ways and must not drift.
        for row in [
            BodyRow {
                addr: "//a:b",
                color: Color::Green,
                detail: None,
            },
            BodyRow {
                addr: "//pkg/ünïcødé:tgt",
                color: Color::White,
                detail: None,
            },
            BodyRow {
                addr: "//a:b",
                color: Color::Red,
                detail: Some("boom: exit status 1"),
            },
            // An empty message still renders its (empty) detail span.
            BodyRow {
                addr: "//a:b",
                color: Color::Red,
                detail: Some(""),
            },
        ] {
            assert_eq!(row.width(), spans_width(&row.render().spans), "{row:?}");
        }
    }

    /// `n` matched targets that all finished and hit cache, plus `n` failed
    /// ones — so every list tab has exactly `n` rows. Addrs are zero-padded so
    /// their sort order is not their insertion order, which is what makes the
    /// partial selection observable.
    fn list_state(n: usize) -> BuildState {
        let mut s = BuildState::new();
        let ok: Vec<String> = (0..n).map(|i| format!("//ok/p{i:06}:t{i}")).collect();
        s.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: ok.clone(),
                complete: true,
            },
        ));
        for addr in &ok {
            s.apply(&ev(1, result_start(addr)));
            s.apply(&ev(2, result_end(addr, None)));
            s.apply(&ev(3, local_cache_hit(addr)));
        }
        for i in 0..n {
            let addr = format!("//bad/p{i:06}:t{i}");
            s.apply(&ev(4, result_start(&addr)));
            s.apply(&ev(5, result_end(&addr, Some(format!("boom {i}")))));
        }
        s
    }

    /// Builds one list tab's rows off a state. A fn pointer so each tab can be
    /// called twice — once for the reference list, once for the windowed one.
    type TabRows = for<'a> fn(&'a BuildState) -> BodyRows<'a>;

    const LIST_TABS: [(&str, TabRows); 4] = [
        ("done", |s| s.done_rows(CountScope::All, "")),
        ("matched", |s| s.matched_rows("")),
        ("cached", |s| s.cached_rows(CountScope::All, "")),
        ("failed", |s| s.failed_rows("")),
    ];

    #[test]
    fn windowing_a_list_matches_the_fully_ordered_list_at_every_offset() {
        // The correctness bar for clipping before rendering: at any viewport
        // position, the rows shown must be exactly the ones the full-list build
        // would have put there, and the clamped scroll offset must agree.
        // Empty and single-row lists are in here because they are the sizes the
        // two partitions have to decline to run at all.
        for n in [0usize, 1, 137] {
            let s = list_state(n);
            for (name, build) in LIST_TABS {
                let reference = all_lines(build(&s));
                assert_eq!(reference.len(), n, "{name}");
                for rows in [0usize, 1, 7, 20, 136, 137, 200] {
                    for scroll in 0..=(reference.len() + 5) {
                        let (want, want_off) = windowed(reference.clone(), rows, scroll);
                        let (got, got_off) = build(&s).window(rows, scroll);
                        assert_eq!(
                            got_off, want_off,
                            "{name} n={n} rows={rows} scroll={scroll}"
                        );
                        assert_eq!(got, want, "{name} n={n} rows={rows} scroll={scroll}");
                    }
                }
            }
        }
    }

    #[test]
    fn placing_a_window_selects_rather_than_sorting_the_whole_list() {
        // The headline of this change is that the ordered tabs stop sorting a
        // list they are about to throw away. That is invisible in the rendered
        // output and allocates nothing, so nothing else here would notice it
        // being undone — count the comparisons instead. A full sort of n
        // elements costs about n·log2(n); the two partitions plus a 20-row sort
        // are linear. At n = 50k that is ~780k against ~150k, so a bound of 8n
        // separates them with room to spare and never needs a clock.
        let n = 50_000usize;
        // Scrambled, not reversed: pdqsort spots a reversed run and flips it in
        // O(n), which would let a full sort slip under the bound.
        let mut rows: Vec<u32> = (0..n as u32)
            .map(|i| i.wrapping_mul(2_654_435_761))
            .collect();
        let mut want = rows.clone();
        want.sort_unstable();

        let calls = Cell::new(0usize);
        place_window(&mut rows, 25_000, 25_020, |a, b| {
            calls.set(calls.get() + 1);
            a.cmp(b)
        });

        assert_eq!(
            &rows[25_000..25_020],
            &want[25_000..25_020],
            "the window must hold the same rows a full sort would put there",
        );
        assert!(
            calls.get() < 8 * n,
            "{} comparisons to place a 20-row window in {n} rows — that is a \
             full sort, not a selection",
            calls.get(),
        );
    }

    #[test]
    fn panning_a_list_tab_clamps_against_the_whole_list() {
        // The pan clamp is deliberately measured over every row, not just the
        // visible ones, so it does not shift as you scroll. This is also the
        // only path that reads `Body::max_width` on a list tab.
        let mut v = TuiProgressView::new("L");
        // One row far wider than the viewport, then a screenful of short ones.
        let wide = format!("//wide:{}", "x".repeat(300));
        v.apply(&ev(0, result_start(&wide)));
        v.apply(&ev(1, result_end(&wide, Some("boom".into()))));
        for i in 0..50 {
            let addr = format!("//s:{i:02}");
            v.apply(&ev(2, result_start(&addr)));
            v.apply(&ev(3, result_end(&addr, Some("boom".into()))));
        }
        v.view.set(ViewMode::Failed);

        // Pan far right: it clamps at the widest row's tail, not at zero.
        v.hscroll(100_000);
        let lines = v.render("⠋", 0, 40, 9);
        let pan = v.hscroll.get();
        assert!(pan > 0, "a list tab must be pannable");
        let row0 = format!("{}", lines[1]);
        assert!(
            !row0.contains("//wide"),
            "pan did not drop the head: {row0}"
        );

        // Scroll the wide row out of the window. The clamp must not move — if it
        // were measured over the visible rows it would collapse to zero here.
        v.scroll(20);
        drop(v.render("⠋", 0, 40, 9));
        assert_eq!(
            v.hscroll.get(),
            pan,
            "the pan clamp shifted when the widest row scrolled out of view",
        );
    }

    #[test]
    fn a_failed_row_keeps_its_error_on_one_line() {
        // A `\n` inside a `Span` does not wrap in ratatui, it corrupts the box
        // the whole viewport is laid out in — and multi-line stderr is exactly
        // what lands on this tab.
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, result_start("//a:bad")));
        v.apply(&ev(
            1,
            result_end("//a:bad", Some("first line\nsecond line".into())),
        ));
        v.view.set(ViewMode::Failed);

        let lines = v.render("⠋", 0, 120, 8);
        assert_eq!(lines.len(), 8, "a multi-line error must not grow the box");
        let row = format!("{}", lines[1]);
        assert!(row.contains("first line"), "{row}");
        assert!(!row.contains("second line"), "{row}");
    }

    #[test]
    fn list_max_width_matches_the_widest_rendered_row() {
        // The pan clamp is measured off the whole list without rendering it.
        let s = list_state(50);
        for (name, build) in LIST_TABS {
            let want = all_lines(build(&s))
                .iter()
                .map(|l| spans_width(&l.spans))
                .max()
                .unwrap_or(0);
            assert_eq!(build(&s).max_width(), want, "{name}");
        }
    }

    #[test]
    fn scrolled_matched_tab_renders_the_ordered_slice() {
        // End to end through `render`: a scrolled sorted list shows the addrs a
        // full sort would have placed at those rows, and the border indicator
        // agrees with them.
        let mut v = TuiProgressView::new("L");
        let addrs: Vec<String> = (0..40).map(|i| format!("//p:t{i:02}")).collect();
        v.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: addrs.clone(),
                complete: true,
            },
        ));
        v.view.set(ViewMode::Matched);
        v.scroll(12);

        // height 9 ⇒ 1 header + 6 body rows + border + help.
        let lines = v.render("⠋", 0, 80, 9);
        let body: Vec<String> = lines[1..7]
            .iter()
            .map(|l| format!("{l}").trim().to_string())
            .collect();
        let mut sorted = addrs.clone();
        sorted.sort();
        assert_eq!(body, sorted[12..18], "rows 13–18 of the ordered list");

        let bottom = format!("{}", lines[lines.len() - 2]);
        assert!(bottom.contains("↑ 13–18 of 40 ↓"), "{bottom}");
    }

    #[test]
    fn the_live_op_index_holds_only_open_ops_not_the_whole_run() {
        // `ops` keeps a target's timeline until its `ResultEnd` (none of these
        // 2,000 targets get one here); the frame path used to walk `ops` itself
        // on every frame. The index it walks instead stays in-flight-sized.
        let mut s = BuildState::new();
        for i in 0..2_000 {
            let addr = format!("//pkg{i}:t");
            s.apply(&ev(0, execute_start(&addr)));
            s.apply(&ev(1, execute_end(&addr)));
        }
        // Three targets left mid-flight: two executing, one writing to cache.
        s.apply(&ev(2, execute_start("//live:a")));
        s.apply(&ev(2, execute_start("//live:b")));
        s.apply(&ev(2, local_write_start("//live:c")));

        assert_eq!(
            s.ops.len(),
            2_003,
            "no ResultEnd fired, so nothing is retired"
        );
        assert_eq!(
            s.open_ops.len(),
            3,
            "the frame path only walks the live ops"
        );
        assert_eq!(s.busy_workers(), 2, "only Execute holds a worker slot");
        // The slow-row scan reads the same index, so it sees all three.
        assert_eq!(s.body_lines(LONG_RUNNING_THRESHOLD_MS + 3).len(), 3);
    }

    #[test]
    fn open_op_index_survives_duplicate_and_mismatched_boundaries() {
        // The index is maintained beside `OpTimeline::active`; every event order
        // the fold tolerates must leave the two agreeing.
        let rescan = |s: &BuildState| {
            s.ops
                .values()
                .filter(|tl| matches!(tl.active, Some((Op::Execute, _))))
                .count()
        };
        let mut s = BuildState::new();
        let steps = [
            execute_start("//a:1"),
            execute_start("//a:2"),
            // A second start on an open target: the overlap guard closes the
            // first span and reopens — still one slot held.
            execute_start("//a:1"),
            // A mismatched close is ignored: //a:1 is executing, not writing.
            local_write_end("//a:1"),
            // Execute → LocalCacheWrite on //a:2 releases its worker slot but
            // leaves the target open.
            local_write_start("//a:2"),
            execute_end("//a:1"),
            // A duplicate close is a no-op.
            execute_end("//a:1"),
            local_write_end("//a:2"),
        ];
        for (i, kind) in steps.into_iter().enumerate() {
            s.apply(&ev(i as u64, kind));
            assert_eq!(s.busy_workers(), rescan(&s), "after step {i}");
            assert_eq!(
                s.open_ops.len(),
                s.ops.values().filter(|tl| tl.active.is_some()).count(),
                "index drifted from the timelines after step {i}",
            );
        }
        assert_eq!(s.busy_workers(), 0);
        assert!(s.open_ops.is_empty(), "nothing left open");
        assert_eq!(s.ops.len(), 2, "both targets stay in the history");
    }

    /// `System` with a per-thread allocation counter. Test binary only — the
    /// shipped allocator is untouched.
    struct CountingAlloc;

    thread_local! {
        /// Allocations made by this thread. A `Cell` with a const initializer:
        /// no allocation and no destructor, so the allocator can bump it without
        /// re-entering itself.
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
    }

    // SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc`
    // contract; the wrapper adds no assumptions of its own. The counter is a
    // const-initialised thread-local `Cell<usize>` — it never allocates and has
    // no destructor, so bumping it cannot re-enter the allocator.
    unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `layout` is forwarded unchanged from the caller.
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `layout` is forwarded unchanged from the caller.
            unsafe { std::alloc::System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `ptr`, `layout` and `new_size` are forwarded unchanged.
            unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: `ptr` and `layout` are forwarded unchanged from the caller.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAlloc = CountingAlloc;

    /// Allocations `f` makes on this thread.
    fn allocs_during(f: impl FnOnce()) -> usize {
        let before = ALLOCS.with(Cell::get);
        f();
        ALLOCS.with(Cell::get).saturating_sub(before)
    }

    #[test]
    fn a_list_frame_allocates_the_same_at_1k_and_20k_targets() {
        // The viewport shows ~20 rows, so one frame must allocate the same
        // whether the list behind it holds 1k or 20k targets: every row used to
        // be formatted into a `Line` (two allocations) that the window then
        // discarded, and a filter lower-cased two `String`s per row on top.
        //
        // Allocation *count*, not bytes — the row buffer is still one
        // allocation of `O(n)` bytes, and the selection still touches all of it.
        // What must not come back is a heap allocation per row.
        let mut small_view = TuiProgressView::new("bench");
        small_view.state = list_state(1_000);
        let mut big_view = TuiProgressView::new("bench");
        big_view.state = list_state(20_000);

        let frame_allocs = |v: &TuiProgressView, mode: ViewMode, filter: &str| -> usize {
            v.view.set(mode);
            v.scroll.set(0);
            v.hscroll.set(0);
            filter.clone_into(&mut v.search_query.borrow_mut());
            // The first frame settles the scroll/pan clamps; measure a steady one.
            let settled = v.render("⠋", 0, 120, 24);
            // Guard against measuring the "nothing here" placeholder: the border
            // only carries a scroll indicator when the list overflows the
            // viewport, which is the whole premise of the measurement.
            let border = format!("{}", settled[settled.len() - 2]);
            assert!(
                border.contains(" of "),
                "the measured frame does not overflow its viewport: {border}",
            );
            drop(settled);
            allocs_during(|| drop(v.render("⠋", 0, 120, 24)))
        };

        for (name, mode) in [
            ("done", ViewMode::Done),
            ("matched", ViewMode::Matched),
            ("cached", ViewMode::Cached),
            ("failed", ViewMode::Failed),
        ] {
            // Unfiltered, then filtered — the filter has to be tried against
            // every row, so it is its own scaling risk.
            for filter in ["", "t7"] {
                let small = frame_allocs(&small_view, mode, filter);
                let big = frame_allocs(&big_view, mode, filter);
                assert!(
                    big <= small + 64,
                    "{name} tab (filter {filter:?}): {small} allocations for a frame \
                     over 1k targets but {big} over 20k — per-frame cost still \
                     scales with the list length",
                );
            }
        }
    }
}
