//! What each Go target carries that `list` can report without `get` — the one
//! place both `list` and the spec builders read labels and driver names from.
//!
//! `list` reports a target's labels, driver and `has_codegen` so a selector
//! can decide without resolving the spec (which, for every Go target, means
//! running its package's `_golist` first). The engine trusts a listed No, and
//! `heph validate` holds every known field to the resolved spec and def. So a
//! spec builder never spells a label or a driver inline — it takes it from
//! here, and [`facts`] answers from the same constants.

use hplugin::provider::ListedFacts;

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

/// `_golist`.
pub(crate) const DRIVER_GOLIST: &str = "go_golist";
/// A first- or third-party library compile.
pub(crate) const DRIVER_COMPILE: &str = "go_compile";
/// The stdlib build, a binary link, a test-binary link, and a test run with a
/// `pre_run`.
pub(crate) const DRIVER_BASH: &str = "bash";
/// A test run without a `pre_run`: the test binary as a literal argv.
pub(crate) const DRIVER_EXEC: &str = "exec";
/// `lint`.
pub(crate) const DRIVER_LINT_FIX: &str = "go_lint_fix";
/// `lint-check`.
pub(crate) const DRIVER_LINT_GATE: &str = "go_lint_gate";
/// `format`.
pub(crate) const DRIVER_FORMAT: &str = "go_format";
/// `format-check`.
pub(crate) const DRIVER_FORMAT_CHECK: &str = "go_format_check";

pub(crate) fn owned(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| (*l).to_string()).collect()
}

/// What `list` knows about one listed name before any `_golist` has run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ListContext {
    /// The package is the standard library's: `build_lib` is a `bash` build,
    /// not a `go_compile`.
    pub(crate) stdlib: bool,
    /// The driver of `test`/`xtest` and their race flavours under this
    /// package's state chain, or `None` when the chain's `test` state does not
    /// parse (the entry then takes the resolve-the-spec path).
    pub(crate) test_driver: Option<&'static str>,
}

/// The driver a test run gets: `bash` when the chain sets a `pre_run`, so its
/// lines run as shell first, `exec` otherwise.
pub(crate) fn test_driver(has_pre_run: bool) -> &'static str {
    if has_pre_run {
        DRIVER_BASH
    } else {
        DRIVER_EXEC
    }
}

/// The facts a target `list` emits under `name` will carry, if it resolves.
/// `bare` is whether the addr has no args: the bare `build` is a `group`
/// forwarding to a host variant, every `build@<variant>` a `bash` link.
///
/// Only `race` changes a target's labels, and `list` never emits a `race=1`
/// addr: race mode is entered through the `test_race`/`xtest_race` names. A
/// name this table does not know lists with every field unknown, which the
/// engine resolves. Every known field is checked against the resolved spec and
/// def by `go_listed_facts_equal_spec_and_def`; a name it cannot reach
/// (`download`, only listed at a third-party module root) stays unknown.
pub(crate) fn facts(name: &str, bare: bool, cx: ListContext) -> ListedFacts {
    let (labels, driver, has_codegen): (&[&str], Option<&str>, Option<bool>) = match name {
        "_golist" => (&[], Some(DRIVER_GOLIST), Some(false)),
        "build_lib" if cx.stdlib => (BUILD, Some(DRIVER_BASH), Some(false)),
        "build_lib" => (BUILD, Some(DRIVER_COMPILE), Some(false)),
        "build" if bare => (&[], Some(hbuiltins::plugingroup::DRIVER_NAME), Some(false)),
        "build" | "build_test" | "build_xtest" => (&[], Some(DRIVER_BASH), Some(false)),
        "test" | "xtest" => (TEST, cx.test_driver, Some(false)),
        "test_race" | "xtest_race" => (TEST_RACE, cx.test_driver, Some(false)),
        // `format` and `lint` write the package's Go files in place, and only
        // `_golist` knows which files those are: codegen stays unknown.
        "lint" => (LINT, Some(DRIVER_LINT_FIX), None),
        "lint-check" => (LINT_CHECK, Some(DRIVER_LINT_GATE), Some(false)),
        "format" => (FORMAT, Some(DRIVER_FORMAT), None),
        "format-check" => (FORMAT_CHECK, Some(DRIVER_FORMAT_CHECK), Some(false)),
        _ => return ListedFacts::default(),
    };
    let mut facts = ListedFacts::default().with_labels(labels.iter().copied());
    if let Some(driver) = driver {
        facts = facts.with_driver(driver);
    }
    if let Some(has_codegen) = has_codegen {
        facts = facts.with_has_codegen(has_codegen);
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing here reads the host: an addr's facts are the same whatever
    /// `goos`/`goarch` lists it, because no input to [`facts`] is one.
    #[test]
    fn go_listed_facts_host_independent() {
        let names = [
            "_golist",
            "build_lib",
            "build",
            "build_test",
            "build_xtest",
            "test",
            "xtest",
            "test_race",
            "xtest_race",
            "lint",
            "lint-check",
            "format",
            "format-check",
            "download",
        ];
        for stdlib in [false, true] {
            for test_driver in [None, Some(DRIVER_EXEC), Some(DRIVER_BASH)] {
                let cx = ListContext {
                    stdlib,
                    test_driver,
                };
                for name in names {
                    for bare in [false, true] {
                        // Two evaluations, as two hosts would make them.
                        assert_eq!(facts(name, bare, cx), facts(name, bare, cx), "{name}");
                    }
                }
            }
        }
        let cx = ListContext {
            stdlib: false,
            test_driver: Some(DRIVER_EXEC),
        };
        assert!(facts("download", true, cx).is_unknown());
        assert_eq!(facts("format", true, cx).has_codegen(), None);
        assert_eq!(facts("test", false, cx).driver(), Some("exec"));
        assert_eq!(facts("build", true, cx).driver(), Some("group"));
        assert_eq!(facts("build", false, cx).driver(), Some("bash"));
    }
}
