use crate::tui::session::SessionSlot;
use std::sync::Arc;

use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use futures::{Stream, StreamExt, TryStreamExt};

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::run::QUERY_LANG_HELP;
use crate::commands::utils::resolve_matcher;
use crate::engine::request_state::RequestState;
use crate::engine::{Discovery, Engine, Gaps, get_cwp};
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(
    override_usage = "heph query <TARGET_ADDRESS>\n       heph query <LABEL> <PACKAGE_MATCHER>\n       heph query -e <EXPR>",
    after_long_help = QUERY_LANG_HELP
)]
pub struct Args {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL", add = ArgValueCompleter::new(complete_target_addr))]
    pub arg1: Option<String>,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Select targets with a query expression, e.g. -e '//pkg/... && !//vendor/...'.
    /// Supports &&, ||, !, parentheses, and the label()/driver()/tree_output() functions.
    /// Mutually exclusive with the positional TARGET arguments.
    #[arg(
        short = 'e',
        long = "expr",
        value_name = "EXPR",
        conflicts_with = "arg1"
    )]
    pub expr: Option<String>,
    /// Print the candidates as providers list them, without resolving each one.
    /// Faster, but the output may include targets that do not exist.
    #[arg(long)]
    pub candidates: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

struct QueryApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
    candidates: bool,
}

/// The addrs `query` prints for `m`, in the walk's order: by default only
/// targets that exist, each match resolved to confirm it; with `candidates`,
/// the listing-level candidates unconfirmed. A candidate the walk rules out is
/// never resolved in either mode.
pub fn select<'a>(
    engine: Arc<Engine>,
    rs: Arc<RequestState>,
    m: &'a Matcher,
    discovery: Discovery,
    candidates: bool,
) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
    if candidates {
        engine.query(rs, m, discovery).left_stream()
    } else {
        engine.query_existing(rs, m, discovery).right_stream()
    }
}

#[async_trait]
impl App for QueryApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!(
            "Querying {}",
            crate::htquery::format(&self.matcher)
        ))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!(
            "Querying {}",
            crate::htquery::format(&self.matcher)
        ))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        // The renderer's background counter, not a private one: the providers'
        // `scope_ended` hook for this request counts on it, and the renderer
        // drains it before the process exits — a private counter would let
        // `heph query` exit with the hook still in flight.
        let rs = self.engine.new_state_full(
            self.fail_fast,
            ctx.event_sender(),
            ctx.bg_pending(),
            Engine::DEFAULT_LOG_TAIL_LINES,
            None,
        );
        // What matched goes to stdout as it is found; what the walk could not
        // resolve is reported on stderr at the end, and fails the command.
        let gaps = Gaps::new(crate::htquery::format(&self.matcher));
        let stream = select(
            Arc::clone(&self.engine),
            rs.clone(),
            &self.matcher,
            Discovery::keep_going_unless(self.fail_fast, &gaps),
            self.candidates,
        );
        tokio::pin!(stream);

        // Output is incremental, so addrs are printed before `finalize` runs; a
        // provider running a target mid-stream records rich failures in `rs`, which
        // `finalize` renders after the addrs already flushed. A whole-graph (`//...`)
        // query records the total graph size for telemetry inside `Engine::query`.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            while let Some(addr) = stream.try_next().await? {
                out.println(addr.format());
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res, gaps = &gaps)
    }
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let m = resolve_matcher(&args.expr, &args.arg1, &args.arg2, &cwp, true)?;
    let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
    let app = QueryApp {
        engine,
        matcher: m,
        fail_fast: global.fail_fast,
        candidates: args.candidates,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app_with(
        app,
        sink,
        session,
        interactive,
        args.dashboard.dashboard,
        shutdown,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::fault_provider::{FaultProvider, Faults};
    use crate::htpkg::PkgBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// `heph query` does not return while its request's `scope_ended` hook is
    /// in flight: the hook is counted on the renderer's background counter
    /// (`ctx.bg_pending()`), which the line backend drains before `run_app`
    /// returns. The provider's hook is held open until it has started and a
    /// further 500ms have passed; a query that counted the hook anywhere else
    /// returns inside that window, before the gate opens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_drains_the_scope_hook_before_returning() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = FaultProvider::new(
            vec![crate::pluginstatictarget::Target {
                addr: "//q:t".to_string(),
                driver: "exec".to_string(),
                run: Some("true".to_string()),
                ..Default::default()
            }],
            Faults {
                scope_ended_gate: Some(Arc::clone(&gate)),
                ..Default::default()
            },
        )?;
        let log = provider.scopes();
        let mut engine = Engine::new(Config::for_tests(root.path()))?;
        engine.register_provider(move |_| Box::new(provider))?;

        let opened = Arc::new(AtomicBool::new(false));
        let opener = {
            let opened = Arc::clone(&opened);
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                while log.ended().is_empty() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                opened.store(true, Ordering::SeqCst);
                gate.add_permits(1);
            })
        };

        let app = QueryApp {
            engine: Arc::new(engine),
            matcher: Matcher::PackagePrefix(PkgBuf::from("")),
            fail_fast: false,
            candidates: true,
        };
        let session = Arc::new(SessionSlot::new());
        let sink = LogSink::new_direct_with(Arc::clone(&session));
        let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
        tui::run_app(app, sink, &session, false, shutdown).await?;
        assert!(
            opened.load(Ordering::SeqCst),
            "heph query returned with its scope_ended hook still in flight"
        );
        opener.await?;
        Ok(())
    }
}
