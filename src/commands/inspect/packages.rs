use clap::Args;
use crate::commands::bootstrap;
use crate::htmatcher;

#[derive(Args)]
pub struct PackagesArgs {
    /// Packages matcher
    pub matcher: Option<String>,
}

#[tokio::main]
pub async fn execute(args: &PackagesArgs) -> anyhow::Result<()>  {
    let e =  bootstrap::new_engine()?;

    let m = match &args.matcher {
        Some(s) => htmatcher::parse(s.as_str())?,
        None => htmatcher::Matcher::PackagePrefix("".to_string())
    };

    let ctoken = crate::hasync::StdCancellationToken::new();
    let it = e.packages(&m, &ctoken).await?;
    for res in it {
        let p = res?;
        println!("{}", p);
    }

    Ok(())
}
