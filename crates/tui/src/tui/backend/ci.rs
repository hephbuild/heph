use tokio::sync::mpsc;

use crate::tui::app::{App, AppContext, CIAppView};
use crate::tui::log_sink::LogSink;
use hcore::events::EventReceiver;
use hcore::shutdown::ShutdownTrigger;

pub async fn run<A: App + 'static>(
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
    // App runs on its own task, as on the interactive backend. Sharing one task
    // with the loop below means every synchronous stretch inside the app — a
    // filesystem walk, Starlark eval, a cache-warm `result_addr` descent — stalls
    // event folding and the blocking `stderr` writes the view does, until the app
    // happens to yield. Spawning lets the fold loop be re-polled on another worker.
    //
    // This also moves the app off the 8 MiB main-thread stack `bootstrap::block_on`
    // runs on and onto a 2 MiB tokio worker stack. That matters: `heph run` on a
    // single address awaits `Engine::result_addr` directly on this future
    // (`commands/run.rs`), and a cache-warm descent resolves the whole dependency
    // subtree inside one `poll`, ~100 KiB of stack per level.
    //
    // What makes the descent safe is `GrowStack`: `Engine::result_addr` returns one,
    // and it is the only way into `result_addr_impl`, so every level of the result
    // spine — the outermost included — polls under `stacker::maybe_grow` and takes a
    // fresh segment when headroom runs low. The spine therefore cannot overflow.
    // Recursion nested *inside* one level (`plugingo::import_closure`,
    // `expand_inputs`, `collect_transitive_deps`) is not wrapped and is bounded only
    // by `RED_ZONE`, exactly as it already is on the batch path — `Engine::result`
    // spawns each target — and on the interactive backend. See `engine::grow_stack`.
    //
    // `Engine::drop` (FUSE unmount, SQLite flush, sandbox rmdir) now runs on the
    // worker that polled the app to completion rather than on the main thread; it
    // blocks on plain OS threads, never on the runtime, so it parks a worker during
    // teardown but cannot deadlock. Same as the interactive backend already does.
    let mut app_handle = tokio::spawn(app.run(ctx));

    let result = loop {
        tokio::select! {
            // Events keep being folded until the app task resolves; whatever the
            // app queued before finishing is picked up by the drain below.
            //
            // Events emitted *after* that — a `bg_pending`-tracked remote-cache
            // upload keeps a sender clone and emits from its own task
            // (`remote_cache.rs`) — are not folded and do not reach the summary.
            // Pre-existing and deliberate here: `view.finish()` prints once, before
            // the background drain. The interactive backend keeps its event arm live
            // across that window and does fold them, so the two backends differ.
            // Changing it means changing what a CI run prints, which is a separate
            // call from this one.
            out = &mut app_handle => break match out {
                Ok(inner) => inner,
                // Preserve the pre-spawn behaviour: an app panic unwound straight
                // out of `run`. Re-raise it here rather than turning it into an
                // ordinary error, so the panic hook and exit code are unchanged.
                Err(join_err) if join_err.is_panic() => {
                    std::panic::resume_unwind(join_err.into_panic())
                }
                Err(join_err) => {
                    Err(anyhow::Error::new(join_err).context("joining the app task"))
                }
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::TUIAppView;
    use async_trait::async_trait;
    use futures::FutureExt;
    use hcore::events::{BuildEvent, BuildEventKind, now_unix_ms};
    use ratatui::text::Line;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// How long `Behavior::BlockUntilFolded` spins before giving up. Only paid on
    /// a regression — the fold lands in microseconds when the app has its own task.
    const FOLD_DEADLINE: Duration = Duration::from_secs(5);

    /// How long `Behavior::LeavesBackgroundWork` keeps its ticket outstanding.
    /// Comfortably above the backend's 10 ms `DRAIN_POLL` so "did `run` wait?" is
    /// not a scheduler coin flip.
    const BG_WORK_DELAY: Duration = Duration::from_millis(150);

    fn result_start(addr: &str) -> BuildEvent {
        BuildEvent {
            at_unix_ms: now_unix_ms(),
            kind: BuildEventKind::ResultStart {
                addr: addr.to_string(),
            },
        }
    }

    /// The CI backend never builds a TUI view; this exists only to satisfy the
    /// associated type.
    struct NoTuiView;

    impl TUIAppView for NoTuiView {
        fn apply(&mut self, _ev: &BuildEvent) {}
        fn rows(&self, _term_height: u16) -> u16 {
            0
        }
        fn render(&self, _spinner: &str, _now_ms: u64, _w: u16, _h: u16) -> Vec<Line<'static>> {
            Vec::new()
        }
    }

    /// Records what the backend folded, in the order it folded it. The counters
    /// are shared with the app so it can observe folding progress while running.
    #[derive(Clone, Default)]
    struct RecordingView {
        addrs: Arc<Mutex<Vec<String>>>,
        folded: Arc<AtomicUsize>,
        begun: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }

    impl CIAppView for RecordingView {
        fn begin(&self) {
            self.begun.fetch_add(1, Ordering::SeqCst);
        }

        fn apply(&mut self, ev: &BuildEvent) {
            if let BuildEventKind::ResultStart { addr } = &ev.kind {
                self.addrs
                    .lock()
                    .expect("recording view poisoned")
                    .push(addr.clone());
            }
            self.folded.fetch_add(1, Ordering::SeqCst);
        }

        fn finish(&self) {
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Flips its flag when dropped. Used to prove the app's owned state — in the
    /// real thing the last `Arc<Engine>`, whose `Drop` unmounts FUSE and flushes
    /// SQLite — is released before `run` returns. `bootstrap::block_on` documents
    /// that as a precondition; before this change it was structural (the future
    /// lived in `run`'s frame), now it rests on tokio dropping a task's future
    /// before waking its `JoinHandle`.
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    enum Behavior {
        /// Emit `n` events back to back, then finish.
        Emit(usize),
        /// Emit `n` events, then fail. The failing-build path in CI mode.
        EmitThenFail(usize),
        /// Emit one event, then hold the thread synchronously until the backend
        /// has folded it. Resolves to whether the fold was observed.
        BlockUntilFolded,
        /// Register one unit of background work and hand it to a detached task
        /// that clears it after a delay — a sandbox cleanup or a cache upload
        /// outliving the app.
        LeavesBackgroundWork,
        /// Unwind out of the app task.
        Panic,
    }

    struct TestApp {
        view: RecordingView,
        behavior: Behavior,
        /// Dropped with the app's future, i.e. when the app task completes.
        probe: Option<DropProbe>,
    }

    impl TestApp {
        fn new(view: RecordingView, behavior: Behavior) -> Self {
            Self {
                view,
                behavior,
                probe: None,
            }
        }

        fn with_probe(mut self, probe: DropProbe) -> Self {
            self.probe = Some(probe);
            self
        }
    }

    #[async_trait]
    impl App for TestApp {
        type Output = bool;
        type TuiView = NoTuiView;
        type CiView = RecordingView;

        fn tui_view(&self) -> NoTuiView {
            NoTuiView
        }

        fn ci_view(&self) -> RecordingView {
            self.view.clone()
        }

        async fn run(self, ctx: AppContext) -> anyhow::Result<bool> {
            let tx = ctx
                .event_sender()
                .expect("the CI backend must plumb an event sender through AppContext");
            match self.behavior {
                Behavior::Emit(n) => {
                    for i in 0..n {
                        tx.send(result_start(&format!("//p:t{i}")))
                            .expect("backend must still hold the receiver");
                    }
                    Ok(true)
                }
                Behavior::BlockUntilFolded => {
                    tx.send(result_start("//p:probe"))
                        .expect("backend must still hold the receiver");
                    // Synchronous on purpose: this is what a filesystem walk,
                    // Starlark eval, or a cache-warm `result_addr` descent looks
                    // like from the backend's side — a stretch of the app's poll
                    // with no await point in it.
                    let deadline = Instant::now() + FOLD_DEADLINE;
                    while Instant::now() < deadline {
                        if self.view.folded.load(Ordering::SeqCst) > 0 {
                            return Ok(true);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(false)
                }
                Behavior::EmitThenFail(n) => {
                    for i in 0..n {
                        tx.send(result_start(&format!("//p:t{i}")))
                            .expect("backend must still hold the receiver");
                    }
                    Err(anyhow::anyhow!("build failed"))
                }
                Behavior::LeavesBackgroundWork => {
                    let bg = ctx.bg_pending();
                    bg.fetch_add(1, Ordering::AcqRel);
                    tokio::spawn(async move {
                        tokio::time::sleep(BG_WORK_DELAY).await;
                        bg.fetch_sub(1, Ordering::AcqRel);
                    });
                    Ok(true)
                }
                Behavior::Panic => panic!("app blew up"),
            }
        }
    }

    /// The app must not share a task with the fold loop. While it holds its
    /// thread without yielding, the backend has to keep draining the event
    /// channel; sharing one `select!` task means nothing is folded until the app
    /// gives the task back, and this deadlocks against the app's own deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn folds_events_while_the_app_holds_its_thread() {
        let view = RecordingView::default();
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        let observed = run(
            TestApp::new(view.clone(), Behavior::BlockUntilFolded),
            LogSink::new_direct(),
            shutdown,
        )
        .await
        .expect("backend must return the app's output");

        assert!(
            observed,
            "the backend must fold events while the app is still running; \
             it saw none in {FOLD_DEADLINE:?}"
        );
        assert_eq!(
            view.addrs
                .lock()
                .expect("recording view poisoned")
                .as_slice(),
            ["//p:probe"]
        );
    }

    /// Racing the app against the fold loop must not drop or reorder events: what
    /// the loop misses before the app task resolves is picked up by the post-loop
    /// drain, and `begin`/`finish` still bracket the run exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn events_are_folded_in_order_and_none_are_lost() {
        const N: usize = 500;
        let view = RecordingView::default();
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        run(
            TestApp::new(view.clone(), Behavior::Emit(N)),
            LogSink::new_direct(),
            shutdown,
        )
        .await
        .expect("backend must return the app's output");

        let expected: Vec<String> = (0..N).map(|i| format!("//p:t{i}")).collect();
        assert_eq!(
            *view.addrs.lock().expect("recording view poisoned"),
            expected,
            "every event must be folded exactly once, in emission order"
        );
        assert_eq!(view.begun.load(Ordering::SeqCst), 1, "begin runs once");
        assert_eq!(view.finished.load(Ordering::SeqCst), 1, "finish runs once");
    }

    /// A panicking app used to unwind straight out of `run`. Now that it runs on
    /// its own task the panic arrives as a `JoinError`, and the backend must
    /// re-raise it rather than turn it into an ordinary `Err` — the panic hook and
    /// the process exit code both hang off the unwind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_panic_unwinds_out_of_the_backend() {
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        let outcome = std::panic::AssertUnwindSafe(run(
            TestApp::new(RecordingView::default(), Behavior::Panic),
            LogSink::new_direct(),
            shutdown,
        ))
        .catch_unwind()
        .await;

        let payload = outcome
            .err()
            .expect("an app panic must unwind out of the backend, not become an error");
        // The *original* payload has to survive: it is what the panic hook renders.
        // Re-panicking with a message of our own would satisfy `is_err` and lose it.
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .expect("panic payload must still be the app's");
        assert!(
            message.contains("app blew up"),
            "the app's own panic payload must be re-raised verbatim, got {message:?}"
        );
    }

    /// The failing-build path — by far the most common outcome in CI mode. The
    /// error has to propagate out of `run`, but only *after* the events emitted
    /// before the failure are folded and the summary is printed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn app_failure_still_folds_events_and_prints_the_summary() {
        let view = RecordingView::default();
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        let err = run(
            TestApp::new(view.clone(), Behavior::EmitThenFail(3)),
            LogSink::new_direct(),
            shutdown,
        )
        .await
        .expect_err("the app's error must reach the caller");

        assert!(err.to_string().contains("build failed"), "got {err:?}");
        assert_eq!(
            *view.addrs.lock().expect("recording view poisoned"),
            ["//p:t0", "//p:t1", "//p:t2"],
            "events emitted before the failure must still be folded"
        );
        assert_eq!(
            view.finished.load(Ordering::SeqCst),
            1,
            "the summary must be printed on a failed run, not skipped"
        );
    }

    /// `run` must not return while background work is outstanding: the process
    /// exiting here tears the sandbox-cleanup thread out mid-rmdir and abandons
    /// in-flight cache uploads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_waits_for_background_work_to_drain() {
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        let started = Instant::now();
        run(
            TestApp::new(RecordingView::default(), Behavior::LeavesBackgroundWork),
            LogSink::new_direct(),
            shutdown,
        )
        .await
        .expect("backend must return the app's output");

        assert!(
            started.elapsed() >= BG_WORK_DELAY,
            "run returned after {:?}, before the {BG_WORK_DELAY:?} of background work drained",
            started.elapsed()
        );
    }

    /// `bootstrap::block_on` relies on the app's state — in the real thing the last
    /// `Arc<Engine>`, whose `Drop` unmounts FUSE and flushes SQLite — being gone by
    /// the time `run` returns. Spawning made that a property of tokio (it drops a
    /// task's future before waking the `JoinHandle`) rather than of this function's
    /// frame, so freeze it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn app_state_is_dropped_before_run_returns() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (shutdown, _shutdown_rx) = ShutdownTrigger::new();
        run(
            TestApp::new(RecordingView::default(), Behavior::Emit(2))
                .with_probe(DropProbe(Arc::clone(&drops))),
            LogSink::new_direct(),
            shutdown,
        )
        .await
        .expect("backend must return the app's output");

        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the app's owned state must be dropped by the time run returns"
        );
    }
}
