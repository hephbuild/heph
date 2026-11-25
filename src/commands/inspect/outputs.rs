use crate::tui::session::SessionSlot;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;
use std::io::Write as _;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::utils::parse_addr_arg;
use crate::engine::{OutputMatcher, ResultOptions};
use crate::htaddr::Addr;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<OutputsView<'static>>,
}

/// `heph inspect outputs --json`.
#[derive(Debug, Serialize, output::Record)]
pub struct OutputsView<'a> {
    /// The target's address.
    addr: &'a Addr,
    /// Every path in its output artifacts, sorted: what the text output prints.
    #[serde(serialize_with = "output::ser::paths")]
    paths: Vec<PathBuf>,
    /// Every path in its support artifacts (`support_files`), sorted.
    #[serde(serialize_with = "output::ser::paths")]
    support_paths: Vec<PathBuf>,
}

/// Every path in `arts`, sorted bytewise (the order the text output always
/// had) and deduplicated.
fn collect_paths(
    arts: &[Arc<dyn hcore::hartifactcontent::Content>],
) -> anyhow::Result<Vec<PathBuf>> {
    // `entry_paths` is header-only for tar-backed cache artifacts, so
    // listing a target's outputs does not read their bytes.
    let mut out = Vec::new();
    for art in arts {
        out.extend(art.entry_paths().context("enumerate artifact paths")?);
    }
    out.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    out.dedup();
    Ok(out)
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
    // `--no-scratch`: run against empty scratch caches. Implies a rebuild.
    let no_scratch = global.no_scratch;
    let json = args.out.clone();
    run_engine_command_out(
        format!("Outputs {}", addr.format()),
        sink,
        session,
        &global,
        &args.out,
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
            let cancelled = || rs.ctoken().is_cancelled();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |result| {
                let paths = collect_paths(&result.artifacts)?;
                if json.enabled() {
                    let view = OutputsView {
                        addr: &addr,
                        paths,
                        support_paths: collect_paths(&result.support_artifacts)?,
                    };
                    return json.record(&view, &cancelled);
                }
                let mut stdout = std::io::stdout().lock();
                for p in &paths {
                    writeln!(stdout, "{}", p.to_string_lossy()).context("printing the outputs")?;
                }
                Ok(())
            })
            .await
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() {
        let addr = crate::htaddr::parse_addr("//a:b").unwrap();
        output::check_keys(&OutputsView {
            addr: &addr,
            paths: vec![PathBuf::from("a/b")],
            support_paths: vec![PathBuf::from("c")],
        })
        .unwrap();
    }
}
