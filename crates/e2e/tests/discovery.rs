//! Keep-going discovery: a selector walk that cannot resolve part of the
//! workspace acts on the rest and says what it skipped.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use heph::engine::discovery::Stage;
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::{Discovery, Gaps, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn bash(addr: &str, run: &str, deps: &[&str], labels: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: if deps.is_empty() {
            HashMap::new()
        } else {
            HashMap::from([(
                String::new(),
                deps.iter().map(|d| (*d).to_string()).collect(),
            )])
        },
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        ..Default::default()
    }
}

/// The static targets, plus the two ways the go provider makes discovery fail:
/// `get` of `builds.0` builds `builds.1` first (resolving a package's targets
/// runs its `_golist`), and every addr in `broken` is listed but fails to
/// resolve.
fn workspace(
    targets: Vec<Target>,
    builds: Option<(&str, &str)>,
    broken: &[&str],
) -> anyhow::Result<htestkit::Workspace> {
    let faults = Faults {
        // Resolving through a build is go's `_golist`; a label walk only
        // reaches it when the listing leaves labels unknown.
        labels_unknown: builds.is_some(),
        builds: match builds {
            Some((t, b)) => vec![(parse_addr(t)?, parse_addr(b)?)],
            None => vec![],
        },
        broken: broken
            .iter()
            .map(|a| parse_addr(a))
            .collect::<anyhow::Result<_>>()?,
        ..Default::default()
    };
    let provider = FaultProvider::new(targets, faults)?;
    htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()
}

fn label_everywhere(l: &str) -> Matcher {
    Matcher::And(vec![
        Matcher::Label(l.to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ])
}

/// `heph r nix //...` where resolving `//a:t3`'s spec builds `//b:t2`, and
/// `//b:t2` fails. The label check cannot be decided for `t3`, so the walk
/// skips it and records why; the healthy `nix` target still runs, and the
/// target whose failure caused the skip is in the failure registry for the
/// boxes above the report.
#[tokio::test]
async fn run_keeps_going_past_a_candidate_that_cannot_resolve() -> anyhow::Result<()> {
    let ws = workspace(
        vec![
            bash("//a:t3", "echo t3 > $OUT", &[], &[]),
            bash("//b:t2", "exit 1", &[], &[]),
            bash("//ok:good", "echo good > $OUT", &[], &["nix"]),
        ],
        Some(("//a:t3", "//b:t2")),
        &[],
    )?;
    let gaps = Gaps::new("label(nix) && //...");
    let opts = ResultOptions {
        discovery: Discovery::KeepGoing(gaps.clone()),
        ..Default::default()
    };
    let rs = ws.engine.new_state_with_fail_fast(false);
    let batch = Arc::clone(&ws.engine)
        .result(
            rs.clone(),
            &label_everywhere("nix"),
            OutputMatcher::All,
            &opts,
        )
        .await?;

    assert_eq!(batch.ok.len(), 1, "the healthy `nix` target still ran");
    assert_eq!(common::artifact_string(&batch.ok[0]).trim(), "good");
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);

    let report = gaps.report();
    assert_eq!(report.skipped, 1, "{report:?}");
    assert_eq!(report.groups[0].stage, Stage::Spec);
    assert_eq!(report.groups[0].examples[0].scope, "//a:t3");

    let failed: Vec<String> = rs.take_failures().iter().map(|f| f.addr.format()).collect();
    assert!(failed.contains(&"//b:t2".to_string()), "{failed:?}");
    Ok(())
}

/// A broken root `BUILD` breaks every package's probe. That is one group with
/// an exact count, not a report per package, and not "no targets matched".
#[tokio::test]
async fn root_build_broken_reports_one_group() -> anyhow::Result<()> {
    let ws = common::Workspace::new();
    ws.write_build_file("", "this is not starlark (");
    ws.write_build_file("a", "target(name = 'x', driver = 'exec', run = ['true'])");
    ws.write_build_file("b", "target(name = 'y', driver = 'exec', run = ['true'])");

    let gaps = Gaps::new("//...");
    let opts = ResultOptions {
        discovery: Discovery::KeepGoing(gaps.clone()),
        ..Default::default()
    };
    let rs = ws.engine.new_state_with_fail_fast(false);
    let batch = Arc::clone(&ws.engine)
        .result(
            rs,
            &Matcher::PackagePrefix(PkgBuf::from("")),
            OutputMatcher::All,
            &opts,
        )
        .await?;
    assert!(batch.ok.is_empty());

    let report = gaps.report();
    assert_eq!(report.groups.len(), 1, "{report:?}");
    let group = &report.groups[0];
    assert_eq!(group.stage, Stage::Probe, "{report:?}");
    // Every package the walk listed, each counted once: at least the root,
    // `a` and `b` (built-in packages probe the root too and count with them).
    let scopes: Vec<&str> = group.examples.iter().map(|e| e.scope.as_str()).collect();
    for want in ["//", "//a", "//b"] {
        assert!(scopes.contains(&want), "{want} missing: {report:?}");
    }
    assert_eq!(group.count, scopes.len(), "{report:?}");
    assert_eq!(report.skipped, group.count);
    assert!(
        group.examples[0].cause.contains("Parse error"),
        "the root cause is what the line keeps: {:?}",
        group.examples[0].cause
    );
    assert!(
        heph::commands::errors::require_non_empty_unless_incomplete(batch.ok, &gaps).is_ok(),
        "an empty match with skips reports the skips, not the selector"
    );
    Ok(())
}

/// `validate`'s overlap check acts on what resolved: one broken candidate is
/// skipped and reported, and the overlap between the healthy ones is still
/// found.
#[tokio::test]
async fn validate_reports_skips_and_still_checks_the_rest() -> anyhow::Result<()> {
    let codegen = |addr: &str| Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some("echo x > gen.go".to_string()),
        out: HashMap::from([(String::new(), vec!["gen.go".to_string()])]),
        codegen: Some("copy".to_string()),
        ..Default::default()
    };
    let ws = workspace(
        vec![codegen("//p:a"), codegen("//p:b")],
        None,
        &["//broken:x"],
    )?;
    let gaps = Gaps::new("validate");
    let rs = ws.engine.new_state_with_fail_fast(false);
    let overlaps = Arc::clone(&ws.engine)
        .codegen_copy_overlaps(
            rs,
            &Matcher::TreeOutputTo(PkgBuf::from("")),
            Discovery::KeepGoing(gaps.clone()),
        )
        .await?;
    assert_eq!(overlaps.len(), 1, "the healthy overlap is still reported");
    let report = gaps.report();
    assert_eq!(report.skipped, 1, "{report:?}");
    assert_eq!(report.groups[0].examples[0].scope, "//broken:x");
    Ok(())
}

/// Ctrl-C after the walk already skipped something: the run exits as
/// cancelled, and the skips are not reported as an incomplete selection.
#[tokio::test]
async fn ctrl_c_with_skips_exits_as_cancelled() -> anyhow::Result<()> {
    let ws = workspace(
        vec![bash("//slow:t", "sleep 30; echo t > $OUT", &[], &["x"])],
        None,
        &["//broken:x"],
    )?;
    let gaps = Gaps::new("label(x) && //...");
    let opts = ResultOptions {
        discovery: Discovery::KeepGoing(gaps.clone()),
        ..Default::default()
    };
    let rs = ws.engine.new_state_with_fail_fast(false);
    let run = tokio::spawn({
        let (engine, rs, opts) = (Arc::clone(&ws.engine), Arc::clone(&rs), opts.clone());
        async move {
            let m = label_everywhere("x");
            engine
                .result(rs, &m, OutputMatcher::All, &opts)
                .await
                .map(|_| ())
        }
    });
    // `//broken` sorts first, so the skip lands before the slow target starts.
    tokio::time::timeout(Duration::from_secs(20), async {
        while gaps.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    rs.ctoken().cancel();

    let err = tokio::time::timeout(Duration::from_secs(20), run)
        .await??
        .expect_err("a cancelled run is not a successful one");
    // The engine half of the contract: the run comes back as a cancellation
    // even though the walk had recorded a skip. (`finalize!` turning that into
    // a cancelled exit with no report is covered in `src/commands/errors.rs`.)
    assert!(heph::commands::errors::is_cancelled(&err), "{err:#}");
    assert!(!gaps.is_empty(), "the skip was recorded before the cancel");
    Ok(())
}
