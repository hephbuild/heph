use std::sync::Arc;

use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::run::QUERY_LANG_HELP;
use crate::commands::utils::resolve_matcher;
use crate::engine::{Discovery, Engine, Gaps, get_cwp};
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
}

struct QueryApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
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
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        // What matched goes to stdout as it is found; what the walk could not
        // resolve is reported on stderr at the end, and fails the command.
        let gaps = Gaps::new(crate::htquery::format(&self.matcher));
        let stream = self.engine.query(
            rs.clone(),
            &self.matcher,
            Discovery::keep_going_unless(self.fail_fast, &gaps),
        );
        tokio::pin!(stream);

        // Output is incremental, so addrs are printed before `finalize` runs; a
        // provider running a target mid-stream records rich failures in `rs`, which
        // `finalize` renders after the addrs already flushed. A whole-graph (`//...`)
        // query records the total graph size for telemetry inside `Engine::query`.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            let mut matched = false;
            while let Some(addr) = stream.try_next().await? {
                matched = true;
                out.println(addr.format());
            }
            if !matched
                && let Some(hint) = crate::commands::errors::listed_facts_hint(rs.listed_decided())
            {
                tracing::warn!("{hint}");
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res, gaps = &gaps)
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let m = resolve_matcher(&args.expr, &args.arg1, &args.arg2, &cwp, true)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = QueryApp {
        engine,
        matcher: m,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
