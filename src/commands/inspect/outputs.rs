use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::{Engine, OutputMatcher, ResultOptions};
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Emit JSON instead of one path per line
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct OutputsView {
    addr: String,
    paths: Vec<String>,
    support_paths: Vec<String>,
}

struct OutputsApp {
    engine: Arc<Engine>,
    addr: Addr,
    json: bool,
    fail_fast: bool,
}

#[async_trait]
impl App for OutputsApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Outputs {}", self.addr.format()))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Outputs {}", self.addr.format()))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        // `OutputMatcher::All` rather than `None`: the paths live inside the
        // artifacts, and `None` resolves hashouts without handing any back.
        let res = self
            .engine
            .clone()
            .result_addr(
                rs.clone(),
                &self.addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await;
        let addr = self.addr.format();
        let json = self.json;
        crate::commands::errors::finalize!(ctx, rs, res, result => {
            // `entry_paths` is header-only for tar-backed cache artifacts, so
            // listing a target's outputs does not read their bytes.
            let collect = |arts: &[Arc<dyn hcore::hartifactcontent::Content>]| -> anyhow::Result<Vec<String>> {
                let mut out = Vec::new();
                for art in arts {
                    for p in art.entry_paths().context("enumerate artifact paths")? {
                        out.push(p.to_string_lossy().into_owned());
                    }
                }
                out.sort();
                out.dedup();
                Ok(out)
            };
            let paths = collect(&result.artifacts)?;
            let support_paths = collect(&result.support_artifacts)?;

            if json {
                let view = OutputsView { addr, paths, support_paths };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).context("serialize outputs")?
                );
            } else {
                for p in &paths {
                    println!("{p}");
                }
            }
            Ok(())
        })
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let base = crate::engine::get_cwp()?;
    let addr = htaddr::parse_addr_with_base(args.addr.as_ref(), &base)
        .with_context(|| format!("parse {}", args.addr))?;
    let (engine, shutdown) = bootstrap::new_engine(&global)?;
    let app = OutputsApp {
        engine,
        addr,
        json: args.json,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
