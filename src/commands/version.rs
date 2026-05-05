#[derive(clap::Args)]
pub struct Args {}

const VERSION: &str = match option_env!("HEPH_BUILD_VERSION") {
    Some(val) => val,
    None => "v0.0.0-dev",
};

pub fn execute(_args: &Args) -> anyhow::Result<()> {
    println!("{}", VERSION);

    Ok(())
}
