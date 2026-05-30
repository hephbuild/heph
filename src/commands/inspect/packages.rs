use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::htmatcher::{self, Matcher};
use crate::htpkg::PkgBuf;
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Packages matcher
    pub matcher: Option<String>,
}

struct PackagesApp {
    engine: Arc<Engine>,
    matcher: Matcher,
}

#[async_trait]
impl App for PackagesApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new(format!("Packages {:?}", self.matcher))
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new(format!("Packages {:?}", self.matcher))
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self.engine.new_state_with_events(true, ctx.event_sender());

        // Output is incremental; listing may run provider targets, recording rich
        // failures in `rs`, which `finalize` prefers over the returned error.
        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            for p in self.engine.packages(&self.matcher, &rs).await? {
                out.println(p?);
            }
            Ok(())
        }
        .await;
        out.close().await;

        crate::commands::errors::finalize!(ctx, rs, res)
    }
}

pub fn execute(args: &Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    execute_async(args.clone(), sink, no_tui)
}

#[tokio::main]
async fn execute_async(args: Args, sink: LogSink, no_tui: bool) -> anyhow::Result<()> {
    let matcher = match &args.matcher {
        Some(s) => htmatcher::parse(s.as_str())?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = PackagesApp { engine, matcher };
    let interactive = tui::should_use_tui(no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
