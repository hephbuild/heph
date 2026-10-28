use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::engine::Engine;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::driver::targetdef::TargetDef;
use crate::htaddr::{self, Addr};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Show the direct def only, without applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
}

#[derive(Serialize)]
struct DefView<'a> {
    /// How `heph run` treats the target, when it is not built like the rest:
    /// `exec, entrypoint (uncached)` (R20). Omitted for an ordinary target, so
    /// its output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    runs_as: Option<String>,
    target_def: &'a TargetDef,
    applied_transitive: Option<&'a Sandbox>,
}

/// `<driver>, entrypoint (uncached)` for an entrypoint; `None` otherwise.
fn runs_as(driver: &str, def: &TargetDef) -> Option<String> {
    def.entrypoint
        .as_ref()
        .map(|_| format!("{driver}, entrypoint (uncached)"))
}

struct DefApp {
    engine: Arc<Engine>,
    addr: Addr,
    no_transitive: bool,
    fail_fast: bool,
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
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        // `get_def` may run provider targets, recording rich failures in `rs`;
        // `finalize` prefers those over the returned error and prints on success.
        let res = if self.no_transitive {
            self.engine
                .clone()
                .get_direct_def(rs.clone(), &self.addr)
                .await
        } else {
            self.engine.clone().get_def(rs.clone(), &self.addr).await
        };
        crate::commands::errors::finalize!(ctx, rs, res, def => {
            let view = DefView {
                runs_as: runs_as(&def.driver, &def.target_def),
                target_def: &def.target_def,
                applied_transitive: def.applied_transitive.as_ref(),
            };
            let json = serde_json::to_string_pretty(&view).context("serialize def")?;
            println!("{json}");
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
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = DefApp {
        engine,
        addr,
        no_transitive: args.no_transitive,
        fail_fast: global.fail_fast,
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::targetdef::{Entrypoint, Workdir};

    /// An entrypoint says how it runs; an ordinary target's view is unchanged
    /// (the field is omitted, not null).
    #[test]
    fn def_view_says_an_entrypoint_runs_uncached() -> anyhow::Result<()> {
        let mut def = TargetDef::default();
        let plain = serde_json::to_value(DefView {
            runs_as: runs_as("exec", &def),
            target_def: &def,
            applied_transitive: None,
        })?;
        assert!(plain.get("runs_as").is_none(), "{plain}");

        def.entrypoint = Some(Entrypoint::new(false, Workdir::Caller));
        let ep = serde_json::to_value(DefView {
            runs_as: runs_as("exec", &def),
            target_def: &def,
            applied_transitive: None,
        })?;
        assert_eq!(ep["runs_as"], "exec, entrypoint (uncached)");
        Ok(())
    }
}
