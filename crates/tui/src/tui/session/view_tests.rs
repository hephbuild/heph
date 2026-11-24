//! `heph session attach` draws what the run drew: the run's view, recorded,
//! replayed into its attached role, renders the same rows. The `Role` seam in
//! `progress.rs` is the only difference, and these tests pin it down.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use hcore::events::{BuildEvent, BuildEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use super::recorder::ViewTap;
use super::*;
use crate::tui::app::{Decide, TUIAppView};
use crate::tui::approval::{ApprovalCenter, ApprovalNoticeView};
use crate::tui::dashboard::approval::test_store;
use crate::tui::progress::{BuildHeader, BuildState, CountScope, ProgressHeader, TuiProgressView};

const WIDTH: u16 = 140;
const HEIGHT: u16 = 16;
const BASE: u64 = 1_700_000_000_000;
/// The clock both views are drawn at: long enough after `BASE` that the slow
/// target counts as long-running.
const NOW: u64 = BASE + 12_000;

/// [`BuildHeader`] whose label the test changes mid-run, as `heph run`'s
/// label changes when its matcher resolves.
struct SharedLabel {
    inner: BuildHeader,
    label: Arc<Mutex<String>>,
}

impl ProgressHeader for SharedLabel {
    fn label(&self) -> String {
        self.label
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn last_render(&self, core: &BuildState, scope: CountScope) -> String {
        self.inner.last_render(core, scope)
    }
}

fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
    BuildEvent {
        at_unix_ms: at,
        kind,
    }
}

fn result_end(addr: &str, error: Option<&str>) -> BuildEventKind {
    BuildEventKind::ResultEnd {
        left_behind: None,
        cancelled: false,
        addr: addr.into(),
        error: error.map(Into::into),
        upstream_of: None,
        exit_status: None,
        log_tail: None,
        timed_out: None,
    }
}

/// A run: two targets that finish (one fails), one still executing past the
/// long-running threshold, one waiting on a lock held by `holder_pid`.
fn events(holder_pid: u32) -> Vec<BuildEvent> {
    let start = |addr: &str| BuildEventKind::ResultStart { addr: addr.into() };
    vec![
        ev(
            BASE,
            BuildEventKind::Matched {
                addrs: vec![
                    "//a:ok".into(),
                    "//a:bad".into(),
                    "//a:slow".into(),
                    "//a:locked".into(),
                ],
                complete: true,
            },
        ),
        ev(BASE + 1, start("//a:ok")),
        ev(BASE + 2, result_end("//a:ok", None)),
        ev(BASE + 3, start("//a:bad")),
        ev(BASE + 4, result_end("//a:bad", Some("boom"))),
        ev(BASE + 5, start("//a:slow")),
        ev(
            BASE + 6,
            BuildEventKind::ExecuteStart {
                output_groups: Vec::new(),
                log_not_streamed: false,
                addr: "//a:slow".into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
                admitted: None,
            },
        ),
        ev(BASE + 7, start("//a:locked")),
        ev(
            BASE + 8,
            BuildEventKind::ResultLockWaitStart {
                addr: "//a:locked".into(),
                holder_pid: Some(holder_pid),
                in_use_by_readers: false,
            },
        ),
    ]
}

fn read_records(dir: &Path) -> Vec<SessionRecord> {
    let file = std::fs::File::open(dir.join(files::EVENTS)).expect("events");
    let mut r = RecordReader::new(file);
    let recs: Vec<_> = std::iter::from_fn(|| r.next_record().expect("read")).collect();
    assert_eq!(r.skipped(), 0, "every record reads back");
    recs
}

fn row(buf: &Buffer, y: u16) -> String {
    (buf.area.left()..buf.area.right())
        .filter_map(|x| buf.cell(Position::new(x, y)))
        .map(|c| c.symbol())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// `lines` as a `width` × [`HEIGHT`] terminal shows them: one string per row.
fn paint(lines: Vec<ratatui::text::Line<'static>>, width: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).expect("terminal");
    terminal
        .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
        .expect("draw");
    let buf = terminal.backend().buffer();
    (buf.area.top()..buf.area.bottom())
        .map(|y| row(buf, y))
        .collect()
}

/// The dashboard as the alternate screen shows it: one string per row.
fn draw(view: &mut TuiProgressView) -> Vec<String> {
    draw_at(view, WIDTH)
}

/// [`draw`] on a terminal `width` columns wide.
fn draw_at(view: &mut TuiProgressView, width: u16) -> Vec<String> {
    let lines = view.render_full(NOW, width, HEIGHT);
    paint(lines, width)
}

const KEYS: &str = " · enter to review";
/// What a viewer's approval prompt row says in place of [`KEYS`], after
/// the pending count.
const ELSEWHERE: &str = " · decide in the terminal that runs it";
/// The line's hint on a viewer, and on the run.
const DETACH_HINT: &str = "q detach · tab ⇥";
const HINT: &str = "tab ⇥";

/// A line row of the viewer, with its seam undone: its banner wording and
/// its hint read as the run's, runs of spaces as one.
fn line_as_live(row: &str) -> String {
    squash(&row.replace(ELSEWHERE, "").replace(DETACH_HINT, HINT))
}

/// A target screen's `attached` frame is `live`'s, apart from the title
/// naming the session and the hints ending in `q detach`.
fn assert_same_target_frame(live: &[String], attached: &[String], id: SessionId, at: &str) {
    assert_eq!(live.len(), attached.len(), "{at}: same height");
    let (live_help, live_rows) = live.split_last().expect("a help row");
    let (attached_help, attached_rows) = attached.split_last().expect("a help row");
    let session = format!("  session {id}");
    for (i, (live_row, attached_row)) in live_rows.iter().zip(attached_rows).enumerate() {
        if attached_row.contains(&session) {
            assert_eq!(
                squash(&attached_row.replace(&session, "")),
                squash(live_row),
                "{at}: the title, row {i}"
            );
        } else {
            assert_eq!(attached_row, live_row, "{at}: row {i}");
        }
    }
    assert_eq!(
        *attached_help,
        format!("{live_help} · q detach"),
        "{at}: the help row"
    );
}

/// The log both views' Log tab reads for the failed target.
const BAD_LOG: &str = "compile error: boom\n";
/// The fix both views' Outputs tab lists for it.
const BAD_FIX: &str = "src/fixed.rs";
/// The LAST LINE both views' list shows for the running target.
const SLOW_LINE: &str = "Compiling slow";

/// The run's own readers and the viewer's answer the same: what differs
/// between them in `heph session attach` is where they read, not the view.
struct SameReader;

impl crate::tui::dashboard::TargetReader for SameReader {
    fn open_log(&self, _addr: &str) -> crate::tui::dashboard::LogOpened {
        crate::tui::dashboard::LogOpened::Tail {
            bytes: BAD_LOG.as_bytes().to_vec(),
            len: BAD_LOG.len() as u64,
            about: "from the failure slot".into(),
        }
    }

    fn outputs(&self, _addr: &str, _failed: bool) -> crate::tui::dashboard::OutputsFound {
        crate::tui::dashboard::OutputsFound::Failure {
            kept: Vec::new(),
            fixes: vec![BAD_FIX.into()],
        }
    }
}

impl crate::tui::dashboard::LogReader for SameReader {
    fn size(&self, addr: &str) -> Option<u64> {
        (addr == "//a:slow").then_some(SLOW_LINE.len() as u64 + 1)
    }

    fn tail(&self, addr: &str, _max: usize) -> Option<Vec<u8>> {
        (addr == "//a:slow").then(|| format!("{SLOW_LINE}\n").into_bytes())
    }
}

/// [`draw`] until `needle` shows (a reader answers off the frame's thread),
/// failing after 10 s.
fn draw_until(view: &mut TuiProgressView, needle: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let frame = draw(view);
        if frame.iter().any(|r| r.contains(needle)) {
            return frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{needle:?} never showed:\n{}",
            frame.join("\n")
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// With readers given, the list's LAST LINE column and the failed target's
/// three screens, as `view` draws them: All tab, then Overview, Log and
/// Outputs of `//a:bad`. The view is left on the list's All tab.
fn draw_screens(view: &mut TuiProgressView) -> [Vec<String>; 4] {
    let list = draw_until(view, SLOW_LINE);
    // All · Running · Pending · Failed, and the failed target is the row.
    for _ in 0..3 {
        view.tab(true);
    }
    // A failed target opens on its Log.
    view.open_selected(false);
    let log = draw_until(view, BAD_LOG.trim_end());
    view.tab(true);
    let outputs = draw_until(view, BAD_FIX);
    view.tab(true);
    let overview = draw_until(view, "//a:bad");
    // From the target screen, back to the list.
    view.back_to_main();
    for _ in 0..3 {
        view.tab(false);
    }
    [list, overview, log, outputs]
}

/// Runs of spaces as one: a title whose session segment is gone pads its
/// right-aligned gauges wider, and that padding is not the seam.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `attached` is `live` with the seam applied: the banner without its keys,
/// the title naming the session (while it fits), the help row ending in
/// `q detach` instead of `ctrl-c stop`, cut where the terminal ends.
/// Everything else row for row.
fn assert_same_but_the_seam(
    live: &[String],
    attached: &[String],
    id: SessionId,
    width: u16,
    at: &str,
) {
    assert_eq!(live.len(), attached.len(), "{at}: same height");
    let (live_help, live_rows) = live.split_last().expect("a help row");
    let (attached_help, attached_rows) = attached.split_last().expect("a help row");
    let session = format!("  session {id}");
    for (i, (live_row, attached_row)) in live_rows.iter().zip(attached_rows).enumerate() {
        if attached_row.contains(&session) {
            // The title row: the session named, the rest the run's title.
            assert_eq!(
                squash(&attached_row.replace(&session, "")),
                squash(live_row),
                "{at}: the title, row {i}"
            );
        } else {
            assert_eq!(
                attached_row.replace(ELSEWHERE, ""),
                live_row.replace(KEYS, "").trim_end(),
                "{at}: row {i}"
            );
        }
    }
    if live_help.ends_with(" · ctrl-c stop") {
        let help = format!(
            "{} · q detach",
            live_help.trim_end_matches(" · ctrl-c stop")
        );
        let help: String = help.chars().take(usize::from(width)).collect();
        assert_eq!(*attached_help, help.trim_end(), "{at}: the help row");
    } else {
        // Cut before the keys that differ: the live row, cut the same.
        assert_eq!(*attached_help, *live_help, "{at}: the help row, cut");
    }
}

/// The decision keys' hint, and what a viewer reads in its place.
const DECISION: &str = "y approve";
const READ_ONLY: &str = "decisions happen in the terminal that runs it";

/// The approval view as a viewer draws it is the run's, apart from the title
/// naming the session and the hint row: the decision keys there, the
/// read-only hint here (I11).
fn assert_same_approval_view(live: &[String], attached: &[String], id: SessionId) {
    assert_eq!(live.len(), attached.len(), "approval view: same height");
    let session = format!("  session {id}");
    let mut hints = 0;
    for (i, (live_row, attached_row)) in live.iter().zip(attached).enumerate() {
        if live_row.contains(DECISION) {
            hints += 1;
            assert!(
                attached_row.contains(READ_ONLY) && !attached_row.contains(DECISION),
                "approval view: the hint row\n{attached_row}"
            );
        } else if attached_row.contains(&session) {
            assert_eq!(
                squash(&attached_row.replace(&session, "")),
                squash(live_row),
                "approval view: the title, row {i}"
            );
        } else {
            assert_eq!(attached_row, live_row, "approval view: row {i}");
        }
    }
    assert_eq!(hints, 1, "one hint row:\n{}", live.join("\n"));
    let joined = live.join("\n");
    assert!(
        joined.contains("//a:gate") && joined.contains("1 of 3 pending"),
        "{joined}"
    );
}

/// A terminal narrower than the frame wants: rows are cut, the seam still the
/// only difference.
const NARROW: u16 = 60;

/// The frames a checkpoint compares beyond the All tab at full width: the
/// All tab narrow, then the Failed tab at both widths. The view is left on
/// the All tab.
fn draw_more(view: &mut TuiProgressView) -> [Vec<String>; 3] {
    let narrow = draw_at(view, NARROW);
    // All · Running · Pending · Failed: three Tabs to Failed.
    for _ in 0..3 {
        view.tab(true);
    }
    let failed = draw(view);
    let failed_narrow = draw_at(view, NARROW);
    for _ in 0..3 {
        view.tab(false);
    }
    [narrow, failed, failed_narrow]
}

/// The line, as the view shows it after `Esc` from the dashboard: one string
/// per row that has anything on it. The view is left with the dashboard open.
fn draw_line(view: &mut TuiProgressView) -> Vec<String> {
    view.back_to_main();
    let rows = paint(view.render("⠋", NOW, WIDTH, HEIGHT), WIDTH)
        .into_iter()
        .filter(|r| !r.is_empty())
        .collect();
    view.tab(true);
    rows
}

/// The run's view, recorded through the taps the interactive backend uses,
/// then replayed from `events.ndjson` into an attached view, draws the same
/// frame at each point the approval prompt changed, and prints the same
/// final line.
#[test]
fn attached_frames_match_live() {
    let root = tempfile::tempdir().expect("tempdir");
    let session_root = SessionRoot::new(root.path());
    // Another session of this process holds the lock, so both views name it.
    let holder = Session::open(&session_root, SessionStart::default()).expect("holder");
    let rec = Session::open(
        &session_root,
        SessionStart {
            header: SessionHeader {
                kind: HeaderKind::Build,
                label: "run //a:...".into(),
            },
            interactive: true,
            ..SessionStart::default()
        },
    )
    .expect("open");

    // The run's view, wired as `run_app` wires it.
    let label = Arc::new(Mutex::new("run //a:...".to_owned()));
    let center = ApprovalCenter::new();
    let mut live = TuiProgressView::with_header(Box::new(SharedLabel {
        inner: BuildHeader::new("unused"),
        label: label.clone(),
    }))
    .with_approval(center.clone());
    // The dashboard's frames are compared first; the line has its own checks.
    live.open_dashboard();
    // The backend's receiver resolves the holder before the fold; the events
    // here are applied by hand, so resolve up front.
    let live_names = HolderNames::new(Some(session_root.clone()), Some(rec.id()));
    live_names.resolve_blocking(std::process::id());
    live.name_lock_holders(live_names);
    let tap = rec.tap();
    let mut view_tap = ViewTap::new(tap.clone(), &live);

    for e in events(std::process::id()) {
        live.apply(&e);
        tap.event(e);
    }
    *label.lock().unwrap_or_else(PoisonError::into_inner) = "run //a:... (4 matched)".into();
    let build = center.build();
    let gate = center.request(
        &build,
        "//a:gate".into(),
        vec![ApprovalNoticeView {
            name: "plan".into(),
            sources: Vec::new(),
        }],
    );
    let next = center.request(&build, "//a:next".into(), Vec::new());
    // One with a row of its own in the model: the ✦ row and Approvals tab.
    let slow = center.request(&build, "//a:slow".into(), Vec::new());
    view_tap.observe(&live);
    let live_prompt = draw(&mut live);
    let live_prompt_line = draw_line(&mut live);
    assert!(live.approval_open());
    let live_view = draw(&mut live);
    live.back_to_main();

    center.respond(gate.seq, true, Vec::new());
    center.respond(next.seq, false, Vec::new());
    center.respond(slow.seq, true, Vec::new());
    assert_eq!(gate.rx.blocking_recv(), Ok(true));
    assert_eq!(next.rx.blocking_recv(), Ok(false));
    assert_eq!(slow.rx.blocking_recv(), Ok(true));
    view_tap.observe(&live);
    let live_after = draw(&mut live);
    let live_after_line = draw_line(&mut live);
    let live_more = draw_more(&mut live);
    let mut live = live
        .with_log_reader(Arc::new(SameReader))
        .with_target_reader(Arc::new(SameReader));
    let live_screens = draw_screens(&mut live);

    let finished_at = NOW + 500;
    live.set_finished(finished_at);
    view_tap.finished(&live, finished_at);
    rec.finish(Some(1), false);
    let live_final = live.final_line().expect("a summary");

    // The viewer: the same view, attached, fed from the recording.
    let feed = SessionFeed::default();
    let mut attached = TuiProgressView::new("run //a:...").attached(rec.id(), feed.clone(), || {});
    attached.open_dashboard();
    let attached_names = FileSource::new(root.path()).holder_names(rec.id());
    attached_names.resolve_blocking(std::process::id());
    attached.name_lock_holders(attached_names);
    let mut checkpoints = Vec::new();
    for record in read_records(rec.dir()) {
        let prompt = match &record.session {
            Some(SessionRecordKind::Approval { prompt, .. }) => Some(prompt.is_some()),
            _ => None,
        };
        match feed.route(record) {
            Routed::Event(e) => attached.apply(&e),
            Routed::Nothing => {}
            other => panic!("nothing is printed in this run: {other:?}"),
        }
        if let Some(shown) = prompt {
            let frame = draw(&mut attached);
            let line = draw_line(&mut attached);
            let more = (!shown).then(|| draw_more(&mut attached));
            // The approval view opens, read-only, while one is pending.
            let view = attached.approval_open().then(|| {
                let view = draw(&mut attached);
                attached.back_to_main();
                view
            });
            assert_eq!(view.is_some(), shown, "a view opens while one is pending");
            checkpoints.push((shown, frame, line, more, view));
        }
    }
    let [
        (true, attached_prompt, attached_prompt_line, None, Some(attached_view)),
        (false, attached_after, attached_after_line, Some(attached_more), None),
    ] = checkpoints.as_slice()
    else {
        panic!(
            "one prompt shown, then cleared: {:?}",
            checkpoints.iter().map(|(s, ..)| s).collect::<Vec<_>>()
        );
    };

    // The frame shows what it should, so equality below is not vacuous.
    let joined = live_prompt.join("\n");
    for needle in [
        "//a:slow",
        // The dashboard's LAST LINE for a lock wait names the holder.
        &format!("held by session {}", holder.id()),
        "(4 matched)",
        // The pending request with a model row: its ✦ row.
        "awaits approval",
    ] {
        assert!(joined.contains(needle), "{needle:?} in\n{joined}");
    }
    // The prompt is the line's and the Approvals tab's: the dashboard has
    // no banner, and never a notice's path.
    assert!(!joined.contains("/ws/.heph"), "{joined}");

    let [live_narrow, live_failed, live_failed_narrow] = &live_more;
    let failed = live_failed.join("\n");
    assert!(
        failed.contains("//a:bad") && !failed.contains("//a:slow"),
        "the failed tab lists the failure, not the running target:\n{failed}"
    );
    assert!(
        live_narrow
            .iter()
            .all(|r| r.chars().count() <= usize::from(NARROW)),
        "{live_narrow:#?}"
    );

    // The line: the prompt row over it while the prompt is up, the
    // approval spotlit; then the root failure.
    let [prompt_row, line] = live_prompt_line.as_slice() else {
        panic!("a prompt row and the line: {live_prompt_line:#?}");
    };
    assert!(
        prompt_row.contains("approval required: //a:gate · 3 pending")
            && prompt_row.ends_with(KEYS),
        "{prompt_row}"
    );
    assert!(line.contains("✦ //a:gate 3 pending"), "{line}");
    assert!(line.ends_with("tab ⇥"), "{line}");
    let [line] = live_after_line.as_slice() else {
        panic!("the line alone: {live_after_line:#?}");
    };
    assert!(
        line.contains("1 failed") && line.contains("✗ //a:bad"),
        "{line}"
    );
    // The viewer's seam on the line: the banner says where to decide, the
    // hint that `q` detaches.
    let [attached_prompt_row, attached_line] = attached_prompt_line.as_slice() else {
        panic!("a prompt row and the line: {attached_prompt_line:#?}");
    };
    assert!(
        attached_prompt_row.ends_with(&format!("3 pending{ELSEWHERE}")),
        "{attached_prompt_row}"
    );
    assert!(attached_line.ends_with(DETACH_HINT), "{attached_line}");
    let as_live = |rows: &[String]| rows.iter().map(|r| line_as_live(r)).collect::<Vec<_>>();
    let expected: Vec<String> = live_prompt_line
        .iter()
        .map(|r| squash(&r.replace(KEYS, "")))
        .collect();
    assert_eq!(
        as_live(attached_prompt_line),
        expected,
        "the line, prompt shown"
    );
    let expected: Vec<String> = live_after_line.iter().map(|r| squash(r)).collect();
    assert_eq!(
        as_live(attached_after_line),
        expected,
        "the line, prompt cleared"
    );

    let id = rec.id();
    assert_same_but_the_seam(&live_prompt, attached_prompt, id, WIDTH, "prompt shown");
    assert_same_but_the_seam(&live_after, attached_after, id, WIDTH, "prompt cleared");
    assert_same_approval_view(&live_view, attached_view, id);
    let [attached_narrow, attached_failed, attached_failed_narrow] = attached_more;
    // Narrow enough that the seam itself is cut, so the cut is compared too.
    let narrow_help = attached_narrow.last().expect("a help row");
    assert!(!narrow_help.contains("q detach"), "{narrow_help}");
    assert_same_but_the_seam(live_narrow, attached_narrow, id, NARROW, "narrow");
    assert_same_but_the_seam(live_failed, attached_failed, id, WIDTH, "failed tab");
    assert_same_but_the_seam(
        live_failed_narrow,
        attached_failed_narrow,
        id,
        NARROW,
        "failed tab, narrow",
    );

    // Given the same readers, the LAST LINE column and the target screens
    // match too: the reader seam is where attach differs, not the view.
    let mut attached = attached
        .with_log_reader(Arc::new(SameReader))
        .with_target_reader(Arc::new(SameReader));
    let attached_screens = draw_screens(&mut attached);
    let [live_list, live_target @ ..] = &live_screens;
    let [attached_list, attached_target @ ..] = &attached_screens;
    assert_same_but_the_seam(live_list, attached_list, id, WIDTH, "LAST LINE");
    for ((live, attached), tab) in live_target
        .iter()
        .zip(attached_target)
        .zip(["Overview", "Log", "Outputs"])
    {
        assert_same_target_frame(live, attached, id, tab);
    }

    // The viewer finishes later than the run did; its summary still reads the
    // run's clock.
    attached.set_finished(finished_at + 60_000);
    assert_eq!(attached.final_line(), Some(live_final));
    holder.finish(Some(0), false);
}

/// The keys that act on the run work in the run's view and nowhere else: an
/// attached view given an approval queue cannot answer it, and `q` detaches
/// only a viewer.
#[test]
fn only_the_live_view_acts_on_the_run() {
    let center = ApprovalCenter::new();
    let mut live = TuiProgressView::new("run").with_approval(center.clone());
    let mut rx = center
        .request(&center.build(), "//a:gate".into(), Vec::new())
        .rx;
    assert!(
        live.approval_open(),
        "the run's view opens the approval view"
    );
    assert!(live.in_approval_view());
    // The lockout runs from the first frame that drew the request.
    drop(live.render_full(1_000, 100, 24));
    live.approval_decide(Decide::Approve, 1_500);
    assert_eq!(rx.try_recv(), Ok(true), "the run's view answers the prompt");

    let center = ApprovalCenter::new();
    let id = SessionId::new(BASE, 7);
    let detached = Arc::new(Mutex::new(0_u32));
    let count = detached.clone();
    let mut attached = TuiProgressView::new("run")
        .with_approval(center.clone())
        .attached(id, SessionFeed::default(), move || {
            *count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        });
    let mut rx = center
        .request(&center.build(), "//a:gate".into(), Vec::new())
        .rx;
    assert!(
        !attached.approval_open(),
        "a viewer holds no queue: its feed recorded no request"
    );
    assert!(!attached.in_approval_view());
    drop(attached.render_full(1_000, 100, 24));
    for decide in [
        Decide::Approve,
        Decide::Reject,
        Decide::ApproveAll,
        Decide::RejectAll,
    ] {
        attached.approval_decide(decide, 9_000);
    }
    attached.advance(9_000);
    assert!(center.is_active(), "the prompt is still pending");
    assert!(center.standing().is_none(), "no standing decision either");
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "nothing answered it",
    );
    assert!(
        attached.pending_approval().is_none(),
        "nor would it be recorded"
    );

    live.quit_while_running();
    assert_eq!(*detached.lock().unwrap_or_else(PoisonError::into_inner), 0);
    attached.quit_while_running();
    assert_eq!(
        *detached.lock().unwrap_or_else(PoisonError::into_inner),
        1,
        "q detaches the viewer",
    );
}

/// `prompt` routed into `feed` as the run's recorder writes it.
fn record_prompt(feed: &SessionFeed, prompt: Option<ApprovalPrompt>) {
    let routed = feed.route(SessionRecord::session(
        BASE,
        SessionRecordKind::Approval {
            prompt: prompt.map(Box::new),
            standing: None,
        },
    ));
    assert!(matches!(routed, Routed::Nothing), "{routed:?}");
}

/// A notice kept in the store under `home`.
fn stored(home: &Path, name: &str, content: &str) -> ApprovalNoticeView {
    test_store::kept(home, name, content)
}

/// A viewer of a session whose notices are in `home`'s store.
fn viewer(feed: &SessionFeed, home: Option<&Path>) -> TuiProgressView {
    let view = TuiProgressView::new("run").attached(SessionId::new(BASE, 7), feed.clone(), || {});
    match home {
        Some(home) => view.with_notices(test_store::roots(home), test_store::read),
        None => view,
    }
}

/// No row names heph's home or a notice's source.
fn assert_no_path(frame: &[String], home: &Path) {
    let joined = frame.join("\n");
    let home = home.display().to_string();
    assert!(
        !joined.contains(&home) && !joined.contains("store/"),
        "a path on screen:\n{joined}"
    );
}

/// An attached viewer opens a pending request read-only and reads its
/// notice from the run's store by the sources the session recorded; y/n/Y/N
/// decide nothing and it stays pending, until the run's terminal decides it
/// and the view closes.
#[test]
fn attached_reads_notice_from_the_store() {
    let home = tempfile::tempdir().expect("tempdir");
    let center = ApprovalCenter::new();
    let build = center.build();
    let plan = stored(home.path(), "plan", "drop the table");
    let mut gate = center.request(&build, "//a:gate".into(), vec![plan]);
    let feed = SessionFeed::default();
    record_prompt(&feed, center.prompt());

    let mut attached = viewer(&feed, Some(home.path()));
    assert!(attached.approval_open(), "Enter/v open the view");
    let frame = draw_until(&mut attached, "drop the table");
    let joined = frame.join("\n");
    assert!(
        joined.contains("//a:gate") && joined.contains("1 of 1 pending"),
        "{joined}"
    );
    assert!(
        joined.contains(READ_ONLY) && !joined.contains(DECISION),
        "no decision key, the read-only hint:\n{joined}"
    );
    assert_no_path(&frame, home.path());

    // Long past any lockout: still nothing decides.
    for (i, decide) in [
        Decide::Approve,
        Decide::Reject,
        Decide::ApproveAll,
        Decide::RejectAll,
    ]
    .into_iter()
    .enumerate()
    {
        attached.approval_decide(decide, NOW + 10_000 * (1 + i as u64));
        attached.advance(NOW + 10_000 * (1 + i as u64));
    }
    let after = draw(&mut attached).join("\n");
    assert!(
        after.contains("drop the table")
            && !after.contains("approved //")
            && !after.contains("rejected //"),
        "the same request, no decision shown:\n{after}"
    );
    assert!(
        matches!(
            gate.rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "still pending"
    );
    assert!(center.standing().is_none(), "no standing decision either");

    // The run's terminal decides it: the viewer's view closes with it.
    let _decided = center.respond(gate.seq, true, Vec::new());
    record_prompt(&feed, center.prompt());
    attached.advance(NOW + 60_000);
    assert!(!attached.in_approval_view());
}

/// The screen clause of `notice_unavailable_never_a_path`: a notice whose
/// source is gone, whose source no longer matches its hashout, one recorded
/// with no source, and any notice of a viewer that does not read the run's
/// store, each read "notice unavailable", and no path shows.
#[test]
fn notice_unavailable_never_a_path() {
    use crate::tui::session::{ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued};
    let home = tempfile::tempdir().expect("tempdir");
    let center = ApprovalCenter::new();
    let build = center.build();
    let plan = stored(home.path(), "plan", "the plan");
    let gone = stored(home.path(), "gone", "removed below");
    let tampered = stored(home.path(), "tampered", "rewritten below");
    std::fs::remove_file(test_store::file(home.path(), "gone")).expect("remove a source");
    std::fs::write(
        test_store::file(home.path(), "tampered"),
        "not what was hashed",
    )
    .expect("rewrite a source");
    let gate = center.request(&build, "//a:gate".into(), vec![plan, gone, tampered]);
    let feed = SessionFeed::default();
    record_prompt(&feed, center.prompt());

    let mut attached = viewer(&feed, Some(home.path()));
    assert!(attached.approval_open());
    let frame = draw_until(&mut attached, "the plan");
    assert_no_path(&frame, home.path());
    for tab in ["gone", "tampered"] {
        attached.tab(true);
        let frame = draw_until(&mut attached, "notice unavailable");
        assert!(frame.join("\n").contains("//a:gate"), "{tab}");
        assert!(!frame.join("\n").contains("the plan"), "{tab}");
        assert_no_path(&frame, home.path());
    }

    // A viewer that does not read the run's home: nothing is readable.
    let mut elsewhere = viewer(&feed, None);
    assert!(elsewhere.approval_open());
    let frame = draw_until(&mut elsewhere, "notice unavailable");
    assert!(!frame.join("\n").contains("the plan"));
    assert_no_path(&frame, home.path());

    // A request whose notice names no source: the view shows it as
    // unavailable.
    drop(gate);
    record_prompt(
        &feed,
        Some(ApprovalPrompt {
            queue: vec![ApprovalQueued {
                seq: 2,
                addr: "//a:sourceless".into(),
                queued_at_unix_ms: 0,
                notices: vec![ApprovalPromptNotice {
                    name: "plan".into(),
                    sources: Vec::new(),
                }],
            }],
        }),
    );
    attached.advance(NOW + 1_000);
    assert!(
        attached.in_approval_view(),
        "it moves on to the next request"
    );
    let frame = draw_until(&mut attached, "notice unavailable");
    let joined = frame.join("\n");
    assert!(joined.contains("//a:sourceless"), "{joined}");
    assert_no_path(&frame, home.path());
}
