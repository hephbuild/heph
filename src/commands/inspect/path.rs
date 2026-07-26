use std::sync::Arc;

use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::utils::resolve_addr;
use crate::engine::Engine;
use crate::htaddr::Addr;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// One end of the chain (e.g. //cmd/server:bin)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub a: String,
    /// The other end of the chain (e.g. //lib:core) — order does not matter
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub b: String,
    /// Follow only directly declared deps, without applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
}

struct PathApp {
    engine: Arc<Engine>,
    a: Addr,
    b: Addr,
    no_transitive: bool,
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
            a,
            b,
            no_transitive,
            fail_fast,
        } = self;
        let rs = engine.new_state_with_events(fail_fast, ctx.event_sender());
        // Kept for the "no path" log line — the addrs themselves move into the walk.
        let (a_label, b_label) = (a.format(), b.format());
        // Resolving defs may run provider targets, recording rich failures in
        // `rs`; `finalize` prefers those over the returned error.
        let res = Arc::clone(&engine)
            .dep_path_between(rs.clone(), a, b, no_transitive)
            .await;
        crate::commands::errors::finalize!(ctx, rs, res, chain => {
            match chain {
                // The chain reads dependent → dependency whichever way the
                // arguments were given, so the direction is visible in the output.
                Some(chain) => {
                    for addr in chain {
                        println!("{}", addr.format());
                    }
                }
                // Unconnected targets leave stdout empty, so callers can test the
                // output for emptiness; the reason goes to the log (stderr) instead.
                None => tracing::info!("no path between {a_label} and {b_label}"),
            }
            Ok(())
        })
    }
}

impl PathApp {
    fn title(&self) -> String {
        format!("Path {} ↔ {}", self.a.format(), self.b.format())
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let a = resolve_addr(args.a.as_ref())?;
    let b = resolve_addr(args.b.as_ref())?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = PathApp {
        engine,
        a,
        b,
        no_transitive: args.no_transitive,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
