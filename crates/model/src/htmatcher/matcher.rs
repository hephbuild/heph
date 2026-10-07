use crate::htaddr::Addr;
use crate::htmatcher::ListedFacts;
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
    /// The spec's driver, compared whole and case-sensitively.
    Driver(String),
    Package(PkgBuf),
    PackagePrefix(PkgBuf),
    TreeOutputTo(PkgBuf),
    Or(Vec<Matcher>),
    And(Vec<Matcher>),
    Not(Box<Matcher>),
}

impl Matcher {
    /// Whether package `pkg` can hold a match, decided from the package path
    /// alone — no `list`, no `probe`, no spec or def resolution.
    ///
    /// The tri-state is about the *whole package*, not one target:
    /// `MatchNo` — no addr in `pkg` can match, so the package can be skipped
    /// outright; `MatchYes` — every addr in it matches; `MatchShrug` — it
    /// depends on the individual target.
    ///
    /// Only `MatchNo` is load-bearing: callers use it to prune a package
    /// before paying for it, and every arm that cannot decide shrugs, so an
    /// over-inclusive answer costs time and never correctness.
    pub fn matches_pkg(&self, pkg: &PkgBuf) -> MatchResult {
        match self {
            // One addr out of the package matches — never all of them.
            Matcher::Addr(a) => {
                if &a.package == pkg {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            // Labels and drivers live on the target, not the package.
            Matcher::Label(_) | Matcher::Driver(_) => MatchResult::MatchShrug,
            // Same reasoning as `matches_addr`: a codegen tree rooted at the
            // target's package can only reach packages on the same root path.
            Matcher::TreeOutputTo(matcher_pkg) => {
                if pkg.has_prefix(matcher_pkg) || matcher_pkg.has_prefix(pkg) {
                    MatchResult::MatchShrug
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Package(p) => {
                if pkg == p {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::PackagePrefix(prefix) => {
                if pkg.has_prefix(prefix) {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Or(matchers) => {
                let mut shrug = false;
                for m in matchers {
                    match m.matches_pkg(pkg) {
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
                    match m.matches_pkg(pkg) {
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
            // Sound under the all/none reading: "no addr matches" negates to
            // "every addr matches", and vice versa.
            Matcher::Not(m) => match m.matches_pkg(pkg) {
                MatchResult::MatchYes => MatchResult::MatchNo,
                MatchResult::MatchNo => MatchResult::MatchYes,
                MatchResult::MatchShrug => MatchResult::MatchShrug,
            },
        }
    }

    pub fn matches_addr(&self, addr: &Addr) -> MatchResult {
        self.matches_listed(addr, &ListedFacts::UNKNOWN)
    }

    /// Whether evaluating `self` can read a listed fact. A walk merges listings
    /// only for a matcher that will act on them — an `Addr` clone and a map
    /// entry per candidate otherwise bought nothing.
    pub fn reads_listed_facts(&self) -> bool {
        match self {
            Matcher::Label(_) | Matcher::Driver(_) | Matcher::TreeOutputTo(_) => true,
            Matcher::Or(ms) | Matcher::And(ms) => ms.iter().any(Matcher::reads_listed_facts),
            Matcher::Not(m) => m.reads_listed_facts(),
            Matcher::Addr(_) | Matcher::Package(_) | Matcher::PackagePrefix(_) => false,
        }
    }

    /// [`matches_addr`](Self::matches_addr), plus the facts the providers'
    /// `list` reported for the target, merged across every lister of its name.
    ///
    /// A known field decides its arm instead of shrugging: `Label` and `Driver`
    /// decide Yes or No, `TreeOutputTo` decides No from `has_codegen == false`.
    /// With every field unknown it is exactly `matches_addr`. The facts are
    /// conditional on the target existing, so a Yes is a candidate and a No is
    /// final — `Not` needs no special case, since both are exact for this one
    /// target.
    pub fn matches_listed(&self, addr: &Addr, facts: &ListedFacts) -> MatchResult {
        match self {
            Matcher::Addr(a) => {
                if a == addr {
                    MatchResult::MatchYes
                } else {
                    MatchResult::MatchNo
                }
            }
            Matcher::Label(l) => match facts.labels() {
                Some(labels) if labels.contains(l) => MatchResult::MatchYes,
                Some(_) => MatchResult::MatchNo,
                None => MatchResult::MatchShrug,
            },
            // Whole string, case-sensitive. A listed driver is never empty, so
            // `driver("")` is No against every known one.
            Matcher::Driver(d) => match facts.driver() {
                Some(driver) if driver == d => MatchResult::MatchYes,
                Some(_) => MatchResult::MatchNo,
                None => MatchResult::MatchShrug,
            },
            Matcher::TreeOutputTo(matcher_pkg) => {
                // A target with no codegen output writes no tree anywhere.
                if facts.has_codegen() == Some(false) {
                    return MatchResult::MatchNo;
                }
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
                    match m.matches_listed(addr, facts) {
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
                    match m.matches_listed(addr, facts) {
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
            Matcher::Not(m) => match m.matches_listed(addr, facts) {
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

    /// Listed labels decide `Label` and every combinator over it; without them
    /// each case shrugs exactly as `matches_addr` does.
    #[test]
    fn label_matcher_decides_from_listed_labels() {
        use MatchResult::{MatchNo as No, MatchShrug as Shrug, MatchYes as Yes};
        let a = addr("foo/bar", "baz");
        let label = |l: &str| Matcher::Label(l.to_string());
        let tree = Matcher::TreeOutputTo(PkgBuf::from("foo"));
        let listed = ListedFacts::default().with_labels(["test", "go-test"]);
        let unknown = ListedFacts::default();
        let empty = ListedFacts::default().with_labels(Vec::<String>::new());

        let cases = [
            (label("test"), Yes, Shrug),
            (label("lint"), No, Shrug),
            // Exact label, not a prefix or substring of one.
            (label("tes"), No, Shrug),
            (label("go-test-race"), No, Shrug),
            (
                Matcher::And(vec![label("test"), label("go-test")]),
                Yes,
                Shrug,
            ),
            (Matcher::And(vec![label("test"), label("lint")]), No, Shrug),
            (
                Matcher::Or(vec![label("lint"), label("go-test")]),
                Yes,
                Shrug,
            ),
            (Matcher::Or(vec![label("lint"), label("fix")]), No, Shrug),
            (Matcher::Not(Box::new(label("test"))), No, Shrug),
            (Matcher::Not(Box::new(label("lint"))), Yes, Shrug),
            // A def-level predicate still shrugs; the label half only decides
            // when it settles the combinator alone.
            (
                Matcher::And(vec![label("test"), tree.clone()]),
                Shrug,
                Shrug,
            ),
            (Matcher::And(vec![label("lint"), tree.clone()]), No, Shrug),
            (Matcher::Or(vec![label("test"), tree]), Yes, Shrug),
            // The addr half still decides on its own.
            (
                Matcher::And(vec![label("test"), Matcher::Package(PkgBuf::from("x"))]),
                No,
                No,
            ),
        ];
        for (m, with, without) in cases {
            assert_eq!(m.matches_listed(&a, &listed), with, "{m:?} listed");
            assert_eq!(m.matches_listed(&a, &unknown), without, "{m:?} unlisted");
            assert_eq!(m.matches_addr(&a), without, "{m:?} addr-only");
        }
        // Known and empty is a decision, not a shrug.
        assert_eq!(label("test").matches_listed(&a, &empty), No);
    }

    fn facts(labels: Option<&[&str]>, driver: Option<&str>, codegen: Option<bool>) -> ListedFacts {
        let mut f = ListedFacts::default();
        if let Some(l) = labels {
            f = f.with_labels(l.iter().copied());
        }
        if let Some(d) = driver {
            f = f.with_driver(d);
        }
        if let Some(c) = codegen {
            f = f.with_has_codegen(c);
        }
        f
    }

    fn driver(d: &str) -> Matcher {
        Matcher::Driver(d.to_string())
    }

    fn label(l: &str) -> Matcher {
        Matcher::Label(l.to_string())
    }

    fn not(m: Matcher) -> Matcher {
        Matcher::Not(Box::new(m))
    }

    /// C6 and the adversarial rows: a driver is compared whole and
    /// case-sensitively, never by prefix.
    #[test]
    fn driver_compares_whole_string() {
        use MatchResult::{MatchNo as No, MatchShrug as Shrug, MatchYes as Yes};
        let a = addr("pkg", "lib");
        let f = facts(Some(&[]), Some("go_compile"), Some(false));
        assert_eq!(driver("go_compile").matches_listed(&a, &f), Yes);
        assert_eq!(driver("go").matches_listed(&a, &f), No);
        assert_eq!(driver("GO_COMPILE").matches_listed(&a, &f), No);
        assert_eq!(driver("go_compile_x").matches_listed(&a, &f), No);
        let bash = facts(None, Some("bash"), None);
        assert_eq!(driver("Bash").matches_listed(&a, &bash), No);
        assert_eq!(
            driver("bash").matches_listed(&a, &ListedFacts::UNKNOWN),
            Shrug
        );
        // Labels too: `test` is not a prefix match for `test-race`.
        let race = facts(Some(&["test-race"]), None, None);
        assert_eq!(label("test").matches_listed(&a, &race), No);
    }

    /// C34: a listed `""` is unknown, so `driver("")` shrugs there and is No
    /// against every known driver.
    #[test]
    fn driver_empty_matches_nothing() {
        use MatchResult::{MatchNo as No, MatchShrug as Shrug};
        let a = addr("pkg", "lib");
        let listed_empty = ListedFacts::default().with_driver("");
        assert_eq!(listed_empty.driver(), None);
        assert_eq!(driver("").matches_listed(&a, &listed_empty), Shrug);
        assert_eq!(
            driver("").matches_listed(&a, &facts(None, Some("bash"), None)),
            No
        );
        // `label("")` is an exact compare: a listed `[""]` has it.
        let empty_label = facts(Some(&[""]), None, None);
        assert_eq!(
            label("").matches_listed(&a, &empty_label),
            MatchResult::MatchYes
        );
        assert_eq!(
            label("").matches_listed(&a, &facts(Some(&[]), None, None)),
            No
        );
    }

    /// C9, C10: `has_codegen == false` is a No; anything else leaves the addr
    /// check, then the def, to decide. Never a Yes.
    #[test]
    fn tree_output_to_reads_has_codegen() {
        use MatchResult::{MatchNo as No, MatchShrug as Shrug};
        let a = addr("pkg", "lib");
        let m = Matcher::TreeOutputTo(PkgBuf::from(""));
        assert_eq!(m.matches_listed(&a, &facts(None, None, Some(false))), No);
        assert_eq!(m.matches_listed(&a, &facts(None, None, Some(true))), Shrug);
        assert_eq!(m.matches_listed(&a, &ListedFacts::UNKNOWN), Shrug);
        // The addr check still says No on its own.
        let far = Matcher::TreeOutputTo(PkgBuf::from("other"));
        assert_eq!(far.matches_listed(&a, &facts(None, None, Some(true))), No);
    }

    /// C11–C15 and the empty-combinator rows: `And`/`Or`/`Not` fold
    /// three-valued per entry.
    #[test]
    fn boolean_folds_over_listed_facts() {
        use MatchResult::{MatchNo as No, MatchShrug as Shrug, MatchYes as Yes};
        let a = addr("pkg", "x");
        let lib = facts(Some(&[]), Some("go_compile"), Some(false));
        let test = facts(Some(&["test", "go-test"]), Some("exec"), Some(false));
        let format = facts(Some(&["format", "fix"]), Some("go_format"), None);
        let test_labels_unknown = facts(None, Some("exec"), Some(false));
        let lib_driver_unknown = facts(Some(&[]), None, Some(false));

        let c11 = Matcher::And(vec![label("test"), driver("exec")]);
        assert_eq!(c11.matches_listed(&a, &test), Yes);
        assert_eq!(c11.matches_listed(&a, &lib), No);
        assert_eq!(c11.matches_listed(&a, &format), No);
        // C12: the driver decides the `And` for the others.
        assert_eq!(c11.matches_listed(&a, &test_labels_unknown), Shrug);
        let c13 = Matcher::Or(vec![label("deploy"), driver("credential")]);
        for f in [&lib, &test, &format] {
            assert_eq!(c13.matches_listed(&a, f), No);
        }
        let c14 = not(label("test"));
        assert_eq!(c14.matches_listed(&a, &lib), Yes);
        assert_eq!(c14.matches_listed(&a, &format), Yes);
        assert_eq!(c14.matches_listed(&a, &test), No);
        let c15 = not(driver("credential"));
        assert_eq!(c15.matches_listed(&a, &lib_driver_unknown), Shrug);
        assert_eq!(c15.matches_listed(&a, &test), Yes);

        assert_eq!(Matcher::And(vec![]).matches_listed(&a, &lib), Yes);
        assert_eq!(Matcher::Or(vec![]).matches_listed(&a, &lib), No);
        assert_eq!(not(Matcher::And(vec![])).matches_listed(&a, &lib), No);
    }

    /// C18: only a matcher with a fact-reading arm makes the walk merge.
    #[test]
    fn fact_free_matcher_reads_no_facts() {
        let a = addr("pkg", "x");
        for m in [
            Matcher::PackagePrefix(PkgBuf::from("")),
            Matcher::Package(PkgBuf::from("pkg")),
            Matcher::Addr(a.clone()),
            not(Matcher::And(vec![Matcher::Addr(a.clone())])),
        ] {
            assert!(!m.reads_listed_facts(), "{m:?}");
        }
        for m in [
            label("x"),
            driver("x"),
            Matcher::TreeOutputTo(PkgBuf::from("")),
            not(Matcher::Or(vec![Matcher::Addr(a.clone()), driver("x")])),
        ] {
            assert!(m.reads_listed_facts(), "{m:?}");
        }
    }

    /// The same recursion as `matches_pkg`: a matcher nested 1,000 deep
    /// evaluates without a special bound.
    #[test]
    fn deeply_nested_matcher_evaluates() {
        let a = addr("pkg", "x");
        let mut m = label("test");
        for _ in 0..1_000 {
            m = not(m);
        }
        let f = facts(Some(&["test"]), None, None);
        assert_eq!(m.matches_listed(&a, &f), MatchResult::MatchYes);
        assert_eq!(m.matches_pkg(&PkgBuf::from("pkg")), MatchResult::MatchShrug);
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

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    #[test]
    fn pkg_package_and_prefix_decide_outright() {
        assert_eq!(
            Matcher::Package(pkg("foo/bar")).matches_pkg(&pkg("foo/bar")),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::Package(pkg("foo/bar")).matches_pkg(&pkg("foo")),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::PackagePrefix(pkg("foo")).matches_pkg(&pkg("foo/bar")),
            MatchResult::MatchYes
        );
        assert_eq!(
            Matcher::PackagePrefix(pkg("foo")).matches_pkg(&pkg("foobar")),
            MatchResult::MatchNo
        );
        assert_eq!(
            Matcher::PackagePrefix(pkg("")).matches_pkg(&pkg("anything")),
            MatchResult::MatchYes
        );
    }

    #[test]
    fn pkg_addr_prunes_every_other_package() {
        let m = Matcher::Addr(addr("foo/bar", "baz"));
        // Its own package holds one match out of possibly many targets.
        assert_eq!(m.matches_pkg(&pkg("foo/bar")), MatchResult::MatchShrug);
        // Nothing else can hold it — not even a parent or child package.
        assert_eq!(m.matches_pkg(&pkg("foo")), MatchResult::MatchNo);
        assert_eq!(m.matches_pkg(&pkg("foo/bar/deep")), MatchResult::MatchNo);
        assert_eq!(m.matches_pkg(&pkg("other")), MatchResult::MatchNo);
    }

    #[test]
    fn pkg_label_shrugs_everywhere() {
        assert_eq!(
            Matcher::Label("lint".to_string()).matches_pkg(&pkg("foo")),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn pkg_and_prunes_on_the_deciding_arm() {
        // The motivating query: `label(lint) && //foo/...`. The label arm can
        // never prune, but the package arm must still prune the whole graph
        // outside `foo` — regardless of which arm is written first.
        let m = Matcher::And(vec![
            Matcher::Label("lint".to_string()),
            Matcher::PackagePrefix(pkg("foo")),
        ]);
        assert_eq!(m.matches_pkg(&pkg("bar")), MatchResult::MatchNo);
        assert_eq!(m.matches_pkg(&pkg("foo/deep")), MatchResult::MatchShrug);

        let flipped = Matcher::And(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::Label("lint".to_string()),
        ]);
        assert_eq!(flipped.matches_pkg(&pkg("bar")), MatchResult::MatchNo);
        assert_eq!(
            flipped.matches_pkg(&pkg("foo/deep")),
            MatchResult::MatchShrug
        );
    }

    #[test]
    fn pkg_and_yes_only_when_every_arm_is_yes() {
        let m = Matcher::And(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::Package(pkg("foo/bar")),
        ]);
        assert_eq!(m.matches_pkg(&pkg("foo/bar")), MatchResult::MatchYes);
        assert_eq!(m.matches_pkg(&pkg("foo/other")), MatchResult::MatchNo);
    }

    #[test]
    fn pkg_or_keeps_the_union() {
        let m = Matcher::Or(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::PackagePrefix(pkg("bar")),
        ]);
        assert_eq!(m.matches_pkg(&pkg("foo/x")), MatchResult::MatchYes);
        assert_eq!(m.matches_pkg(&pkg("bar/y")), MatchResult::MatchYes);
        assert_eq!(m.matches_pkg(&pkg("baz")), MatchResult::MatchNo);

        // One unknowable arm makes the whole union unknowable, never prunable.
        let with_label = Matcher::Or(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::Label("lint".to_string()),
        ]);
        assert_eq!(with_label.matches_pkg(&pkg("baz")), MatchResult::MatchShrug);
    }

    #[test]
    fn pkg_not_inverts_all_and_none() {
        // `!//foo/...` prunes exactly the foo subtree.
        let m = Matcher::Not(Box::new(Matcher::PackagePrefix(pkg("foo"))));
        assert_eq!(m.matches_pkg(&pkg("foo/x")), MatchResult::MatchNo);
        assert_eq!(m.matches_pkg(&pkg("bar")), MatchResult::MatchYes);

        // `!label(x)` is still per-target, so it prunes nothing.
        let l = Matcher::Not(Box::new(Matcher::Label("x".to_string())));
        assert_eq!(l.matches_pkg(&pkg("foo")), MatchResult::MatchShrug);

        // `!//foo:bar` must NOT prune `foo` — the package's other targets match.
        let a = Matcher::Not(Box::new(Matcher::Addr(addr("foo", "bar"))));
        assert_eq!(a.matches_pkg(&pkg("foo")), MatchResult::MatchShrug);
        assert_eq!(a.matches_pkg(&pkg("other")), MatchResult::MatchYes);
    }

    #[test]
    fn pkg_tree_output_to_keeps_the_whole_root_path() {
        let m = Matcher::TreeOutputTo(pkg("foo/gen"));
        // A target in an ancestor package can codegen down into `foo/gen`.
        assert_eq!(m.matches_pkg(&pkg("foo")), MatchResult::MatchShrug);
        assert_eq!(m.matches_pkg(&pkg("foo/gen/deep")), MatchResult::MatchShrug);
        assert_eq!(m.matches_pkg(&pkg("bar")), MatchResult::MatchNo);
    }

    #[test]
    fn pkg_never_prunes_a_package_holding_an_addr_level_match() {
        // Exhaustive consistency check: if any addr in a package matches,
        // `matches_pkg` must not answer MatchNo for that package.
        let pkgs = ["", "foo", "foo/bar", "foo/bar/baz", "other"];
        let names = ["a", "b"];
        let matchers = vec![
            Matcher::Addr(addr("foo/bar", "a")),
            Matcher::Label("l".to_string()),
            Matcher::Package(pkg("foo/bar")),
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::TreeOutputTo(pkg("foo/bar")),
            Matcher::And(vec![
                Matcher::Label("l".to_string()),
                Matcher::PackagePrefix(pkg("foo")),
            ]),
            Matcher::Or(vec![
                Matcher::Package(pkg("other")),
                Matcher::Addr(addr("foo", "b")),
            ]),
            Matcher::Not(Box::new(Matcher::Addr(addr("foo", "a")))),
            Matcher::Not(Box::new(Matcher::PackagePrefix(pkg("foo")))),
            Matcher::And(vec![
                Matcher::PackagePrefix(pkg("foo")),
                Matcher::Not(Box::new(Matcher::Package(pkg("foo/bar")))),
            ]),
        ];

        for m in &matchers {
            for p in pkgs {
                if m.matches_pkg(&pkg(p)) != MatchResult::MatchNo {
                    continue;
                }
                for n in names {
                    assert_eq!(
                        m.matches_addr(&addr(p, n)),
                        MatchResult::MatchNo,
                        "matcher {m:?} pruned package {p:?} but //{p}:{n} is not a definite non-match",
                    );
                }
            }
        }
    }
}
