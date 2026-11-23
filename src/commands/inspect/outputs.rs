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
use crate::engine::{OutputMatcher, ResultOptions};
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Emit JSON instead of one path per line
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
}

#[derive(Serialize)]
struct OutputsView {
    addr: String,
    paths: Vec<String>,
    support_paths: Vec<String>,
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
    let json = args.json;
    // `--no-scratch`: run against empty scratch caches. Implies a rebuild.
    let no_scratch = global.no_scratch;
    run_engine_command(
        format!("Outputs {}", addr.format()),
        sink,
        session,
        &global,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `OutputMatcher::All` rather than `None`: the paths live inside the
            // artifacts, and `None` resolves hashouts without handing any back.
            let res = Arc::clone(&engine)
                .result_addr(
                    rs.clone(),
                    &addr,
                    OutputMatcher::All,
                    &ResultOptions {
                        no_scratch,
                        ..Default::default()
                    },
                )
                .await;
            let addr = addr.format();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |result| {
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
            .await
        },
    )
    .await
}
