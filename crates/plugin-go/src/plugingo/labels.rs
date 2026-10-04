//! The labels each Go target carries — the one place both `list` and the spec
//! builders read them from.
//!
//! `list` reports a target's labels so a label selector can decide membership
//! without resolving the spec (which, for every Go target, means running its
//! package's `_golist` first). The engine holds `list` to that: a resolved spec
//! whose labels differ from the listed ones fails the run. So a spec builder
//! never spells a label inline — it takes the set from here, and [`listed`]
//! answers from the same constants.

/// Every compile: `build_lib` and the test-augmented libraries.
pub(crate) const BUILD: &[&str] = &["go-build"];
/// The ordinary test runners, `test` and `xtest`.
pub(crate) const TEST: &[&str] = &["test", "go-test"];
/// The race runners. Deliberately *not* [`TEST`]: `label(test)` keeps meaning
/// the ordinary suite, and a race build is several times slower.
pub(crate) const TEST_RACE: &[&str] = &["test-race", "go-test-race"];
/// The lint fixer owns the plain labels, plus `fix`.
pub(crate) const LINT: &[&str] = &["go-lint", "lint", "fix"];
/// The read-only lint checker.
pub(crate) const LINT_CHECK: &[&str] = &["go-lint-check", "lint-check"];
/// The format fixer owns the plain labels, plus `fix`.
pub(crate) const FORMAT: &[&str] = &["go-format", "format", "fix"];
/// The read-only format checker.
pub(crate) const FORMAT_CHECK: &[&str] = &["go-format-check", "format-check"];

pub(crate) fn owned(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| (*l).to_string()).collect()
}

/// The labels a target `list` emits under `name` will carry, for every
/// variant `list` emits it with. Only `race` changes a target's labels, and
/// `list` never emits a `race=1` addr: race mode is entered through the
/// `test_race`/`xtest_race` names, which have their own entry here.
///
/// `None` for a name whose labels this table does not know, which `list`
/// reports as "unknown" so the engine resolves the spec to decide. Every name
/// here is checked against its resolved spec by
/// `go_list_labels_equal_spec_labels`; a name it cannot reach stays out.
pub(crate) fn listed(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        // `download` is deliberately absent: it is only listed for a
        // third-party module root, which no workspace walk reaches to check.
        "_golist" | "build" | "build_test" | "build_xtest" => &[],
        "build_lib" => BUILD,
        "test" | "xtest" => TEST,
        "test_race" | "xtest_race" => TEST_RACE,
        "lint" => LINT,
        "lint-check" => LINT_CHECK,
        "format" => FORMAT,
        "format-check" => FORMAT_CHECK,
        _ => return None,
    })
}
