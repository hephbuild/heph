use crate::engine::provider::TargetSpec;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(Addr),
    Package(PkgBuf),
    PackagePrefix(PkgBuf),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MatchResult {
    MatchYes,
    MatchNo,
}

impl Matcher {
    pub fn matches(&self, spec: &TargetSpec) -> MatchResult {
        let yes = match self {
            Matcher::Addr(addr) => spec.addr == *addr,
            Matcher::Label(label) => spec.labels.iter().any(|l| l == &label.format()),
            Matcher::Package(pkg) => spec.addr.package == *pkg,
            Matcher::PackagePrefix(prefix) => spec.addr.package.has_prefix(prefix),
            Matcher::Or(matchers) => matchers.iter().any(|m| m.matches(spec) == MatchResult::MatchYes),
            Matcher::And(matchers) => matchers.iter().all(|m| m.matches(spec) == MatchResult::MatchYes),
            Matcher::Not(m) => m.matches(spec) == MatchResult::MatchNo,
        };
        if yes { MatchResult::MatchYes } else { MatchResult::MatchNo }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr { package: PkgBuf::from(pkg), name: name.to_string(), args: HashMap::new() }
    }

    fn spec(pkg: &str, name: &str, labels: &[&str]) -> TargetSpec {
        TargetSpec {
            addr: addr(pkg, name),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn addr_match() {
        let s = spec("foo/bar", "build", &[]);
        assert_eq!(Matcher::Addr(addr("foo/bar", "build")).matches(&s), MatchResult::MatchYes);
        assert_eq!(Matcher::Addr(addr("foo/bar", "test")).matches(&s), MatchResult::MatchNo);
    }

    #[test]
    fn label_match() {
        let s = spec("foo", "bar", &["//labels:lint"]);
        assert_eq!(Matcher::Label(addr("labels", "lint")).matches(&s), MatchResult::MatchYes);
        assert_eq!(Matcher::Label(addr("labels", "other")).matches(&s), MatchResult::MatchNo);
    }

    #[test]
    fn package_match() {
        let s = spec("foo/bar", "build", &[]);
        assert_eq!(Matcher::Package(PkgBuf::from("foo/bar")).matches(&s), MatchResult::MatchYes);
        assert_eq!(Matcher::Package(PkgBuf::from("foo")).matches(&s), MatchResult::MatchNo);
    }

    #[test]
    fn package_prefix_match() {
        let s = spec("foo/bar/baz", "build", &[]);
        assert_eq!(Matcher::PackagePrefix(PkgBuf::from("foo/bar")).matches(&s), MatchResult::MatchYes);
        assert_eq!(Matcher::PackagePrefix(PkgBuf::from("other")).matches(&s), MatchResult::MatchNo);
    }

    #[test]
    fn or_match() {
        let s = spec("foo", "bar", &[]);
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Package(PkgBuf::from("foo")),
        ]);
        assert_eq!(m.matches(&s), MatchResult::MatchYes);
    }

    #[test]
    fn and_match() {
        let s = spec("foo", "bar", &["//labels:lint"]);
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label(addr("labels", "lint")),
        ]);
        assert_eq!(m.matches(&s), MatchResult::MatchYes);
        let m2 = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo")),
            Matcher::Label(addr("labels", "other")),
        ]);
        assert_eq!(m2.matches(&s), MatchResult::MatchNo);
    }

    #[test]
    fn not_match() {
        let s = spec("foo", "bar", &[]);
        assert_eq!(Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other")))).matches(&s), MatchResult::MatchYes);
        assert_eq!(Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo")))).matches(&s), MatchResult::MatchNo);
    }
}
