//! The dashboard's list over a real [`BuildState`] fold: which tab each
//! state lands in, in what order, and in whose words.

use hcore::capacity::Unit;
use hcore::events::{BuildEvent, BuildEventKind, CacheOutcomeKind, TimedOutData, YieldingToData};
use ratatui::text::Line;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::tui::progress::{BuildState, CountScope};
use crate::tui::text::Redact;

const T0: u64 = 1_700_000_000_000;
const CPU: u64 = hcore::capacity::MICROS_PER_CPU;

fn at(ms: u64, kind: BuildEventKind) -> BuildEvent {
    BuildEvent {
        at_unix_ms: T0 + ms,
        kind,
    }
}

fn matched(addrs: &[&str]) -> BuildEventKind {
    BuildEventKind::Matched {
        addrs: addrs.iter().map(|a| (*a).to_owned()).collect(),
        complete: true,
    }
}

fn start(addr: &str) -> BuildEventKind {
    BuildEventKind::ResultStart { addr: addr.into() }
}

fn exec(addr: &str) -> BuildEventKind {
    BuildEventKind::ExecuteStart {
        output_groups: Vec::new(),
        addr: addr.into(),
        driver: "exec".into(),
        cache: true,
        declared: None,
        reserved: Some([("cpu".into(), CPU)].into()),
        queued_ms: None,
        admitted: None,
    }
}

fn exec_end(addr: &str) -> BuildEventKind {
    BuildEventKind::ExecuteEnd {
        addr: addr.into(),
        error: None,
    }
}

fn hit(addr: &str) -> BuildEventKind {
    BuildEventKind::CacheOutcome {
        addr: addr.into(),
        outcome: CacheOutcomeKind::LocalHit,
    }
}

/// A `ResultEnd`; `error` with `upstream_of` is a dependency failure.
fn end(addr: &str, error: Option<&str>, upstream_of: Option<&str>) -> BuildEventKind {
    BuildEventKind::ResultEnd {
        cancelled: false,
        addr: addr.into(),
        error: error.map(Into::into),
        upstream_of: upstream_of.map(Into::into),
        exit_status: error.map(|_| "exit status: 101".into()),
        log_tail: None,
        timed_out: None,
    }
}

fn stopped(addr: &str) -> BuildEventKind {
    BuildEventKind::ResultEnd {
        cancelled: true,
        addr: addr.into(),
        error: Some("cancelled".into()),
        upstream_of: None,
        exit_status: None,
        log_tail: None,
        timed_out: None,
    }
}

fn admission(
    addr: &str,
    pool: &str,
    removals_pending: Option<u64>,
    yielding_to: Option<YieldingToData>,
) -> BuildEventKind {
    BuildEventKind::AdmissionWaitStart {
        addr: addr.into(),
        pool: pool.into(),
        need: 4 * CPU,
        free: 2 * CPU,
        ahead: 6,
        removals_pending,
        yielding_to,
    }
}

fn fold(events: &[BuildEvent]) -> BuildState {
    let mut s = BuildState::new();
    for e in events {
        s.apply(e);
    }
    s
}

fn text(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// One frame of `tab`'s list at `T0 + now`, 120 columns by 20 rows.
fn frame(s: &BuildState, st: &mut ListState, tab: Tab, now: u64) -> Vec<String> {
    frame_with(s, st, tab, now, &LastLines::default(), &Redact::default())
}

fn frame_with(
    s: &BuildState,
    st: &mut ListState,
    tab: Tab,
    now: u64,
    lines: &LastLines,
    redact: &Redact,
) -> Vec<String> {
    st.set_tab(tab);
    let inp = ListInputs {
        model: s.dashboard(),
        waits: s,
        lines,
        now_ms: T0 + now,
        redact,
    };
    list_lines(&inp, st, 120, 20).iter().map(text).collect()
}

fn title() -> Title<'static> {
    Title {
        command: "heph r build //...",
        session: None,
        clock: "1s",
        done: 0,
        total: None,
        provisional: false,
        standing: None,
    }
}

/// One frame of target screen `t` at `T0 + now`, 120 × 30.
fn target_frame(s: &BuildState, t: &mut TargetScreen, now: u64, redact: &Redact) -> Vec<String> {
    let inp = TargetInputs {
        model: s.dashboard(),
        waits: s,
        redact,
        now_ms: T0 + now,
        scope: CountScope::Matched,
        detach: false,
    };
    t.frame(&title(), &inp, 120, 30).iter().map(text).collect()
}

/// Frames until `ready` holds (the feed thread answers between frames),
/// failing after 10s.
fn target_frame_until(
    s: &BuildState,
    t: &mut TargetScreen,
    redact: &Redact,
    ready: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let f = target_frame(s, t, 10_000, redact);
        if ready(&f) {
            return f;
        }
        assert!(std::time::Instant::now() < deadline, "{f:#?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `addr`'s screen, on `tab`.
fn open_on(
    s: &BuildState,
    addr: &str,
    tab: TargetTab,
    reader: Option<Arc<StubReader>>,
) -> TargetScreen {
    let m = s.dashboard();
    let id = id_of(s, addr);
    let mut t = TargetScreen::open(m, id, false, reader.map(|r| r as Arc<dyn TargetReader>));
    while t.tab() != tab {
        t.cycle_tab(true);
    }
    t
}

/// A [`TargetReader`] answering from memory, recording each call's thread.
#[derive(Default)]
struct StubReader {
    logs: Mutex<HashMap<String, Vec<u8>>>,
    /// Logs followed live: their bytes come in one chunk, then nothing.
    live: Mutex<HashMap<String, Vec<u8>>>,
    outputs: Mutex<HashMap<String, OutputsFound>>,
    threads: Mutex<Vec<std::thread::ThreadId>>,
}

impl StubReader {
    fn set_log(&self, addr: &str, bytes: &[u8]) {
        self.logs
            .lock()
            .expect("lock")
            .insert(addr.into(), bytes.to_vec());
    }

    fn set_live(&self, addr: &str, bytes: &[u8]) {
        self.live
            .lock()
            .expect("lock")
            .insert(addr.into(), bytes.to_vec());
    }

    fn set_outputs(&self, addr: &str, found: OutputsFound) {
        self.outputs
            .lock()
            .expect("lock")
            .insert(addr.into(), found);
    }

    fn threads(&self) -> Vec<std::thread::ThreadId> {
        self.threads.lock().expect("lock").clone()
    }
}

impl TargetReader for StubReader {
    fn open_log(&self, addr: &str) -> LogOpened {
        self.threads
            .lock()
            .expect("lock")
            .push(std::thread::current().id());
        if let Some(b) = self.live.lock().expect("lock").get(addr) {
            /// One chunk, then a run still going.
            struct Once(Option<Vec<u8>>);
            impl LogFeed for Once {
                fn poll(&mut self) -> FeedStep {
                    self.0.take().map_or(FeedStep::Idle, FeedStep::Bytes)
                }
            }
            return LogOpened::Follow {
                feed: Box::new(Once(Some(b.clone()))),
                about: "live".into(),
            };
        }
        match self.logs.lock().expect("lock").get(addr) {
            Some(b) => LogOpened::Tail {
                bytes: b.clone(),
                len: b.len() as u64,
                about: "from the cache".into(),
            },
            None => LogOpened::Nothing("no log yet: the target has not run".into()),
        }
    }

    fn outputs(&self, addr: &str, _failed: bool) -> OutputsFound {
        self.threads
            .lock()
            .expect("lock")
            .push(std::thread::current().id());
        self.outputs
            .lock()
            .expect("lock")
            .get(addr)
            .cloned()
            .unwrap_or(OutputsFound::Nothing)
    }
}

/// The rows of a frame that name a target, in order, as their addresses.
fn addrs_in(rows: &[String]) -> Vec<&str> {
    rows.iter()
        .filter_map(|r| r.split_whitespace().find(|w| w.starts_with("//")))
        .collect()
}

/// Every state the fold knows, one target each.
fn every_state() -> Vec<BuildEvent> {
    vec![
        at(
            0,
            matched(&[
                "//t:q", "//t:b", "//t:r", "//t:d", "//t:c", "//t:f", "//t:df", "//t:s",
            ]),
        ),
        at(0, start("//t:b")),
        at(1_000, admission("//t:b", "cpu", None, None)),
        at(100, exec("//t:d")),
        at(900, exec_end("//t:d")),
        at(1_000, end("//t:d", None, None)),
        at(1_500, hit("//t:c")),
        at(1_500, end("//t:c", None, None)),
        at(2_000, exec("//t:r")),
        at(2_100, exec("//t:f")),
        at(2_200, end("//t:f", Some("boom"), None)),
        at(2_300, end("//t:df", Some("dep"), Some("//t:f"))),
        at(3_000, stopped("//t:s")),
    ]
}

/// Each state lands in its tab with its words; blocked and pending rows
/// share the Pending tab, blocked first, and its count is their sum.
#[test]
fn dashboard_lists_every_state() {
    let s = fold(&every_state());
    let m = s.dashboard();
    let mut st = ListState::default();
    let row = |rows: &[String], addr: &str| -> String {
        rows.iter()
            .find(|r| r.contains(&format!("{addr} ")))
            .unwrap_or_else(|| panic!("{addr} in {rows:#?}"))
            .clone()
    };

    let running = frame(&s, &mut st, Tab::Running, 42_000);
    assert_eq!(addrs_in(&running), ["//t:r"]);
    let r = row(&running, "//t:r");
    assert!(r.contains("▶ //t:r") && r.contains("▶ execute"), "{r}");
    assert!(r.contains("40s"), "the op's elapsed: {r}");

    let pending = frame(&s, &mut st, Tab::Pending, 42_000);
    assert_eq!(addrs_in(&pending), ["//t:b", "//t:q"], "blocked first");
    let counts = m.counts(CountScope::Matched);
    assert_eq!(m.tab_len(Tab::Pending, CountScope::Matched), 2);
    assert_eq!(counts.tab(Tab::Pending), counts.blocked + counts.queued);
    let b = row(&pending, "//t:b");
    assert!(b.contains("⧗ //t:b") && b.contains("waits cpu 4"), "{b}");
    // The elapsed time once, in TIME; LAST LINE keeps the rest.
    assert!(b.contains("(2 free)"), "{b}");
    assert_eq!(b.matches("41s").count(), 1, "{b}");
    let q = row(&pending, "//t:q");
    assert!(q.contains("· //t:q") && q.contains("pending"), "{q}");

    let failed = frame(&s, &mut st, Tab::Failed, 42_000);
    assert_eq!(addrs_in(&failed), ["//t:f", "//t:df"], "roots first");
    let f = row(&failed, "//t:f");
    assert!(f.contains("✗ //t:f") && f.contains("exit 101"), "{f}");
    let df = row(&failed, "//t:df");
    assert!(df.contains("⊘ //t:df") && df.contains("dep failed"), "{df}");
    assert!(df.contains("root //t:f"), "{df}");

    let done = frame(&s, &mut st, Tab::Done, 42_000);
    assert_eq!(addrs_in(&done), ["//t:d"]);
    let d = row(&done, "//t:d");
    assert!(
        d.contains("✓ //t:d") && d.contains("done") && d.contains("0.8s"),
        "{d}"
    );

    let cached = frame(&s, &mut st, Tab::Cached, 42_000);
    assert_eq!(addrs_in(&cached), ["//t:c"]);
    let c = row(&cached, "//t:c");
    assert!(c.contains("● //t:c") && c.contains("cached local"), "{c}");

    // A stop has no tab of its own: it shows in All only.
    let all = frame(&s, &mut st, Tab::All, 42_000);
    let stop = row(&all, "//t:s");
    assert!(
        stop.contains("○ //t:s") && stop.contains("stopped"),
        "{stop}"
    );
    for tab in Tab::LIST.iter().filter(|&&t| t != Tab::All) {
        let rows = frame(&s, &mut st, *tab, 42_000);
        assert!(!addrs_in(&rows).contains(&"//t:s"), "{tab:?}: {rows:#?}");
    }
    assert_eq!(addrs_in(&all).len(), 8, "every target in All: {all:#?}");
    assert_eq!(counts.all(), 8);
}

/// All reads running, waiting, failed, recently finished (newest first),
/// pending — and a frame is the same as the one before it.
#[test]
fn all_tab_activity_order() {
    let mut events = every_state();
    events.push(at(4_000, start("//t:r2")));
    events.push(at(4_000, exec("//t:r2")));
    let mut s = fold(&events);
    // `//t:r2` is a dependency: every target's scope.
    let mut st = ListState::default();
    st.scope = CountScope::All;
    let first = frame(&s, &mut st, Tab::All, 42_000);
    assert_eq!(
        addrs_in(&first),
        [
            "//t:r", "//t:r2", "//t:b", "//t:f", "//t:df", "//t:s", "//t:c", "//t:d", "//t:q"
        ],
    );
    assert_eq!(frame(&s, &mut st, Tab::All, 42_000), first, "stable");
    // An event that moves no target leaves the order alone.
    s.apply(&at(
        5_000,
        BuildEventKind::PoolUsage {
            pool: "cpu".into(),
            held: 2 * CPU,
            capacity: 8 * CPU,
            waiting: 1,
            removals_pending: 0,
        },
    ));
    assert_eq!(
        addrs_in(&frame(&s, &mut st, Tab::All, 42_000)),
        addrs_in(&first)
    );
    // A finish moves the row to the front of the recent ones.
    s.apply(&at(6_000, exec_end("//t:r")));
    s.apply(&at(6_000, end("//t:r", None, None)));
    assert_eq!(
        addrs_in(&frame(&s, &mut st, Tab::All, 42_000)),
        [
            "//t:r2", "//t:b", "//t:f", "//t:df", "//t:r", "//t:s", "//t:c", "//t:d", "//t:q"
        ],
    );
}

/// A Pending row says what the wait event carried, in the words of
/// `admission_wait_text`, and a
/// lock held by another heph names its session.
#[test]
fn wait_text_is_the_shared_formatter_pending_row() {
    let yielding = YieldingToData {
        pool: "memory".into(),
        need: 1 << 30,
    };
    for (pool, removals, yields) in [
        ("cpu", None, None),
        ("cpu", None, Some(yielding)),
        ("sandbox", Some(2), None),
    ] {
        let s = fold(&[
            at(0, start("//w:a")),
            at(1_000, admission("//w:a", pool, removals, yields.clone())),
        ]);
        let words = s.wait_words("//w:a", T0 + 42_000).expect("a wait");
        let expected = hcore::events::admission_wait_text(
            "//w:a",
            pool,
            Unit::default_for(pool),
            4 * CPU,
            2 * CPU,
            yields.as_ref().map(|y| (y, Unit::default_for(&y.pool))),
            removals,
            "41s",
        );
        let head = words.phase.strip_prefix("waits ").expect("waits …");
        assert!(
            expected.starts_with(&format!("//w:a waits for {head}")),
            "{words:?} vs {expected}"
        );
        assert!(
            expected.ends_with(&format!(" {}", words.detail)),
            "{words:?} vs {expected}"
        );
        assert_eq!(words.waited_ms, 41_000);
        // The row's LAST LINE is the formatter's words less the elapsed
        // time, which the TIME column shows: the time once per row.
        assert_eq!(
            words.row_detail,
            words.detail.replacen(", 41s)", ")", 1),
            "{words:?}"
        );
        assert!(!words.row_detail.contains("41s"), "{words:?}");
        match (pool, &yields) {
            ("cpu", None) => assert_eq!(words.row_detail, "(2 free)"),
            (_, Some(_)) => assert!(
                words
                    .row_detail
                    .starts_with("(2 free, yielding to a memory ")
                    && words.row_detail.ends_with(" run)"),
                "{words:?}"
            ),
            _ => assert!(
                words
                    .row_detail
                    .ends_with(") — cleanup is behind: 2 removals pending"),
                "{words:?}"
            ),
        }
        let mut st = ListState::default();
        let rows = frame(&s, &mut st, Tab::Pending, 42_000);
        let row = rows.iter().find(|r| r.contains("//w:a")).expect("the row");
        assert!(row.contains(&words.phase), "{row}");
        let shown: String = words.row_detail.chars().take(20).collect();
        assert!(row.contains(&shown), "{row}");
        assert_eq!(row.matches("41s").count(), 1, "{row}");
    }

    // A lock held by another heph: its session, as the spotlight names it.
    let root = tempfile::tempdir().expect("tempdir");
    let holder = crate::tui::session::Session::open(
        &crate::tui::session::SessionRoot::new(root.path()),
        crate::tui::session::SessionStart::default(),
    )
    .expect("open a session");
    let names = crate::tui::session::HolderNames::new(
        Some(crate::tui::session::SessionRoot::new(root.path())),
        None,
    );
    let pid = std::process::id();
    let mut s = BuildState::new();
    s.name_lock_holders(names.clone());
    names.resolve_blocking(pid);
    s.apply(&at(0, start("//w:l")));
    s.apply(&at(
        1_000,
        BuildEventKind::ResultLockWaitStart {
            addr: "//w:l".into(),
            holder_pid: Some(pid),
            in_use_by_readers: false,
        },
    ));
    let words = s.wait_words("//w:l", T0 + 42_000).expect("a lock wait");
    assert_eq!(words.phase, format!("locked: {}", holder.id()));
    assert_eq!(
        words.detail,
        format!("held by session {} (pid {pid})", holder.id())
    );
    let mut st = ListState::default();
    let rows = frame(&s, &mut st, Tab::Pending, 42_000);
    assert!(
        rows.iter()
            .any(|r| r.contains("⧗ //w:l") && r.contains("held by session")),
        "{rows:#?}"
    );
}

/// A run that timed out is a failed root, ✗, its phase `timed out 10m`;
/// no state of its own.
#[test]
fn timed_out_root_reads_as_failure_failed_tab() {
    let s = fold(&[
        at(0, start("//t:slow")),
        at(0, exec("//t:slow")),
        at(
            600_000,
            BuildEventKind::ResultEnd {
                cancelled: false,
                addr: "//t:slow".into(),
                error: Some("timed out after 10m".into()),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: Some(TimedOutData {
                    timeout_ms: 600_000,
                    abandoned: false,
                }),
            },
        ),
    ]);
    let m = s.dashboard();
    let counts = m.counts(CountScope::Matched);
    assert_eq!(
        (counts.failed, counts.dep_failed, counts.stopped),
        (1, 0, 0)
    );
    let mut st = ListState::default();
    let rows = frame(&s, &mut st, Tab::Failed, 700_000);
    let row = rows
        .iter()
        .find(|r| r.contains("//t:slow"))
        .expect("in Failed");
    assert!(
        row.contains("✗ //t:slow") && row.contains("timed out 10m"),
        "{row}"
    );
}

fn id_of(s: &BuildState, addr: &str) -> Id {
    let m = s.dashboard();
    (0..m.id_count())
        .filter_map(|i| Id::try_from(i).ok())
        .find(|&i| m.addr(i) == addr)
        .expect("the target")
}

fn op(addr: &str, kind: &str) -> BuildEventKind {
    let addr = addr.to_owned();
    match kind {
        "lock" => BuildEventKind::ResultLockWaitStart {
            addr,
            holder_pid: None,
            in_use_by_readers: true,
        },
        "unlock" => BuildEventKind::ResultLockWaitEnd { addr },
        "write" => BuildEventKind::LocalCacheWriteStart { addr },
        _ => BuildEventKind::LocalCacheWriteEnd { addr, error: None },
    }
}

/// The Overview's bars come from the op Start/End stamps: a finished run's
/// admission wait, execute and local write, and a running target's lock
/// wait and open execute bar, drawn on one scale.
#[test]
fn timeline_from_events() {
    let mut queued = exec("//t:a");
    if let BuildEventKind::ExecuteStart { queued_ms, .. } = &mut queued {
        *queued_ms = Some(500);
    }
    let s = fold(&[
        at(0, start("//t:a")),
        at(0, start("//t:run")),
        at(100, op("//t:run", "lock")),
        at(600, op("//t:run", "unlock")),
        at(1_000, queued),
        at(1_000, exec("//t:run")),
        at(4_000, exec_end("//t:a")),
        at(4_000, op("//t:a", "write")),
        at(4_500, op("//t:a", "written")),
        at(4_600, end("//t:a", None, None)),
    ]);
    let m = s.dashboard();
    let brief = |p: &detail::Phase| (p.label, p.start_ms - T0, p.end_ms - T0, p.ms, p.open);
    let done: Vec<_> = m
        .phases(id_of(&s, "//t:a"), T0 + 3_000)
        .iter()
        .map(brief)
        .collect();
    assert_eq!(
        done,
        [
            ("queued", 500, 1_000, 500, false),
            ("execute", 1_000, 4_000, 3_000, false),
            ("local write", 4_000, 4_500, 500, false),
        ]
    );
    let running: Vec<_> = m
        .phases(id_of(&s, "//t:run"), T0 + 3_000)
        .iter()
        .map(brief)
        .collect();
    assert_eq!(
        running,
        [
            ("lock wait", 100, 600, 500, false),
            ("execute", 1_000, 3_000, 2_000, true),
        ]
    );

    // One scale: 40 bar columns over the 4s from 0.5s to 4.5s.
    let lines = target::timeline_lines(&m.phases(id_of(&s, "//t:a"), T0 + 3_000), 65);
    assert!(text(&lines[0]).contains("timeline") && text(&lines[0]).ends_with("4.0s"));
    let bars: Vec<(usize, usize)> = lines[1..]
        .iter()
        .map(|l| {
            let bar = l.spans.last().expect("a bar").content.to_string();
            let lead = bar.chars().take_while(|&c| c == ' ').count();
            (lead, bar.chars().filter(|&c| c == '█').count())
        })
        .collect();
    assert_eq!(bars, [(0, 5), (5, 30), (35, 5)]);
    // The running bar ends in ▶ at now.
    let lines = target::timeline_lines(&m.phases(id_of(&s, "//t:run"), T0 + 3_000), 65);
    let exec_bar = lines
        .last()
        .expect("execute")
        .spans
        .last()
        .expect("bar")
        .content
        .to_string();
    assert!(exec_bar.ends_with('▶'), "{exec_bar:?}");
    assert_eq!(exec_bar.chars().count(), 40, "{exec_bar:?}");

    // On the Overview: the bars, and running time from the first op.
    let mut t = open_on(&s, "//t:run", TargetTab::Overview, None);
    let f = target_frame(&s, &mut t, 3_000, &Redact::default());
    assert!(
        f.iter().any(|r| r.contains("▶ running · 2.0s so far")),
        "{f:#?}"
    );
    assert!(f.iter().any(|r| r.contains("⧖ lock wait")), "{f:#?}");
    assert!(
        f.iter().any(|r| r.contains("execute") && r.ends_with('▶')),
        "{f:#?}"
    );
}

fn file(path: &str, size: u64) -> OutputFile {
    OutputFile {
        path: path.into(),
        kind: FileKind::File,
        size,
        hash: Some(format!("{size:012}abcd")),
    }
}

/// Outputs lists a finished target's groups with path, size and hash,
/// folding past three files (Enter expands, Enter again folds); a cache
/// hit shows the restored list; a running target only its declared group
/// names; a failed one what its slot kept, or that the run failed.
#[test]
fn outputs_tab_by_group() {
    let mut declared = exec("//o:run");
    if let BuildEventKind::ExecuteStart { output_groups, .. } = &mut declared {
        *output_groups = vec!["bin".into(), String::new()];
    }
    let s = fold(&[
        at(0, start("//o:done")),
        at(0, exec("//o:done")),
        at(1_000, exec_end("//o:done")),
        at(1_000, end("//o:done", None, None)),
        at(0, start("//o:hit")),
        at(10, hit("//o:hit")),
        at(20, end("//o:hit", None, None)),
        at(0, start("//o:run")),
        at(0, declared),
        at(0, start("//o:fail")),
        at(0, exec("//o:fail")),
        at(500, end("//o:fail", Some("boom"), None)),
        at(0, start("//o:bare")),
        at(0, exec("//o:bare")),
        at(500, end("//o:bare", Some("boom"), None)),
    ]);
    let reader = Arc::new(StubReader::default());
    let rust: Vec<OutputFile> = (1..=5)
        .map(|i| file(&format!("gen/f{i}.rs"), i * 1_000))
        .collect();
    let listing = OutputsFound::Listing(vec![
        OutputGroup {
            name: "rust".into(),
            size: 15_000,
            hash: None,
            files: rust,
            unavailable: None,
        },
        OutputGroup {
            name: "docs".into(),
            size: 300,
            hash: Some("93c751b0be4f0000".into()),
            files: vec![OutputFile {
                path: "gen/docs".into(),
                kind: FileKind::Dir,
                size: 0,
                hash: None,
            }],
            unavailable: None,
        },
    ]);
    reader.set_outputs("//o:done", listing.clone());
    reader.set_outputs("//o:hit", listing);
    reader.set_outputs(
        "//o:fail",
        OutputsFound::Failure {
            kept: vec![file("out/report.xml", 42)],
            fixes: vec!["src/lib.rs".into()],
        },
    );
    let r = Redact::default();

    let mut t = open_on(&s, "//o:done", TargetTab::Outputs, Some(reader.clone()));
    let f = target_frame_until(&s, &mut t, &r, |f| {
        f.iter().any(|l| l.contains("more files"))
    });
    let rows = f.join("\n");
    for want in [
        "rust",
        "gen/f1.rs",
        "gen/f3.rs",
        "… 2 more files",
        "docs",
        "gen/docs/",
        "000000003000",
    ] {
        assert!(rows.contains(want), "{want}: {rows}");
    }
    assert!(!rows.contains("gen/f4.rs"), "{rows}");
    assert!(
        rows.contains("✓ done") && rows.contains("2 groups · 6 files"),
        "{rows}"
    );
    assert!(rows.contains("heph inspect outputs //o:done"), "{rows}");
    assert!(t.enter(), "Enter acts on Outputs");
    let rows = target_frame(&s, &mut t, 10_000, &r).join("\n");
    assert!(
        rows.contains("gen/f4.rs") && rows.contains("gen/f5.rs"),
        "{rows}"
    );
    assert!(!rows.contains("more files"), "{rows}");
    t.enter();
    let rows = target_frame(&s, &mut t, 10_000, &r).join("\n");
    assert!(
        rows.contains("… 2 more files") && !rows.contains("gen/f4.rs"),
        "{rows}"
    );

    let mut t = open_on(&s, "//o:hit", TargetTab::Outputs, Some(reader.clone()));
    let f = target_frame_until(&s, &mut t, &r, |f| {
        f.iter().any(|l| l.contains("gen/f1.rs"))
    });
    assert!(
        f.iter().any(|l| l.contains("● cached · 2 groups")),
        "{f:#?}"
    );

    let mut t = open_on(&s, "//o:run", TargetTab::Outputs, Some(reader.clone()));
    let calls = reader.threads().len();
    let rows = target_frame(&s, &mut t, 10_000, &r).join("\n");
    assert!(rows.contains("bin") && rows.contains("(unnamed)"), "{rows}");
    assert_eq!(rows.matches("not written yet").count(), 2, "{rows}");
    assert_eq!(
        reader.threads().len(),
        calls,
        "a running target's outputs are not read"
    );

    let mut t = open_on(&s, "//o:fail", TargetTab::Outputs, Some(reader.clone()));
    let f = target_frame_until(&s, &mut t, &r, |f| {
        f.iter().any(|l| l.contains("report.xml"))
    });
    let rows = f.join("\n");
    assert!(
        rows.contains("failure_out") && rows.contains("out/report.xml"),
        "{rows}"
    );
    assert!(rows.contains("src/lib.rs (written back)"), "{rows}");
    assert!(rows.contains("heph inspect failure //o:fail"), "{rows}");

    let mut t = open_on(&s, "//o:bare", TargetTab::Outputs, Some(reader));
    target_frame_until(&s, &mut t, &r, |f| {
        f.iter().any(|l| l.contains("no outputs: the run failed"))
    });
}

/// The Overview's waiting row says what the Pending row and the spotlight
/// say: the shared formatter's phase and detail.
#[test]
fn wait_text_is_the_shared_formatter_overview() {
    let yielding = YieldingToData {
        pool: "memory".into(),
        need: 1 << 30,
    };
    for (pool, removals, yields) in [
        ("cpu", None, None),
        ("cpu", None, Some(yielding)),
        ("sandbox", Some(2), None),
    ] {
        let s = fold(&[
            at(0, start("//w:a")),
            at(1_000, admission("//w:a", pool, removals, yields)),
        ]);
        let words = s.wait_words("//w:a", T0 + 42_000).expect("a wait");
        let mut t = open_on(&s, "//w:a", TargetTab::Overview, None);
        let f = target_frame(&s, &mut t, 42_000, &Redact::default());
        let row = f
            .iter()
            .find(|r| r.trim_start().starts_with("waiting"))
            .expect("a waiting row");
        let value = row
            .trim_start()
            .strip_prefix("waiting")
            .expect("label")
            .trim();
        let whole = format!("{} {}", words.phase, words.detail);
        let shown = value.trim_end_matches('…');
        assert!(whole.starts_with(shown), "{value:?} vs {whole:?}");
        assert!(shown.len() > words.phase.len(), "{value:?}");
        assert!(
            f.iter().any(|r| r.contains(&format!("⧗ {}", words.phase))),
            "{f:#?}"
        );
    }

    // A lock held by readers: the same words.
    let s = fold(&[at(0, start("//w:l")), at(1_000, op("//w:l", "lock"))]);
    let words = s.wait_words("//w:l", T0 + 42_000).expect("a lock wait");
    let mut t = open_on(&s, "//w:l", TargetTab::Overview, None);
    let f = target_frame(&s, &mut t, 42_000, &Redact::default());
    let want = format!("{} {}", words.phase, words.detail);
    assert!(f.iter().any(|r| r.contains(&want)), "{want:?} in {f:#?}");
}

/// A root failure names what it took down, on the Overview and above
/// its Log; a timed-out run says so, and that it was abandoned.
#[test]
fn overview_took_down_and_timeout() {
    let s = fold(&[
        at(0, start("//f:root")),
        at(0, exec("//f:root")),
        at(0, start("//f:d1")),
        at(0, start("//f:d2")),
        at(500, end("//f:root", Some("boom"), None)),
        at(501, end("//f:d1", Some("dep"), Some("//f:root"))),
        at(502, end("//f:d2", Some("dep"), Some("//f:root"))),
        at(0, start("//f:slow")),
        at(0, exec("//f:slow")),
        at(
            600_000,
            BuildEventKind::ResultEnd {
                cancelled: false,
                addr: "//f:slow".into(),
                error: Some("timed out after 10m".into()),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: Some(TimedOutData {
                    timeout_ms: 600_000,
                    abandoned: true,
                }),
            },
        ),
    ]);
    let r = Redact::default();
    let mut t = open_on(&s, "//f:root", TargetTab::Overview, None);
    let f = target_frame(&s, &mut t, 1_000, &r);
    assert!(
        f.iter()
            .any(|l| l.contains("took down") && l.contains("//f:d1, //f:d2")),
        "{f:#?}"
    );
    let mut t = open_on(&s, "//f:root", TargetTab::Log, None);
    let f = target_frame(&s, &mut t, 1_000, &r);
    assert!(
        f.iter().any(|l| l.contains("✗ failed after")
            && l.contains("exit 101")
            && l.contains("took down //f:d1, //f:d2")),
        "{f:#?}"
    );
    let mut t = open_on(&s, "//f:slow", TargetTab::Overview, None);
    let f = target_frame(&s, &mut t, 700_000, &r);
    let row = f
        .iter()
        .find(|l| l.trim_start().starts_with("timeout"))
        .expect("a timeout row");
    assert!(
        row.contains("timed out 10m") && row.contains("abandoned"),
        "{row}"
    );
}

/// The Overview's resources row is what a run reserved: a cache hit, a
/// pending target and a dependency failure never ran, so they have none.
#[test]
fn overview_resources_only_for_a_run() {
    let s = fold(&[
        at(0, start("//r:ran")),
        at(0, exec("//r:ran")),
        at(100, exec_end("//r:ran")),
        at(100, end("//r:ran", None, None)),
        at(0, start("//r:hit")),
        at(10, hit("//r:hit")),
        at(10, end("//r:hit", None, None)),
        at(0, start("//r:pending")),
        at(0, start("//r:dep")),
        at(200, end("//r:dep", Some("dep"), Some("//r:ran"))),
    ]);
    let r = Redact::default();
    let resources_row = |addr: &str| {
        let mut t = open_on(&s, addr, TargetTab::Overview, None);
        let f = target_frame(&s, &mut t, 1_000, &r);
        f.into_iter()
            .find(|l| l.trim_start().starts_with("resources"))
    };
    let ran = resources_row("//r:ran").expect("a resources row for a run");
    assert!(ran.contains("cpu"), "{ran}");
    for addr in ["//r:hit", "//r:pending", "//r:dep"] {
        assert_eq!(resources_row(addr), None, "{addr}");
    }
}

/// I9: a stream whose failure, log, outputs and unavailable texts name
/// heph's home: no dashboard tab, LAST LINE column or target screen shows
/// it.
#[test]
fn no_home_path_on_any_screen_dashboard_and_target() {
    let home = "/home/u/.heph";
    let redact = Redact::of([std::path::PathBuf::from(home)]);
    let fail = |addr: &str| BuildEventKind::ResultEnd {
        cancelled: false,
        addr: addr.into(),
        error: Some(format!("{home}/sandbox/x/run.sh: not found")),
        upstream_of: None,
        exit_status: None,
        log_tail: None,
        timed_out: None,
    };
    let mut s = BuildState::new();
    s.set_redact(redact.clone());
    for e in [
        at(
            0,
            matched(&["//h:fail", "//h:dep", "//h:done", "//h:gone", "//h:wait"]),
        ),
        at(0, start("//h:fail")),
        at(0, exec("//h:fail")),
        at(0, start("//h:dep")),
        at(0, start("//h:done")),
        at(0, exec("//h:done")),
        at(0, start("//h:gone")),
        at(0, exec("//h:gone")),
        at(0, start("//h:wait")),
        at(100, admission("//h:wait", "cpu", None, None)),
        at(500, fail("//h:fail")),
        at(501, end("//h:dep", Some("dep"), Some("//h:fail"))),
        at(600, end("//h:done", None, None)),
        at(600, end("//h:gone", None, None)),
    ] {
        s.apply(&e);
    }
    let reader = Arc::new(StubReader::default());
    let log = format!("compiling\nerror: {home}/sandbox/x/main.rs:3: bad\n");
    reader.set_log("//h:fail", log.as_bytes());
    reader.set_log("//h:done", log.as_bytes());
    reader.set_outputs(
        "//h:fail",
        OutputsFound::Unavailable(format!(
            "outputs unavailable: cannot list {home}/failures/x"
        )),
    );
    reader.set_outputs(
        "//h:done",
        OutputsFound::Listing(vec![OutputGroup {
            name: "out".into(),
            size: 1,
            hash: None,
            files: Vec::new(),
            unavailable: Some(format!("not in the local cache: {home}/cache/x")),
        }]),
    );

    let mut seen = Vec::new();
    // The dashboard, every tab, with the log's last line in the column.
    let mut lines = LastLines::default();
    let mut st = ListState::default();
    for tab in [Tab::All, Tab::Failed, Tab::Pending] {
        frame_with(&s, &mut st, tab, 1_000, &lines, &redact);
        let ids: Vec<Id> = st.visible().to_vec();
        lines.apply(
            ids.iter()
                .map(|&id| TailUpdate {
                    id,
                    finished: true,
                    size: Some(log.len() as u64),
                    line: Some(list::last_line(log.as_bytes())),
                })
                .collect(),
            &ids,
        );
        let f = frame_with(&s, &mut st, tab, 1_000, &lines, &redact);
        if tab != Tab::Pending {
            assert!(f.iter().any(|r| r.contains("main.rs:3")), "{tab:?}: {f:#?}");
        }
        seen.extend(f);
    }
    // Each target screen, every tab, its feed answered.
    for addr in ["//h:fail", "//h:dep", "//h:done", "//h:gone", "//h:wait"] {
        for tab in [TargetTab::Overview, TargetTab::Log, TargetTab::Outputs] {
            let mut t = open_on(&s, addr, tab, Some(reader.clone()));
            let f = target_frame_until(&s, &mut t, &redact, |f| {
                !f.iter()
                    .any(|l| l.contains("opening the log") || l.contains("reading "))
            });
            seen.extend(f);
        }
    }
    let all = seen.join("\n");
    assert!(
        all.contains("main.rs:3") && all.contains("not in the local cache"),
        "{all}"
    );
    assert!(!all.contains(home), "{all}");
    assert!(!all.contains("/home/u"), "{all}");
}

/// Target screens never read on the frame's thread: the log and the
/// outputs are read by the screen's own thread, and a frame only takes
/// what it found.
#[test]
fn frame_does_no_io_target_screens() {
    let s = fold(&[
        at(0, start("//n:a")),
        at(0, exec("//n:a")),
        at(1_000, exec_end("//n:a")),
        at(1_000, end("//n:a", None, None)),
    ]);
    let reader = Arc::new(StubReader::default());
    reader.set_log("//n:a", b"one\ntwo\n");
    reader.set_outputs("//n:a", OutputsFound::Nothing);
    let r = Redact::default();
    let mut t = open_on(&s, "//n:a", TargetTab::Overview, Some(reader.clone()));
    for _ in 0..5 {
        target_frame(&s, &mut t, 2_000, &r);
    }
    assert!(reader.threads().is_empty(), "Overview reads nothing");
    t.cycle_tab(true);
    target_frame_until(&s, &mut t, &r, |f| f.iter().any(|l| l.contains("two")));
    t.cycle_tab(true);
    target_frame_until(&s, &mut t, &r, |f| {
        f.iter().any(|l| l.contains("no outputs held"))
    });
    for _ in 0..5 {
        target_frame(&s, &mut t, 2_000, &r);
    }
    let threads = reader.threads();
    assert_eq!(
        threads.len(),
        2,
        "one log open, one outputs read: {threads:?}"
    );
    let me = std::thread::current().id();
    assert!(
        threads.iter().all(|&th| th != me),
        "read on the frame's thread"
    );
}

/// One full-screen frame of the whole view, `cols` × `rows`, at `T0 + 10s`.
fn view_frame(v: &mut crate::tui::TuiProgressView, cols: u16, rows: u16) -> Vec<String> {
    use crate::tui::app::TUIAppView;
    v.render_full(T0 + 10_000, cols, rows)
        .iter()
        .map(text)
        .collect()
}

/// The cell where `needle` first starts on frame row `row`, if it is there.
fn cell_in(f: &[String], row: usize, needle: &str) -> Option<(u16, u16)> {
    let line = f.get(row)?;
    let at = line.find(needle)?;
    let col = crate::tui::text::width(line.get(..at)?);
    Some((u16::try_from(col).ok()?, u16::try_from(row).ok()?))
}

/// The cell where `needle` first starts on any row.
fn cell_of(f: &[String], needle: &str) -> (u16, u16) {
    (0..f.len())
        .find_map(|r| cell_in(f, r, needle))
        .unwrap_or_else(|| panic!("{needle} in {f:#?}"))
}

/// The columns the rule under a tab row marks heavy: the active tab.
fn heavy(rule: &str) -> std::ops::Range<u16> {
    let mut cols = rule
        .chars()
        .enumerate()
        .filter(|&(_, c)| c == '━')
        .filter_map(|(i, _)| u16::try_from(i).ok());
    let start = cols.next().unwrap_or(0);
    start..cols.last().unwrap_or(start) + 1
}

fn click(v: &mut crate::tui::TuiProgressView, (col, row): (u16, u16)) {
    use crate::tui::app::{MouseInput, TUIAppView};
    v.mouse(MouseInput::Click { col, row });
}

fn wheel(v: &mut crate::tui::TuiProgressView, down: bool) {
    use crate::tui::app::{MouseInput, TUIAppView};
    v.mouse(MouseInput::Wheel {
        col: 10,
        row: 10,
        down,
    });
}

/// Three targets (done, running, pending) and a reader whose done target
/// has a folded output group.
fn mouse_view() -> crate::tui::TuiProgressView {
    let reader = Arc::new(StubReader::default());
    let files: Vec<OutputFile> = (1..=5)
        .map(|i| file(&format!("gen/f{i}.rs"), i * 1_000))
        .collect();
    reader.set_outputs(
        "//m:a",
        OutputsFound::Listing(vec![OutputGroup {
            name: "rust".into(),
            size: 15_000,
            hash: None,
            files,
            unavailable: None,
        }]),
    );
    let mut v = crate::tui::TuiProgressView::new("heph r build //m:...")
        .with_target_reader(reader as Arc<dyn TargetReader>);
    for e in [
        at(0, matched(&["//m:a", "//m:b", "//m:c"])),
        at(0, start("//m:a")),
        at(0, exec("//m:a")),
        at(1_000, exec_end("//m:a")),
        at(1_000, end("//m:a", None, None)),
        at(0, start("//m:b")),
        at(0, exec("//m:b")),
    ] {
        crate::tui::app::TUIAppView::apply(&mut v, &e);
    }
    v.open_dashboard();
    v
}

/// Clicks do what the keys do, on the element the last frame drew there:
/// each dashboard tab, a row (once selects, again opens), the target tabs,
/// the breadcrumb, a folded output group; after a resize, the new layout.
/// No click anywhere resolves an approval.
#[test]
fn mouse_clicks_hit_the_drawn_element() {
    let (cols, rows) = (120, 30);
    let mut v = mouse_view();

    // Each dashboard tab, by its label on the tab row.
    for name in ["Running", "Pending", "Failed", "Done", "Cached", "All"] {
        let f = view_frame(&mut v, cols, rows);
        let at = cell_in(&f, 2, name).unwrap_or_else(|| panic!("{name}: {f:#?}"));
        click(&mut v, at);
        let f = view_frame(&mut v, cols, rows);
        assert!(heavy(&f[3]).contains(&at.0), "{name}: {f:#?}");
    }

    // A row: the first click selects it, the second opens it.
    let f = view_frame(&mut v, cols, rows);
    let c = cell_of(&f, "//m:c");
    assert!(!f[usize::from(c.1)].starts_with(" ▸"), "{f:#?}");
    click(&mut v, c);
    let f = view_frame(&mut v, cols, rows);
    assert!(f[usize::from(c.1)].starts_with(" ▸"), "selected: {f:#?}");
    assert!(cell_in(&f, 2, "◂ esc").is_none(), "one click opens nothing");
    click(&mut v, c);
    let f = view_frame(&mut v, cols, rows);
    assert!(
        cell_in(&f, 2, "◂ esc").is_some() && f[2].contains("//m:c"),
        "the second click opens it: {f:#?}"
    );

    // The target screen's tabs, then the breadcrumb back to the list.
    for name in ["Log", "Outputs", "Overview"] {
        let at = cell_in(&f, 2, name).unwrap_or_else(|| panic!("{name}: {f:#?}"));
        click(&mut v, at);
        let g = view_frame(&mut v, cols, rows);
        assert!(heavy(&g[3]).contains(&at.0), "{name}: {g:#?}");
    }
    click(&mut v, cell_of(&f, "◂ esc"));
    let f = view_frame(&mut v, cols, rows);
    assert!(
        cell_in(&f, 2, "◂ esc").is_none() && f.iter().any(|l| l.contains("TARGET")),
        "back on the list: {f:#?}"
    );

    // A folded group on the done target's Outputs.
    let a = cell_of(&f, "//m:a");
    click(&mut v, a);
    click(&mut v, a);
    let f = view_frame(&mut v, cols, rows);
    click(&mut v, cell_in(&f, 2, "Outputs").expect("Outputs tab"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let f = loop {
        let f = view_frame(&mut v, cols, rows);
        if f.iter().any(|l| l.contains("more files")) {
            break f;
        }
        assert!(std::time::Instant::now() < deadline, "{f:#?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(!f.iter().any(|l| l.contains("gen/f4.rs")), "{f:#?}");
    // Off the fold's row: nothing expands.
    click(&mut v, (5, u16::try_from(f.len() - 3).unwrap_or(0)));
    assert!(
        !view_frame(&mut v, cols, rows)
            .iter()
            .any(|l| l.contains("gen/f4.rs"))
    );
    click(&mut v, cell_of(&f, "more files"));
    let f = view_frame(&mut v, cols, rows);
    assert!(
        f.iter().any(|l| l.contains("gen/f4.rs")) && !f.iter().any(|l| l.contains("more files")),
        "expanded: {f:#?}"
    );
    click(&mut v, cell_of(&f, "◂ esc"));

    // A narrower terminal draws the tabs elsewhere; a click reads the
    // frame it drew.
    let wide = view_frame(&mut v, cols, rows);
    let narrow = view_frame(&mut v, 60, rows);
    let failed = cell_in(&narrow, 2, "✗").unwrap_or_else(|| panic!("{narrow:#?}"));
    assert_ne!(Some(failed), cell_in(&wide, 2, "✗"), "the layout moved");
    click(&mut v, failed);
    let f = view_frame(&mut v, 60, rows);
    assert!(heavy(&f[3]).contains(&failed.0), "{f:#?}");
}

/// No click anywhere decides an approval: a pending request survives a
/// click on every cell of every screen the clicks reach (its ✦ row opens
/// the approval view, whose breadcrumb goes back), and a row under the ✦
/// row that leads All is hit where it is drawn.
#[test]
fn mouse_clicks_never_decide_an_approval() {
    use crate::tui::app::TUIAppView;
    let center = crate::tui::approval::ApprovalCenter::new();
    let ticket = center.request(&center.build(), "//m:c".into(), Vec::new());
    let mut v = mouse_view().with_approval(center.clone());
    let (cols, rows) = (100, 24);

    // Under the ✦ row, the list's rows are where they are drawn.
    let f = view_frame(&mut v, cols, rows);
    let gate = cell_of(&f, "awaits approval");
    let a = cell_of(&f, "//m:a ");
    assert!(a.1 > gate.1, "the ✦ row leads All: {f:#?}");
    assert!(!f[usize::from(a.1)].starts_with(" ▸"), "{f:#?}");
    click(&mut v, a);
    let f = view_frame(&mut v, cols, rows);
    assert!(f[usize::from(a.1)].starts_with(" ▸"), "{f:#?}");

    // The ✦ row, clicked as a row is opened, opens its approval view.
    click(&mut v, gate);
    click(&mut v, gate);
    assert!(
        v.in_approval_view(),
        "{:#?}",
        view_frame(&mut v, cols, rows)
    );
    let f = view_frame(&mut v, cols, rows);
    assert!(f[2].contains("//m:c"), "{f:#?}");

    for row in 0..rows {
        for col in 0..cols {
            click(&mut v, (col, row));
            drop(view_frame(&mut v, cols, rows));
        }
    }
    assert!(center.is_active(), "the request is still pending");
    assert_eq!(center.current().map(|c| c.addr), Some("//m:c".to_owned()));
    drop(ticket);
}

/// The wheel scrolls the list with the selection kept on screen, and the
/// Log; up in a following Log stops following, down leaves it following.
#[test]
fn wheel_scrolls_and_stops_following() {
    // The list: 40 pending targets in a 14-row frame.
    let addrs: Vec<String> = (0..40).map(|i| format!("//w:t{i:02}")).collect();
    let refs: Vec<&str> = addrs.iter().map(String::as_str).collect();
    let mut v = crate::tui::TuiProgressView::new("heph r build //w:...");
    crate::tui::app::TUIAppView::apply(&mut v, &at(0, matched(&refs)));
    v.open_dashboard();
    let selected = |f: &[String]| -> Option<String> {
        f.iter()
            .find(|l| l.starts_with(" ▸"))
            .and_then(|l| l.split_whitespace().find(|w| w.starts_with("//")))
            .map(str::to_owned)
    };
    let f = view_frame(&mut v, 100, 14);
    assert!(f.iter().any(|l| l.contains("1–5 of 40")), "{f:#?}");
    assert_eq!(selected(&f).as_deref(), Some("//w:t00"));
    wheel(&mut v, true);
    let f = view_frame(&mut v, 100, 14);
    assert!(f.iter().any(|l| l.contains("4–8 of 40")), "{f:#?}");
    assert_eq!(selected(&f).as_deref(), Some("//w:t03"), "kept on screen");
    for _ in 0..20 {
        wheel(&mut v, true);
    }
    let f = view_frame(&mut v, 100, 14);
    assert!(
        f.iter().any(|l| l.contains("36–40 of 40")),
        "the end: {f:#?}"
    );
    wheel(&mut v, false);
    let f = view_frame(&mut v, 100, 14);
    assert!(f.iter().any(|l| l.contains("33–37 of 40")), "{f:#?}");
    // t35 is still in the window (32–36): the wheel leaves it selected.
    assert_eq!(selected(&f).as_deref(), Some("//w:t35"), "still on screen");

    // The Log: 200 lines, following its end.
    let s = fold(&[
        at(0, start("//w:log")),
        at(0, exec("//w:log")),
        at(1_000, exec_end("//w:log")),
        at(1_000, end("//w:log", None, None)),
    ]);
    let reader = Arc::new(StubReader::default());
    let log: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    reader.set_log("//w:log", log.as_bytes());
    let r = Redact::default();
    let mut t = open_on(&s, "//w:log", TargetTab::Log, Some(reader));
    let shows = |f: &[String], n: u32| {
        f.iter()
            .any(|l| l.trim_end().ends_with(&format!("line {n}")))
    };
    let f = target_frame_until(&s, &mut t, &r, |f| shows(f, 200));
    let first = (1..=200).find(|&n| shows(&f, n)).expect("a line in view");
    t.wheel(true);
    let f = target_frame(&s, &mut t, 10_000, &r);
    assert!(
        shows(&f, 200),
        "down while following stays at the end: {f:#?}"
    );
    t.wheel(false);
    let f = target_frame(&s, &mut t, 10_000, &r);
    assert!(!shows(&f, 200), "up stops following: {f:#?}");
    assert!(shows(&f, first - 3), "three lines up: {f:#?}");
    t.wheel(true);
    let f = target_frame(&s, &mut t, 10_000, &r);
    assert!(shows(&f, first), "down scrolls back: {f:#?}");

    // Through the whole view, as the terminal drives it: `l` on a running
    // target, frames until its end shows, one wheel step up.
    let reader = Arc::new(StubReader::default());
    reader.set_live("//w:live", log.as_bytes());
    let mut v = crate::tui::TuiProgressView::new("heph r build //w:...")
        .with_target_reader(reader as Arc<dyn TargetReader>);
    for e in [
        at(0, matched(&["//w:live"])),
        at(0, start("//w:live")),
        at(0, exec("//w:live")),
    ] {
        crate::tui::app::TUIAppView::apply(&mut v, &e);
    }
    v.open_dashboard();
    drop(view_frame(&mut v, 140, 40));
    crate::tui::app::TUIAppView::open_selected(&mut v, true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let f = loop {
        let f = view_frame(&mut v, 140, 40);
        if shows(&f, 200) {
            break f;
        }
        assert!(std::time::Instant::now() < deadline, "{f:#?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let first = (1..=200).find(|&n| shows(&f, n)).expect("a line in view");
    wheel(&mut v, false);
    let f = view_frame(&mut v, 140, 40);
    assert!(!shows(&f, 200), "up stops following, view: {f:#?}");
    assert!(shows(&f, first - 3), "three lines up, view: {f:#?}");

    // The same on a log still being written, followed live.
    let s = fold(&[at(0, start("//w:live")), at(0, exec("//w:live"))]);
    let reader = Arc::new(StubReader::default());
    reader.set_live("//w:live", log.as_bytes());
    let mut t = open_on(&s, "//w:live", TargetTab::Log, Some(reader));
    let f = target_frame_until(&s, &mut t, &r, |f| shows(f, 200));
    let first = (1..=200).find(|&n| shows(&f, n)).expect("a line in view");
    t.wheel(false);
    let f = target_frame(&s, &mut t, 10_000, &r);
    assert!(!shows(&f, 200), "up stops following: {f:#?}");
    assert!(shows(&f, first - 3), "three lines up, live: {f:#?}");
}
