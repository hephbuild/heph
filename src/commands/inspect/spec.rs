use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address
    pub addr: String,
}

struct SpecApp {
    engine: Arc<Engine>,
    addr: Addr,
}

#[async_trait]
impl App for SpecApp {
    type Output = ();

    fn label(&self) -> String {
        format!("Spec {}", self.addr.format())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let res = self
            .engine
            .clone()
            .get_spec(self.engine.new_state(), &self.addr)
            .await?;

        let json = serde_json::to_string_pretty(&*res).context("serialize spec")?;

        tui::paused!(ctx, {
            println!("{json}");
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
    let app = SpecApp { engine, addr };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
