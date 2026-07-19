mod common;

use common::require_go;

/// End-to-end lint against the *downloaded* heph-govet: the `http_fetch` target
/// fetches the released binary, the engine caches and stages it, and `_lint-analyze` execs
/// it as an `x/tools` unitchecker over a real package.
///
/// This is the shape that broke in production ("wait for heph-govet: Permission
/// denied"): nothing below the `_lint-analyze` target's own `run` was at fault — the bits
/// on the staged file are right — so the failure can only be reproduced by
/// actually exec'ing it, which is what this does.
#[tokio::test]
async fn test_lint_execs_the_downloaded_govet() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("with_dep")?)?;

    // `_lint-analyze` is the analyze unit: it runs heph-govet and writes facts + report.
    let result = ws.run("//lib:_lint-analyze").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce serialized facts, got {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("lint-report.json")),
        "analyze must produce a report, got {paths:?}"
    );
    Ok(())
}

/// Same for the format targets, which exec the same downloaded binary in its
/// `-format` mode: `format-check` reports, and must not rewrite anything.
#[tokio::test]
async fn test_format_check_execs_the_downloaded_govet() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("with_dep")?)?;

    // The fixture is gofmt-clean, so the check gate passes — the point is that it
    // gets far enough to actually run the tool.
    ws.run("//lib:format-check").await?;
    Ok(())
}

/// A dependent package: its dep's facts feed the analysis. A package that exports
/// no facts writes a **zero-byte** `lint.facts` (x/tools `Set.Encode` returns nil),
/// and consuming that must be a no-op, not a decode failure.
#[tokio::test]
async fn test_lint_consumes_a_deps_empty_facts() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("with_dep")?)?;

    // cmd imports lib, so lib's facts are wired into cmd's analysis.
    let result = ws.run("//cmd:_lint-analyze").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce facts, got {paths:?}"
    );
    Ok(())
}

/// `unsafe` is a pseudo-package: no archive, so `go list` never reports it as a lib
/// and it gets no `lib_*` dep group. unitchecker nonetheless resolves every import
/// through the cfg's ImportMap, so a package importing `unsafe` fails with
/// `could not import unsafe (can't resolve import "")` unless the driver maps it.
#[tokio::test]
async fn test_lint_package_importing_unsafe() -> anyhow::Result<()> {
    require_go!();
    let ws = common::make_workspace_host(common::fixture("lint_unsafe")?)?;

    let result = ws.run("//mem:_lint-analyze").await?;

    let paths = common::artifact_paths(&result);
    assert!(
        paths.iter().any(|p| p.ends_with("lint.facts")),
        "analyze must produce facts, got {paths:?}"
    );
    Ok(())
}
