//! The driver each Go target resolves to — the one place both `list` and the
//! spec builders read it from.
//!
//! `list` reports a target's driver so a walk for one driver's targets (`heph
//! auth` finding the workspace's `credential`s) drops every Go target without
//! resolving its spec, which for a Go target means running its package's
//! `_golist` first. The engine trusts that listing, so a spec builder never
//! spells a driver inline — it takes it from here, and [`listed`] answers from
//! the same constants. `heph validate` holds the two to each other, and
//! `go_list_labels_and_drivers_equal_spec` runs that check over the fixtures.

/// `_golist`.
pub(crate) const GOLIST: &str = "go_golist";
/// `build_lib`.
pub(crate) const COMPILE: &str = "go_compile";
/// The link of a binary (`build@v=…`) or a test binary (`build_test`,
/// `build_xtest`).
pub(crate) const LINK: &str = "bash";
/// The bare `build`: a group forwarding to the host variant's `build@v=…`.
pub(crate) const HOST_BUILD: &str = hbuiltins::plugingroup::DRIVER_NAME;
/// `lint`.
pub(crate) const LINT_FIX: &str = "go_lint_fix";
/// `lint-check`.
pub(crate) const LINT_GATE: &str = "go_lint_gate";
/// `format`.
pub(crate) const FORMAT: &str = "go_format";
/// `format-check`.
pub(crate) const FORMAT_CHECK: &str = "go_format_check";

/// The test runners (`test`, `xtest` and their race flavours): `exec` passes
/// argv literally, and a package with `pre_run` lines needs a shell to run
/// them first.
pub(crate) fn test_runner(pre_run: bool) -> &'static str {
    if pre_run { "bash" } else { "exec" }
}

/// Whether `name` is a test runner, whose driver [`listed`] needs the
/// package's test env to answer.
pub(crate) fn is_test_runner(name: &str) -> bool {
    matches!(name, "test" | "xtest" | "test_race" | "xtest_race")
}

/// The driver a first-party target `list` emits will resolve to.
///
/// A `build` with no args is the host-default group, the gate `get` uses (a
/// listed variant `build` always carries `v` and `vp`). `test_pre_run` is
/// whether the package's test env has `pre_run` lines, `None` when it could not
/// be read — `get` fails on that too, so the runner stays unknown and the walk
/// resolves it to report the error.
///
/// `None` for a name this table does not know, which `list` reports as
/// "unknown". Only first-party packages: a stdlib `build_lib` is a `bash`
/// target, and no workspace walk lists stdlib or third-party packages to check
/// a claim about them.
pub(crate) fn listed(
    addr: &hmodel::htaddr::Addr,
    test_pre_run: Option<bool>,
) -> Option<&'static str> {
    Some(match addr.name.as_str() {
        "_golist" => GOLIST,
        "build_lib" => COMPILE,
        "build" if addr.args.is_empty() => HOST_BUILD,
        "build" | "build_test" | "build_xtest" => LINK,
        name if is_test_runner(name) => test_runner(test_pre_run?),
        "lint" => LINT_FIX,
        "lint-check" => LINT_GATE,
        "format" => FORMAT,
        "format-check" => FORMAT_CHECK,
        _ => return None,
    })
}
