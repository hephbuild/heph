use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use super::log_sink::LogSink;

pub(crate) enum Control {
    Pause(oneshot::Sender<()>),
    Resume,
}

pub struct AppContext {
    sink: LogSink,
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl AppContext {
    pub(crate) fn direct(sink: LogSink) -> Self {
        Self {
            sink,
            control: None,
        }
    }

    pub(crate) fn with_control(sink: LogSink, control: mpsc::UnboundedSender<Control>) -> Self {
        Self {
            sink,
            control: Some(control),
        }
    }

    pub fn sink(&self) -> &LogSink {
        &self.sink
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

#[async_trait(?Send)]
pub trait App {
    type Output;

    /// Shown next to the spinner in interactive mode; logged once in CI mode.
    fn label(&self) -> String;

    /// Run the business logic. May call `ctx.pause()` to suspend the TUI.
    async fn run(self, ctx: AppContext) -> anyhow::Result<Self::Output>;
}
