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
    pub out: output::JsonArgs<HashinRecord<'static>>,
}

/// `heph inspect hashin --json`.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct HashinRecord<'a> {
    /// The target's address.
    addr: &'a Addr,
    /// The target's input hash: the key of its cache entry.
    hashin: &'a str,
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
    let json = args.out.clone();
    run_engine_command_out(
        format!("Hashin {}", addr.format()),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `meta` may run provider targets, recording rich failures in `rs`;
            // `finalize` prefers those over the returned error and prints on success.
            let res = Arc::clone(&engine).meta(rs.clone(), &addr).await;
            let cancelled = || rs.ctoken().is_cancelled();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |meta| {
                if json.enabled() {
                    let record = HashinRecord {
                        addr: &addr,
                        hashin: &meta.hashin,
                    };
                    return json.record(&record, &cancelled);
                }
                writeln!(std::io::stdout(), "{}", meta.hashin).context("printing the hashin")
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
        output::check_keys(&HashinRecord {
            addr: &addr,
            hashin: "h",
        })
        .unwrap();
    }
}
