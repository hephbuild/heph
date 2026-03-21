use std::path::PathBuf;
use clap::Args;
use crate::{engine, htaddr, htmatcher};
use crate::commands::bootstrap;

#[derive(Args)]
#[command(override_usage = "run <TARGET_ADDRESS>\n       run <LABEL> <PACKAGE_MATCHER>")]
pub struct RunArgs {
    /// Target address (e.g., //pkg:name) OR Label
    #[arg(value_name = "TARGET_ADDRESS/LABEL")]
    pub arg1: String,
    /// Package matcher (only if first argument is a Label)
    #[arg(value_name = "PACKAGE_MATCHER")]
    pub arg2: Option<String>,
}

pub fn execute(args: &RunArgs) -> anyhow::Result<()> {
    if let Some(package_matcher) = &args.arg2 {
        let label = &args.arg1;
        execute_matcher(htmatcher::Matcher::And(vec![
            htmatcher::Matcher::Label(htaddr::parse_addr(label).map_err(anyhow::Error::msg)?),
            htmatcher::Matcher::Package(package_matcher.clone()),
        ]))
    } else {
        let address = &args.arg1;
        match htaddr::parse_addr(address) {
            Ok(addr) => execute_matcher(htmatcher::Matcher::Addr(addr)),
            Err(e) => {
                anyhow::bail!("Error: '{}' is not a valid target address: {}", address, e);
            }
        }
    }
}

fn execute_matcher(m: htmatcher::Matcher) -> anyhow::Result<()> {
   let e =  bootstrap::new_engine()?;

    e.result(m);

    Ok(())
}
