//! Two ctrl-c to stop the build from a full screen.
//!
//! On the dashboard and the target screens the first ctrl-c only arms a stop
//! for [`WINDOW_MS`]; a second inside that window stops the build as one
//! press does elsewhere. Any other key disarms it. Everything else keeps one
//! press (E6): the line, a finished build, an attached viewer, and every
//! press once a stop is under way, so the hard abort still takes today's
//! number of presses. A real SIGINT never reaches this: it goes straight to
//! the shutdown trigger.
//!
//! The rule lives here, once: [`StopArm::press`] decides, the backend acts.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How long the first press keeps the stop armed.
pub const WINDOW_MS: u64 = 3_000;

/// The hint row while armed starts with this.
pub const ARMED_PROMPT: &str = "ctrl-c again to stop the build";

/// What a ctrl-c does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlC {
    /// Nothing reaches the build: the stop is armed, and the hint row asks
    /// for a second press.
    Arm,
    /// One press as it has always been: stop the build (detach, for a
    /// viewer), and leave a finished one.
    Stop,
}

/// Where a ctrl-c lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct Press {
    /// A full screen (the dashboard, a target screen) is up.
    pub full_screen: bool,
    /// The view is a viewer of another process's session.
    pub attached: bool,
    /// The build has finished; only the held screen is left.
    pub finished: bool,
    /// A stop is already under way, from a key or a SIGINT.
    pub stopping: bool,
}

/// The armed window, and whether this guard already let a stop through.
#[derive(Debug, Default)]
pub struct StopArm {
    armed_at: Option<u64>,
    stopped: bool,
}

impl StopArm {
    /// A ctrl-c at `now_ms`, landing on `at`.
    pub fn press(&mut self, at: Press, now_ms: u64) -> CtrlC {
        let armed = self.armed_left(now_ms).is_some();
        self.armed_at = None;
        let one_press = !at.full_screen || at.attached || at.finished;
        if one_press || at.stopping || self.stopped || armed {
            // A finished build has nothing to stop: the press leaves, which
            // is the backend's half of `Stop`. Not a stop under way either.
            self.stopped |= !at.finished;
            return CtrlC::Stop;
        }
        self.armed_at = Some(now_ms);
        CtrlC::Arm
    }

    /// Any other key: the stop is off, and the build never noticed.
    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    /// While armed, how long is left of the window.
    pub fn armed_left(&self, now_ms: u64) -> Option<u64> {
        let at = self.armed_at?;
        WINDOW_MS
            .checked_sub(now_ms.saturating_sub(at))
            .filter(|&left| left > 0)
    }

    /// The hint row while armed, `None` otherwise.
    pub fn prompt(&self, now_ms: u64) -> Option<String> {
        let left = self.armed_left(now_ms)?;
        Some(format!(
            "{ARMED_PROMPT} · {}.{}s · any other key keeps it running",
            left / 1000,
            left % 1000 / 100
        ))
    }
}

/// Whether `ev` is a ctrl-c key press.
pub fn is_ctrl_c(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
    )
}

/// Whether `ev` disarms an armed stop: any key press but ctrl-c. A mouse
/// event, a resize, a focus change or a paste does not; nobody meant those
/// as an answer.
pub fn disarms(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(KeyEvent {
            kind: KeyEventKind::Press,
            ..
        })
    ) && !is_ctrl_c(ev)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{MouseEvent, MouseEventKind};

    use super::*;

    const DASH: Press = Press {
        full_screen: true,
        attached: false,
        finished: false,
        stopping: false,
    };

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    /// After the confirming press every further ctrl-c goes to the shutdown
    /// without arming, so the hard abort keeps its press count; a build that
    /// finishes inside the window leaves the arming moot; a mouse event or a
    /// resize does not disarm, any key does; and the approval view, opened
    /// from the line, arms like the other full screens.
    #[test]
    fn ctrl_c_after_stop_goes_direct() {
        // The approval view is a full screen: the first press arms, the
        // second stops; closed, the line stops at once.
        {
            use crate::tui::app::TUIAppView;
            let center = crate::tui::approval::ApprovalCenter::new();
            let _ticket = center.request(&center.build(), "//a:gate".into(), Vec::new());
            let mut v = crate::tui::TuiProgressView::new("run").with_approval(center);
            assert!(v.approval_open());
            assert!(v.full_screen() && !v.is_on_main_view());
            assert_eq!(v.ctrl_c(0, false, false), CtrlC::Arm, "the view arms");
            assert!(v.in_approval_view(), "arming leaves the view up");
            assert_eq!(v.ctrl_c(1_000, false, false), CtrlC::Stop);
            let center = crate::tui::approval::ApprovalCenter::new();
            let _ticket = center.request(&center.build(), "//a:gate".into(), Vec::new());
            let mut v = crate::tui::TuiProgressView::new("run").with_approval(center);
            assert!(v.approval_open());
            v.back_to_main();
            assert_eq!(
                v.ctrl_c(0, false, false),
                CtrlC::Stop,
                "the line: one press"
            );
        }

        let mut s = StopArm::default();
        assert_eq!(s.press(DASH, 0), CtrlC::Arm);
        assert_eq!(s.press(DASH, 2_999), CtrlC::Stop, "inside the window");
        // A second confirming press would arm again if the guard forgot.
        assert_eq!(s.press(DASH, 3_100), CtrlC::Stop);
        assert_eq!(s.press(DASH, 60_000), CtrlC::Stop);
        assert_eq!(s.prompt(60_000), None);

        // A stop under way from elsewhere (a SIGINT) is direct too.
        let mut s = StopArm::default();
        let stopping = Press {
            stopping: true,
            ..DASH
        };
        assert_eq!(s.press(stopping, 0), CtrlC::Stop);
        assert_eq!(s.press(DASH, 10), CtrlC::Stop, "and stays so");

        // The window closes after 3 s: the next press arms again.
        let mut s = StopArm::default();
        assert_eq!(s.press(DASH, 0), CtrlC::Arm);
        assert!(s.prompt(2_400).is_some_and(|p| p.contains(" · 0.6s · ")));
        assert_eq!(s.prompt(3_000), None);
        assert_eq!(s.press(DASH, 3_000), CtrlC::Arm);

        // Finished inside the window: one press leaves, and is no stop.
        let mut s = StopArm::default();
        assert_eq!(s.press(DASH, 0), CtrlC::Arm);
        let finished = Press {
            finished: true,
            ..DASH
        };
        assert_eq!(s.press(finished, 500), CtrlC::Stop);
        assert!(!s.stopped, "leaving a finished build stops nothing");

        // Mouse and resize keep it armed; a key disarms.
        let mouse = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!disarms(&mouse));
        assert!(!disarms(&Event::Resize(80, 24)));
        assert!(!disarms(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(disarms(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(disarms(&key(KeyCode::Esc, KeyModifiers::NONE)));
        let mut s = StopArm::default();
        assert_eq!(s.press(DASH, 0), CtrlC::Arm);
        s.disarm();
        assert_eq!(s.press(DASH, 100), CtrlC::Arm, "disarmed: arms again");
    }

    /// The line, a viewer and a finished build keep one press.
    #[test]
    fn one_press_where_e6_keeps_it() {
        for at in [
            Press::default(),
            Press {
                attached: true,
                ..DASH
            },
            Press {
                finished: true,
                ..DASH
            },
        ] {
            let mut s = StopArm::default();
            assert_eq!(s.press(at, 0), CtrlC::Stop, "{at:?}");
        }
    }
}
