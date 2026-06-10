use tokio::sync::mpsc;

use crate::commands::bootstrap::ShutdownTrigger;
use crate::engine::event::EventReceiver;
use crate::tui::app::{App, AppContext, CIAppView};
use crate::tui::log_sink::LogSink;

pub async fn run<A: App>(
    app: A,
    sink: LogSink,
    _shutdown: ShutdownTrigger,
) -> anyhow::Result<A::Output> {
    // The app owns its non-TUI rendering too: the backend only drives the
    // event stream and hands each event to the view.
    let mut view = app.ci_view();
    view.begin();

    // We own the build-event channel: sender to the app via AppContext, we keep
    // the receiver.
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut events: Option<EventReceiver> = Some(event_rx);
    // Shared with the app's request state so we can wait for fire-and-forget
    // sandbox cleanups to drain before returning (and tearing down the runtime /
    // exiting the process out from under the cleaner thread).
    let bg_pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ctx = AppContext::direct(sink, Some(event_tx), std::sync::Arc::clone(&bg_pending));
    let app_fut = app.run(ctx);
    tokio::pin!(app_fut);

    let result = loop {
        tokio::select! {
            // Bias toward draining events so the final summary reflects the
            // full stream, but the app future is what terminates the loop.
            out = &mut app_fut => break out,
            maybe_evt = async {
                match events.as_mut() {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match maybe_evt {
                    // Greedily drain the rest of the buffered backlog in this
                    // same wake so an emitted burst folds in one pass instead of
                    // one event per `select!` iteration.
                    Some(ev) => {
                        view.apply(&ev);
                        if let Some(r) = events.as_mut() {
                            super::fold_buffered(r, &mut view, |v, e| v.apply(e));
                        }
                    }
                    // Sender dropped — stop polling, keep awaiting the app.
                    None => events = None,
                }
            }
        }
    };

    // Drain any events buffered before the sender dropped so the final
    // summary is accurate even if the app future completed first.
    if let Some(r) = events.as_mut() {
        super::fold_buffered(r, &mut view, |v, ev| v.apply(ev));
    }

    view.finish();

    // Block return until background sandbox cleanups have drained. No TUI to
    // keep alive here, but the process must not exit out from under the cleaner
    // thread mid-rmdir. Poll cheaply — cleanups are short rmdirs.
    while bg_pending.load(std::sync::atomic::Ordering::Acquire) > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    result
}
