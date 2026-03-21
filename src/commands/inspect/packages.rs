use clap::Args;
use crate::commands::bootstrap;
use crate::htmatcher;

#[derive(Args)]
pub struct PackagesArgs {
    /// Packages matcher
    pub matcher: Option<String>,
}

pub fn execute(args: &PackagesArgs) -> anyhow::Result<()>  {
    let e =  bootstrap::new_engine()?;

    let m = match &args.matcher {
        Some(s) => htmatcher::parse(s.as_str())?,
        None => htmatcher::Matcher::PackagePrefix("".parse()?)
    };

    for p in e.packages(&m) {
        println!("{}", p);
    }

    Ok(())
}
