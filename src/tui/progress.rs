//! Client-side aggregator for the engine's build-progress event stream.
//!
//! [`BuildState`] folds [`BuildEvent`]s into running/completed/errored counts,
//! cache hit/miss statistics, and a list of targets that are currently executing.
//! It uses only the server-stamped `at_unix_ms` off each event — never a local
//! receipt clock — so elapsed times stay correct across a future client/server
//! process split. Rendering callers pass their own `now_ms` wall clock.

use std::collections::{HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::engine::event::{BuildEvent, BuildEventKind};
use crate::tui::app::{CIAppView, TUIAppView};

/// Total number of pinned viewport rows the progress block renders into:
/// 1 top border + [`MAX_LONG_RUNNING`] body rows + 1 bottom border.
pub const PROGRESS_ROWS: u16 = 8;

/// Maximum number of long-running target rows shown before collapsing the
/// remainder into a single "+N more" line.
pub const MAX_LONG_RUNNING: usize = 6;

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
const BRAILLE_FILL: [char; WORKERS_PER_CELL + 1] = ['⣿', '⠁', '⠃', '⠇', '⠧', '⠷', '⠿', '⡿', '⣿'];

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
        let glyph = BRAILLE_FILL.get(busy_here).copied().unwrap_or('⣿');
        let style = if busy_here == 0 {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Blue)
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

/// Folds the engine's build-progress event stream into renderable state.
#[derive(Debug, Default)]
pub struct BuildState {
    /// Addrs between `ResultStart` and `ResultEnd` — the "running" set.
    in_flight_results: HashSet<String>,
    /// addr → server `ExecuteStart` timestamp; used for long-running detection.
    executing: HashMap<String, u64>,
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
    /// Targets whose driver actually ran to success (`ExecuteEnd` with no error).
    /// Distinct from `completed`, which includes cache hits that never executed.
    built: usize,
    local_hits: usize,
    local_misses: usize,
    remote_hits: usize,
    remote_misses: usize,
    /// Worker capacity announced by the engine via `MaxWorkers`. `None` until
    /// the event lands (no worker indicator rendered before then).
    max_workers: Option<usize>,
    /// Server timestamp of the first event seen — the run's start anchor for the
    /// header's elapsed clock. `None` until any event lands.
    started_at_ms: Option<u64>,
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
                    if error.is_some() {
                        self.errored += 1;
                    } else {
                        self.completed += 1;
                    }
                }
                // Record every terminal addr; matched progress is computed as
                // `matched ∩ finished` so it works regardless of whether the
                // `Matched` event arrived before or after this result.
                self.finished.insert(addr.clone());
            }
            BuildEventKind::ExecuteStart { addr, .. } => {
                self.executing.insert(addr.clone(), ev.at_unix_ms);
            }
            BuildEventKind::ExecuteEnd { addr, error } => {
                self.executing.remove(addr);
                if error.is_none() {
                    self.built += 1;
                }
            }
            BuildEventKind::LocalCacheHit { .. } => self.local_hits += 1,
            BuildEventKind::LocalCacheMiss { .. } => self.local_misses += 1,
            BuildEventKind::RemoteCacheHit { .. } => self.remote_hits += 1,
            BuildEventKind::RemoteCacheMiss { .. } => self.remote_misses += 1,
            // Read/Write markers are not aggregated into counters.
            BuildEventKind::RemoteCacheRead { .. } | BuildEventKind::RemoteCacheWrite { .. } => {}
        }
    }

    /// Targets executing longer than `threshold_ms`, as `(addr, elapsed_ms)`
    /// sorted by elapsed descending (longest first).
    pub fn long_running(&self, now_ms: u64, threshold_ms: u64) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .executing
            .iter()
            .filter_map(|(addr, &start)| {
                let elapsed = now_ms.saturating_sub(start);
                (elapsed > threshold_ms).then(|| (addr.clone(), elapsed))
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

    /// Header counts, in render order: `(built, cached, running, failed)`.
    /// `built` is targets whose driver actually ran; `cached` is cache hits
    /// (local + remote); `running` is in-flight results; `failed` is errors.
    pub fn header_counts(&self) -> (usize, usize, usize, usize) {
        let cached = self.local_hits + self.remote_hits;
        (
            self.built,
            cached,
            self.in_flight_results.len(),
            self.errored,
        )
    }

    /// Workers currently holding an execute slot (targets mid-`Execute`). This is
    /// the semaphore-bound busy count, not `running` (which includes targets
    /// blocked on deps without a permit).
    pub fn busy_workers(&self) -> usize {
        self.executing.len()
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

    /// The long-running ("slow") target rows: up to [`MAX_LONG_RUNNING`] rows of
    /// `addr (Ns)`, with a trailing "+N more" line when the list overflows. At
    /// most [`MAX_LONG_RUNNING`] lines total (the collapse line consumes a slot).
    pub fn long_running_lines(&self, now_ms: u64) -> Vec<Line<'static>> {
        let long = self.long_running(now_ms, LONG_RUNNING_THRESHOLD_MS);
        let overflow = long.len() > MAX_LONG_RUNNING;
        // When the list overflows, the "+N more" collapse line consumes one
        // slot, so we show one fewer detailed row.
        let shown = if overflow {
            MAX_LONG_RUNNING - 1
        } else {
            MAX_LONG_RUNNING
        };
        let mut lines = Vec::with_capacity(MAX_LONG_RUNNING);
        for (addr, elapsed_ms) in long.iter().take(shown) {
            let secs = *elapsed_ms / 1000;
            lines.push(Line::from(format!("  {addr} ({secs}s)")));
        }
        if overflow {
            let extra = long.len() - shown;
            lines.push(Line::from(format!("  +{extra} more")));
        }
        lines
    }
}

/// Shared aggregation backing both paved-road views: a label plus the folded
/// [`BuildState`] and the list of errored targets. Both `TuiProgressView` and
/// `CiProgressView` wrap one of these so the fold logic lives in one place.
struct ProgressCore {
    label: String,
    state: BuildState,
    /// Errored `(addr, message)` collected from `ResultEnd`.
    errors: Vec<(String, String)>,
}

impl ProgressCore {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: BuildState::new(),
            errors: Vec::new(),
        }
    }

    /// Fold one event: collect errors, then update the aggregate counters.
    fn fold(&mut self, ev: &BuildEvent) {
        if let BuildEventKind::ResultEnd {
            addr,
            error: Some(err),
        } = &ev.kind
        {
            self.errors.push((addr.clone(), err.clone()));
        }
        self.state.apply(ev);
    }
}

/// The paved-road [`TUIAppView`]: a spinner+label header over the aggregated
/// [`BuildState`], rendering the long-running block and a persistent final
/// summary. Used by every command's `tui_view`.
pub struct TuiProgressView {
    core: ProgressCore,
}

impl TuiProgressView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            core: ProgressCore::new(label),
        }
    }

    /// The rounded top border:
    /// `╭─ 1m05s · D / N done · N cached · N failed · <workers> ──────╮`.
    /// The leading field is the elapsed-time clock. "done" shows matched progress
    /// `done / total` (total prefixed `~` while the matcher is still resolving),
    /// falling back to the executed count before any match streams. The "running"
    /// count is omitted — the worker braille (flush right) conveys concurrency.
    fn header_line(&self, now_ms: u64, width: u16) -> Line<'static> {
        let width = usize::from(width).max(MIN_BOX_WIDTH);
        let (built, cached, _running, failed) = self.core.state.header_counts();
        let busy = self.core.state.busy_workers();
        let max_workers = self.core.state.max_workers().unwrap_or(0);
        let elapsed = human_elapsed(self.core.state.elapsed_ms(now_ms));

        let done = match self.core.state.matched_progress() {
            Some((done, total, complete)) => {
                let tilde = if complete { "" } else { "~" };
                format!("{done} / {tilde}{total} done")
            }
            None => format!("{built} done"),
        };

        let mut left: Vec<Span<'static>> = Vec::with_capacity(5);
        left.push(Span::raw("╭─ "));
        left.push(Span::from(elapsed).bold());
        left.push(Span::raw(format!(
            " · {done} · {cached} cached · {failed} failed"
        )));
        // Worker braille sits after the failed count, separated by " · ".
        let workers = worker_spans(max_workers, busy);
        if !workers.is_empty() {
            left.push(Span::raw(" · "));
            left.extend(workers);
        }
        // Space between the counts and the dash fill.
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
        let label = &self.core.label;
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
            spans.push(Span::raw(banner_slice(label, window, now_ms)));
        }
        spans.push(Span::raw("─╯"));
        Line::from(spans)
    }
}

impl TUIAppView for TuiProgressView {
    fn apply(&mut self, ev: &BuildEvent) {
        self.core.fold(ev);
    }

    fn rows(&self) -> u16 {
        PROGRESS_ROWS
    }

    /// Layout (top to bottom), a rounded box pinned to [`PROGRESS_ROWS`]:
    /// ```text
    /// ╭─ rheph · N built · N cached · N running · N failed ──── <workers> ─╮
    ///   <slow rows...>            (padded to keep the box a fixed height)
    /// ╰─── <label> ───────────────────────────────────────────────────────╯
    /// ```
    /// The `spinner` is unused — liveness is conveyed by the worker braille and
    /// the scrolling label.
    fn render(&self, _spinner: &str, now_ms: u64, width: u16) -> Vec<Line<'static>> {
        let body_rows = usize::from(PROGRESS_ROWS) - 2;
        let mut lines = Vec::with_capacity(usize::from(PROGRESS_ROWS));
        lines.push(self.header_line(now_ms, width));
        for line in self
            .core
            .state
            .long_running_lines(now_ms)
            .into_iter()
            .take(body_rows)
        {
            lines.push(line);
        }
        // Pad the body so the bottom border always pins to the last row.
        while lines.len() < body_rows + 1 {
            lines.push(Line::from(""));
        }
        lines.push(self.bottom_line(now_ms, width));
        lines
    }

    /// Final build report. The summary goes straight to stderr (not the log
    /// sink) so it survives the torn-down inline viewport; per-target errors go
    /// through the error logger. Skipped when no build activity was seen (e.g.
    /// inspect/query commands), to avoid all-zero noise.
    fn last_render(&self) {
        if !self.core.state.has_activity() {
            return;
        }
        use std::io::Write;
        drop(writeln!(
            std::io::stderr().lock(),
            "{}",
            self.core.state.summary()
        ));
        for (addr, msg) in &self.core.errors {
            tracing::error!("{addr}: {msg}");
        }
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
        if let BuildEventKind::ExecuteStart {
            addr,
            driver,
            cache: true,
        } = &ev.kind
        {
            tracing::info!("running {addr} [{driver}]");
        }
        self.core.fold(ev);
    }

    fn finish(&self) {
        if let Some(n) = self.core.state.matched_total() {
            tracing::info!("matched {n} targets");
        }
        tracing::info!("{}", self.core.state.summary());
        for (addr, err) in &self.core.errors {
            tracing::error!("{addr}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at_unix_ms: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent { at_unix_ms, kind }
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
        // Sorted descending by elapsed: slow (7000) before mid (6000).
        assert_eq!(long[0].0, "//slow:a");
        assert_eq!(long[0].1, 7_000);
        assert_eq!(long[1].0, "//mid:b");
        assert_eq!(long[1].1, 6_000);
    }

    #[test]
    fn execute_end_removes_from_long_running() {
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//slow:a")));
        s.apply(&ev(100, execute_end("//slow:a")));
        assert!(s.long_running(10_000, 5_000).is_empty());
    }

    fn max_workers(count: usize) -> BuildEventKind {
        BuildEventKind::MaxWorkers { count }
    }

    #[test]
    fn tui_view_box_layout_header_body_label() {
        let mut v = TuiProgressView::new("Running //a:b");
        v.apply(&ev(0, execute_start("//slow:x")));
        let lines = v.render("⠋", 10_000, 80);

        // Always a fixed-height box.
        assert_eq!(lines.len(), usize::from(PROGRESS_ROWS));

        // Top border: rounded corners + title, not the label.
        let header = format!("{}", lines.first().expect("header line"));
        assert!(header.starts_with("╭─"), "header: {header}");
        assert!(header.ends_with('╮'), "header: {header}");
        // Leading field is the elapsed clock (10s after the start anchor at t=0).
        assert!(header.contains("10s"), "header: {header}");
        assert!(!header.contains("Running //a:b"), "header: {header}");

        // Bottom border: rounded corners + the label.
        let footer = format!("{}", lines.last().expect("footer line"));
        assert!(footer.starts_with("╰─"), "footer: {footer}");
        assert!(footer.ends_with('╯'), "footer: {footer}");
        assert!(footer.contains("Running //a:b"), "footer: {footer}");

        // The slow row sits in the body between the borders.
        assert!(
            lines[1..lines.len() - 1]
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

        let header = format!("{}", v.render("⠋", 100, 120).first().expect("header"));
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
        let header = format!("{}", v.render("⠋", 100, 120).first().expect("header"));
        assert!(header.contains("1 / ~3 done"), "{header}");

        // Matcher resolves: the `~` drops.
        v.apply(&ev(3, matched(&[], true)));
        let header = format!("{}", v.render("⠋", 100, 120).first().expect("header"));
        assert!(header.contains("1 / 3 done"), "{header}");
        assert!(!header.contains('~'), "{header}");
    }

    #[test]
    fn header_braille_sits_after_failed_count() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, max_workers(8)));
        v.apply(&ev(1, execute_start("//a:b")));
        let header = format!("{}", v.render("⠋", 100, 120).first().expect("header"));
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
        let header = format!("{}", v.render("⠋", 120, 120).first().expect("header"));
        assert!(header.contains('⠁'), "expected 1-busy braille: {header}");
    }

    #[test]
    fn no_worker_braille_before_max_workers_event() {
        let v = TuiProgressView::new("L");
        let header = format!("{}", v.render("⠋", 0, 120).first().expect("header"));
        for g in ['⠁', '⠃', '⠇', '⠧', '⠷', '⠿', '⡿', '⣿'] {
            assert!(!header.contains(g), "no braille expected: {header}");
        }
    }

    #[test]
    fn ci_view_event_folds_and_collects_errors() {
        let mut v = CiProgressView::new("Running //a:b");
        v.apply(&ev(1, result_start("//a:b")));
        v.apply(&ev(2, result_end("//a:b", Some("boom".into()))));
        // ResultEnd errors are retained for the final ci_finish report.
        assert_eq!(
            v.core.errors,
            vec![("//a:b".to_string(), "boom".to_string())]
        );
        assert_eq!(v.core.state.errored, 1);
    }

    #[test]
    fn has_activity_gates_final_summary() {
        // Empty view (e.g. an inspect/query run): no summary should print.
        let empty = TuiProgressView::new("Spec //a:b");
        assert!(!empty.core.state.has_activity());

        // Any observed event flips it on.
        let mut active = TuiProgressView::new("Running //a:b");
        active.apply(&ev(1, result_start("//a:b")));
        active.apply(&ev(2, result_end("//a:b", None)));
        assert!(active.core.state.has_activity());
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
        let lines = v.render("⠋", 10_000, 100);
        // Fixed-height box: header + body + footer.
        assert_eq!(lines.len(), usize::from(PROGRESS_ROWS));
        // The footer carries the label.
        let footer = format!("{}", lines.last().expect("footer line"));
        assert!(footer.contains("Running //x:y"), "{footer}");
        // The slow block overflowed, so a "+N more" collapse appears in the body.
        assert!(
            lines[1..lines.len() - 1]
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
    fn elapsed_anchors_to_first_event() {
        let mut s = BuildState::new();
        assert_eq!(s.elapsed_ms(5_000), 0, "no anchor before any event");
        s.apply(&ev(2_000, result_start("//a:b")));
        // Later events do not move the anchor backward or forward.
        s.apply(&ev(4_000, result_end("//a:b", None)));
        assert_eq!(s.elapsed_ms(9_000), 7_000);
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
        assert_eq!(glyph(6), "⠿");
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
        // With only 8 busy, the first cell is full and the second is idle.
        let partial = worker_spans(10, 8);
        assert_eq!(partial[0].content, "⣿");
        assert_eq!(partial[1].content, "⣿");
        assert_eq!(partial[1].style.fg, Some(Color::DarkGray));
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
}
