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
use crate::engine::driver::TargetAddr;
use crate::tui::{self, LogSink};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    /// Explore the dependency tree in an interactive TUI (requires a terminal)
    ///
    /// A terminal UI has no JSON form: `--json` lists the direct deps instead.
    #[arg(short = 'i', long = "interactive", conflicts_with = "output")]
    pub interactive: bool,
    #[command(flatten)]
    pub dashboard: crate::commands::progress_app::DashboardArgs,
    #[command(flatten)]
    pub out: output::JsonArgs<DepRow<'static>>,
}

/// `heph inspect deps --json`: one element per declared input, in the order
/// the def declares them.
#[derive(Debug, serde::Serialize, output::Record)]
pub struct DepRow<'a> {
    /// The input's reference: the target, with its output group and filters
    /// when it names any (`//pkg:name|out`).
    r#ref: &'a TargetAddr,
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

    if args.interactive {
        let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
        if !tui::should_use_tui(global.no_tui) {
            anyhow::bail!("--interactive requires a terminal (stderr is not a TTY)");
        }
        return super::deps_explorer::run(engine, addr, sink, shutdown).await;
    }

    let json = args.out.clone();
    run_engine_command_out(
        format!("Deps {}", addr.format()),
        sink,
        session,
        &global,
        &args.out,
        args.dashboard.dashboard,
        move |engine, rs, ctx| async move {
            // `get_def` may run provider targets, recording rich failures in `rs`;
            // `finalize` prefers those over the returned error and prints on success.
            let res = Arc::clone(&engine).get_def(rs.clone(), &addr).await;
            let cancelled = || rs.ctoken().is_cancelled();
            crate::commands::errors::finalize(&ctx, &rs, res, None, |def| {
                let rows = def.target_def.inputs.iter().map(|input| DepRow {
                    r#ref: &input.r#ref,
                });
                if json.enabled() {
                    return json.list_iter(rows, &cancelled);
                }
                let mut stdout = std::io::stdout().lock();
                for row in rows {
                    writeln!(stdout, "{}", row.r#ref).context("printing the deps")?;
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
        let r#ref = TargetAddr {
            r#ref: crate::htaddr::parse_addr("//a:b").unwrap(),
            output: Some("out".into()),
            filters: Vec::new(),
        };
        output::check_keys(&DepRow { r#ref: &r#ref }).unwrap();
    }

    /// `-i` is a terminal UI, so it cannot be asked for as JSON.
    #[test]
    fn interactive_conflicts_with_the_output_flags() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        for flag in [&["--json"][..], &["--jq", "."], &["--fields", "ref"]] {
            let argv = [&["deps", "//a:b", "-i"][..], flag].concat();
            let err = Cli::try_parse_from(&argv).err().expect("refused");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}"
            );
        }
    }
}
