use crate::version::VERSION;

#[derive(clap::Args)]
pub struct Args {}

pub fn execute(_args: &Args) -> anyhow::Result<()> {
    println!("{}", VERSION);

    Ok(())
}
