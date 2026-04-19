use crate::htaddr::Addr;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(Addr),
    Package(String),
    PackagePrefix(String),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}

impl Matcher {
    pub fn match_addr(&self, addr: &Addr) -> bool {
        match self {
            Matcher::Addr(a) => addr == a,
            Matcher::Label(_) => false,
            Matcher::Package(p) => &addr.package == p,
            Matcher::PackagePrefix(prefix) => {
                prefix.is_empty()
                    || addr.package == prefix.as_str()
                    || addr.package.starts_with(&format!("{}/", prefix))
            }
            Matcher::Or(matchers) => matchers.iter().any(|m| m.match_addr(addr)),
            Matcher::And(matchers) => matchers.iter().all(|m| m.match_addr(addr)),
            Matcher::Not(m) => !m.match_addr(addr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn addr(package: &str, name: &str) -> Addr {
        Addr { package: package.to_string(), name: name.to_string(), args: HashMap::new() }
    }

    #[test]
    fn package_prefix_exact_match() {
        assert!(Matcher::PackagePrefix("foo/bar".to_string()).match_addr(&addr("foo/bar", "t")));
    }

    #[test]
    fn package_prefix_child_match() {
        assert!(Matcher::PackagePrefix("foo/bar".to_string()).match_addr(&addr("foo/bar/baz", "t")));
        assert!(Matcher::PackagePrefix("foo".to_string()).match_addr(&addr("foo/bar/baz", "t")));
    }

    #[test]
    fn package_prefix_no_partial_component_match() {
        assert!(!Matcher::PackagePrefix("foo/ba".to_string()).match_addr(&addr("foo/bar/baz", "t")));
    }

    #[test]
    fn package_prefix_empty_prefix_matches_all() {
        assert!(Matcher::PackagePrefix("".to_string()).match_addr(&addr("", "t")));
        assert!(Matcher::PackagePrefix("".to_string()).match_addr(&addr("foo/bar", "t")));
        assert!(Matcher::PackagePrefix("".to_string()).match_addr(&addr("foo/bar/baz", "t")));
    }
}
