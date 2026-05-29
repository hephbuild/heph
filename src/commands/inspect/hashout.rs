use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::bootstrap;
use crate::engine::{Engine, OutputMatcher, ResultOptions};
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address
    pub addr: String,
}

struct HashoutApp {
    engine: Arc<Engine>,
    addr: Addr,
}

#[async_trait]
impl App for HashoutApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Hashout {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Hashout {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self.engine.new_state_with_events(true, ctx.event_sender());
        let res = self
            .engine
            .clone()
            .result_addr(
                rs,
                &self.addr,
                OutputMatcher::None,
                &ResultOptions::default(),
            )
            .await?;

        tui::paused!(ctx, {
            for art in &res.artifacts_meta {
                println!("{}", art.hashout);
            }
        });

        Ok(())
    }
}

pub fn execute(args: &Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, no_tui)
}

#[tokio::main]
async fn execute_async(args: Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let addr =
        htaddr::parse_addr(args.addr.as_ref()).with_context(|| format!("parse {}", args.addr))?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = HashoutApp { engine, addr };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
