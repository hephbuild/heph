use crate::htmatcher;

pub fn parse(s: &str) -> anyhow::Result<htmatcher::Matcher> {
    let matcher = htmatcher::Matcher::Package(s.parse()?);// TODO

    Ok(matcher)
}