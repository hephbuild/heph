//! The paved road for a command that is not *about* building, but may build.
//!
//! `heph auth status` probes a credential under its runner, and the runner is a
//! target — a devenv shell can take minutes to build the first time. A command
//! that does that without a progress view looks hung. Every command that can
//! execute a target therefore runs under [`tui::run_app`], like `run` does: the
//! interactive TUI on a terminal, the line backend everywhere else (CI, agents,
//! a piped stderr).
//!
//! The command's own output — a table, `--json` — still belongs on stdout, and
//! must never land inside a live frame. Print it with [`tui::paused!`] once it is
//! computed, or through a [`tui::BufferedStdout`] when it is incremental.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::tui::{self, App, AppContext, LogSink};

/// An [`App`] whose whole body is a closure, with the stock progress views
/// labelled `label`. For commands that have no bespoke UI.
pub(crate) struct ProgressApp<F> {
    label: String,
    body: F,
}

#[async_trait]
impl<F, Fut> App for ProgressApp<F>
where
    F: FnOnce(AppContext) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::new(self.label.clone())
    }

    fn ci_view(&self) -> Self::CiView {
        tui::CiProgressView::new(self.label.clone())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        (self.body)(ctx).await
    }
}

/// Run `body` under the TUI (on a terminal, unless `no_tui`) or the line
/// backend, and return what it returns — the exit code is the body's.
pub(crate) async fn run_with_progress<F, Fut>(
    label: impl Into<String>,
    sink: LogSink,
    no_tui: bool,
    shutdown: hcore::shutdown::ShutdownTrigger,
    body: F,
) -> anyhow::Result<()>
where
    F: FnOnce(AppContext) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let app = ProgressApp {
        label: label.into(),
        body,
    };
    tui::run_app(app, sink, tui::should_use_tui(no_tui), shutdown).await
}

/// A request whose build events reach `ctx`'s view, and whose sandbox cleanups
/// the renderer waits on before it tears down.
///
/// Fail-fast, as [`Engine::new_state`] is: these commands decide keep-going for
/// their discovery walk at the call site (`Discovery::keep_going_unless`), and
/// moving them onto the TUI is not a reason to change what a failure does.
pub(crate) fn request_state(engine: &Arc<Engine>, ctx: &AppContext) -> Arc<RequestState> {
    engine.new_state_full(
        true,
        ctx.event_sender(),
        ctx.bg_pending(),
        Engine::DEFAULT_LOG_TAIL_LINES,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body's result is the command's result, on the line backend an agent
    /// gets: an error must still come back out (a non-zero exit), and a success
    /// must not be turned into one.
    #[tokio::test]
    async fn the_body_decides_the_exit() {
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        let err = run_with_progress("t", LogSink::new_direct(), true, shutdown, |_ctx| async {
            anyhow::bail!("2 of 3 credentials are not available here")
        })
        .await
        .expect_err("a failing body fails the command");
        assert!(err.to_string().contains("2 of 3"), "{err:#}");

        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        run_with_progress(
            "t",
            LogSink::new_direct(),
            true,
            shutdown,
            |ctx| async move {
                assert!(!ctx.interactive(), "no_tui means the line backend");
                Ok(())
            },
        )
        .await
        .expect("a passing body passes");
    }
}
