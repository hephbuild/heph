//! The log sink's numbered lines, through a real recorder and the file it
//! writes: numbering across threads at the terminal's filter, `Log` records
//! past the cap up to the ceiling, and the replay a closed full screen prints
//! (I3).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hcore::events::{BuildEvent, BuildEventKind};

use super::cleanup::Retention;
use super::recorder::Faults;
use super::tests::{Captured, read_records, start};
use super::*;
use crate::tui::log_sink::{LogSink, MakeLogSink, Replayed, SinkLine};

const WAIT: Duration = Duration::from_secs(10);

/// A slot holding a session opened under `root` with `faults`.
fn slot_with(root: &Path, faults: Faults) -> Arc<SessionSlot> {
    let session = Session::open_with(
        &SessionRoot::new(root),
        start(),
        Retention::default(),
        faults,
    )
    .expect("open");
    let slot = Arc::new(SessionSlot::new());
    slot.install(session);
    slot
}

/// Every numbered `Log` record in the session's `events.ndjson`, in file
/// order: `(seq, level, msg)`.
fn log_records(slot: &SessionSlot) -> Vec<(u64, String, String)> {
    let dir = slot.dir().expect("a session");
    read_records(&dir)
        .0
        .into_iter()
        .filter_map(|r| match r.session {
            Some(SessionRecordKind::Log {
                level,
                msg,
                seq: Some(seq),
            }) => Some((seq, level, msg)),
            _ => None,
        })
        .collect()
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<SinkLine>) -> Vec<SinkLine> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The value of `name=` in a formatted line.
fn field(text: &str, name: &str) -> usize {
    text.split_whitespace()
        .find_map(|w| w.strip_prefix(name)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {name}= in {text:?}"))
        .parse()
        .expect("a number")
}

/// Many threads logging under a `-v` filter: every line that passes the
/// filter takes the next number, once, in the order the printer receives it,
/// and `events.ndjson` holds the same numbers in the same order with the same
/// text and level. A line below the filter takes no number.
#[test]
fn numbering_across_threads_at_the_terminal_filter() {
    const THREADS: usize = 8;
    const PER: usize = 250;
    let root = tempfile::tempdir().expect("tempdir");
    let slot = slot_with(root.path(), Faults::default());
    let sink = LogSink::new_direct_with(Arc::clone(&slot));
    let mut rx = sink.switch_to_buffered();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(MakeLogSink::new(sink.clone()))
            .finish(),
    );
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let dispatch = &dispatch;
            s.spawn(move || {
                tracing::dispatcher::with_default(dispatch, || {
                    for i in 0..PER {
                        tracing::trace!(t, i, "below the filter");
                        if i % 2 == 0 {
                            tracing::debug!(t, i, "line");
                        } else {
                            tracing::info!(t, i, "line");
                        }
                    }
                });
            });
        }
    });
    let lines = drain(&mut rx);
    let n = u64::try_from(THREADS * PER).expect("fits");
    let seqs: Vec<u64> = lines.iter().map(|l| l.seq).collect();
    assert_eq!(
        seqs,
        (1..=n).collect::<Vec<_>>(),
        "no number reused or skipped"
    );

    let mut next = [0usize; THREADS];
    let mut want = Vec::with_capacity(lines.len());
    for line in &lines {
        let text = std::str::from_utf8(&line.bytes).expect("utf8");
        assert!(!text.contains("below the filter"), "{text}");
        let (t, i) = (field(text, "t"), field(text, "i"));
        assert_eq!(i, next[t], "thread {t}'s lines in its own order");
        next[t] += 1;
        let level = if i % 2 == 0 { "DEBUG" } else { "INFO" };
        let msg = text.strip_suffix('\n').expect("a line");
        want.push((line.seq, level.to_owned(), msg.to_owned()));
    }
    let (tap, _) = slot.replay_source().expect("a session");
    tap.flush(WAIT).expect("flush");
    assert_eq!(log_records(&slot), want, "the printer and the file agree");
    slot.finish(Some(0));
}

/// `Log` records keep being written past the cap (after `Truncated`), up to
/// the ceiling (both injected small), and stop there: past it no later line
/// is written, even a shorter one that would fit. The replay prints the lines
/// on disk, in order, and one WARN with the count it skipped.
#[test]
fn log_records_outlive_the_cap_up_to_the_ceiling() {
    const CEILING: u64 = 4096;
    let root = tempfile::tempdir().expect("tempdir");
    let slot = slot_with(
        root.path(),
        Faults {
            events_cap: Some(4096),
            log_ceiling: Some(CEILING),
            ..Faults::default()
        },
    );
    let sink = LogSink::new_direct_with(Arc::clone(&slot));
    let mut rx = sink.switch_to_buffered();
    let (queued, mark) = sink.replay_mark(&mut rx);
    assert!(queued.is_empty());
    assert_eq!(mark.last_printed, 0);
    // Long enough that the room left under the ceiling fits a short line.
    let pad = "x".repeat(1000);
    for i in 0..100 {
        sink.write_diagnostic(&format!("long {i:03} {pad}\n"));
    }
    for i in 0..20 {
        sink.write_diagnostic(&format!("s{i}\n"));
    }

    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let mut printed = Vec::new();
    let replayed = tracing::subscriber::with_default(subscriber, || {
        sink.replay(mark, WAIT, |b| {
            printed.push(String::from_utf8_lossy(b).into_owned());
        })
    });

    let dir = slot.dir().expect("a session");
    let events = std::fs::read_to_string(dir.join(files::EVENTS)).expect("events");
    let (before, past) = events
        .split_once("\"Truncated\"")
        .expect("the cap was reached");
    let past = past.split_once('\n').expect("a line").1;
    assert!(before.contains("long 000"), "under the cap");
    let past_bytes: usize = past.lines().map(|l| l.len() + 1).sum();
    assert!(past.contains("\"Log\""), "Log records outlive the cap");
    assert!(
        u64::try_from(past_bytes).expect("fits") <= CEILING,
        "{past_bytes} bytes past the cap"
    );

    let logs = log_records(&slot);
    let kept = u64::try_from(logs.len()).expect("fits");
    let seqs: Vec<u64> = logs.iter().map(|l| l.0).collect();
    assert_eq!(
        seqs,
        (1..=kept).collect::<Vec<_>>(),
        "stopped at the ceiling"
    );
    assert!(kept < 100, "the ceiling dropped some: {kept}");
    let on_disk: Vec<String> = logs.iter().map(|l| format!("{}\n", l.2)).collect();
    assert_eq!(printed, on_disk);
    assert_eq!(
        replayed,
        Replayed {
            through: 120,
            printed: kept,
            skipped: 120 - kept,
        }
    );
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains(&format!("{} log lines", 120 - kept)), "{log}");
    slot.finish(Some(0));
}

/// Feed-level `replay_by_sequence_prints_each_line_once` (I3): the lines a
/// closed full screen kept off the terminal, read back from disk, are exactly
/// `(last printed, through]`: none before, none twice, in order, and a line
/// sent just before the replay — still in the recorder's 250 ms batch — is
/// in it, because the replay flushes first. Then several threads emitting
/// while a printer opens and closes screens at varied points around the
/// batch: the terminal shows every line once, numbered 1..=N in order.
///
/// Its screen clause (the dashboard's alternate screen, scrollback restored)
/// is asserted by unit 5's `logs_while_dashboard_open_flush_in_order`.
#[test]
fn replay_by_sequence_prints_each_line_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let slot = slot_with(root.path(), Faults::default());
    let sink = LogSink::new_direct_with(Arc::clone(&slot));
    let mut rx = sink.switch_to_buffered();

    for i in 1..=3 {
        sink.write_diagnostic(&format!("before {i}\n"));
    }
    let (queued, mark) = sink.replay_mark(&mut rx);
    let queued: Vec<u64> = queued.iter().map(|l| l.seq).collect();
    assert_eq!(queued, [1, 2, 3], "printed before the screen opens");
    assert_eq!(mark.last_printed, 3);
    for i in 4..=6 {
        sink.write_diagnostic(&format!("up {i}\n"));
    }
    sink.write_diagnostic("just before\n");
    let mut printed = Vec::new();
    let replayed = sink.replay(mark, WAIT, |b| {
        printed.push(String::from_utf8_lossy(b).into_owned());
    });
    assert_eq!(printed, ["up 4\n", "up 5\n", "up 6\n", "just before\n"]);
    assert_eq!(
        replayed,
        Replayed {
            through: 7,
            printed: 4,
            skipped: 0
        }
    );
    assert!(
        drain(&mut rx).iter().all(|l| l.seq <= replayed.through),
        "every queued line is covered by the replay"
    );
    // A second screen with nothing written meanwhile prints nothing.
    let (queued, mark) = sink.replay_mark(&mut rx);
    assert!(queued.is_empty());
    assert_eq!(mark.last_printed, 7);
    let again = sink.replay(mark, WAIT, |b| {
        panic!("printed twice: {}", String::from_utf8_lossy(b));
    });
    assert_eq!(again.printed, 0);

    concurrent_screens(&sink, &mut rx, &slot, 7);
    slot.finish(Some(0));
}

/// Threads write lines while this thread prints them like the backend does,
/// opening and closing a full screen over and over, for varied times around
/// the recorder's 250 ms batch. Every line past `printed_before` reaches the
/// terminal once, in number order, with the text the file holds for it.
fn concurrent_screens(
    sink: &LogSink,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SinkLine>,
    slot: &SessionSlot,
    printed_before: u64,
) {
    const THREADS: usize = 4;
    const PER: usize = 300;
    const UP_MS: [u64; 6] = [0, 1, 5, 20, 0, 300];
    let finished = AtomicUsize::new(0);
    // (number, bytes) as the terminal got them.
    let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let finished = &finished;
            s.spawn(move || {
                for i in 0..PER {
                    sink.write_diagnostic(&format!("t={t} i={i}\n"));
                    std::thread::sleep(Duration::from_micros(200));
                }
                finished.fetch_add(1, Ordering::Release);
            });
        }
        let mut through = printed_before;
        for cycle in 0.. {
            let last = finished.load(Ordering::Acquire) == THREADS;
            // The line: print what is queued past what a replay covered.
            for l in drain(rx) {
                if l.seq > through {
                    out.push((l.seq, l.bytes));
                }
            }
            let (queued, mark) = sink.replay_mark(rx);
            out.extend(
                queued
                    .into_iter()
                    .filter(|l| l.seq > through)
                    .map(|l| (l.seq, l.bytes)),
            );
            // The screen is up: lines are dropped, only their numbers kept.
            std::thread::sleep(Duration::from_millis(UP_MS[cycle % UP_MS.len()]));
            let held = drain(rx).iter().map(|l| l.seq).max().unwrap_or(0);
            let first = mark.last_printed + 1;
            let r = sink.replay(mark, WAIT, |b| out.push((0, b.to_vec())));
            assert_eq!(r.skipped, 0, "cycle {cycle}");
            assert!(
                held <= r.through,
                "cycle {cycle}: a dropped line past the replay"
            );
            assert_eq!(r.printed, r.through - mark.last_printed, "cycle {cycle}");
            let start = out.len() - usize::try_from(r.printed).expect("fits");
            for (k, entry) in out[start..].iter_mut().enumerate() {
                entry.0 = first + u64::try_from(k).expect("fits");
            }
            through = r.through;
            if last {
                for l in drain(rx) {
                    if l.seq > through {
                        out.push((l.seq, l.bytes));
                    }
                }
                break;
            }
        }
    });

    let n = printed_before + u64::try_from(THREADS * PER).expect("fits");
    let seqs: Vec<u64> = out.iter().map(|o| o.0).collect();
    assert_eq!(
        seqs,
        (printed_before + 1..=n).collect::<Vec<_>>(),
        "each line once, in order"
    );
    let mut next = [0usize; THREADS];
    for (_, bytes) in &out {
        let text = std::str::from_utf8(bytes).expect("utf8");
        let (t, i) = (field(text, "t"), field(text, "i"));
        assert_eq!(i, next[t], "thread {t}'s lines in its own order");
        next[t] += 1;
    }
    let (tap, _) = slot.replay_source().expect("a session");
    tap.flush(WAIT).expect("flush");
    let on_disk: Vec<(u64, Vec<u8>)> = log_records(slot)
        .into_iter()
        .filter(|l| l.0 > printed_before)
        .map(|(seq, _, msg)| (seq, format!("{msg}\n").into_bytes()))
        .collect();
    assert_eq!(out, on_disk, "the terminal shows what the file numbers");
}

/// A recorder that stopped (here a panic in it, as a bug would) fails the
/// replay's flush at once rather than being waited on: the replay prints the
/// lines that reached the disk before it stopped and one WARN with the count
/// of those that did not.
#[test]
fn replay_after_the_recorder_stopped() {
    let root = tempfile::tempdir().expect("tempdir");
    let slot = slot_with(
        root.path(),
        Faults {
            panic_on_event: true,
            ..Faults::default()
        },
    );
    let sink = LogSink::new_direct_with(Arc::clone(&slot));
    let mut rx = sink.switch_to_buffered();
    let (_, mark) = sink.replay_mark(&mut rx);
    for i in 0..3 {
        sink.write_diagnostic(&format!("kept {i}\n"));
    }
    let (tap, _) = slot.replay_source().expect("a session");
    tap.flush(WAIT).expect("on disk");
    tap.event(BuildEvent {
        at_unix_ms: 1,
        kind: BuildEventKind::ResultStart {
            addr: "//a:x".into(),
        },
    });
    // Answered once the recorder is gone: it never acknowledges.
    assert_eq!(tap.flush(WAIT), Err(FlushFailed::Stopped));
    for i in 0..2 {
        sink.write_diagnostic(&format!("lost {i}\n"));
    }

    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let mut printed = Vec::new();
    let began = Instant::now();
    let replayed = tracing::subscriber::with_default(subscriber, || {
        sink.replay(mark, Duration::from_secs(60), |b| {
            printed.push(String::from_utf8_lossy(b).into_owned());
        })
    });
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "a stopped recorder is not waited for: {:?}",
        began.elapsed()
    );
    assert_eq!(printed, ["kept 0\n", "kept 1\n", "kept 2\n"]);
    assert_eq!(
        replayed,
        Replayed {
            through: 5,
            printed: 3,
            skipped: 2
        }
    );
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("2 log lines"), "{log}");
    assert!(log.contains("recording stopped"), "{log}");
    slot.finish(Some(1));
}
