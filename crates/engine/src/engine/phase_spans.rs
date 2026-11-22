//! Where a target's result spends its time, phase by phase: the spans a
//! `ResultEnd` carries (`ResultEnd.spans`, [`PhaseSpan`]).
//!
//! A result scope that something listens to gets a [`PhaseRecorder`]; one
//! nobody listens to gets none, and every mark below is a no-op on `None`.
//! The recorder holds a fixed array of marks — a [`Phase`] and two offsets
//! from the request's [`ClockAnchor`] — so marking a phase allocates nothing.
//! Names are looked up, and the `Vec` built, only when the `ResultEnd` is.
//!
//! The anchor is stamped once per request: a monotonic `Instant` and the wall
//! clock read at the same moment. A span's times are the anchor's wall time
//! plus a monotonic offset, so a wall-clock step during a build moves no span
//! against another and changes no duration.
//!
//! Spans are diagnostics, and live only on the event (session store I1): never
//! on an `EResult`, a memo value or key, the cache, or any hash; and nothing
//! about them reaches a target's argv, env or files.

use hcore::events::{Phase, PhaseSpan};
use std::future::Future;
use std::time::{Instant, SystemTime};

/// A request's time origin: one monotonic instant and the wall clock at it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClockAnchor {
    at: Instant,
    unix_us: u64,
}

impl ClockAnchor {
    /// Stamped now.
    pub(crate) fn now() -> Self {
        let unix_us = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        Self {
            at: Instant::now(),
            unix_us,
        }
    }

    /// Nanoseconds from the anchor to `t`; 0 for an instant before it.
    fn offset_ns(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.at).as_nanos()).unwrap_or(u64::MAX)
    }

    /// Microseconds since the epoch for an offset from the anchor.
    fn unix_us(&self, offset_ns: u64) -> u64 {
        self.unix_us.saturating_add(offset_ns / 1_000)
    }
}

/// How many phases one result records. A result enters about a dozen on its
/// longest path; marks past this are dropped, never grown into.
const CAPACITY: usize = 24;

/// An end offset not yet recorded: the phase is still open.
const OPEN: u64 = u64::MAX;

#[derive(Debug, Clone, Copy)]
struct Mark {
    phase: Phase,
    start_ns: u64,
    end_ns: u64,
}

#[derive(Debug)]
struct Marks {
    len: usize,
    buf: [Mark; CAPACITY],
}

/// The phases one result scope went through. See the module docs.
#[derive(Debug)]
pub(crate) struct PhaseRecorder {
    anchor: ClockAnchor,
    marks: parking_lot::Mutex<Marks>,
}

impl PhaseRecorder {
    pub(crate) fn new(anchor: ClockAnchor) -> Self {
        Self {
            anchor,
            marks: parking_lot::Mutex::new(Marks {
                len: 0,
                buf: [Mark {
                    phase: Phase::Deps,
                    start_ns: 0,
                    end_ns: OPEN,
                }; CAPACITY],
            }),
        }
    }

    /// Append a mark; `None` once the array is full.
    fn push(&self, phase: Phase, start_ns: u64, end_ns: u64) -> Option<usize> {
        let mut marks = self.marks.lock();
        let idx = marks.len;
        let slot = marks.buf.get_mut(idx)?;
        *slot = Mark {
            phase,
            start_ns,
            end_ns,
        };
        marks.len = idx + 1;
        Some(idx)
    }

    fn close(&self, idx: usize, end: Instant) {
        let end_ns = self.anchor.offset_ns(end);
        if let Some(mark) = self.marks.lock().buf.get_mut(idx) {
            mark.end_ns = end_ns;
        }
    }

    /// `phase` ran from `start` to `end`, both measured elsewhere (the run
    /// clock's spawn and exit).
    pub(crate) fn record(&self, phase: Phase, start: Instant, end: Instant) {
        let start_ns = self.anchor.offset_ns(start);
        let end_ns = self.anchor.offset_ns(end).max(start_ns);
        // A full array drops the mark: spans are diagnostics.
        let _kept: Option<usize> = self.push(phase, start_ns, end_ns);
    }

    /// The spans so far, ordered by start: a phase still open is closed now
    /// (a result that failed or was cancelled inside it). A phase entered
    /// twice is two spans.
    pub(crate) fn spans(&self) -> Vec<PhaseSpan> {
        let now_ns = self.anchor.offset_ns(Instant::now());
        let marks = self.marks.lock();
        let mut taken: Vec<Mark> = marks.buf.iter().take(marks.len).copied().collect();
        drop(marks);
        taken.sort_by_key(|m| (m.start_ns, m.end_ns));
        taken
            .into_iter()
            .map(|m| {
                let end_ns = if m.end_ns == OPEN { now_ns } else { m.end_ns };
                PhaseSpan {
                    name: std::borrow::Cow::Borrowed(m.phase.name()),
                    start_unix_us: self.anchor.unix_us(m.start_ns),
                    end_unix_us: self.anchor.unix_us(end_ns.max(m.start_ns)),
                }
            })
            .collect()
    }
}

/// Open `phase` on `rec`, now; it closes when the guard drops, whether the
/// code it covers finished, failed or was cancelled. Does nothing on `None`.
pub(crate) fn open(rec: Option<&PhaseRecorder>, phase: Phase) -> PhaseGuard<'_> {
    let open = rec.and_then(|rec| {
        let start_ns = rec.anchor.offset_ns(Instant::now());
        rec.push(phase, start_ns, OPEN).map(|idx| (rec, idx))
    });
    PhaseGuard(open)
}

/// `fut`, timed as `phase` on `rec`: the phase opens now and closes when the
/// returned future completes or is dropped. Await it at once.
///
/// A struct, not an `async fn`: an `async fn` keeps its argument and the
/// future it awaits as two slots, doubling the size of every caller's future
/// that wraps a large one (`clippy::large_futures`).
pub(crate) fn timed<F: Future>(rec: Option<&PhaseRecorder>, phase: Phase, fut: F) -> Timed<'_, F> {
    Timed {
        fut,
        _phase: open(rec, phase),
    }
}

pin_project_lite::pin_project! {
    /// See [`timed`].
    #[must_use = "a future does nothing unless awaited"]
    pub(crate) struct Timed<'a, F> {
        #[pin]
        fut: F,
        _phase: PhaseGuard<'a>,
    }
}

impl<F: Future> Future for Timed<'_, F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        let this = self.project();
        let polled = this.fut.poll(cx);
        if polled.is_ready() {
            // Closed at completion, not when the caller drops the future.
            this._phase.close();
        }
        polled
    }
}

/// Closes its phase on drop. See [`open`].
#[must_use = "the phase closes when the guard drops"]
pub(crate) struct PhaseGuard<'a>(Option<(&'a PhaseRecorder, usize)>);

impl PhaseGuard<'_> {
    /// Close the phase now; later calls, and the drop, do nothing.
    fn close(&mut self) {
        if let Some((rec, idx)) = self.0.take() {
            rec.close(idx, Instant::now());
        }
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn anchor(at: Instant, unix_us: u64) -> ClockAnchor {
        ClockAnchor { at, unix_us }
    }

    fn names(spans: &[PhaseSpan]) -> Vec<&str> {
        spans.iter().map(|s| s.name.as_ref()).collect()
    }

    /// Recorded and guarded phases come back ordered by start, named, and
    /// offset from the anchor's wall time; a repeated phase is two spans.
    #[test]
    fn records_phases_in_start_order() {
        let t0 = Instant::now();
        let rec = PhaseRecorder::new(anchor(t0, 1_000_000));
        let ms = |n| t0 + Duration::from_millis(n);
        rec.record(Phase::Process, ms(30), ms(50));
        rec.record(Phase::LockWait, ms(5), ms(10));
        rec.record(Phase::Resolve, ms(0), ms(5));
        rec.record(Phase::LockWait, ms(20), ms(30));

        let spans = rec.spans();
        assert_eq!(
            names(&spans),
            [
                "heph.resolve",
                "heph.lock_wait",
                "heph.lock_wait",
                "heph.process"
            ]
        );
        let process = spans.last().expect("process span");
        assert_eq!(process.start_unix_us, 1_030_000);
        assert_eq!(process.end_unix_us, 1_050_000);
        assert_eq!(process.duration_us(), 20_000);
    }

    /// A guard closes its phase on drop; a phase still open when the spans
    /// are taken (a failure or cancellation inside it) is closed then, not
    /// dropped, and never ends before it started.
    #[test]
    fn open_marks_close_on_drop_or_at_spans() {
        let rec = PhaseRecorder::new(ClockAnchor::now());
        {
            let _deps = open(Some(&rec), Phase::Deps);
        }
        let still_open = open(Some(&rec), Phase::Stage);
        let spans = rec.spans();
        assert_eq!(names(&spans), ["heph.deps", "heph.stage"]);
        for s in &spans {
            assert!(s.end_unix_us >= s.start_unix_us, "{s:?}");
        }
        drop(still_open);
        assert_eq!(rec.spans().len(), 2, "closing adds no span");
    }

    /// A timed future closes its phase when it completes, not when the
    /// caller gets round to dropping it; one dropped unfinished (cancelled)
    /// closes it at the drop.
    #[tokio::test]
    async fn timed_closes_at_completion_or_drop() {
        let rec = PhaseRecorder::new(ClockAnchor::now());
        let done = timed(Some(&rec), Phase::CacheRead, async { 7 });
        let mut done = Box::pin(done);
        assert_eq!(done.as_mut().await, 7);
        std::thread::sleep(Duration::from_millis(20));
        drop(done);
        let read = rec.spans().pop().expect("cache read span");
        assert!(
            read.duration_us() < 20_000,
            "closed at completion: {read:?}"
        );

        let parked = timed(Some(&rec), Phase::LockWait, std::future::pending::<()>());
        let polled = tokio::time::timeout(Duration::from_millis(10), parked).await;
        assert!(polled.is_err(), "never completes");
        assert_eq!(names(&rec.spans()), ["heph.cache.read", "heph.lock_wait"]);
    }

    /// No recorder, no marks: the path a request nobody listens to takes.
    #[test]
    fn no_recorder_is_a_no_op() {
        let guard = open(None, Phase::Deps);
        drop(guard);
    }

    /// Past the fixed capacity marks are dropped, never grown into; a guard
    /// whose mark was dropped closes nothing.
    #[test]
    fn capacity_overflow_drops_marks() {
        let t0 = Instant::now();
        let rec = PhaseRecorder::new(anchor(t0, 0));
        for _ in 0..CAPACITY + 5 {
            rec.record(Phase::LockWait, t0, t0);
        }
        let late = open(Some(&rec), Phase::Collect);
        drop(late);
        let spans = rec.spans();
        assert_eq!(spans.len(), CAPACITY);
        assert!(spans.iter().all(|s| s.name == "heph.lock_wait"));
    }

    /// Durations come from the monotonic clock only: two anchors at the same
    /// instant whose wall clocks disagree by an hour (a clock step) give the
    /// same durations, shifted as a whole.
    #[test]
    fn a_wall_clock_jump_changes_no_duration() {
        let t0 = Instant::now();
        let hour_us = 3_600_000_000;
        let a = PhaseRecorder::new(anchor(t0, 5 * hour_us));
        let b = PhaseRecorder::new(anchor(t0, 4 * hour_us));
        for rec in [&a, &b] {
            rec.record(Phase::Stage, t0, t0 + Duration::from_millis(3));
            rec.record(
                Phase::Process,
                t0 + Duration::from_millis(3),
                t0 + Duration::from_millis(40),
            );
        }
        let (sa, sb) = (a.spans(), b.spans());
        let durations = |s: &[PhaseSpan]| s.iter().map(PhaseSpan::duration_us).collect::<Vec<_>>();
        assert_eq!(durations(&sa), [3_000, 37_000]);
        assert_eq!(durations(&sa), durations(&sb));
        for (x, y) in sa.iter().zip(&sb) {
            assert_eq!(x.start_unix_us - y.start_unix_us, hour_us);
        }
    }

    /// An instant before the anchor, or an end before its start, clamps
    /// rather than wrapping.
    #[test]
    fn out_of_order_instants_clamp() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_millis(10);
        let rec = PhaseRecorder::new(anchor(later, 100));
        rec.record(Phase::Collect, later + Duration::from_millis(5), later);
        rec.record(Phase::Stage, t0, t0);
        let spans = rec.spans();
        assert_eq!(spans[0].start_unix_us, 100);
        assert_eq!(spans[1].duration_us(), 0);
    }
}
