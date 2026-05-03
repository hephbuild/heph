use anyhow::Context;
use crate::commands::bootstrap;
use crate::{htaddr};
use crate::engine::{OutputMatcher, ResultOptions};

#[derive(clap::Args)]
pub struct Args {
    /// Target address
    pub addr: String,
}

#[tokio::main]
pub async fn execute(args: &Args) -> anyhow::Result<()>  {
    let e =  bootstrap::new_engine()?;

    let addr = htaddr::parse_addr(args.addr.as_ref()).with_context(|| format!("parse {}", args.addr))?;

    let res = e.clone().result_addr(e.clone().new_state(), &addr, OutputMatcher::None, &ResultOptions::default()).await?;

    for art in res.artifacts_meta {
        println!("{}", art.hashout);
    }

    Ok(())
}
