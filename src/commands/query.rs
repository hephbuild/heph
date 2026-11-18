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
