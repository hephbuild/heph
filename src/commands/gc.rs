use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct GcArgs {}

struct GcApp {
    engine: Arc<Engine>,
    fail_fast: bool,
}

#[async_trait]
impl App for GcApp {
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::GcCiView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::with_header(Box::new(tui::GcHeader::new("GC")))
    }

    fn ci_view(&self) -> Self::CiView {
        tui::GcCiView::new("GC")
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        // GC resolves every cached target via `get_spec`, which may run
        // providers (filesystem walks, Starlark) and record failures in `rs`.
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        let res = self.engine.clone().gc_all(rs.clone()).await;
        // The progress view renders the sweep summary (TUI final line / CI finish);
        // no extra print here, which would duplicate it.
        crate::commands::errors::finalize!(ctx, rs, res, _stats => { Ok(()) })
    }
}

pub fn execute(args: &GcArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(_args: GcArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = GcApp {
        engine,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
