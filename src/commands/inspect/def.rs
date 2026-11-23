use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command;
use crate::commands::utils::parse_addr_arg;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::driver::targetdef::TargetDef;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Show the direct def only, without applying transitive deps
    #[arg(long)]
    pub no_transitive: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

#[derive(Serialize)]
struct DefView<'a> {
    /// How `heph run` treats the target, when it is not built like the rest:
    /// `exec, entrypoint (uncached)` (R20). Omitted for an ordinary target, so
    /// its output is unchanged. A string for humans: a program keys on
    /// `target_def.entrypoint` (`accepts_args`, `workdir`) instead.
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

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let addr = parse_addr_arg(&args.addr)?;
    let no_transitive = args.no_transitive;
    run_engine_command(
        format!("Def {}", addr.format()),
        sink,
        session,
        &global,
        global.no_tui,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `get_def` may run provider targets, recording rich failures in `rs`;
            // `finalize` prefers those over the returned error and prints on success.
            let res = if no_transitive {
                Arc::clone(&engine).get_direct_def(rs.clone(), &addr).await
            } else {
                Arc::clone(&engine).get_def(rs.clone(), &addr).await
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
        },
    )
    .await
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
        // The machine-readable form agents key on: whether it takes args and
        // where it runs.
        assert_eq!(
            ep["target_def"]["entrypoint"],
            serde_json::json!({"accepts_args": false, "workdir": "caller"}),
            "{ep}"
        );
        Ok(())
    }
}
