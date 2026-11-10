//! `go_format` (`//pkg:format`) and `go_format_check` (`//pkg:format-check`)
//! when the formatter fails: the fixer is an `in_place` target, so the files it
//! formatted before failing are written back while the target stays red (D12
//! of the failure-outputs spec); the checker declares no outputs and never
//! writes.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use common::require_go;
use heph::engine::error::{FailureFixes, TargetFailure};
use heph::engine::{OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Unformatted, and sorted before the broken file: formatted before the
/// formatter dies.
const UNFORMATTED: &str = "package lib\n\nfunc  A()  int {return 1}\n";
const FORMATTED: &str = "package lib\n\nfunc A() int { return 1 }\n";
/// Sorted last, and not Go: `heph-govet -format` fails on it with exit 2.
const BROKEN: &str = "package lib\n\nfunc Broken( {\n";

/// Resolve `addr`, which must fail, and return the failures it recorded.
async fn resolve_failing(
    ws: &htestkit::Workspace,
    addr: &str,
) -> anyhow::Result<(String, Vec<Arc<TargetFailure>>)> {
    let rs = ws.engine.new_state();
    let res = ws
        .engine
        .clone()
        .result_addr(
            rs.clone(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    assert!(res.is_err(), "{addr} must fail");
    let error = res.err().map(|e| format!("{e:#}")).unwrap_or_default();
    Ok((error, rs.take_failures()))
}

/// Pin `path`'s mtime well in the past, so a rewrite of it cannot go unseen
/// within the filesystem's timestamp granularity.
fn age(path: &Path) -> anyhow::Result<SystemTime> {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(then)?;
    Ok(std::fs::metadata(path)?.modified()?)
}

/// A formatter that fails part-way still writes back what it formatted
/// before failing (D12): `a.go` is formatted, the target is red, and
/// `lib.go`, already formatted, is not rewritten, so its mtime is kept.
#[tokio::test]
async fn failure_writes_back_formatted_siblings() -> anyhow::Result<()> {
    require_go!();
    let dir = common::fixture("with_dep")?;
    let lib = dir.path().join("lib");
    std::fs::write(lib.join("a.go"), UNFORMATTED)?;
    std::fs::write(lib.join("z_bad.go"), BROKEN)?;
    let kept = age(&lib.join("lib.go"))?;
    let lib_go = std::fs::read_to_string(lib.join("lib.go"))?;
    let ws = common::make_workspace_tree_govet(dir)?;

    let (error, failures) = resolve_failing(&ws, "//lib:format").await?;

    assert_eq!(
        std::fs::read_to_string(lib.join("a.go"))?,
        FORMATTED,
        "the sibling formatted before the failure is written back: {error}"
    );
    assert_eq!(std::fs::read_to_string(lib.join("z_bad.go"))?, BROKEN);
    assert_eq!(std::fs::read_to_string(lib.join("lib.go"))?, lib_go);
    assert_eq!(
        std::fs::metadata(lib.join("lib.go"))?.modified()?,
        kept,
        "an unchanged file is not rewritten"
    );
    let fixes = failures
        .iter()
        .find(|f| f.addr.format() == "//lib:format")
        .map(|f| f.fixes.clone());
    let Some(Some(FailureFixes::Written { paths, .. })) = fixes else {
        panic!("//lib:format must record its written fixes, got {fixes:?}: {error}");
    };
    assert_eq!(paths, ["lib/a.go"]);
    Ok(())
}

/// The checker is unaffected: it reports the unformatted file and writes
/// nothing, on failure as on success.
#[tokio::test]
async fn check_never_writes() -> anyhow::Result<()> {
    require_go!();
    let dir = common::fixture("with_dep")?;
    let a = dir.path().join("lib/a.go");
    std::fs::write(&a, UNFORMATTED)?;
    let kept = age(&a)?;
    let ws = common::make_workspace_tree_govet(dir)?;

    let (error, failures) = resolve_failing(&ws, "//lib:format-check").await?;

    assert_eq!(
        std::fs::read_to_string(&a)?,
        UNFORMATTED,
        "the check never formats: {error}"
    );
    assert_eq!(std::fs::metadata(&a)?.modified()?, kept);
    assert!(
        failures.iter().all(|f| f.fixes.is_none()),
        "a check has no fixes to write: {:?}",
        failures.iter().map(|f| &f.fixes).collect::<Vec<_>>()
    );
    Ok(())
}
