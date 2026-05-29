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
    /// Skip applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
}

#[derive(Serialize)]
struct DefView<'a> {
    target_def: &'a TargetDef,
    applied_transitive: Option<&'a Sandbox>,
}

struct DefApp {
    engine: Arc<Engine>,
    addr: Addr,
    no_transitive: bool,
}

#[async_trait]
impl App for DefApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Def {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Def {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self.engine.new_state_with_events(true, ctx.event_sender());
        let res = if self.no_transitive {
            self.engine.clone().get_direct_def(rs, &self.addr).await?
        } else {
            self.engine.clone().get_def(rs, &self.addr).await?
        };

        let view = DefView {
            target_def: &res.target_def,
            applied_transitive: res.applied_transitive.as_ref(),
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
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = DefApp {
        engine,
        addr,
        no_transitive: args.no_transitive,
    };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
