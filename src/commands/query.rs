use clap::Args;
use futures::TryStreamExt;
use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::get_cwp;

#[derive(Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct QueryArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
}

#[tokio::main]
pub async fn execute(args: &QueryArgs) -> anyhow::Result<()> {
    let e = bootstrap::new_engine()?;

    let cwp = get_cwp()?;
    let m = matcher_from_args(&args.arg1, &args.arg2, &cwp, true)?;

    let rs = e.new_state();
    let stream = e.query(rs, &m);
    tokio::pin!(stream);
    while let Some(addr) = stream.try_next().await? {
        println!("{}", addr.format());
    }

    Ok(())
}
