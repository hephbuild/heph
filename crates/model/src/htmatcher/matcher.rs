use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MatchResult {
    MatchYes,
    MatchNo,
    MatchShrug,
}

/// Matcher predicates over a target. `TreeOutputTo(pkg)` selects targets whose
/// codegen-tree outputs land inside (or contain) the package `pkg` — mirrors
/// heph's `CodegenPackage` (see `internal/tmatch/match.go::MatchDef`). The
/// argument is a package path (e.g. `cmd/foo/gen`), **not** an output group
/// name. Resolution requires the target's `def` because the output paths and
/// their `codegen_tree` modes are only known after `Driver::parse`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Matcher {
    Addr(Addr),
    Label(String),
    Package(PkgBuf),
    PackagePrefix(PkgBuf),
    TreeOutputTo(PkgBuf),
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
            Matcher::TreeOutputTo(matcher_pkg) => {
                // Cheap addr-only reject: the codegen tree of a target at pkg
                // `def_pkg` lands under `def_pkg`, so the matcher's package
                // and the target's package must lie on the same root path.
                // If neither is a prefix of the other, no output can match.
                // Otherwise we need the def to inspect output paths.
                if addr.package.has_prefix(matcher_pkg) || matcher_pkg.has_prefix(&addr.package) {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
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
    use std::collections::BTreeMap;

    fn addr(pkg: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
    }

    #[test]
    fn addr_exact_match() {
        let a = addr("foo/bar", "baz");
        assert_eq!(
            Matcher::Addr(a.clone()).matches_addr(&a),
            MatchResult::MatchYes
        );
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
        assert_eq!(
            Matcher::Label("my_label".to_string()).matches_addr(&a),
            MatchResult::MatchShrug
        );
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
        let m = Matcher::Or(vec![
            Matcher::Package(PkgBuf::from("other")),
            Matcher::Label("lbl".to_string()),
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
        let m = Matcher::And(vec![
            Matcher::Package(PkgBuf::from("foo/bar")),
            Matcher::Label("lbl".to_string()),
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
        assert_eq!(
            Matcher::Not(Box::new(Matcher::Label("lbl".to_string()))).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn exclude_composition_drops_specific_addr_in_package() {
        // Mirrors the CLI -e contract: `And([Package(p), Not(Addr(a))])`
        // includes targets in p but excludes a specifically.
        let pkg = PkgBuf::from("foo");
        let bad = addr("foo", "bad");
        let good = addr("foo", "good");
        let outside = addr("other", "good");

        let m = Matcher::And(vec![
            Matcher::Package(pkg),
            Matcher::Not(Box::new(Matcher::Addr(bad.clone()))),
        ]);

        assert_eq!(m.matches_addr(&bad), MatchResult::MatchNo);
        assert_eq!(m.matches_addr(&good), MatchResult::MatchYes);
        assert_eq!(m.matches_addr(&outside), MatchResult::MatchNo);
    }

    #[test]
    fn tree_output_to_addr_shrugs_when_packages_overlap() {
        // matcher pkg `foo/gen` and target at pkg `foo` may or may not match
        // — need def to inspect outputs.
        let a = addr("foo", "bar");
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo/gen")).matches_addr(&a),
            MatchResult::MatchShrug
        );
        // matcher pkg under target's pkg: also possible.
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("foo")).matches_addr(&a),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn tree_output_to_addr_no_when_packages_unrelated() {
        // matcher pkg `bar` cannot be reached by codegen of target at `foo`.
        let a = addr("foo", "bar");
        assert_eq!(
            Matcher::TreeOutputTo(PkgBuf::from("bar")).matches_addr(&a),
            MatchResult::MatchNo
        );
    }
}
