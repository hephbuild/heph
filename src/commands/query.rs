use clap::Args;
use futures::TryStreamExt;
use crate::commands::bootstrap;
use crate::htmatcher;
use crate::htpkg::PkgBuf;

#[derive(Args)]
pub struct QueryArgs {
    /// Targets matcher
    pub matcher: Option<String>,
}

#[tokio::main]
pub async fn execute(args: &QueryArgs) -> anyhow::Result<()> {
    let e = bootstrap::new_engine()?;

    let m = match &args.matcher {
        Some(s) => htmatcher::parse(s.as_str())?,
        None => htmatcher::Matcher::PackagePrefix(PkgBuf::from(""))
    };

    let rs = e.new_state();
    let stream = e.query(&m, rs);
    tokio::pin!(stream);
    while let Some(addr) = stream.try_next().await? {
        println!("{}", addr.format());
    }

    Ok(())
}
