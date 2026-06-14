use std::sync::Arc;

use async_trait::async_trait;
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot};

use super::log_sink::LogSink;
use heph_core::events::{BuildEvent, EventSender};
type PendingCounter = std::sync::Arc<std::sync::atomic::AtomicUsize>;

pub(crate) enum Control {
    Pause(oneshot::Sender<()>),
    Resume,
}

pub struct AppContext {
    sink: LogSink,
    control: Option<mpsc::UnboundedSender<Control>>,
    /// Build-event sender, plumbed into the request state the app builds so the
    /// engine's progress events reach the renderer's view. The backend owns the
    /// receiving end. `None` only in contexts with no renderer.
    events: Option<EventSender>,
    /// Background-work (sandbox cleanup) counter shared with the request state
    /// the app builds. The backend owns the other clone and polls it on exit so
    /// the renderer stays up until fire-and-forget cleanups have drained.
    bg_pending: PendingCounter,
}

impl AppContext {
    pub(crate) fn direct(
        sink: LogSink,
        events: Option<EventSender>,
        bg_pending: PendingCounter,
    ) -> Self {
        Self {
            sink,
            control: None,
            events,
            bg_pending,
        }
    }

    pub(crate) fn with_control(
        sink: LogSink,
        control: mpsc::UnboundedSender<Control>,
        events: Option<EventSender>,
        bg_pending: PendingCounter,
    ) -> Self {
        Self {
            sink,
            control: Some(control),
            events,
            bg_pending,
        }
    }

    pub fn sink(&self) -> &LogSink {
        &self.sink
    }

    /// The build-event sender to plumb into the request state, e.g.
    /// `engine.new_state_full(fail_fast, ctx.event_sender(), ctx.bg_pending())`.
    pub fn event_sender(&self) -> Option<EventSender> {
        self.events.clone()
    }

    /// The background-work counter to plumb into the request state alongside
    /// `event_sender`, so the engine's sandbox cleanups register against the
    /// counter the renderer watches on exit.
    pub fn bg_pending(&self) -> PendingCounter {
        Arc::clone(&self.bg_pending)
    }

    pub fn interactive(&self) -> bool {
        self.control.is_some()
    }

    /// Suspend the TUI for the lifetime of the returned guard. The renderer
    /// drains pending log lines, restores the terminal, and switches the
    /// log sink to direct passthrough. On guard drop the renderer resumes.
    /// In CI mode this is a no-op.
    pub async fn pause(&self) -> PauseGuard<'_> {
        pause_inner(&self.control).await;
        PauseGuard { ctx: self }
    }

    /// Standalone pause handle, cloneable and `'static`. Suitable for
    /// background tasks that need to pause/resume the TUI without
    /// borrowing the `AppContext`.
    pub fn pauser(&self) -> Pauser {
        Pauser {
            control: self.control.clone(),
        }
    }
}

pub struct PauseGuard<'a> {
    ctx: &'a AppContext,
}

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        resume_inner(&self.ctx.control);
    }
}

#[derive(Clone)]
pub struct Pauser {
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Pauser {
    pub async fn pause(&self) -> OwnedPauseGuard {
        pause_inner(&self.control).await;
        OwnedPauseGuard {
            control: self.control.clone(),
        }
    }
}

pub struct OwnedPauseGuard {
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Drop for OwnedPauseGuard {
    fn drop(&mut self) {
        resume_inner(&self.control);
    }
}

async fn pause_inner(control: &Option<mpsc::UnboundedSender<Control>>) {
    if let Some(tx) = control {
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx.send(Control::Pause(ack_tx)).is_ok() {
            drop(ack_rx.await);
        }
    }
}

fn resume_inner(control: &Option<mpsc::UnboundedSender<Control>>) {
    if let Some(tx) = control {
        drop(tx.send(Control::Resume));
    }
}

/// Aggregates the engine's build-event stream and renders progress in the
/// interactive TUI. The renderer drives the terminal lifecycle and feeds events
/// in; *what* gets drawn is entirely the view's call.
/// [`crate::tui::TuiProgressView`] is the paved road; implement this directly
/// for bespoke UI.
pub trait TUIAppView: Send {
    /// Fold one build event into the view's state. The repaint happens later on
    /// the render tick, so this must not draw.
    fn apply(&mut self, ev: &BuildEvent);

    /// Number of pinned inline viewport rows to reserve, given the full terminal
    /// height. The paved-road view targets one third of the terminal.
    fn rows(&self, term_height: u16) -> u16;

    /// Scroll the body by `delta` rows (negative = up, positive = down). Called
    /// from the backend's key handler; the repaint happens on the next render
    /// tick. No-op by default for views without a scrollable body.
    fn scroll(&mut self, _delta: i32) {}

    /// Pan the body horizontally by `delta` columns (negative = left, positive =
    /// right) for content wider than the viewport. No-op by default.
    fn hscroll(&mut self, _delta: i32) {}

    /// Cycle the body view (the `Tab` / `Shift+Tab` keys). `forward` advances to
    /// the next view, otherwise the previous. Called from the backend's key
    /// handler; the repaint happens on the next render tick. No-op by default for
    /// views with a single view.
    fn tab(&mut self, _forward: bool) {}

    /// Toggle the header counters between the matched-target set and every
    /// observed target (the `a` key). Called from the backend's key handler; the
    /// repaint happens on the next render tick. No-op by default for views without
    /// a scope toggle.
    fn toggle_scope(&mut self) {}

    /// Whether the view is currently capturing keystrokes into a search query
    /// (the `/` filter). While `true` the backend routes printable keys, Backspace,
    /// Enter and Esc to the search methods below instead of the normal shortcuts.
    /// Default `false` for views without search.
    fn is_searching(&self) -> bool {
        false
    }

    /// Enter search-input mode (the `/` key). No-op by default.
    fn search_start(&mut self) {}

    /// Append a typed character to the active search query. No-op by default.
    fn search_input(&mut self, _c: char) {}

    /// Delete the last character of the active search query. No-op by default.
    fn search_backspace(&mut self) {}

    /// Cancel search: leave input mode and clear the filter (the `Esc` key).
    /// No-op by default.
    fn search_cancel(&mut self) {}

    /// Confirm search: leave input mode but keep the filter applied so the body
    /// can be scrolled (the `Enter` key). No-op by default.
    fn search_confirm(&mut self) {}

    /// The lines for the pinned viewport, including the spinner row built from
    /// `spinner`. Called every render tick. `width` is the current terminal
    /// column count and `height` the reserved viewport row count, so the view can
    /// size borders, scroll long labels, and grow its body to fill the rows.
    fn render(&self, spinner: &str, now_ms: u64, width: u16, height: u16) -> Vec<Line<'static>>;

    /// Whether the viewport should stay open after the run finishes and wait for
    /// an explicit quit (`q` / Ctrl-C) instead of auto-exiting — e.g. the user
    /// navigated away from the main view and would lose what they are looking at
    /// on auto-tear-down. Default `false` (auto-exit).
    fn hold_after_finish(&self) -> bool {
        false
    }

    /// Mark the run as finished so the view can surface a "done — press q to
    /// quit" notice. Called once when the viewport is being held open. No-op by
    /// default.
    fn set_finished(&mut self) {}

    /// After the live viewport is torn down, render a final build summary
    /// directly to the terminal (stderr) so it persists in scrollback. NOT
    /// routed through the log sink. No-op by default.
    fn last_render(&self) {}
}

/// Aggregates the engine's build-event stream and renders progress in non-TUI
/// (CI / piped) mode, through the log sink. [`crate::tui::CiProgressView`] is
/// the paved road; implement this directly for bespoke output.
pub trait CIAppView: Send {
    /// Called once before the run starts.
    fn begin(&self) {}

    /// Fold the event and optionally print a concise line for it.
    fn apply(&mut self, ev: &BuildEvent);

    /// Print the final summary once the event stream ends.
    fn finish(&self) {}
}

#[async_trait]
pub trait App: Send {
    type Output: Send;

    /// The interactive (TUI) view for this command's progress.
    type TuiView: TUIAppView;
    /// The non-TUI (CI / piped) view for this command's progress.
    type CiView: CIAppView;

    /// Build the interactive view. Use [`crate::tui::TuiProgressView`] for the
    /// paved road.
    fn tui_view(&self) -> Self::TuiView;

    /// Build the non-TUI view. Use [`crate::tui::CiProgressView`] for the paved
    /// road.
    fn ci_view(&self) -> Self::CiView;

    /// Run the business logic. May call `ctx.pause()` to suspend the TUI.
    async fn run(self, ctx: AppContext) -> anyhow::Result<Self::Output>;
}
