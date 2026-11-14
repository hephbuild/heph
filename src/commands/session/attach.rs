//! `heph session attach`: the session's own TUI (or line output), fed from its
//! recording instead of an engine.
//!
//! The backends run unchanged: the app's `run` replays each record's build
//! event into `ctx.event_sender()` and prints its log lines through the sink,
//! then follows until `End`. `q` and Ctrl-C detach — they end this viewer, never
//! the session — and the viewer records no session of its own.

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use tokio::sync::mpsc;

use super::show::new_view;
use super::{SessionExit, ViewOpts, escape_text, require_known, resolve};
use crate::tui::session::{SessionItem, SessionMeta, SessionRecordKind, SessionSource};
use crate::tui::{self, App, AppContext, CIAppView, LogSink};

/// How the follow ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `q` or Ctrl-C: the session goes on.
    Detached,
    /// The session wrote its `End`.
    Ended(Option<i32>),
    /// The process went away without writing `End`.
    Lost,
}

pub(crate) async fn attach(
    sources: &[&dyn SessionSource],
    opts: ViewOpts,
    arg: &str,
    sink: LogSink,
    no_tui: bool,
) -> anyhow::Result<()> {
    let (i, entry) = resolve(sources, arg, opts).await?;
    let meta = require_known(&entry)?.clone();
    let source = *sources
        .get(i)
        .ok_or_else(|| anyhow::anyhow!("the session's source went missing"))?;
    let stream = source.subscribe(entry.id).await?;

    // Ctrl-C, from the TUI's key handler or a SIGINT, detaches.
    let (shutdown, detach_rx) = hcore::shutdown::ShutdownTrigger::new();
    let sigint = shutdown.clone();
    let sigint_task = tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            sigint.trigger();
        }
    });
    let (quit_tx, quit_rx) = mpsc::unbounded_channel();
    let app = AttachApp {
        meta,
        stream,
        detach_rx,
        quit_tx,
        quit_rx,
    };
    let outcome = tui::run_app(app, sink, tui::should_use_tui(no_tui), shutdown).await;
    sigint_task.abort();
    outcome_result(entry.id, outcome?)
}

/// The command's result for how the follow ended (A14).
pub(crate) fn outcome_result(
    id: crate::tui::session::SessionId,
    outcome: Outcome,
) -> anyhow::Result<()> {
    match outcome {
        Outcome::Detached | Outcome::Ended(Some(0)) => Ok(()),
        Outcome::Ended(Some(code)) => Err(SessionExit::mirrored(code).into()),
        Outcome::Ended(None) => Err(SessionExit::own(anyhow::anyhow!(
            "session {id} ended without an exit code (killed by a signal, or it panicked)"
        ))
        .into()),
        Outcome::Lost => Err(SessionExit::own(anyhow::anyhow!(
            "session {id} was lost: its process went away without recording its end"
        ))
        .into()),
    }
}

struct AttachApp {
    meta: SessionMeta,
    stream: BoxStream<'static, anyhow::Result<SessionItem>>,
    detach_rx: mpsc::UnboundedReceiver<()>,
    quit_tx: mpsc::UnboundedSender<()>,
    quit_rx: mpsc::UnboundedReceiver<()>,
}

/// The line form: a session recorded without a TUI already logged every line a
/// line view would print, so its log records are the whole output; one that
/// had a TUI gets the stock line view.
pub(crate) enum LineView {
    Quiet,
    Progress(Box<tui::CiProgressView>),
}

impl CIAppView for LineView {
    fn begin(&self) {
        if let Self::Progress(v) = self {
            v.begin();
        }
    }

    fn apply(&mut self, ev: &hcore::events::BuildEvent) {
        if let Self::Progress(v) = self {
            v.apply(ev);
        }
    }

    fn finish(&self) {
        if let Self::Progress(v) = self {
            v.finish();
        }
    }
}

#[async_trait]
impl App for AttachApp {
    type Output = Outcome;
    type TuiView = tui::TuiProgressView;
    type CiView = LineView;

    fn tui_view(&self) -> Self::TuiView {
        let quit = self.quit_tx.clone();
        new_view(&self.meta).with_quit(move || {
            // The app may already be gone; then there is nothing to detach.
            let _gone = quit.send(());
        })
    }

    fn ci_view(&self) -> Self::CiView {
        if self.meta.interactive {
            LineView::Progress(Box::new(tui::CiProgressView::new(self.meta.label.clone())))
        } else {
            LineView::Quiet
        }
    }

    fn records_session(&self) -> bool {
        false
    }

    async fn run(mut self, ctx: AppContext) -> anyhow::Result<Outcome> {
        let events = ctx.event_sender();
        loop {
            let item = tokio::select! {
                item = self.stream.next() => item,
                _ = self.detach_rx.recv() => return Ok(Outcome::Detached),
                _ = self.quit_rx.recv() => return Ok(Outcome::Detached),
            };
            // A follower that stopped without saying how: nothing more will
            // come, and nothing says the session ended cleanly.
            let Some(item) = item else {
                return Ok(Outcome::Lost);
            };
            match item? {
                SessionItem::Record(record) => {
                    if let (Some(tx), Some(ev)) = (&events, record.build_event()) {
                        // The renderer is gone only when the backend is
                        // tearing down; the next select ends the loop.
                        drop(tx.send(ev));
                    }
                    if let Some(SessionRecordKind::Log { msg, .. }) = &record.session {
                        let mut line = escape_text(msg).into_owned();
                        if !line.ends_with('\n') {
                            line.push('\n');
                        }
                        ctx.sink().write_diagnostic(&line);
                    }
                }
                SessionItem::CaughtUp { skipped } => {
                    if skipped > 0 {
                        tracing::warn!(skipped, "some recorded lines could not be read");
                    }
                }
                SessionItem::Ended { lost: true, .. } => return Ok(Outcome::Lost),
                SessionItem::Ended { exit, lost: false } => return Ok(Outcome::Ended(exit)),
            }
        }
    }
}
