//! `heph inspect runners` — the exec runners this workspace can use.
//!
//! Deliberately **static**. Live sessions are not listed here and cannot be:
//! every `heph` invocation owns its own `Engine` and its own session pool, so a
//! separate process would render an empty table every time — which reads as
//! "nothing is running" rather than "this command cannot see it". Live sessions
//! belong to the build that owns them, and surface in its own output.

use anyhow::Context;
use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Emit JSON instead of the text listing
    #[arg(long)]
    pub json: bool,
}

#[derive(serde::Serialize)]
struct RunnerRow {
    /// Registry name. A runner target's **driver** name selects it.
    name: String,
    /// Whether this is the runner the workspace applies by default.
    is_default: bool,
}

#[derive(serde::Serialize)]
struct RunnersView {
    /// `defaultRunner:` from `.hephconfig`, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    default_runner: Option<String>,
    runners: Vec<RunnerRow>,
}

struct RunnersApp {
    engine: std::sync::Arc<Engine>,
    json: bool,
}

#[async_trait]
impl App for RunnersApp {
    type Output = ();
    type TuiView = crate::tui::TuiProgressView;
    type CiView = crate::tui::CiProgressView;

    fn tui_view(&self) -> Self::TuiView {
        crate::tui::TuiProgressView::new("Runners".to_string())
    }

    fn ci_view(&self) -> Self::CiView {
        crate::tui::CiProgressView::new("Runners".to_string())
    }

    async fn run(self, _ctx: AppContext) -> anyhow::Result<()> {
        let default = self.engine.default_runner().map(|a| a.format());

        let mut names: Vec<String> = self.engine.exec_runners.keys().cloned().collect();
        names.sort();

        // `local` is always available and is never in the registry: it is the
        // absence of a runner, not an entry. Listing it anyway is honest — a
        // reader looking for "what can I put in `runner =`" needs to see the
        // opt-out alongside the rest.
        let view = RunnersView {
            default_runner: default.clone(),
            runners: names
                .into_iter()
                .map(|name| RunnerRow {
                    is_default: false,
                    name,
                })
                .collect(),
        };

        if self.json {
            let json = serde_json::to_string_pretty(&view).context("serialize runners")?;
            println!("{json}");
            return Ok(());
        }

        match &view.default_runner {
            Some(d) => println!("defaultRunner: {d}"),
            None => println!("defaultRunner: (unset — targets run in the host process)"),
        }
        if view.runners.is_empty() {
            println!("(no exec runners registered)");
        } else {
            println!("registered runners (selected by a runner target's `driver`):");
            for r in &view.runners {
                println!("  {}", r.name);
            }
        }
        println!("`runner = None` on a target opts out of the default.");
        Ok(())
    }
}

pub fn execute(args: &Args, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(args: Args, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = RunnersApp {
        engine,
        json: args.json,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}
