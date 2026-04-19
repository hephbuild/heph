use crate::htmatcher;
use crate::htpkg::PkgBuf;

pub fn parse(s: &str) -> anyhow::Result<htmatcher::Matcher> {
    let matcher = htmatcher::Matcher::Package(PkgBuf::from(s));// TODO

    Ok(matcher)
}
