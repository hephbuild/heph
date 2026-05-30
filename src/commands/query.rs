use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::{Engine, get_cwp};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct Args {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Exclude target address (repeatable, e.g. -e //pkg:target)
    #[arg(short = 'e', long = "exclude", value_name = "TARGET_ADDRESS")]
    pub exclude: Vec<String>,
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
        crate::tui::TuiProgressView::new(format!("Querying {:?}", self.matcher))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Querying {:?}", self.matcher))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        let stream = self.engine.query(rs.clone(), &self.matcher);
        tokio::pin!(stream);

        // Output is incremental, so addrs are printed before `finalize` runs; a
        // provider running a target mid-stream records rich failures in `rs`, which
        // `finalize` renders after the addrs already flushed.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            while let Some(addr) = stream.try_next().await? {
                out.println(addr.format());
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, global.clone())
}

#[tokio::main]
async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &args.exclude, &cwp, true)?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = QueryApp {
        engine,
        matcher: m,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
