//! Client-side aggregator for the engine's build-progress event stream.
//!
//! [`BuildState`] folds [`BuildEvent`]s into running/completed/errored counts,
//! cache hit/miss statistics, and a list of targets that are currently executing.
//! It uses only the server-stamped `at_unix_ms` off each event — never a local
//! receipt clock — so elapsed times stay correct across a future client/server
//! process split. Rendering callers pass their own `now_ms` wall clock.

use std::collections::{HashMap, HashSet};

use ratatui::text::Line;

use crate::engine::event::{BuildEvent, BuildEventKind};
use crate::tui::app::{CIAppView, TUIAppView};

/// Total number of pinned viewport rows the progress block renders into.
/// 1 spinner line + 1 summary line + up to [`MAX_LONG_RUNNING`] rows.
pub const PROGRESS_ROWS: u16 = 8;

/// Maximum number of long-running target rows shown before collapsing the
/// remainder into a single "+N more" line.
pub const MAX_LONG_RUNNING: usize = 6;

/// A target is considered "taking long" once its execute span exceeds this.
pub const LONG_RUNNING_THRESHOLD_MS: u64 = 5_000;

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
    local_hits: usize,
    local_misses: usize,
    remote_hits: usize,
    remote_misses: usize,
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
        match &ev.kind {
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
            BuildEventKind::ExecuteEnd { addr, .. } => {
                self.executing.remove(addr);
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
}

impl TUIAppView for TuiProgressView {
    fn apply(&mut self, ev: &BuildEvent) {
        self.core.fold(ev);
    }

    fn rows(&self) -> u16 {
        PROGRESS_ROWS
    }

    /// Layout (top to bottom):
    /// ```text
    /// <spinner> <summary>
    /// <slow rows...>
    /// <label>
    /// ```
    fn render(&self, spinner: &str, now_ms: u64) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(usize::from(PROGRESS_ROWS));
        lines.push(Line::from(format!(
            "{spinner} {}",
            self.core.state.summary()
        )));
        lines.extend(self.core.state.long_running_lines(now_ms));
        lines.push(Line::from(self.core.label.clone()));
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

    #[test]
    fn tui_view_layout_spinner_summary_slow_label() {
        let mut v = TuiProgressView::new("Running //a:b");
        v.apply(&ev(0, execute_start("//slow:x")));
        let lines = v.render("⠋", 10_000);

        // Row 1: spinner + summary (not the label).
        let header = format!("{}", lines.first().expect("header line"));
        assert!(header.contains("⠋"), "header: {header}");
        assert!(
            header.contains("done 0"),
            "summary expected in header: {header}"
        );
        assert!(
            !header.contains("Running //a:b"),
            "label must not be in the header: {header}"
        );

        // Last row: the label.
        let last = format!("{}", lines.last().expect("label line"));
        assert_eq!(last, "Running //a:b");

        // The slow block sits in between.
        assert!(
            lines.iter().any(|l| format!("{l}").contains("//slow:x")),
            "expected slow row, got {lines:?}"
        );
        assert!(usize::from(v.rows()) >= lines.len());
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
        let lines = v.render("⠋", 10_000);
        // Header + slow block (≤ MAX_LONG_RUNNING) + label ≤ PROGRESS_ROWS.
        assert!(
            lines.len() <= usize::from(PROGRESS_ROWS),
            "render produced {} lines",
            lines.len()
        );
        // The slow block overflowed, so a "+N more" collapse precedes the label.
        let label = format!("{}", lines.last().expect("label line"));
        assert_eq!(label, "Running //x:y");
        let collapse = format!("{}", lines[lines.len() - 2]);
        assert!(
            collapse.contains("more"),
            "expected collapse line, got {collapse}"
        );
    }
}
