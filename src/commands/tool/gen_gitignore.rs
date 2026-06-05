use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::{Engine, get_root, gitignore};
use crate::tui::{self, App, AppContext, BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {}

struct GenGitignoreApp {
    engine: Arc<Engine>,
    root: PathBuf,
    fail_fast: bool,
}

#[async_trait]
impl App for GenGitignoreApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Generating .gitignore".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Generating .gitignore".to_string())
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());

        let out = BufferedStdout::new(&ctx);
        let res: anyhow::Result<()> = async {
            let entries = Arc::clone(&self.engine)
                .codegen_copy_gitignore_patterns(rs.clone())
                .await?;

            let path = self.root.join(".gitignore");
            let existing = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            };
            let updated = gitignore::render(&existing, &entries);
            if updated == existing {
                out.println(format!("{} already up to date", path.display()));
            } else {
                std::fs::write(&path, &updated)
                    .with_context(|| format!("writing {}", path.display()))?;
                out.println(format!(
                    "Updated {} ({} entr{})",
                    path.display(),
                    entries.len(),
                    if entries.len() == 1 { "y" } else { "ies" }
                ));
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

async fn execute_async(_args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let root = get_root()?;
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = GenGitignoreApp {
        engine,
        root,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
