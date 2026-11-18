//! `heph session attach`: the session's own TUI (or line output), fed from its
//! recording instead of an engine.
//!
//! The backends run unchanged, and the TUI is the run's own
//! [`tui::TuiProgressView`] in its attached role: the same view, render
//! functions and layout, with only what acts on the run left out (see
//! `Role` in `progress.rs`). The app's `run` routes each record through
//! [`SessionFeed::route`]: a build event goes to `ctx.event_sender()`, a log
//! line or failure report is printed through the sink, and the run's label,
//! approval prompt and finish instant go into the feed the view reads. What
//! the run printed below its final summary is printed below this viewer's,
//! once its TUI is torn down. `q` and Ctrl-C detach — they end this viewer,
//! never the session — and the viewer records no session of its own.

use crate::tui::session::SessionSlot;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use hcore::shutdown::ShutdownTrigger;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::show::new_view;
use super::{SessionExit, ViewOpts, escape_replayed, require_known, resolve};
use crate::tui::session::{
    HeaderKind, HolderNames, Routed, SessionFeed, SessionId, SessionItem, SessionMeta,
    SessionSource,
};
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
    session: &Arc<SessionSlot>,
    no_tui: bool,
) -> anyhow::Result<()> {
    let (i, entry) = resolve(sources, arg, opts).await?;
    let meta = require_known(&entry)?.clone();
    let source = *sources
        .get(i)
        .ok_or_else(|| anyhow::anyhow!("the session's source went missing"))?;
    let stream = source.subscribe(entry.id).await?;

    // Ctrl-C, from the TUI's key handler or a SIGINT, detaches.
    let (shutdown, detach_rx) = ShutdownTrigger::new();
    let sigint_task = match trigger_on(SignalKind::interrupt(), shutdown.clone()) {
        Ok(task) => Some(task),
        Err(e) => {
            tracing::warn!(error = %e, "listening for ctrl-c; it will kill heph instead of detaching");
            None
        }
    };
    let (quit_tx, quit_rx) = mpsc::unbounded_channel();
    let after = AfterTeardown::default();
    let app = AttachApp {
        id: entry.id,
        meta,
        holders: source.holder_names(entry.id),
        feed: SessionFeed::default(),
        after: after.clone(),
        stream,
        detach_rx,
        quit_tx,
        quit_rx,
    };
    let outcome = tui::run_app(
        app,
        sink.clone(),
        session,
        tui::should_use_tui(no_tui),
        shutdown,
    )
    .await;
    if let Some(task) = sigint_task {
        task.abort();
    }
    // The run printed these below its final summary; the viewer's own summary
    // was printed at its teardown, so they land in the same place.
    sink.write_diagnostic(&after.take());
    let outcome = outcome?;
    if let Some(note) = exit_note(entry.id, outcome) {
        sink.write_diagnostic(&note);
    }
    outcome_result(entry.id, outcome)
}

/// Pulls `trigger` on every `kind` signal, from a spawned task.
///
/// The listener is armed here, on the calling thread, before the task is
/// spawned: `tokio::signal::ctrl_c()` arms nothing until it is first polled,
/// so a ctrl-c that landed before a worker polled the task kept SIGINT's
/// default action and killed heph instead of detaching. The kind is a
/// parameter only so the test can prove this with a signal nobody else in
/// the test binary listens for.
pub(crate) fn trigger_on(
    kind: SignalKind,
    trigger: ShutdownTrigger,
) -> std::io::Result<JoinHandle<()>> {
    let mut sig = signal(kind)?;
    Ok(tokio::spawn(async move {
        while sig.recv().await.is_some() {
            trigger.trigger();
        }
    }))
}

/// The stderr line saying why `attach` exits as it does, when the code alone
/// would mislead (U1): a mirrored 125 reads like this command's own failure,
/// and a code outside 0–255 is not an exit status, so 125 stands in for it.
/// An `End` with no code is an error of its own, rendered with its reason.
pub(crate) fn exit_note(id: SessionId, outcome: Outcome) -> Option<String> {
    let Outcome::Ended(Some(code)) = outcome else {
        return None;
    };
    match u8::try_from(code) {
        Ok(SessionExit::OWN_FAILURE) => Some(format!("heph: session {id} exited 125\n")),
        Ok(_) => None,
        Err(_) => Some(format!(
            "heph: session {id} exited {code}, not an exit status (0-255); exiting 125\n"
        )),
    }
}

/// The command's result for how the follow ended (A14).
pub(crate) fn outcome_result(id: SessionId, outcome: Outcome) -> anyhow::Result<()> {
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

/// Text the run printed after its TUI was torn down, held until the viewer's
/// own TUI is: printed above a live viewport it would land above the summary.
#[derive(Debug, Clone, Default)]
struct AfterTeardown(Arc<Mutex<String>>);

impl AfterTeardown {
    fn push(&self, text: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_str(text);
    }

    fn take(&self) -> String {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

struct AttachApp {
    id: SessionId,
    meta: SessionMeta,
    /// How this viewer names a lock holder's pid; the run's view named it
    /// from its own sessions directory the same way.
    holders: HolderNames,
    /// What the run's terminal showed besides build events, read by the view.
    feed: SessionFeed,
    after: AfterTeardown,
    stream: BoxStream<'static, anyhow::Result<SessionItem>>,
    detach_rx: mpsc::UnboundedReceiver<()>,
    quit_tx: mpsc::UnboundedSender<()>,
    quit_rx: mpsc::UnboundedReceiver<()>,
}

/// The line form: only selects which stock line view a session gets, and
/// renders nothing itself. A session recorded without a TUI already logged
/// every line a line view would print, so its log records are the whole
/// output; one that had a TUI gets the line view of its kind.
pub(crate) enum LineView {
    Quiet,
    Progress(Box<tui::CiProgressView>),
    Gc(Box<tui::GcCiView>),
}

impl LineView {
    fn as_view(&mut self) -> Option<&mut dyn CIAppView> {
        match self {
            Self::Quiet => None,
            Self::Progress(v) => Some(v.as_mut()),
            Self::Gc(v) => Some(v.as_mut()),
        }
    }

    fn as_view_ref(&self) -> Option<&dyn CIAppView> {
        match self {
            Self::Quiet => None,
            Self::Progress(v) => Some(v.as_ref()),
            Self::Gc(v) => Some(v.as_ref()),
        }
    }
}

impl CIAppView for LineView {
    fn begin(&self) {
        if let Some(v) = self.as_view_ref() {
            v.begin();
        }
    }

    fn apply(&mut self, ev: &hcore::events::BuildEvent) {
        if let Some(v) = self.as_view() {
            v.apply(ev);
        }
    }

    fn finish(&self) {
        if let Some(v) = self.as_view_ref() {
            v.finish();
        }
    }

    fn name_lock_holders(&mut self, names: HolderNames) {
        if let Some(v) = self.as_view() {
            v.name_lock_holders(names);
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
        new_view(&self.meta).attached(self.id, self.feed.clone(), move || {
            // The app may already be gone; then there is nothing to detach.
            let _gone = quit.send(());
        })
    }

    fn ci_view(&self) -> Self::CiView {
        if !self.meta.interactive {
            LineView::Quiet
        } else {
            match self.meta.header {
                HeaderKind::Gc => {
                    LineView::Gc(Box::new(tui::GcCiView::new(self.meta.label.clone())))
                }
                HeaderKind::Build | HeaderKind::Unknown => {
                    LineView::Progress(Box::new(tui::CiProgressView::new(self.meta.label.clone())))
                }
            }
        }
    }

    fn records_session(&self) -> bool {
        false
    }

    /// The backend hands these to the view and resolves through them, as it
    /// does a run's own.
    fn lock_holders(&self) -> Option<HolderNames> {
        Some(self.holders.clone())
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
                SessionItem::Record(record) => match self.feed.route(record) {
                    Routed::Event(ev) => {
                        if let Some(tx) = &events {
                            // The renderer is gone only when the backend is
                            // tearing down; the next select ends the loop.
                            drop(tx.send(ev));
                        }
                    }
                    // The run's own colours, on a viewer that takes colour.
                    Routed::Print(text) => ctx
                        .sink()
                        .write_diagnostic(&escape_replayed(&text, ctx.sink().color_enabled())),
                    Routed::AfterTeardown(text) => self
                        .after
                        .push(&escape_replayed(&text, ctx.sink().color_enabled())),
                    Routed::Nothing => {}
                },
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
