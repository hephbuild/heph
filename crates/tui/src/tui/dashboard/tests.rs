//! The dashboard's list over a real [`BuildState`] fold: which tab each
//! state lands in, in what order, and in whose words.

use hcore::capacity::Unit;
use hcore::events::{BuildEvent, BuildEventKind, CacheOutcomeKind, TimedOutData, YieldingToData};
use ratatui::text::Line;

use super::*;
use crate::tui::progress::{BuildState, CountScope};

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
    st.set_tab(tab);
    let lines = LastLines::default();
    let inp = ListInputs {
        model: s.dashboard(),
        waits: s,
        lines: &lines,
        now_ms: T0 + now,
    };
    list_lines(&inp, st, 120, 20).iter().map(text).collect()
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
    assert!(b.contains("(2 free, 6 ahead, 41s)"), "{b}");
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
/// `admission_wait_text` (no queue position for a yielding waiter), and a
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
            6,
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
        if yields.is_some() {
            assert!(!words.detail.contains("ahead"), "{words:?}");
        }
        let mut st = ListState::default();
        let rows = frame(&s, &mut st, Tab::Pending, 42_000);
        let row = rows.iter().find(|r| r.contains("//w:a")).expect("the row");
        assert!(row.contains(&words.phase), "{row}");
        let shown: String = words.detail.chars().take(20).collect();
        assert!(row.contains(&shown), "{row}");
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
