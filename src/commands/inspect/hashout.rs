use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context as _;
use clap_complete::engine::ArgValueCompleter;
use std::io::Write as _;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::utils::parse_addr_arg;
use crate::engine::{OutputMatcher, ResultOptions};
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<HashoutRow<'static>>,
}

/// `heph inspect hashout --json`: one element per output artifact, in the
/// order the text output prints them.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct HashoutRow<'a> {
    /// The content hash of one output artifact.
    hashout: &'a str,
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
        format!("Hashout {}", addr.format()),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `result_addr` runs the target; rich failures land in `rs`. `finalize`
            // prefers those over the returned marker and prints the hashes on success.
            let res = Arc::clone(&engine)
                .result_addr(
                    rs.clone(),
                    &addr,
                    OutputMatcher::None,
                    &ResultOptions {
                        no_scratch,
                        ..Default::default()
                    },
                )
                .await;
            let cancelled = || rs.ctoken().is_cancelled();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |result| {
                let hashouts = result.artifacts_meta.iter().map(|art| HashoutRow {
                    hashout: &art.hashout,
                });
                if json.enabled() {
                    return json.list_iter(hashouts, &cancelled);
                }
                let mut stdout = std::io::stdout().lock();
                for row in hashouts {
                    writeln!(stdout, "{}", row.hashout).context("printing the hashouts")?;
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
        output::check_keys(&HashoutRow { hashout: "h" }).unwrap();
    }
}
