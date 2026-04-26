use std::io;
use std::io::Cursor;
use clap::Args;
use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::get_cwp;
use crate::htmatcher::Matcher;

#[derive(Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
    /// Print output artifacts to stdout
    #[arg(long = "cat-out")]
    pub cat_out: bool,
    /// Print output file list to stdout
    #[arg(long = "list-out")]
    pub list_out: bool,
}

#[tokio::main]
pub async fn execute(args: &RunArgs) -> anyhow::Result<()> {
    let base_pkg = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &base_pkg, false)?;

   let e =  bootstrap::new_engine()?;

    let result = match m {
        Matcher::Addr(addr) => {
            vec![e.clone().result_addr(e.new_state(), &addr).await?]
        },
        _ => {
            e.clone().result(e.new_state(), &m).await?
        }
    };

    if args.cat_out {
        for r in result {
            for a in r.artifacts {
                for e in a.walk()? {
                    io::copy(&mut Cursor::new(e?.data), &mut io::stdout())?;
                }
            }
        }
    } else if args.list_out {
        for r in result {
            for a in r.artifacts {
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
