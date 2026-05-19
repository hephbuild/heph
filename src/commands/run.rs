use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Args;

use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::{Engine, OutputMatcher, ResultOptions, get_cwp};
use crate::htmatcher::Matcher;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(Args, Clone)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Force execution
    #[arg(long = "force")]
    pub force: bool,
    /// Print output artifacts to stdout
    #[arg(long = "cat-out")]
    pub cat_out: bool,
    /// Print output file list to stdout
    #[arg(long = "list-out")]
    pub list_out: bool,
}

struct RunApp {
    args: RunArgs,
    engine: Arc<Engine>,
    matcher: Matcher,
}

#[async_trait(?Send)]
impl App for RunApp {
    type Output = ();

    fn label(&self) -> String {
        match &self.matcher {
            Matcher::Addr(a) => format!("Running {}", a.format()),
            other => format!("Running {other:?}"),
        }
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let opts = ResultOptions {
            force: self.args.force,
        };
        let rs = self.engine.new_state();

        let result = match self.matcher {
            Matcher::Addr(addr) => {
                vec![
                    self.engine
                        .clone()
                        .result_addr(rs, &addr, OutputMatcher::All, &opts)
                        .await?,
                ]
            }
            m => self.engine.clone().result(rs, &m, &opts).await?,
        };

        let _guard = ctx.pause().await;
        if self.args.cat_out {
            for r in &result {
                for a in &r.artifacts {
                    for e in a.walk()? {
                        io::copy(&mut e?.data, &mut io::stdout())?;
                    }
                }
            }
        } else if self.args.list_out {
            for r in &result {
                for a in &r.artifacts {
                    for e in a.walk()? {
                        println!("{}", e?.path.display());
                    }
                }
            }
        } else {
            println!("{} matched", result.len());
        }

        Ok(())
    }
}

pub fn execute(args: &RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, no_tui)
}

#[tokio::main]
async fn execute_async(args: RunArgs, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &base_pkg, false)?;
    let engine = bootstrap::new_engine()?;
    let app = RunApp {
        args,
        engine,
        matcher: m,
    };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive).await
}
