use crate::engine::provider::TargetSpec;
use crate::htmatcher::{MatchResult, Matcher};

impl Matcher {
    pub fn matches_spec(&self, spec: &TargetSpec) -> MatchResult {
        match self {
            Matcher::Addr(addr) => {
                if spec.addr == *addr {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Label(addr) => {
                let label = addr.format();
                if spec.labels.contains(&label) {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Package(pkg) => {
                if spec.addr.package == *pkg {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::PackagePrefix(pkg) => {
                if spec.addr.package.has_prefix(pkg) {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Or(matchers) => {
                let mut has_shrug = false;
                for m in matchers {
                    match m.matches_spec(spec) {
                        MatchResult::MatchYes => return MatchResult::MatchYes,
                        MatchResult::MatchShrug => has_shrug = true,
                        MatchResult::MatchNo => {}
                    }
                }
                if has_shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::And(matchers) => {
                let mut has_shrug = false;
                for m in matchers {
                    match m.matches_spec(spec) {
                        MatchResult::MatchNo => return MatchResult::MatchNo,
                        MatchResult::MatchShrug => has_shrug = true,
                        MatchResult::MatchYes => {}
                    }
                }
                if has_shrug {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchYes
                }
            }
            Matcher::Not(m) => match m.matches_spec(spec) {
                MatchResult::MatchYes => MatchResult::MatchNo,
                MatchResult::MatchNo => MatchResult::MatchYes,
                MatchResult::MatchShrug => MatchResult::MatchShrug,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use crate::engine::provider::TargetSpec;
    use crate::htaddr::Addr;
    use crate::htmatcher::{MatchResult, Matcher};
    use crate::htpkg::PkgBuf;

    fn spec(pkg: &str, name: &str, labels: &[&str]) -> TargetSpec {
        TargetSpec {
            addr: Addr {
                package: PkgBuf::from(pkg),
                name: name.to_string(),
                args: HashMap::new(),
            },
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn addr_yes() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Addr(s.addr.clone());
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn addr_no() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Addr(Addr {
            package: PkgBuf::from("foo/bar"),
            name: "other".to_string(),
            args: HashMap::new(),
        });
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn package_yes() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Package(PkgBuf::from("foo/bar"));
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn package_no() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Package(PkgBuf::from("foo/other"));
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn package_prefix_yes() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::PackagePrefix(PkgBuf::from("foo"));
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn package_prefix_no() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::PackagePrefix(PkgBuf::from("other"));
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn label_yes() {
        let s = spec("foo/bar", "baz", &["//tag:release"]);
        let m = Matcher::Label(Addr {
            package: PkgBuf::from("tag"),
            name: "release".to_string(),
            args: HashMap::new(),
        });
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn label_no() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Label(Addr {
            package: PkgBuf::from("tag"),
            name: "release".to_string(),
            args: HashMap::new(),
        });
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn or_yes_short_circuits() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Package(PkgBuf::from("other")),
        ]);
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn or_all_no() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("a")),
            Matcher::Package(PkgBuf::from("b")),
        ]);
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn and_yes() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::PackagePrefix(PkgBuf::from("foo")),
        ]);
        assert_eq!(m.matches_spec(&s), MatchResult::MatchYes);
    }

    #[test]
    fn and_no_short_circuits() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Package(PkgBuf::from("other")),
        ]);
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);
    }

    #[test]
    fn not_inverts() {
        let s = spec("foo/bar", "baz", &[]);
        let m = Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("foo/bar"))));
        assert_eq!(m.matches_spec(&s), MatchResult::MatchNo);

        let m2 = Matcher::Not(Box::new(Matcher::Package(PkgBuf::from("other"))));
        assert_eq!(m2.matches_spec(&s), MatchResult::MatchYes);
    }
}
