use crate::commands::bootstrap;
use crate::htaddr;
use anyhow::Context;

#[derive(clap::Args)]
pub struct Args {
    /// Target address
    pub addr: String,
}

#[tokio::main]
pub async fn execute(args: &Args) -> anyhow::Result<()> {
    let e = bootstrap::new_engine()?;

    let addr =
        htaddr::parse_addr(args.addr.as_ref()).with_context(|| format!("parse {}", args.addr))?;

    let def = e.clone().get_def(e.clone().new_state(), &addr).await?;

    for input in def.target_def.inputs {
        eprintln!("{}", input.r#ref);
    }

    Ok(())
}
