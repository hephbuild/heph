use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Package matcher (defaults to all packages)
    pub matcher: Option<String>,
}

struct PackagesApp {
    engine: Arc<Engine>,
    matcher: Matcher,
    fail_fast: bool,
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
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

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

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let matcher = match &args.matcher {
        Some(s) => htpkg::parse(s.as_str(), &crate::engine::get_cwp()?)?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = PackagesApp {
        engine,
        matcher,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
