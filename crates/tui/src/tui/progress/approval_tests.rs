//! The approval view wired into [`TuiProgressView`]: the lockout, the
//! decide-and-move-on flow, which screens decide, and what its frames draw.
//! Timing runs on an injected clock: every call takes its `now_ms`.

use std::path::Path;

use ratatui::style::Color;
use ratatui::text::Line;
use tokio::sync::oneshot::error::TryRecvError;

use super::TuiProgressView;
use super::test_events::{T0, at, start};
use crate::tui::app::{Decide, TUIAppView};
use crate::tui::approval::{ApprovalCenter, ApprovalNoticeView, ApprovalTicket};
use crate::tui::dashboard::Tab;
use crate::tui::dashboard::approval::{FLASH_HINT, FLASH_MS, LOCKOUT_MS, test_store};
use crate::tui::session::ApprovalStanding;

const COLS: u16 = 120;
const ROWS: u16 = 24;

/// A live view on `center`, its glyph static so frames compare as text.
fn view(center: &ApprovalCenter) -> TuiProgressView {
    let mut v = TuiProgressView::new("heph r build //p:...").with_approval(center.clone());
    v.motion = false;
    v
}

fn texts(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(ToString::to_string).collect()
}

fn frame(v: &mut TuiProgressView, now: u64) -> Vec<String> {
    texts(&v.render_full(now, COLS, ROWS))
}

/// The decision keys' hint colour on the frame at `now`.
fn hint_color(v: &mut TuiProgressView, now: u64) -> Option<Color> {
    let lines = v.render_full(now, COLS, ROWS);
    lines
        .last()
        .and_then(|l| l.spans.get(1))
        .and_then(|s| s.style.fg)
}

fn pending(t: &mut ApprovalTicket) -> bool {
    matches!(t.rx.try_recv(), Err(TryRecvError::Empty))
}

/// A notice kept in the store under `home`.
fn notice(home: &Path, name: &str, content: &str) -> ApprovalNoticeView {
    test_store::kept(home, name, content)
}

/// A view reading its notices from the store under `home`.
fn with_store(v: TuiProgressView, home: &Path) -> TuiProgressView {
    v.with_notices(test_store::roots(home), test_store::read)
}

/// Frames at `now` until the notice reader has answered (it reads on a
/// thread of its own), then that frame.
fn settled(v: &mut TuiProgressView, now: u64, cols: u16, rows: u16) -> Vec<Line<'static>> {
    for _ in 0..400 {
        let f = v.render_full(now, cols, rows);
        if !texts(&f).iter().any(|r| r.contains("reading the notice")) {
            return f;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the notice reader never answered");
}

/// y, n, Y and N at 0 s and 0.4 s leave the request pending and are not
/// replayed at 0.5 s; at 0.5 s they act. The hint is greyed until then and
/// nothing else on screen changes. After y the view moves to the next
/// request, which starts its own lockout (a y 0.1 s later is dropped);
/// switching notice tabs does not restart it; Esc at 0 s goes back.
#[test]
fn approve_armed_after_delay() {
    for decide in [
        Decide::Approve,
        Decide::Reject,
        Decide::ApproveAll,
        Decide::RejectAll,
    ] {
        let center = ApprovalCenter::new();
        let build = center.build();
        let mut t = center.request(&build, "//p:a".into(), Vec::new());
        let mut v = view(&center);
        assert!(v.approval_open());
        let at0 = frame(&mut v, T0);
        assert_eq!(hint_color(&mut v, T0), Some(Color::DarkGray), "{at0:#?}");
        v.approval_decide(decide, T0);
        v.advance(T0 + 400);
        let at400 = frame(&mut v, T0 + 400);
        v.approval_decide(decide, T0 + 400);
        assert_eq!(
            hint_color(&mut v, T0 + LOCKOUT_MS - 1),
            Some(Color::DarkGray)
        );
        v.advance(T0 + LOCKOUT_MS);
        let at500 = frame(&mut v, T0 + LOCKOUT_MS);
        assert_eq!(hint_color(&mut v, T0 + LOCKOUT_MS), Some(Color::Yellow));
        assert_eq!(at0, at400, "{decide:?}: the lockout changes nothing");
        assert_eq!(
            at0, at500,
            "{decide:?}: arming changes only the hint's colour"
        );
        assert!(pending(&mut t), "{decide:?}: dropped, not replayed");
        assert!(center.standing().is_none(), "{decide:?}");

        v.approval_decide(decide, T0 + LOCKOUT_MS);
        let approve = matches!(decide, Decide::Approve | Decide::ApproveAll);
        assert_eq!(t.rx.try_recv(), Ok(approve), "{decide:?} at 0.5 s");
        let standing = match decide {
            Decide::ApproveAll => Some(ApprovalStanding::Approve),
            Decide::RejectAll => Some(ApprovalStanding::Reject),
            Decide::Approve | Decide::Reject => None,
        };
        assert_eq!(center.standing(), standing, "{decide:?}");
    }

    // The next request gets its own lockout; a notice tab keeps the clock.
    let home = tempfile::tempdir().expect("tempdir");
    let center = ApprovalCenter::new();
    let build = center.build();
    let mut a = center.request(&build, "//p:a".into(), Vec::new());
    let mut b = center.request(
        &build,
        "//p:b".into(),
        vec![
            notice(home.path(), "plan", "the plan"),
            notice(home.path(), "changelog", "the changes"),
        ],
    );
    let mut v = with_store(view(&center), home.path());
    assert!(v.approval_open());
    drop(frame(&mut v, T0));
    v.approval_decide(Decide::Approve, T0 + LOCKOUT_MS);
    assert_eq!(a.rx.try_recv(), Ok(true));
    // During the flash a key is dropped, and the hint row says so.
    let flashing = frame(&mut v, T0 + LOCKOUT_MS + 1).join("\n");
    assert!(
        flashing.contains(FLASH_HINT) && !flashing.contains("y approve"),
        "{flashing}"
    );
    let next = T0 + LOCKOUT_MS + FLASH_MS;
    v.advance(next);
    let f = frame(&mut v, next);
    assert!(f[2].contains("//p:b"), "moved on: {f:#?}");
    v.approval_decide(Decide::Approve, next + 100);
    assert!(pending(&mut b), "a second y 0.1 s later is dropped");
    v.tab(true);
    let f = frame(&mut v, next + 200);
    assert!(f[2].contains("//p:b"), "{f:#?}");
    v.approval_decide(Decide::Approve, next + LOCKOUT_MS - 1);
    assert!(pending(&mut b));
    v.approval_decide(Decide::Approve, next + LOCKOUT_MS);
    assert_eq!(
        b.rx.try_recv(),
        Ok(true),
        "the tab switch did not restart the clock"
    );
    v.advance(next + LOCKOUT_MS + FLASH_MS);
    assert!(!v.in_approval_view(), "none left: the view closes");

    // Esc decides nothing, so it works at once.
    let mut c = center.request(&build, "//p:c".into(), Vec::new());
    assert!(v.approval_open());
    drop(frame(&mut v, T0));
    v.back_to_main();
    assert!(!v.in_approval_view() && !v.full_screen());
    assert!(pending(&mut c), "Esc leaves it pending");
}

/// With a request pending, no decision key decides on the line, the
/// dashboard (its list and the Approvals tab) or a target screen: the
/// backend routes them only while the approval view is up, and the view
/// decides nothing without it. `n` there stays a log search's next match.
#[test]
fn decision_keys_only_in_approval_view() {
    use crate::tui::app::ScreenKey;
    use crate::tui::backend::interactive::{decision_key, screen_key};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    for (c, decide) in [
        ('y', Decide::Approve),
        ('n', Decide::Reject),
        ('Y', Decide::ApproveAll),
        ('N', Decide::RejectAll),
    ] {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(decision_key(&key, false), None, "{c} off the view");
        assert_eq!(decision_key(&key, true), Some(decide), "{c} in the view");
        let ctrl = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(decision_key(&ctrl, true), None, "ctrl-{c}");
    }
    assert_eq!(screen_key(KeyCode::Char('n')), Some(ScreenKey::Next));
    assert_eq!(screen_key(KeyCode::Char('N')), Some(ScreenKey::Prev));

    let center = ApprovalCenter::new();
    let mut t = center.request(&center.build(), "//p:gate".into(), Vec::new());
    let mut v = view(&center);
    v.apply(&at(0, start("//p:gate")));
    v.apply(&at(0, start("//p:other")));
    let mut screens = Vec::new();
    let now = T0 + 10_000;
    let press_all = |v: &mut TuiProgressView, now: u64| {
        for d in [
            Decide::Approve,
            Decide::Reject,
            Decide::ApproveAll,
            Decide::RejectAll,
        ] {
            v.approval_decide(d, now);
        }
    };

    // The line.
    drop(v.render("⠋", now, COLS, 2));
    screens.push(("line", v.in_approval_view()));
    press_all(&mut v, now + LOCKOUT_MS);
    // The dashboard's All list, its ✦ row selected.
    v.open_dashboard();
    let f = frame(&mut v, now);
    assert!(f.iter().any(|r| r.contains("awaits approval")), "{f:#?}");
    screens.push(("dashboard", v.in_approval_view()));
    press_all(&mut v, now + LOCKOUT_MS);
    // The Approvals tab.
    v.list.set_tab(Tab::Approvals);
    let f = frame(&mut v, now);
    assert!(f.iter().any(|r| r.contains("awaits approval")), "{f:#?}");
    screens.push(("approvals tab", v.in_approval_view()));
    press_all(&mut v, now + LOCKOUT_MS);
    // A target screen.
    v.list.set_tab(Tab::All);
    let other = v.state.dash.id("//p:other").expect("in the model");
    v.list.select(other);
    v.open_selected(false);
    let f = frame(&mut v, now);
    assert!(v.target.is_some(), "{f:#?}");
    screens.push(("target", v.in_approval_view()));
    press_all(&mut v, now + LOCKOUT_MS);

    for (screen, in_view) in screens {
        assert!(!in_view, "{screen} is not the approval view");
    }
    assert!(pending(&mut t), "still pending");
    assert!(center.standing().is_none());
}

/// The decide-and-move-on flow, in process: opened from the Approvals tab
/// on the second of three requests, y decides that one; the header shows
/// the decision for 1 s, then the oldest pending request; Y there decides
/// it and the third, and a request arriving later is approved without a
/// prompt; the view closes back to the dashboard, and the line and the
/// title row show the standing decision. N does the same for rejection.
#[test]
fn approval_view_decides_and_moves_on() {
    for (all, approve, indicator) in [
        (Decide::ApproveAll, true, "auto-approving (Y)"),
        (Decide::RejectAll, false, "auto-rejecting (N)"),
    ] {
        let center = ApprovalCenter::new();
        let build = center.build();
        let mut v = view(&center);
        let mut tickets: Vec<ApprovalTicket> = ["//p:a", "//p:b", "//p:c"]
            .into_iter()
            .map(|addr| {
                v.apply(&at(0, start(addr)));
                center.request(&build, addr.into(), Vec::new())
            })
            .collect();
        v.open_dashboard();
        v.list.set_tab(Tab::Approvals);
        let b = v.state.dash.id("//p:b").expect("in the model");
        drop(frame(&mut v, T0));
        v.list.select(b);
        v.open_selected(false);
        assert!(v.in_approval_view());
        let f = frame(&mut v, T0);
        assert!(
            f[2].contains("//p:b") && f[2].contains("2 of 3 pending"),
            "{f:#?}"
        );

        // y decides the request on screen, not the front of the queue.
        v.approval_decide(Decide::Approve, T0 + LOCKOUT_MS);
        assert_eq!(tickets[1].rx.try_recv(), Ok(true));
        assert!(pending(&mut tickets[0]) && pending(&mut tickets[2]));
        for now in [T0 + LOCKOUT_MS, T0 + LOCKOUT_MS + FLASH_MS - 1] {
            v.advance(now);
            let f = frame(&mut v, now);
            assert!(
                f[2].contains("✓ approved //p:b"),
                "the decision shows: {f:#?}"
            );
        }
        let next = T0 + LOCKOUT_MS + FLASH_MS;
        v.advance(next);
        let f = frame(&mut v, next);
        assert!(
            f[2].contains("//p:a") && f[2].contains("1 of 2 pending"),
            "{f:#?}"
        );

        // Y/N: this request, the one still queued, and every later one.
        v.approval_decide(all, next + LOCKOUT_MS);
        assert_eq!(tickets[0].rx.try_recv(), Ok(approve));
        assert_eq!(tickets[2].rx.try_recv(), Ok(approve));
        let mut later = center.request(&build, "//p:later".into(), Vec::new());
        assert_eq!(later.rx.try_recv(), Ok(approve), "decided as it arrives");
        assert!(center.queue().is_empty(), "never queued");
        let f = frame(&mut v, next + LOCKOUT_MS);
        let mark = if approve {
            "✓ approved //p:a"
        } else {
            "✗ rejected //p:a"
        };
        assert!(f[2].contains(mark), "{f:#?}");

        // Nothing left: back to the dashboard it was opened from.
        v.advance(next + LOCKOUT_MS + FLASH_MS);
        assert!(!v.in_approval_view() && v.full_screen() && v.target.is_none());
        let f = frame(&mut v, next + LOCKOUT_MS + FLASH_MS);
        assert!(f[0].contains(indicator), "the title row: {f:#?}");
        assert!(
            !f.iter().any(|r| r.contains("awaits approval")),
            "no ✦ row left: {f:#?}"
        );
        v.back_to_main();
        let line = texts(&v.render("⠋", T0 + 5_000, COLS, 1));
        assert_eq!(line.len(), 1, "no prompt row: {line:#?}");
        assert!(line[0].contains(indicator), "the line: {line:#?}");
    }
}

/// I13 for a notice: each row of the log-text adversarial table, drawn as
/// a notice, leaves no control character in any cell and no row wider
/// than the screen.
#[test]
fn control_bytes_never_reach_terminal_notice() {
    let long = "z".repeat(1 << 20);
    let cases: Vec<(&str, String, Option<&str>)> = vec![
        ("empty", String::new(), None),
        ("blank lines", "\n\n\n".into(), None),
        ("no newline", "last".into(), Some("last")),
        ("crlf", "one\r\ntwo\r\n".into(), Some("two")),
        ("cr progress", "10%\r20%\r30%\n".into(), Some("30%")),
        (
            "modes",
            "a\x1b[?1049lb\x1b[?1003hc\x1b[2Jd\n".into(),
            Some("abcd"),
        ),
        (
            "osc",
            "\x1b]8;;http://x\x07link\x1b]8;;\x07 \x1b]52;c;Zm9v\x07ok\n".into(),
            Some("link ok"),
        ),
        ("lone csi", "x\u{9b}y\n".into(), None),
        ("invalid utf8", "a\u{fffd}b\n".into(), Some("a\u{fffd}b")),
        ("nul", "a\0b\n".into(), Some("ab")),
        ("tabs", "a\tb\n".into(), Some("a       b")),
        ("long line", long, None),
        ("wide", "日本語 e\u{301}\n".into(), Some("日本語 e\u{301}")),
    ];
    let cols: u16 = 40;
    for (name, content, want) in cases {
        let home = tempfile::tempdir().expect("tempdir");
        let center = ApprovalCenter::new();
        let _t = center.request(
            &center.build(),
            "//p:gate".into(),
            vec![notice(home.path(), "plan", &content)],
        );
        let mut v = with_store(view(&center), home.path());
        assert!(v.approval_open());
        let lines = settled(&mut v, T0, cols, ROWS);
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.chars().any(char::is_control),
                    "{name}: {:?}",
                    span.content
                );
            }
            assert!(line.width() <= usize::from(cols), "{name}: {line:?}");
        }
        let drawn = texts(&lines).join("\n");
        assert!(!drawn.contains("notice unavailable"), "{name}: {drawn}");
        if let Some(want) = want {
            assert!(drawn.contains(want), "{name}: {want:?} in\n{drawn}");
        }
    }
}

/// I9 on the approval view: a notice naming heph's home shows it redacted,
/// a notice whose source is gone reads "notice unavailable", and neither the
/// view, the prompt row nor the line ever shows a notice's path.
#[test]
fn no_home_path_on_any_screen_approval_view() {
    let home = tempfile::tempdir().expect("tempdir");
    let home_str = home.path().display().to_string();
    let center = ApprovalCenter::new();
    let build = center.build();
    let text = format!("writes {home_str}/cache/out and nothing else");
    let gone = notice(home.path(), "changelog", "deleted below");
    std::fs::remove_file(test_store::file(home.path(), "changelog")).expect("remove a source");
    let _t = center.request(
        &build,
        "//p:gate".into(),
        vec![notice(home.path(), "plan", &text), gone],
    );
    let mut v = with_store(view(&center), home.path())
        .with_redact(crate::tui::text::Redact::heph_home(home.path()));
    let line = texts(&v.render("⠋", T0, COLS, 2)).join("\n");
    assert!(!line.contains(&home_str), "{line}");
    assert!(v.approval_open());
    let plan = texts(&settled(&mut v, T0, COLS, ROWS)).join("\n");
    assert!(!plan.contains(&home_str), "{plan}");
    assert!(
        plan.contains(&format!(
            "writes {}/cache/out",
            crate::tui::text::HOME_TOKEN
        )),
        "{plan}"
    );
    v.tab(true);
    let changelog = texts(&settled(&mut v, T0, COLS, ROWS)).join("\n");
    assert!(changelog.contains("notice unavailable"), "{changelog}");
    assert!(!changelog.contains(&home_str), "{changelog}");
}
