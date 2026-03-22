use clap::Args;
use crate::commands::bootstrap;
use crate::{htaddr, htmatcher};
use crate::hasync::{Cancellable, StdCancellationToken};
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
}

#[tokio::main]
pub async fn execute(args: &RunArgs) -> anyhow::Result<()> {
    if let Some(package_matcher) = &args.arg2 {
        let label = &args.arg1;
        execute_matcher(Matcher::And(vec![
            Matcher::Label(htaddr::parse_addr(label).map_err(anyhow::Error::msg)?),
            Matcher::Package(package_matcher.clone()),
        ])).await
    } else {
        let address = &args.arg1;
        match htaddr::parse_addr(address) {
            Ok(addr) => execute_matcher(Matcher::Addr(addr)).await,
            Err(e) => {
                anyhow::bail!("Error: '{}' is not a valid target address: {}", address, e);
            }
        }
    }
}

async fn execute_matcher(m: Matcher) -> anyhow::Result<()> {
   let e =  bootstrap::new_engine()?;

   let addr = match m {
       Matcher::Addr(addr) => addr,
       _ => anyhow::bail!("Matcher not supported yet"),
   };

    let result = e.result(e.new_state(), &addr).await?;

    println!("{} artifacts", result.artifacts.len());

    Ok(())
}
