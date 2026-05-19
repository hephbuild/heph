use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;

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
}

struct QueryApp {
    engine: Arc<Engine>,
    matcher: Matcher,
}

#[async_trait(?Send)]
impl App for QueryApp {
    type Output = ();

    fn label(&self) -> String {
        format!("Querying {:?}", self.matcher)
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self.engine.new_state();
        let stream = self.engine.query(rs, &self.matcher);
        tokio::pin!(stream);

        let out = BufferedStdout::new(&ctx);
        while let Some(addr) = stream.try_next().await? {
            out.println(addr.format());
        }
        out.close().await;

        Ok(())
    }
}

pub fn execute(args: &Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, no_tui)
}

#[tokio::main]
async fn execute_async(args: Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let cwp = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &cwp, true)?;
    let engine = bootstrap::new_engine()?;
    let app = QueryApp { engine, matcher: m };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive).await
}
