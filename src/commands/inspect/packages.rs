use crate::commands::bootstrap;
use crate::htmatcher;
use crate::htpkg::PkgBuf;

#[derive(clap::Args)]
pub struct Args {
    /// Packages matcher
    pub matcher: Option<String>,
}

#[tokio::main]
pub async fn execute(args: &Args) -> anyhow::Result<()>  {
    let e =  bootstrap::new_engine()?;

    let m = match &args.matcher {
        Some(s) => htmatcher::parse(s.as_str())?,
        None => htmatcher::Matcher::PackagePrefix(PkgBuf::from(""))
    };

    let ctoken = crate::hasync::StdCancellationToken::new();
    let it = e.packages(&m, &ctoken).await?;
    for res in it {
        let p = res?;
        println!("{}", p);
    }

    Ok(())
}
