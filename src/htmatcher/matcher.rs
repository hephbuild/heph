use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MatchResult {
    MatchYes,
    MatchNo,
    MatchShrug,
}

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

impl Matcher {
    pub fn matches_addr(&self, addr: &Addr) -> MatchResult {
        match self {
            Matcher::Addr(a) => {
                if a == addr {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Label(_) => MatchResult::MatchShrug,
            Matcher::Package(pkg) => {
                if &addr.package == pkg {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::PackagePrefix(prefix) => {
                if addr.package.has_prefix(prefix) {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Or(matchers) => {
                let mut shrug = false;
                for m in matchers {
                    match m.matches_addr(addr) {
                        MatchResult::MatchYes => return MatchResult::MatchYes,
                        MatchResult::MatchShrug => shrug = true,
                        MatchResult::MatchNo => {}
                    }
                }
                if shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::And(matchers) => {
                let mut shrug = false;
                for m in matchers {
                    match m.matches_addr(addr) {
                        MatchResult::MatchNo => return MatchResult::MatchNo,
                        MatchResult::MatchShrug => shrug = true,
                        MatchResult::MatchYes => {}
                    }
                }
                if shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchYes
                }
            }
            Matcher::Not(m) => match m.matches_addr(addr) {
                MatchResult::MatchYes => MatchResult::MatchNo,
                MatchResult::MatchNo => MatchResult::MatchYes,
                MatchResult::MatchShrug => MatchResult::MatchShrug,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr {
            package: PkgBuf::from(pkg),
            name: name.to_string(),
            args: HashMap::new(),
        }
    }

    #[test]
    fn addr_exact_match() {
        let a = addr("foo/bar", "baz");
        assert_eq!(Matcher::Addr(a.clone()).matches_addr(&a), MatchResult::MatchYes);
    }

    #[test]
    fn addr_no_match() {
        let a = addr("foo/bar", "baz");
        let b = addr("foo/bar", "qux");
        assert_eq!(Matcher::Addr(a).matches_addr(&b), MatchResult::MatchNo);
    }

    #[test]
    fn label_always_shrugs() {
        let a = addr("foo/bar", "baz");
        let label = addr("", "my_label");
        assert_eq!(Matcher::Label(label).matches_addr(&a), MatchResult::MatchShrug);
    }

    #[test]
    fn package_match() {
        let a = addr("foo/bar", "baz");
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo/bar")).matches_addr(&a),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Package(PkgBuf::from("foo")).matches_addr(&a),
            MatchResult::MatchNo
        );
    }

    #[test]
    fn package_prefix_match() {
        let a = addr("foo/bar/baz", "t");
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("foo/bar")).matches_addr(&a),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("foo/ba")).matches_addr(&a),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::PackagePrefix(PkgBuf::from("")).matches_addr(&a),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn or_yes_if_any_yes() {
        let a = addr("foo/bar", "t");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Package(PkgBuf::from("foo/bar")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchYes);
    }

    #[test]
    fn or_no_if_all_no() {
        let a = addr("foo/bar", "t");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("a")),
            Matcher::Package(PkgBuf::from("b")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchNo);
    }

    #[test]
    fn or_shrug_if_no_yes_but_some_shrug() {
        let a = addr("foo/bar", "t");
        let label = addr("", "lbl");
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Label(label),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchShrug);
    }

    #[test]
    fn and_yes_if_all_yes() {
        let a = addr("foo/bar", "t");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::PackagePrefix(PkgBuf::from("foo")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchYes);
    }

    #[test]
    fn and_no_if_any_no() {
        let a = addr("foo/bar", "t");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Package(PkgBuf::from("other")),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchNo);
    }

    #[test]
    fn and_shrug_if_no_no_but_some_shrug() {
        let a = addr("foo/bar", "t");
        let label = addr("", "lbl");
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Label(label),
        ]);
        assert_eq!(m.matches_addr(&a), MatchResult::MatchShrug);
    }

    #[test]
    fn not_flips() {
        let a = addr("foo/bar", "t");
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo/bar")))).matches_addr(&a),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other")))).matches_addr(&a),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn not_shrug_stays_shrug() {
        let a = addr("foo/bar", "t");
        let label = addr("", "lbl");
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Label(label))).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }
}
