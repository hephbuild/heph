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
        let address = arg1;
        match htaddr::parse_addr(address) {
            Ok(addr) => Ok(Matcher::Addr(addr)),
            Err(e) => {
                anyhow::bail!("Error: '{}' is not a valid target address: {}", address, e);
            }
        }
    }
}
