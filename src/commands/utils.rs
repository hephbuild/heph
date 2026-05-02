use anyhow::Context;
use crate::{engine, htaddr, htpkg};
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;

pub fn matcher_from_args(arg1: &String, arg2: &Option<String>, base_pkg: &PkgBuf, allow_all: bool) -> anyhow::Result<Matcher> {
    if let Some(package_matcher) = &arg2 {
        let label = arg1;

        if label == "all" {
            if !allow_all {
                anyhow::bail!("label `all` not allowed")
            }

            return htpkg::parse(package_matcher, &engine::get_cwp()?);
        }

        Ok(Matcher::And(vec![
            Matcher::Label(htaddr::parse_addr_with_base(label, base_pkg)?),
            htpkg::parse(package_matcher, &engine::get_cwp()?)?,
        ]))
    } else {
        let addr_str = arg1;
        let addr = htaddr::parse_addr(addr_str).with_context(|| format!("parse {}", addr_str))?;
        Ok(Matcher::Addr(addr))
    }
}
