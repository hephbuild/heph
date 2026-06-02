//! Client-side aggregator for the engine's build-progress event stream.
//!
//! [`BuildState`] folds [`BuildEvent`]s into running/completed/errored counts,
//! cache hit/miss statistics, and a list of targets that are currently executing.
//! It uses only the server-stamped `at_unix_ms` off each event — never a local
//! receipt clock — so elapsed times stay correct across a future client/server
//! process split. Rendering callers pass their own `now_ms` wall clock.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::engine::event::{BuildEvent, BuildEventKind};
use crate::tui::app::{CIAppView, TUIAppView};

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
/// window and the clamped scroll offset. When content extends past the window
/// bottom, the last visible row is replaced by a combined `+N more` collapse
/// that counts *every* hidden line below (slow targets and lock waits alike).
/// Scroll is clamped so the window never runs off the end.
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
    let mut window: Vec<Line<'static>> = lines.drain(scroll..scroll + rows).collect();
    // Rows still hidden below the window. The collapse line itself displaces one
    // shown row, so the reported count is the hidden rows plus that displaced one.
    let hidden_below = max_scroll - scroll;
    if hidden_below > 0
        && let Some(last) = window.last_mut()
    {
        *last = Line::from(format!("  +{} more", hidden_below + 1));
    }
    (window, scroll)
}

/// Columns shifted per Left/Right key press when panning a wide body.
pub const HSCROLL_STEP: usize = 4;

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
/// slow-target / lock-wait breakdown; the [`ViewMode::Failed`] view lists every
/// errored target. `Tab` cycles through `[Default]` plus one entry per
/// [`HeaderItem::Tab`] the header model exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The live breakdown: slow targets + lock waits (or idle art when empty).
    Default,
    /// The list of failed targets.
    Failed,
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
    fn spans(&self) -> &[Span<'static>] {
        match self {
            HeaderItem::Text(spans) | HeaderItem::Tab { spans, .. } => spans,
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
    Execute,
    LocalCacheWrite,
}

impl Op {
    /// Single glyph shown before the op's elapsed time. Must stay **BMP
    /// single-width** (plain-text rows; a double-width glyph would clip). Future
    /// remote ops use `↓`/`↑` (U+2193/2191), never the emoji `⬇`/`⬆`.
    fn icon(self) -> char {
        match self {
            Op::Execute => '▶',
            Op::LocalCacheWrite => '⊕',
        }
    }

    /// Pipeline ordinal for stable left-to-right ordering of the breakdown.
    fn order(self) -> u8 {
        match self {
            Op::Execute => 0,
            Op::LocalCacheWrite => 1,
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
        _ => None,
    }
}

/// Folds the engine's build-progress event stream into renderable state.
#[derive(Debug, Default)]
pub struct BuildState {
    /// Addrs between `ResultStart` and `ResultEnd` — the "running" set.
    in_flight_results: HashSet<String>,
    /// addr → per-target operation timeline. Drives the worker braille (count of
    /// targets whose active op is `Execute`) and the slow-target breakdown rows.
    /// Entries persist for the request (completed durations are kept so a finished
    /// op still shows in the breakdown while a later op is active).
    ops: HashMap<String, OpTimeline>,
    /// The matched top-level target set, accumulated as the matcher streams.
    matched: HashSet<String>,
    /// Whether any `Matched` event has been seen (gates display of the line).
    matched_seen: bool,
    /// Whether the matched set is final (matcher fully resolved). While false
    /// the total is provisional and rendered with a `~` prefix.
    matched_complete: bool,
    /// Every addr that reached `ResultEnd` (deduped). Used to compute matched
    /// progress as `matched ∩ finished` — order-independent, since `Matched`
    /// events can arrive after some matched results already finished.
    finished: HashSet<String>,
    completed: usize,
    errored: usize,
    /// Failed targets in failure order, each with its error message (if the
    /// `ResultEnd` carried one). Drives the [`ViewMode::Failed`] body.
    failed: Vec<(String, Option<String>)>,
    /// Targets whose driver actually ran to success (`ExecuteEnd` with no error).
    /// Distinct from `completed`, which includes cache hits that never executed.
    built: usize,
    local_hits: usize,
    local_misses: usize,
    remote_hits: usize,
    remote_misses: usize,
    /// Addrs that had a cache hit (local or remote). The header's "cached" count
    /// is `matched ∩ cache_hit` so it tracks matched targets only, not the
    /// transitive deps that also hit cache.
    cache_hit: HashSet<String>,
    /// Worker capacity announced by the engine via `MaxWorkers`. `None` until
    /// the event lands (no worker indicator rendered before then).
    max_workers: Option<usize>,
    /// Server timestamp of the first event seen — the run's start anchor for the
    /// header's elapsed clock. `None` until any event lands.
    started_at_ms: Option<u64>,
    /// Addrs blocked on the result lock past the notice threshold, mapped to the
    /// holder's pid (`None` if unknown). Added on `ResultLockWaitStart`, removed
    /// on `ResultLockWaitEnd`, so it reflects only currently-blocked waits.
    lock_waits: HashMap<String, Option<u32>>,
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
        }
        match &ev.kind {
            BuildEventKind::MaxWorkers { count } => {
                self.max_workers = Some(*count);
            }
            BuildEventKind::Matched { addrs, complete } => {
                self.matched_seen = true;
                self.matched.extend(addrs.iter().cloned());
                if *complete {
                    self.matched_complete = true;
                }
            }
            BuildEventKind::ResultStart { addr } => {
                self.in_flight_results.insert(addr.clone());
            }
            BuildEventKind::ResultEnd { addr, error } => {
                if self.in_flight_results.remove(addr) {
                    if let Some(err) = error {
                        self.errored += 1;
                        self.failed.push((addr.clone(), Some(err.clone())));
                    } else {
                        self.completed += 1;
                    }
                }
                // Record every terminal addr; matched progress is computed as
                // `matched ∩ finished` so it works regardless of whether the
                // `Matched` event arrived before or after this result.
                self.finished.insert(addr.clone());
            }
            // The op timeline (folded above) tracks Execute's duration; here we
            // keep only the `built` counter side effect on a successful end.
            BuildEventKind::ExecuteStart { .. } => {}
            BuildEventKind::ExecuteEnd { error, .. } => {
                if error.is_none() {
                    self.built += 1;
                }
            }
            BuildEventKind::LocalCacheHit { addr } => {
                self.local_hits += 1;
                self.cache_hit.insert(addr.clone());
            }
            BuildEventKind::LocalCacheMiss { .. } => self.local_misses += 1,
            BuildEventKind::RemoteCacheHit { addr } => {
                self.remote_hits += 1;
                self.cache_hit.insert(addr.clone());
            }
            BuildEventKind::RemoteCacheMiss { .. } => self.remote_misses += 1,
            BuildEventKind::ResultLockWaitStart { addr, holder_pid } => {
                self.lock_waits.insert(addr.clone(), *holder_pid);
            }
            BuildEventKind::ResultLockWaitEnd { addr } => {
                self.lock_waits.remove(addr);
            }
            // Read/Write markers are not aggregated into counters. The local
            // cache-write span is folded into the op timeline above, so the
            // counter match ignores it here.
            BuildEventKind::RemoteCacheRead { .. }
            | BuildEventKind::RemoteCacheWrite { .. }
            | BuildEventKind::LocalCacheWriteStart { .. }
            | BuildEventKind::LocalCacheWriteEnd { .. } => {}
            // GC progress is tracked by GcHeader, not the build counters. The
            // elapsed-clock anchor at the top of `apply` still runs, so the
            // clock works during a gc sweep.
            BuildEventKind::GcTargetSwept { .. } => {}
        }
    }

    /// Targets whose *currently-active* op has run longer than `threshold_ms`,
    /// with the per-op breakdown for that target. Returns
    /// `(addr, active_elapsed_ms, breakdown)` sorted by active elapsed descending
    /// (then addr). The breakdown is the target's completed ops plus the live
    /// active op, each at least one second, ordered by [`Op::order`]. A target
    /// with no active op is never slow (it dropped off when its last op ended).
    fn long_running(&self, now_ms: u64, threshold_ms: u64) -> Vec<SlowTarget> {
        let mut out: Vec<SlowTarget> = self
            .ops
            .iter()
            .filter_map(|(addr, tl)| {
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
    pub fn matched_progress(&self) -> Option<(usize, usize, bool)> {
        if !self.matched_seen {
            return None;
        }
        let done = self
            .matched
            .iter()
            .filter(|a| self.finished.contains(*a))
            .count();
        Some((done, self.matched.len(), self.matched_complete))
    }

    /// The three freestanding count fields — `(done, cached, failed)` — each
    /// without separators. The header model wraps these in [`HeaderItem`]s (so
    /// the view owns the ` · ` joins); [`BuildState::counts_segment`] joins them
    /// for the plain-text final summary.
    pub fn count_fields(&self) -> (String, String, String) {
        let (built, cached, _running, failed) = self.header_counts();
        let done = match self.matched_progress() {
            Some((done, total, complete)) => {
                let tilde = if complete { "" } else { "~" };
                format!("{done} / {tilde}{total} done")
            }
            None => format!("{built} done"),
        };
        (done, format!("{cached} cached"), format!("{failed} failed"))
    }

    /// The textual count segment shared by the live header and the final
    /// summary: `D / ~N done · C cached · F failed`. No elapsed clock, no worker
    /// braille — callers prepend the elapsed field themselves.
    pub fn counts_segment(&self) -> String {
        let (done, cached, failed) = self.count_fields();
        format!("{done} · {cached} · {failed}")
    }

    /// Body rows for the [`ViewMode::Failed`] view: one line per failed target,
    /// addr in red followed by its error message (when one was reported).
    /// Empty when nothing has failed.
    pub fn failed_lines(&self) -> Vec<Line<'static>> {
        self.failed
            .iter()
            .map(|(addr, err)| {
                let mut spans = vec![Span::styled(
                    format!("  {addr}"),
                    Style::default().fg(Color::Red),
                )];
                if let Some(err) = err {
                    // Keep the message on one line; the viewport clips overflow.
                    let msg = err.lines().next().unwrap_or(err);
                    spans.push(Span::styled(
                        format!("  {msg}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }

    /// Header counts, in render order: `(built, cached, running, failed)`.
    /// `built` is targets whose driver actually ran; `cached` counts matched
    /// targets that hit cache (`matched ∩ cache_hit`), falling back to all cache
    /// hits before any `Matched` event arrives; `running` is in-flight results;
    /// `failed` is errors.
    pub fn header_counts(&self) -> (usize, usize, usize, usize) {
        let cached = if self.matched_seen {
            self.matched
                .iter()
                .filter(|a| self.cache_hit.contains(*a))
                .count()
        } else {
            self.local_hits + self.remote_hits
        };
        (
            self.built,
            cached,
            self.in_flight_results.len(),
            self.errored,
        )
    }

    /// Workers currently holding an execute slot (targets whose active op is
    /// `Execute`). This is the semaphore-bound busy count, not `running` (which
    /// includes targets blocked on deps without a permit).
    pub fn busy_workers(&self) -> usize {
        self.ops
            .values()
            .filter(|tl| matches!(tl.active, Some((Op::Execute, _))))
            .count()
    }

    /// Announced worker capacity, or `None` before the `MaxWorkers` event.
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
        let matched = if self.matched_seen {
            let done = self
                .matched
                .iter()
                .filter(|a| self.finished.contains(*a))
                .count();
            // `~` marks a provisional total while the matcher is still streaming.
            let tilde = if self.matched_complete { "" } else { "~" };
            format!("matched {done} / {tilde}{}, ", self.matched.len())
        } else {
            String::new()
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
    /// or is in. The collapse into a "+N more" line is applied later, over the
    /// *combined* slow + lock-wait body (see [`BuildState::body_lines`]), so the
    /// overflow count covers both kinds of rows together.
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

    /// The full body: lock-wait notices first (they take priority), then every
    /// slow-target row. Uncollapsed — the caller windows/collapses it to the
    /// available rows, so a "+N more" count spans locks and slow rows alike.
    pub fn body_lines(&self, now_ms: u64) -> Vec<Line<'static>> {
        let mut body = self.lock_wait_lines();
        body.extend(self.slow_rows(now_ms));
        body
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

/// Format a byte count as a compact human-readable size (`512 B`, `3.4 KiB`,
/// `1.2 GiB`). Binary units; one decimal past the bytes band.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS.get(unit).copied().unwrap_or("B"))
}

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
    /// state ignore it.
    fn header(&self, core: &BuildState) -> Vec<HeaderItem>;
    /// The bottom-border label.
    fn label(&self) -> String;
    /// Final summary segment printed after the run (after the elapsed clock).
    /// Empty ⇒ nothing is printed.
    fn last_render(&self, core: &BuildState) -> String;
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
    fn header(&self, core: &BuildState) -> Vec<HeaderItem> {
        let (done, cached, failed) = core.count_fields();
        let mut items = vec![
            HeaderItem::text(done),
            HeaderItem::text(cached),
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

    fn last_render(&self, core: &BuildState) -> String {
        if core.has_activity() {
            core.counts_segment()
        } else {
            String::new()
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

    fn header(&self, _core: &BuildState) -> Vec<HeaderItem> {
        vec![HeaderItem::text(self.segment())]
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn last_render(&self, _core: &BuildState) -> String {
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
        }
    }

    /// The selectable body views in cycle order: [`ViewMode::Default`] first,
    /// then one entry per [`HeaderItem::Tab`] the header model exposes, in header
    /// order. `Tab` walks this list.
    fn view_modes(&self) -> Vec<ViewMode> {
        let mut modes = vec![ViewMode::Default];
        modes.extend(self.model.header(&self.state).iter().filter_map(|i| {
            if let HeaderItem::Tab { mode, .. } = i {
                Some(*mode)
            } else {
                None
            }
        }));
        modes
    }

    /// The dim help row pinned under the box: the keys the viewport responds to.
    fn help_line(&self) -> Line<'static> {
        Line::from(Span::styled(
            "  ↑/↓ scroll · ←/→ pan · tab/⇧tab switch view",
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
        let items = self.model.header(&self.state);
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 2);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" · "));
            }
            match item {
                HeaderItem::Text(item_spans) => spans.extend(item_spans.iter().cloned()),
                HeaderItem::Tab { mode, spans: item_spans } if *mode == active => {
                    // Active tab: paint a background so the selected view reads
                    // as highlighted in the header.
                    spans.extend(item_spans.iter().map(|s| {
                        Span::styled(s.content.clone(), s.style.bg(Color::Blue).fg(Color::White))
                    }));
                }
                HeaderItem::Tab { spans: item_spans, .. } => {
                    spans.extend(item_spans.iter().cloned())
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
        let elapsed = human_elapsed(self.state.elapsed_ms(now_ms));

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
    /// a banner. Total visible width always equals `width`.
    fn bottom_line(&self, now_ms: u64, width: u16) -> Line<'static> {
        let width = usize::from(width).max(MIN_BOX_WIDTH);
        // "╰─── " (5) + window + "─╯" (2) == width  ⇒  window = width - 7.
        let window = width.saturating_sub(7);
        let label = self.model.label();
        let label_len = label.chars().count();

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
        // top-left.
        self.scroll.set(0);
        self.hscroll.set(0);
    }

    /// Layout (top to bottom), a rounded box sized to `height` rows (one third of
    /// the terminal, see [`rows_for_height`]), with a dim help row pinned beneath:
    /// ```text
    /// ╭─ heph · N built · N cached · N running · N failed ──── <workers> ─╮
    ///   <slow rows + lock waits, scrollable, collapsed to "+N more">
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
        let body = match view {
            ViewMode::Default => self.state.body_lines(now_ms),
            ViewMode::Failed => self.state.failed_lines(),
        };
        if body.is_empty() {
            self.scroll.set(0);
            self.hscroll.set(0);
            match view {
                // Default view: the dim drifting idle field.
                ViewMode::Default => {
                    lines.extend(art_lines(now_ms, usize::from(width.max(1)), body_rows))
                }
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
            // stops once the longest line's tail reaches the right edge.
            let avail = usize::from(width.max(1));
            let max_w = body.iter().map(|l| spans_width(&l.spans)).max().unwrap_or(0);
            let hscroll = self.hscroll.get().min(max_w.saturating_sub(avail));
            self.hscroll.set(hscroll);

            let (window, scroll) = windowed(body, body_rows, self.scroll.get());
            self.scroll.set(scroll);
            lines.extend(window.into_iter().map(|l| hscroll_line(l, hscroll)));
        }
        // Pad the body so the bottom border always pins to the same row.
        while lines.len() < body_rows + 1 {
            lines.push(Line::from(""));
        }
        lines.push(self.bottom_line(now_ms, width));
        lines.push(self.help_line());
        lines
    }

    /// Final report — the elapsed clock plus the header model's summary segment,
    /// printed straight to stderr (not the log sink) so it survives the torn-down
    /// inline viewport. Per-target failures are rendered separately (rich
    /// diagnostics from the failure registry). The model returns an empty segment
    /// when there was nothing to report (e.g. inspect/query, or a no-op gc), in
    /// which case nothing is printed.
    fn last_render(&self) {
        let segment = self.model.last_render(&self.state);
        if segment.is_empty() {
            return;
        }
        let elapsed = human_elapsed_ms(self.state.elapsed_ms(crate::engine::event::now_unix_ms()));
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
}

impl CiProgressView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            core: ProgressCore::new(label),
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
            _ => {}
        }
        self.core.fold(ev);
    }

    fn finish(&self) {
        if let Some(n) = self.core.state.matched_total() {
            tracing::info!("matched {n} targets");
        }
        tracing::info!("{}", self.core.state.summary());
    }
}

/// CI (non-TUI) view for `heph gc`: folds the gc sweep into a [`GcHeader`] and
/// logs a single summary line at the end. Per-target progress is silent (the
/// command also prints its `GcStats` on completion).
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
        tracing::info!("gc: {}", self.header.segment());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at_unix_ms: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent { at_unix_ms, kind }
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

    fn max_workers(count: usize) -> BuildEventKind {
        BuildEventKind::MaxWorkers { count }
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

        let seg = header_text(&h.header(&core));
        assert!(seg.contains("2 targets"), "{seg}");
        assert!(seg.contains("1.0 KiB"), "{seg}");
        assert_eq!(h.label(), "GC");
        assert!(!h.last_render(&core).is_empty());
    }

    #[test]
    fn build_header_segment_has_counts_and_braille_and_ignores_gc() {
        let mut core = BuildState::new();
        core.apply(&ev(0, max_workers(8)));
        core.apply(&ev(1, execute_start("//a:b")));
        // A gc event must not perturb the build counters.
        core.apply(&ev(2, gc_swept(9, 9)));

        let h = BuildHeader::new("L");
        let text = header_text(&h.header(&core));
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
    fn render_fits_within_progress_rows_and_collapses_slow_block() {
        let mut v = TuiProgressView::new("Running //x:y");
        // 20 long-running targets, all started at 0.
        for i in 0..20 {
            v.apply(&ev(0, execute_start(&format!("//pkg:t{i}"))));
        }
        let height = 8u16;
        let lines = v.render("⠋", 10_000, 100, height);
        // The box fills exactly the rows it was given.
        assert_eq!(lines.len(), usize::from(height));
        // The bottom border (second-to-last row) carries the label.
        let footer = format!("{}", lines[lines.len() - 2]);
        assert!(footer.contains("Running //x:y"), "{footer}");
        // 20 slow rows can't fit the small body, so a "+N more" collapse appears.
        assert!(
            lines[1..lines.len() - 2]
                .iter()
                .any(|l| format!("{l}").contains("more")),
            "expected collapse line in body, got {lines:?}"
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
            let s = format!("{}", short.bottom_line(0, w));
            let l = format!("{}", long.bottom_line(0, w));
            assert_eq!(s.chars().count(), usize::from(w), "short @ {w}: {s}");
            assert_eq!(l.chars().count(), usize::from(w), "long @ {w}: {l}");
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

    fn lock_wait_start(addr: &str, pid: u32) -> BuildEventKind {
        BuildEventKind::ResultLockWaitStart {
            addr: addr.into(),
            holder_pid: Some(pid),
        }
    }

    #[test]
    fn body_collapse_counts_locks_and_slow_together() {
        // 2 lock waits + 4 slow targets = 6 body rows. Regression: the old
        // collapse counted only the slow overflow and ignored hidden lock rows.
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, lock_wait_start("//l:1", 1)));
        v.apply(&ev(0, lock_wait_start("//l:2", 2)));
        for i in 0..4 {
            v.apply(&ev(0, execute_start(&format!("//s:{i}"))));
        }
        // height 7 → body_rows = 4. 6 items > 4 ⇒ window shows 3 + a collapse.
        // total=6, body_rows=4, max_scroll=2, scroll=0 ⇒ hidden_below=2 ⇒ +3 more.
        let lines = v.render("⠋", 10_000, 80, 7);
        let collapse = lines
            .iter()
            .map(|l| format!("{l}"))
            .find(|t| t.contains("more"))
            .expect("collapse line");
        assert!(collapse.contains("+3 more"), "{collapse}");
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

        let lines = s.failed_lines();
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
        for item in h.header(&core) {
            let text: String = item.spans().iter().map(|s| s.content.to_string()).collect();
            assert!(!text.contains('·'), "item baked in a separator: {text}");
        }
    }

    #[test]
    fn tab_cycles_default_failed_and_back() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//slow:x")));
        v.apply(&ev(1, result_start("//a:bad")));
        v.apply(&ev(2, result_end("//a:bad", Some("boom".into()))));

        // Default view shows the slow row, not the failed target.
        let body = |v: &TuiProgressView| -> String {
            v.render("⠋", 10_000, 80, 8)
                .iter()
                .map(|l| format!("{l}"))
                .collect()
        };
        assert_eq!(v.view.get(), ViewMode::Default);
        assert!(body(&v).contains("//slow:x"));

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
    fn shift_tab_cycles_backwards() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, result_start("//a:bad")));
        v.apply(&ev(1, result_end("//a:bad", Some("boom".into()))));

        // Two modes (Default, Failed): backward from Default wraps to Failed.
        assert_eq!(v.view.get(), ViewMode::Default);
        v.tab(false);
        assert_eq!(v.view.get(), ViewMode::Failed);
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
        v.tab(true);
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
        v.tab(true); // → Failed, but nothing has failed
        let body: String = v
            .render("⠋", 10_000, 80, 8)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("no failed targets"), "{body}");
    }

    #[test]
    fn scroll_advances_the_body_window_and_clamps() {
        let mut v = TuiProgressView::new("L");
        for i in 0..6 {
            v.apply(&ev(0, execute_start(&format!("//s:{i}"))));
        }
        // body_rows = 4, 6 slow rows, max_scroll = 2.
        // Scroll past the end; render clamps to the bottom ⇒ no collapse there.
        v.scroll(10);
        let body: String = v
            .render("⠋", 10_000, 80, 7)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(!body.contains("more"), "no collapse at bottom: {body}");
        // Back to the top: the collapse returns.
        v.scroll(-10);
        let body: String = v
            .render("⠋", 10_000, 80, 7)
            .iter()
            .map(|l| format!("{l}"))
            .collect();
        assert!(body.contains("more"), "collapse at top: {body}");
    }
}
