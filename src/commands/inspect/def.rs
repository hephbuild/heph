use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use serde::Serialize;

use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::driver::targetdef::TargetDef;
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address
    pub addr: String,
}

#[derive(Serialize)]
struct DefView<'a> {
    target_def: &'a TargetDef,
    applied_transitive: &'a Sandbox,
}

struct DefApp {
    engine: Arc<Engine>,
    addr: Addr,
}

#[async_trait(?Send)]
impl App for DefApp {
    type Output = ();

    fn label(&self) -> String {
        format!("Def {}", self.addr.format())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let res = self
            .engine
            .clone()
            .get_def(self.engine.new_state(), &self.addr)
            .await?;

        let view = DefView {
            target_def: &res.target_def,
            applied_transitive: &res.applied_transitive,
        };
        let json = serde_json::to_string_pretty(&view).context("serialize def")?;

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
    let engine = bootstrap::new_engine()?;
    let app = DefApp { engine, addr };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive).await
}
