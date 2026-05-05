use std::convert::AsRef;
use futures::TryStreamExt;
use crate::commands::bootstrap;
use crate::commands::utils::matcher_from_args;
use crate::engine::get_cwp;

#[derive(clap::Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct Args {}

const VERSION_OPT: Option<&str> = option_env!("HEPH_BUILD_VERSION");

fn version() -> &'static str {
    VERSION_OPT.unwrap_or("v0.0.0-dev")
}

pub fn execute(args: &Args) -> anyhow::Result<()> {
    println!("{}", version());

    Ok(())
}
