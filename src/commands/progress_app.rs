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

use crate::tui::session::SessionSlot;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::{GlobalOptions, bootstrap};
use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::tui::{self, App, AppContext, LogSink};

/// `-D` / `--dashboard`, flattened into the args of every command that shows
/// per-target build progress (`run`, `query`, `validate`, `inspect …`, the
/// `tool` commands that resolve targets, `auth`). A command whose header has no
/// dashboard (`tool gc`, `tool clean`) does not take it: its dashboard would be
/// an empty list.
#[derive(clap::Args, Clone, Copy, Debug, Default)]
pub struct DashboardArgs {
    /// Open the dashboard from the start, and hold the finished build until q
    ///
    /// The command starts on the dashboard instead of the line. When it ends,
    /// whichever screen is up stays until you press q (Esc still steps back
    /// to the line, which holds too); the exit code is the command's. Without
    /// a terminal it prints one warning and runs with the plain output.
    #[arg(short = 'D', long = "dashboard")]
    pub dashboard: bool,
}

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
/// `dashboard` is the command's `-D` ([`DashboardArgs`]).
pub(crate) async fn run_with_progress<F, Fut>(
    label: impl Into<String>,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    no_tui: bool,
    dashboard: bool,
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
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app_with(app, sink, session, interactive, dashboard, shutdown).await
}

/// Build the engine `global` describes, then run `body` on it under
/// [`run_with_progress`]. `body` gets the engine and a request on it from
/// [`engine_request`]; the exit code is the body's.
///
/// `no_tui` is the command's, which is `global.no_tui` unless the command
/// turns the TUI off itself (`--json`). `dashboard` is its `-D`.
pub(crate) async fn run_engine_command<F, Fut>(
    label: impl Into<String>,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
    no_tui: bool,
    dashboard: bool,
    body: F,
) -> anyhow::Result<()>
where
    F: FnOnce(Arc<Engine>, Arc<RequestState>, AppContext) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let (engine, shutdown) = bootstrap::new_engine(global, session)?;
    let fail_fast = global.fail_fast;
    run_with_progress(
        label,
        sink,
        session,
        no_tui,
        dashboard,
        shutdown,
        move |ctx| {
            let rs = engine_request(&engine, fail_fast, &ctx);
            body(engine, rs, ctx)
        },
    )
    .await
}

/// The request a [`run_engine_command`] body runs on: its build events reach
/// `ctx`'s view, and it is fail-fast as the command line says (`fail_fast`),
/// unlike [`request_state`].
fn engine_request(engine: &Arc<Engine>, fail_fast: bool, ctx: &AppContext) -> Arc<RequestState> {
    engine.new_state_with_events(fail_fast, ctx.event_sender())
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
        let session = Arc::new(SessionSlot::new());
        let sink = || LogSink::new_direct_with(Arc::clone(&session));
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        let err = run_with_progress("t", sink(), &session, true, false, shutdown, |_ctx| async {
            anyhow::bail!("2 of 3 credentials are not available here")
        })
        .await
        .expect_err("a failing body fails the command");
        assert!(err.to_string().contains("2 of 3"), "{err:#}");

        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        run_with_progress(
            "t",
            sink(),
            &session,
            true,
            false,
            shutdown,
            |ctx| async move {
                assert!(!ctx.interactive(), "no_tui means the line backend");
                Ok(())
            },
        )
        .await
        .expect("a passing body passes");
    }

    /// An engine command's request is fail-fast exactly when the command line
    /// says so — not always, as [`request_state`]'s is — so `--fail-fast`
    /// (and its absence) reaches `inspect`, `query` and the rest unchanged.
    #[tokio::test]
    async fn engine_request_takes_fail_fast_from_the_command_line() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Arc::new(Engine::new(crate::engine::Config::for_tests(root.path()))?);
        for fail_fast in [false, true] {
            let session = Arc::new(SessionSlot::new());
            let sink = LogSink::new_direct_with(Arc::clone(&session));
            let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
            let engine = Arc::clone(&engine);
            run_with_progress(
                "t",
                sink,
                &session,
                true,
                false,
                shutdown,
                move |ctx| async move {
                    let rs = engine_request(&engine, fail_fast, &ctx);
                    assert_eq!(rs.fail_fast(), fail_fast);
                    Ok(())
                },
            )
            .await?;
        }
        Ok(())
    }

    /// The whole command line, as `main` parses it.
    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        _global: crate::commands::global::GlobalOptions,
        #[command(subcommand)]
        _cmd: crate::commands::Commands,
    }

    /// The leaf subcommand's `--dashboard`, or clap's refusal.
    fn dashboard_flag(argv: &[&str]) -> Result<bool, clap::Error> {
        use clap::CommandFactory as _;
        let mut m = TestCli::command().try_get_matches_from(argv)?;
        while let Some((_, sub)) = m.remove_subcommand() {
            m = sub;
        }
        Ok(m.get_flag("dashboard"))
    }

    /// Every command that shows per-target build progress takes `-D` and
    /// `--dashboard`, off unless given; a command whose header has no dashboard
    /// (`tool gc`, `tool clean`) refuses it, as clap refuses any unknown flag.
    #[test]
    fn dashboard_flag_on_every_progress_command() {
        let progress: &[&[&str]] = &[
            &["run", "//p:t"],
            &["query", "//..."],
            &["validate"],
            &["inspect", "packages"],
            &["inspect", "labels"],
            &["inspect", "hashin", "//p:t"],
            &["inspect", "hashout", "//p:t"],
            &["inspect", "outputs", "//p:t"],
            &["inspect", "spec", "//p:t"],
            &["inspect", "def", "//p:t"],
            &["inspect", "deps", "//p:t"],
            &["inspect", "revdeps", "//p:t"],
            &["inspect", "path", "//p:a", "//p:b"],
            &["inspect", "states"],
            &["auth", "status"],
            &["auth", "explain", "//p:c"],
            &["auth", "login"],
            &["tool", "build-fmt"],
            &["tool", "codegen", "gitignore"],
            &["tool", "codegen", "status"],
            &["tool", "codegen", "keep", "p/f.go"],
            &["tool", "codegen", "discard", "--all"],
            &["tool", "codegen", "why", "p/f.go"],
            &["tool", "codegen", "import"],
            &["tool", "scratch", "head", "//c:go"],
            &["tool", "scratch", "pull", "//c:go"],
        ];
        for cmd in progress {
            let argv = |flag: Option<&'static str>| -> Vec<&str> {
                std::iter::once("heph")
                    .chain(cmd.iter().copied())
                    .chain(flag)
                    .collect()
            };
            for flag in ["-D", "--dashboard"] {
                let got = dashboard_flag(&argv(Some(flag)));
                assert!(matches!(got, Ok(true)), "{cmd:?} {flag}: {got:?}");
            }
            let got = dashboard_flag(&argv(None));
            assert!(matches!(got, Ok(false)), "{cmd:?}: {got:?}");
        }

        for cmd in [
            &["tool", "gc"][..],
            &["tool", "clean"],
            &["tool", "scratch", "ls"],
        ] {
            let argv: Vec<&str> = std::iter::once("heph")
                .chain(cmd.iter().copied())
                .chain(["-D"])
                .collect();
            let err = dashboard_flag(&argv).expect_err("no dashboard, no -D");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{cmd:?}: {err}"
            );
        }
    }
}
