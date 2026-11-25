use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use futures::StreamExt;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::progress_app::run_engine_command_out;
use crate::commands::utils::resolve_addr;
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::htpkg::{self, PkgBuf};
use crate::tui::{BufferedStdout, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target whose users to find: a `//pkg:name` or relative (`:name`,
    /// `./pkg:name`) address, or a path to an existing file (sugar for its `fs`
    /// file target)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Restrict the search to packages matching this matcher (e.g. //pkg/...);
    /// defaults to the whole workspace
    #[arg(long, value_name = "PACKAGE_MATCHER")]
    pub scope: Option<String>,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<RevdepRow>,
}

/// `heph inspect revdeps --json`: one element per dependent, as it is found.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct RevdepRow {
    /// A target that declares the given target as a direct input.
    addr: Addr,
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
    // The target whose reverse dependencies (users) we want.
    let addr = resolve_addr(args.addr.as_ref())?;
    // Packages to search for users (whole workspace by default).
    let scope = match &args.scope {
        Some(s) => htpkg::parse(s.as_str(), &crate::engine::get_cwp()?)
            .with_context(|| format!("parse scope {s}"))?,
        None => Matcher::PackagePrefix(PkgBuf::from("")),
    };
    let json = args.out.clone();
    run_engine_command_out(
        format!("Revdeps {}", addr.format()),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // Stream dependents as they are found and print incrementally. Resolving
            // in-scope targets records rich failures in `rs`, which `finalize`
            // prefers over the returned error.
            let gaps = crate::engine::Gaps::new(crate::htquery::format(&scope));
            let dependents = Arc::clone(&engine).revdeps(
                rs.clone(),
                addr,
                &scope,
                crate::engine::Discovery::keep_going_unless(rs.fail_fast(), &gaps),
            );
            if json.enabled() {
                let cancelled = || rs.ctoken().is_cancelled();
                let mut list = json.printer().start_list()?;
                let rows = dependents.map(|dependent| dependent.map(|addr| RevdepRow { addr }));
                // A closed stdout stops the walk: the stream is dropped unpolled.
                let res = list.push_stream(rows).await;
                return crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |flow| {
                    // Closed only when the walk is whole (D10).
                    if flow == output::Flow::Continue && gaps.is_empty() {
                        list.finish(&cancelled)?;
                    }
                    Ok(())
                })
                .await;
            }
            let out = BufferedStdout::new(&ctx);
            let res: anyhow::Result<()> = async {
                futures::pin_mut!(dependents);
                while let Some(dependent) = dependents.next().await {
                    out.println(dependent?.format());
                }
                Ok(())
            }
            .await;
            out.close().await;

            crate::commands::errors::finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(())).await
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
        output::check_keys(&RevdepRow { addr }).unwrap();
    }
}
