use tokio::sync::mpsc;

use crate::tui::app::{App, AppContext, CIAppView};
use crate::tui::log_sink::LogSink;
use hcore::events::EventReceiver;
use hcore::shutdown::ShutdownTrigger;

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
                    Some(ev) => view.apply(&ev),
                    // Sender dropped — stop polling, keep awaiting the app.
                    None => events = None,
                }
            }
        }
    };

    // Drain any events buffered before the sender dropped so the final
    // summary is accurate even if the app future completed first.
    if let Some(r) = events.as_mut() {
        while let Ok(ev) = r.try_recv() {
            view.apply(&ev);
        }
    }

    view.finish();

    // Block return until background work has drained: sandbox cleanups (the
    // process must not exit out from under the cleaner thread mid-rmdir) and
    // remote-cache uploads (a cold run's whole point is to populate the cache, so
    // we never abandon a push that is still making progress — each one carries its
    // own deadline).
    //
    // Report while waiting. There is no TUI here, so a silent poll makes a long
    // drain indistinguishable from a hang: the run prints its summary and then the
    // process just sits there. Saying what is outstanding, and that the count is
    // going down, is the difference between "uploading 400 revisions" and "heph is
    // wedged".
    let started = std::time::Instant::now();
    let mut next_report = started + DRAIN_REPORT_EVERY;
    let mut announced = false;
    while bg_pending.load(std::sync::atomic::Ordering::Acquire) > 0 {
        tokio::time::sleep(DRAIN_POLL).await;
        if std::time::Instant::now() >= next_report {
            next_report += DRAIN_REPORT_EVERY;
            tracing::info!(
                pending = bg_pending.load(std::sync::atomic::Ordering::Acquire),
                elapsed_secs = started.elapsed().as_secs(),
                "waiting for background cache uploads, history trims and sandbox cleanup to finish",
            );
            announced = true;
        }
    }
    if announced {
        tracing::info!(
            elapsed_secs = started.elapsed().as_secs(),
            "background work drained",
        );
    }

    result
}

/// Poll interval for the background-work drain. Short because most runs drain
/// almost immediately — cleanups are brief rmdirs.
const DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// How often the drain reports what it is still waiting on. Long enough that a
/// normal run says nothing at all, short enough that a slow drain never looks like
/// a hang.
const DRAIN_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
