use std::sync::Arc;

use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::Engine;
use crate::htaddr::Addr;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target the chain starts at (e.g. //cmd/server:bin)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub from: String,
    /// Target the chain must reach (e.g. //lib:core)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub to: String,
}

struct PathApp {
    engine: Arc<Engine>,
    from: Addr,
    to: Addr,
    fail_fast: bool,
}

#[async_trait]
impl App for PathApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(self.title())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(self.title())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let PathApp {
            engine,
            from,
            to,
            fail_fast,
        } = self;
        let rs = engine.new_state_with_events(fail_fast, ctx.event_sender());
        // Resolving defs may run provider targets, recording rich failures in
        // `rs`; `finalize` prefers those over the returned error.
        let res = Arc::clone(&engine).dep_path(rs.clone(), from, to).await;
        crate::commands::errors::finalize!(ctx, rs, res, chain => {
            // No chain at all means the two targets are unconnected: print
            // nothing, so callers can test the output for emptiness.
            for addr in chain.into_iter().flatten() {
                println!("{}", addr.format());
            }
            Ok(())
        })
    }
}

impl PathApp {
    fn title(&self) -> String {
        format!("Path {} → {}", self.from.format(), self.to.format())
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let from = super::revdeps::resolve_addr(args.from.as_ref())?;
    let to = super::revdeps::resolve_addr(args.to.as_ref())?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = PathApp {
        engine,
        from,
        to,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
