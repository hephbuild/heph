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
use crate::tui::app::TUIAppView;
use crate::tui::approval::{ApprovalCenter, ApprovalNoticeView};
use crate::tui::progress::{
    BuildHeader, BuildState, CountScope, HeaderItem, ProgressHeader, TuiProgressView,
};

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
    fn header(&self, core: &BuildState, scope: CountScope) -> Vec<HeaderItem> {
        self.inner.header(core, scope)
    }

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

/// The view's frame as the terminal shows it: one string per row.
fn draw(view: &TuiProgressView) -> Vec<String> {
    draw_at(view, WIDTH)
}

/// [`draw`] on a terminal `width` columns wide.
fn draw_at(view: &TuiProgressView, width: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).expect("terminal");
    let lines = view.render("⠋", NOW, width, HEIGHT);
    terminal
        .draw(|f| f.render_widget(Paragraph::new(Text::from(lines)), f.area()))
        .expect("draw");
    let buf = terminal.backend().buffer();
    (buf.area.top()..buf.area.bottom())
        .map(|y| row(buf, y))
        .collect()
}

const KEYS: &str = "  [y] approve · [n] reject · enter view";

/// `attached` is `live` with the seam applied: the banner without its keys,
/// the help row with the session hint, cut where the terminal ends. Everything
/// else row for row.
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
    let expected: Vec<String> = live_rows.iter().map(|r| r.replace(KEYS, "")).collect();
    assert_eq!(attached_rows, expected.as_slice(), "{at}: the frame");
    let help: String = format!("{live_help} · session {id} · q detach")
        .chars()
        .take(usize::from(width))
        .collect();
    assert_eq!(*attached_help, help.trim_end(), "{at}: the help row");
}

/// A terminal narrower than the frame wants: rows are cut, the seam still the
/// only difference.
const NARROW: u16 = 60;

/// The frames a checkpoint compares beyond the main view at full width: the
/// main view narrow, then the failed tab at both widths. The view is left on
/// its main tab.
fn draw_more(view: &mut TuiProgressView) -> [Vec<String>; 3] {
    let narrow = draw_at(view, NARROW);
    // Shift+Tab from the main view lands on the last tab, Failed.
    view.tab(false);
    let failed = draw(view);
    let failed_narrow = draw_at(view, NARROW);
    view.tab(true);
    [narrow, failed, failed_narrow]
}

/// The run's view, recorded through the taps the interactive backend uses,
/// then replayed from `events.ndjson` into an attached view, draws the same
/// frame at each point the approval prompt changed, and prints the same
/// final line.
#[test]
fn attached_view_draws_what_the_run_drew() {
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
            content: "drop the table".into(),
            path: "/ws/.heph/notices/plan.txt".into(),
            hash: hcore::approval_notice::notice_hash(b"drop the table"),
        }],
    );
    let next = center.request(&build, "//a:next".into(), Vec::new());
    view_tap.observe(&live);
    let live_prompt = draw(&live);

    center.respond(gate.seq, true, Vec::new());
    center.respond(next.seq, false, Vec::new());
    assert_eq!(gate.rx.blocking_recv(), Ok(true));
    assert_eq!(next.rx.blocking_recv(), Ok(false));
    view_tap.observe(&live);
    let live_after = draw(&live);
    let live_more = draw_more(&mut live);

    let finished_at = NOW + 500;
    live.set_finished(finished_at);
    view_tap.finished(&live, finished_at);
    rec.finish(Some(1), false);
    let live_final = live.final_line().expect("a summary");

    // The viewer: the same view, attached, fed from the recording.
    let feed = SessionFeed::default();
    let mut attached = TuiProgressView::new("run //a:...").attached(rec.id(), feed.clone(), || {});
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
            let frame = draw(&attached);
            let more = (!shown).then(|| draw_more(&mut attached));
            checkpoints.push((shown, frame, more));
        }
    }
    let [
        (true, attached_prompt, None),
        (false, attached_after, Some(attached_more)),
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
        "approval required: //a:gate",
        "2 pending",
        "plan: /ws/.heph/notices/plan.txt",
        "//a:slow",
        &format!("locked by session {}", holder.id()),
        "(4 matched)",
    ] {
        assert!(joined.contains(needle), "{needle:?} in\n{joined}");
    }
    assert!(live_prompt.iter().any(|r| r.contains(KEYS)), "{joined}");

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

    let id = rec.id();
    assert_same_but_the_seam(&live_prompt, attached_prompt, id, WIDTH, "prompt shown");
    assert_same_but_the_seam(&live_after, attached_after, id, WIDTH, "prompt cleared");
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
    assert!(live.approval_active());
    live.approval_respond(true);
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
    assert!(!attached.approval_active(), "a viewer holds no queue");
    attached.approval_respond(true);
    attached.approval_toggle_notice();
    assert!(center.is_active(), "the prompt is still pending");
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
