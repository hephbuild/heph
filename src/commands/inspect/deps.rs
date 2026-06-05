use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address
    pub addr: String,
    /// Explore deps interactively
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,
}

struct DepsApp {
    engine: Arc<Engine>,
    addr: Addr,
    fail_fast: bool,
}

#[async_trait]
impl App for DepsApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Deps {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Deps {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        // `get_def` may run provider targets, recording rich failures in `rs`;
        // `finalize` prefers those over the returned error and prints on success.
        let res = self.engine.clone().get_def(rs.clone(), &self.addr).await;
        crate::commands::errors::finalize!(ctx, rs, res, def => {
            for input in &def.target_def.inputs {
                println!("{}", input.r#ref);
            }
            Ok(())
        })
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let addr =
        htaddr::parse_addr(args.addr.as_ref()).with_context(|| format!("parse {}", args.addr))?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let interactive = tui::should_use_tui(global.no_tui);

    if args.interactive {
        if !interactive {
            anyhow::bail!("--interactive requires a terminal (stderr is not a TTY)");
        }
        return super::deps_explorer::run(engine, addr, sink, shutdown).await;
    }

    let app = DepsApp {
        engine,
        addr,
        fail_fast: global.fail_fast,
    };
    tui::run_app(app, sink, interactive, shutdown).await
}
