use async_trait::async_trait;
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot};

use super::log_sink::LogSink;
use crate::engine::event::{BuildEvent, EventSender};

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
}

impl AppContext {
    pub(crate) fn direct(sink: LogSink, events: Option<EventSender>) -> Self {
        Self {
            sink,
            control: None,
            events,
        }
    }

    pub(crate) fn with_control(
        sink: LogSink,
        control: mpsc::UnboundedSender<Control>,
        events: Option<EventSender>,
    ) -> Self {
        Self {
            sink,
            control: Some(control),
            events,
        }
    }

    pub fn sink(&self) -> &LogSink {
        &self.sink
    }

    /// The build-event sender to plumb into the request state, e.g.
    /// `engine.new_state_with_events(fail_fast, ctx.event_sender())`.
    pub fn event_sender(&self) -> Option<EventSender> {
        self.events.clone()
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

    /// Number of pinned inline viewport rows to reserve.
    fn rows(&self) -> u16;

    /// The lines for the pinned viewport, including the spinner row built from
    /// `spinner`. Called every render tick.
    fn render(&self, spinner: &str, now_ms: u64) -> Vec<Line<'static>>;

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
