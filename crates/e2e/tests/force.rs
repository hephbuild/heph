//! `--force` on a selector run reaches every selected target, whichever path in
//! the request resolves it first.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use heph::engine::ResultOptions;
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;

fn bash(addr: &str, run: &str, deps: &[(&str, &str)], labels: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: deps
            .iter()
            .map(|(group, dep)| ((*group).to_string(), vec![(*dep).to_string()]))
            .collect(),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        ..Default::default()
    }
}

/// `heph r nix //... --force` over the chain `T1 (label nix) → T2 → T3`, where
/// resolving T3's spec builds T2. T1's cached output is stale and T2 fails on
/// it. The walk has to resolve T3's spec to check its label, so it reaches T1
/// as a dependency before it reaches it as a match. Forcing T1 only at the
/// top-level call left that first touch a stale cache hit: T2 failed, so T3's
/// spec failed, so the walk failed, and T1 was never re-run.
#[tokio::test]
async fn forced_selector_repairs_a_stale_target_that_discovery_depends_on() -> anyhow::Result<()> {
    // State T1 reads but its input hash does not cover, so its cache entry can
    // go stale — what an out-of-date nix target looks like from the engine.
    let state = tempfile::tempdir()?;
    let state_file = state.path().join("state");
    std::fs::write(&state_file, "stale")?;

    // Resolving `//a:t3` builds `//b:t2` first: the go provider's shape, where
    // resolving a package's targets runs that package's `_golist`.
    let provider = FaultProvider::new(
        vec![
            bash(
                "//nix:t1",
                &format!("cat '{}' > $OUT", state_file.display()),
                &[],
                &["nix"],
            ),
            bash(
                "//b:t2",
                "grep -q fresh $SRC_T1 && echo ok > $OUT",
                &[("t1", "//nix:t1")],
                &[],
            ),
            bash("//a:t3", "echo t3 > $OUT", &[], &[]),
        ],
        Faults {
            builds: vec![(parse_addr("//a:t3")?, parse_addr("//b:t2")?)],
            ..Default::default()
        },
    )?;
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()?;

    // Cache T1 as stale, then move the state on under it.
    drop(ws.run("//nix:t1").await?);
    std::fs::write(&state_file, "fresh")?;

    let nix = Matcher::And(vec![
        Matcher::Label("nix".to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ]);
    let forced = ResultOptions {
        force: true,
        ..Default::default()
    };
    let results = ws.run_matcher_with(&nix, &forced).await?;
    assert_eq!(results.len(), 1, "only T1 carries the label");
    assert_eq!(
        common::artifact_string(&results[0]).trim(),
        "fresh",
        "T1 was served from cache instead of re-run"
    );
    Ok(())
}
