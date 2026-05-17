use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::{OutputMatcher, ResultOptions, get_cwp};
use crate::htmatcher::Matcher;
use clap::Args;
use std::io;

#[derive(Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Force execution
    #[arg(long = "force")]
    pub force: bool,
    /// Print output artifacts to stdout
    #[arg(long = "cat-out")]
    pub cat_out: bool,
    /// Print output file list to stdout
    #[arg(long = "list-out")]
    pub list_out: bool,
}

pub fn execute(args: &RunArgs) -> anyhow::Result<()> {
    // Cap worker_threads to physical parallelism. The default
    // `#[tokio::main]` runtime spawns far more workers than needed for our
    // workload (160+ on a 10-core box), and idle workers cost real park /
    // unpark scheduling traffic.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    rt.block_on(execute_inner(args))
}

async fn execute_inner(args: &RunArgs) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &base_pkg, false)?;

    let e = bootstrap::new_engine()?;

    let opts = ResultOptions { force: args.force };

    let result = match m {
        Matcher::Addr(addr) => {
            vec![
                e.clone()
                    .result_addr(e.new_state(), &addr, OutputMatcher::All, &opts)
                    .await?,
            ]
        }
        _ => e.clone().result(e.new_state(), &m, &opts).await?,
    };

    if args.cat_out {
        for r in &result {
            for a in &r.artifacts {
                for e in a.walk()? {
                    io::copy(&mut e?.data, &mut io::stdout())?;
                }
            }
        }
    } else if args.list_out {
        for r in &result {
            for a in &r.artifacts {
                for e in a.walk()? {
                    println!("{}", e?.path.display());
                }
            }
        }
    } else {
        println!("{} matched", result.len());
    }

    Ok(())
}
