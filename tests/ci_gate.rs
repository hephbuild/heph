//! Guards on the CI gate — which pull requests get a build at all.
//!
//! `heph.yml` deliberately does not build a stacked PR (one whose base is
//! another PR's branch rather than the default branch) unless it carries the
//! `ci/force-ci` label. That saves a deep stack's CI cost times its depth, and
//! it is safe for one reason and one reason only: the `master` repository
//! ruleset — where the required status checks live — has condition
//! `~DEFAULT_BRANCH`, so it applies to exactly the PRs whose base is master,
//! which is exactly the set the gate lets through. Nothing reaches master
//! without first being a PR that targets master, and those always build.
//!
//! Every assertion below protects a way that reasoning can be broken silently,
//! in the direction where nothing goes red:
//!
//!   - a new job added with no `needs:` builds on every stacked PR forever,
//!     and no check ever complains;
//!   - dropping `labeled` from the trigger list leaves `ci/force-ci` inert —
//!     the label is accepted, and simply never starts a run;
//!   - dropping `edited` reopens the one seam in the argument above. A job
//!     skipped by an `if:` concludes `skipped`, which branch protection reads
//!     as a pass. GitHub retargets a child PR at master by itself when the
//!     base merges, and that retarget fires no `synchronize` — so without
//!     `edited` the child sits at `master` carrying the skipped checks from
//!     its stacked days, mergeable over content nothing ever built;
//!   - a `branches:` filter on `pull_request:` would skip stacked PRs at the
//!     *workflow* level, which publishes no checks at all rather than skipped
//!     ones. An empty check list reads like a pass and no label can override
//!     it. That has already shipped once (#240).
//!
//! What this cannot cover: whether the ruleset still targets only the default
//! branch. That lives in repo settings, outside this tree. If required checks
//! are ever extended to non-default branches, stacked PRs become unmergeable
//! and the gate has to go with it.
//!
//! Cheap on purpose: one file read and some string work, no build.

use std::collections::{HashMap, HashSet};
use std::path::Path;

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/heph.yml");
    std::fs::read_to_string(&path).expect("read .github/workflows/heph.yml")
}

/// `(job id, job body)` for every top-level job, in file order.
///
/// A job's *leading* comment block lands in the body of the job above it, so
/// content assertions run over [`without_comments`].
fn workflow_jobs(workflow: &str) -> Vec<(&str, String)> {
    let (_, body) = workflow
        .split_once("\njobs:\n")
        .expect("the workflow has a top-level `jobs:` key");

    let mut jobs: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in body.lines() {
        let header = line
            .strip_prefix("  ")
            .filter(|rest| !rest.starts_with([' ', '#', '-']))
            .and_then(|rest| rest.strip_suffix(':'))
            .filter(|id| {
                !id.is_empty()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
        match header {
            Some(id) => jobs.push((id, Vec::new())),
            None => {
                if let Some((_, lines)) = jobs.last_mut() {
                    lines.push(line);
                }
            }
        }
    }
    jobs.into_iter()
        .map(|(id, lines)| (id, lines.join("\n")))
        .collect()
}

/// The `needs:` of a job body, tokenised.
///
/// Tokenised rather than substring-matched: one job id can be a prefix of
/// another, and a `contains` check would report the short one present when
/// only the long one is.
///
/// Only the inline `[a, b]` form is understood. A `needs:` written as a YAML
/// block list parses as empty, which reads as an orphan and fails
/// `every_job_is_behind_the_gate` — noisy, but closed. Leave it that way: the
/// alternative is a parser that can silently mis-attribute a dependency.
fn job_needs(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| line.starts_with("needs:"))
        .map(|line| {
            line.strip_prefix("needs:")
                .unwrap_or(line)
                .trim()
                .trim_matches(['[', ']'])
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `text` with whole-line YAML comments removed.
///
/// A content assertion must not be satisfiable by prose — least of all here,
/// where the surrounding comments spell out the very strings being asserted.
fn without_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `on:` block — everything from the top-level `on:` key to the next
/// top-level key.
fn trigger_block(workflow: &str) -> String {
    let (_, rest) = workflow
        .split_once("\non:\n")
        .expect("the workflow has a top-level `on:` key");
    rest.lines()
        .take_while(|line| line.is_empty() || line.starts_with([' ', '#']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `pull_request:` sub-block of `on:`.
fn pull_request_trigger(workflow: &str) -> String {
    let block = trigger_block(workflow);
    let (_, rest) = block
        .split_once("\n  pull_request:")
        .expect("the workflow triggers on `pull_request`");
    rest.lines()
        .take_while(|line| line.is_empty() || line.starts_with("    ") || line.starts_with("  #"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every job must be reachable from `gate` through `needs:`.
///
/// This is the whole safety property, stated once. A job that is not
/// downstream of `gate` runs on every stacked PR — burning three platforms'
/// worth of runners on a tree nobody can merge — and there is no check
/// anywhere that turns red to say so. The failure mode of the *feature* is
/// silent excess, so the guard has to be structural rather than a spot check
/// on the jobs that happen to exist today.
#[test]
fn every_job_is_behind_the_gate() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    assert!(
        jobs.iter().any(|(id, _)| *id == "gate"),
        "`heph.yml` has no `gate` job. Every other assertion here is about \
         what hangs off it."
    );

    let needs: HashMap<&str, Vec<&str>> = jobs
        .iter()
        .map(|(id, body)| (*id, job_needs(body)))
        .collect();

    let mut reaches_gate: HashSet<&str> = HashSet::new();
    // Fixed point rather than recursion: `needs:` is a DAG, so repeating the
    // sweep until it stops growing settles it in at most `jobs.len()` passes.
    loop {
        let before = reaches_gate.len();
        for (id, deps) in &needs {
            if deps
                .iter()
                .any(|dep| *dep == "gate" || reaches_gate.contains(dep))
            {
                reaches_gate.insert(id);
            }
        }
        if reaches_gate.len() == before {
            break;
        }
    }

    let orphans: Vec<&str> = jobs
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| *id != "gate" && !reaches_gate.contains(id))
        .collect();
    assert!(
        orphans.is_empty(),
        "these jobs are not downstream of `gate`, so they run even on a \
         stacked PR that CI is supposed to skip entirely: {orphans:?}. Give \
         each one `needs: gate` (plus `if: needs.gate.outputs.run == 'true'` \
         if it is a root job), or make it depend on something that already is."
    );
}

/// The two root jobs must consult the gate's verdict, not merely wait for it.
///
/// `needs: gate` alone is worse than nothing: `gate` always succeeds, so a
/// root job that only depends on it runs unconditionally while *looking*
/// gated — and `every_job_is_behind_the_gate` above would still pass.
///
/// Scoped to jobs whose `needs` is *exactly* `[gate]`, which is what a root
/// job looks like. A job with `needs: [gate, x]` is exempt on purpose: `x` is
/// itself downstream of the gate, so the job is already gated transitively and
/// an `if:` would be redundant. The exemption is a consequence of the
/// reachability rule above, not a hole in it.
#[test]
fn the_root_jobs_check_the_gates_verdict() {
    let workflow = workflow();
    for (id, body) in workflow_jobs(&workflow) {
        if job_needs(&body) != ["gate"] {
            continue;
        }
        let body = without_comments(&body);
        assert!(
            body.contains("needs.gate.outputs.run == 'true'"),
            "the `{id}` job depends on `gate` but never reads \
             `needs.gate.outputs.run`. `gate` always succeeds, so `{id}` runs \
             on every stacked PR while looking gated."
        );
    }
}

/// The gate decides on the PR's base and the `ci/force-ci` label — both.
///
/// Drop the base comparison and every PR builds; drop the label and the escape
/// hatch is gone, with nothing to signal it but a stacked PR that stays
/// stubbornly unbuilt.
#[test]
fn the_gate_decides_on_the_base_branch_and_the_force_label() {
    let workflow = workflow();
    let (_, body) = workflow_jobs(&workflow)
        .into_iter()
        .find(|(id, _)| *id == "gate")
        .expect("`heph.yml` has a `gate` job");
    let body = without_comments(&body);

    assert!(
        body.contains("default_branch"),
        "the `gate` job never mentions `default_branch`. Without comparing the \
         PR's base to it, the gate cannot tell a stacked PR from the one that \
         is about to land."
    );
    assert!(
        body.contains("github.event.pull_request.base.ref"),
        "the `gate` job never reads the PR's base ref, so its base comparison \
         cannot be against the PR's actual base."
    );
    assert!(
        body.contains("ci/force-ci"),
        "the `gate` job never mentions `ci/force-ci`, so there is no way to \
         build a mid-stack PR on demand."
    );
    assert!(
        body.contains("$GITHUB_OUTPUT"),
        "the `gate` job publishes no output, so `needs.gate.outputs.run` is \
         empty everywhere and every gated job is skipped — including on \
         master."
    );
}

/// Labelling a PR, and retargeting one, must each be able to start a run.
///
/// `labeled`: without it, labelling a stacked PR does nothing at all — the
/// label sticks, no run is queued, and the only way to build the PR is to push
/// to it, which is precisely what someone reaching for the label is avoiding.
///
/// `edited`: without it, GitHub's automatic retarget of a child PR onto master
/// (when the base merges) queues nothing, and the child keeps the `skipped`
/// checks it earned while stacked. Branch protection reads `skipped` as a
/// pass, so the child becomes mergeable into master over content that was
/// never built. This is the single assertion the gate's safety argument rests
/// on that is not enforced anywhere else in the tree.
#[test]
fn a_label_or_a_retarget_can_start_a_run() {
    let workflow = workflow();
    let trigger = without_comments(&pull_request_trigger(&workflow));
    let types = trigger
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("types:"))
        .unwrap_or_else(|| {
            panic!(
                "`on.pull_request` declares no `types:`, so it uses the default \
                 (opened/synchronize/reopened) and the `ci/force-ci` label \
                 cannot start a run"
            )
        });
    assert!(
        types.contains("labeled"),
        "`on.pull_request.types` omits `labeled`, so adding `ci/force-ci` to a \
         stacked PR queues nothing. Found: {types}"
    );
    assert!(
        types.contains("edited"),
        "`on.pull_request.types` omits `edited`, so GitHub's automatic \
         retarget of a stacked PR onto master queues no run and the PR stays \
         mergeable on its stale `skipped` checks. Found: {types}"
    );
    for required in ["opened", "synchronize", "reopened"] {
        assert!(
            types.contains(required),
            "`on.pull_request.types` omits `{required}`. Naming any `types:` \
             replaces the default set, so the omitted event now gets no CI at \
             all. Found: {types}"
        );
    }
}

/// The gate's reason has to be readable without a browser.
///
/// The `summary` job reports the verdict to `$GITHUB_STEP_SUMMARY`, which
/// renders on the run page and nowhere else — it is not in
/// `gh run view --log`, not in `gh pr checks --json`. An agent staring at
/// eleven `skipping` rows and an exit code of 0 has no way to reach it, so the
/// gate carries the reason on two surfaces that do survive into `gh`: stdout,
/// and a `::notice` annotation in the check-run API. The annotation is the one
/// asserted here — stdout is a substring of it either way, and the annotation
/// is the surface reachable without resolving a run id.
#[test]
fn the_gates_reason_reaches_a_cli_reader() {
    let workflow = workflow();
    let (_, body) = workflow_jobs(&workflow)
        .into_iter()
        .find(|(id, _)| *id == "gate")
        .expect("`heph.yml` has a `gate` job");
    let body = without_comments(&body);

    assert!(
        body.contains("::notice title=CI skipped::"),
        "the `gate` job emits no annotation when it skips. The run-page step \
         summary is invisible to `gh`, so the reason has to reach the \
         check-run API too."
    );
}

/// `summary` must distinguish "the gate skipped" from "the gate broke".
///
/// It is `always()`, so it also runs when `gate` was cancelled (the PR
/// concurrency group cancels superseded runs constantly) or failed outright.
/// A cancelled gate has empty outputs, so a summary that branches on
/// `outputs.run` alone explains a broken workflow as "this PR is stacked" —
/// a confident wrong answer from the one job whose entire purpose is to
/// answer that question.
#[test]
fn the_summary_tells_a_skip_apart_from_a_broken_gate() {
    let workflow = workflow();
    let (_, body) = workflow_jobs(&workflow)
        .into_iter()
        .find(|(id, _)| *id == "summary")
        .expect("`heph.yml` has a `summary` job");
    let body = without_comments(&body);

    assert!(
        body.contains("needs.gate.result"),
        "the `summary` job never reads `needs.gate.result`, so it cannot tell \
         a deliberate stacked-PR skip from a gate that was cancelled or \
         failed — and it will report the former for both."
    );
}

/// The stacked-PR decision belongs to `gate`, never to a `branches:` filter.
///
/// A `branches:` filter on `pull_request:` matches the PR's *base*, so
/// restricting it to master skips stacked PRs at the workflow level. That does
/// not produce skipped checks — it produces *no* checks, an empty list that
/// reads like a pass, that `ci/force-ci` cannot override, and that nothing in
/// the run explains. Fixed once already in #240; the gate is the supported way
/// to say the same thing.
#[test]
fn the_pull_request_trigger_is_unfiltered() {
    let workflow = workflow();
    let trigger = without_comments(&pull_request_trigger(&workflow));
    assert!(
        !trigger
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with("branches:") || line.starts_with("branches-ignore:")),
        "`on.pull_request` carries a branch filter. It matches the PR's base, \
         so a stacked PR gets no workflow run and therefore no checks at all — \
         an empty check list, not a skipped one, with no way to override it. \
         Skip stacked PRs in the `gate` job instead:\n{trigger}"
    );
}
