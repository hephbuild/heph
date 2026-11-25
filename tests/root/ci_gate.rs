//! Guards on the CI gate — which pull requests get a build at all.
//!
//! `heph.yml` deliberately does not build the middle layers of a stack: a PR
//! whose base is another PR's branch rather than the default branch, and on
//! which another open PR is stacked. The bottom builds (it targets master), and
//! so does the top, whose tree contains every layer below it. Actions
//! concurrency is limited — one run is more jobs than the account can run at
//! once — so a stack costs two runs per push instead of one per layer. The
//! `ci/force-ci` label builds a middle layer anyway.
//!
//! It is safe for one reason and one reason only: the `master` repository
//! ruleset — where the required status checks live — has condition
//! `~DEFAULT_BRANCH`, so it applies to exactly the PRs whose base is master,
//! and the gate builds every one of those unconditionally. Nothing reaches
//! master without first being a PR that targets master. (An atomic `gh stack
//! merge` holds every layer to the ruleset; a middle layer then carries the
//! top's results, posted by `stack-status.yml` — see `stack_status.rs`.)
//!
//! Every assertion below protects a way that reasoning can be broken silently,
//! in the direction where nothing goes red:
//!
//!   - a new job added with no `needs:` builds on every stacked PR forever,
//!     and no check ever complains;
//!   - dropping `labeled` from the trigger list leaves `ci/force-ci` inert —
//!     the label is accepted, and simply never starts a run;
//!   - dropping `unlabeled` means taking the label off a middle layer no
//!     longer stops its build, which keeps a whole run in the queue for the
//!     account's limited concurrent jobs;
//!   - a gate script that decides wrongly — skips a master PR or the top of a
//!     stack, builds a middle layer — which `the_gate_script_decides_*` runs
//!     case by case rather than reading;
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
    // Whole tokens: `unlabeled` contains `labeled`, so a substring check would
    // pass with `labeled` itself gone.
    let listed: Vec<&str> = types
        .trim_start_matches("types:")
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(str::trim)
        .collect();
    let types = listed.join(", ");
    assert!(
        listed.contains(&"labeled"),
        "`on.pull_request.types` omits `labeled`, so adding `ci/force-ci` to a \
         stacked PR queues nothing. Found: {types}"
    );
    assert!(
        listed.contains(&"unlabeled"),
        "`on.pull_request.types` omits `unlabeled`, so removing `ci/force-ci` \
         from a stacked PR leaves its forced run building to completion, \
         holding a whole run's worth of the account's concurrent jobs. \
         Found: {types}"
    );
    assert!(
        listed.contains(&"edited"),
        "`on.pull_request.types` omits `edited`, so GitHub's automatic \
         retarget of a stacked PR onto master queues no run and the PR stays \
         mergeable on its stale `skipped` checks. Found: {types}"
    );
    for required in ["opened", "synchronize", "reopened"] {
        assert!(
            listed.contains(&required),
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

/// A test-only cdylib (`crates/plugin-entrypoint-test-cdylib`) is built and
/// uploaded by `build` for `bin_e2e`, and must never become a release asset.
/// The exclusion is by name: it is uploaded as `test-*`, and everything that
/// publishes or measures artifacts downloads `heph*`. A fixture renamed
/// `heph-*`, or a download pattern widened past `heph`, would ship it silently
/// in every Pre-release.
#[test]
fn test_fixtures_are_never_published() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let build = jobs
        .iter()
        .find(|(id, _)| *id == "build")
        .map(|(_, body)| without_comments(body))
        .expect("`heph.yml` has a `build` job");
    let fixture = build
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("TEST_ENTRYPOINT_NAME="))
        .expect("`build` names the entrypoint test cdylib");
    assert!(
        fixture.trim_matches('"').starts_with("test-"),
        "the test cdylib is uploaded as {fixture}; anything not `test-*` can \
         match the release's `heph*` pattern"
    );

    let perf = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/perf.yml"),
    )
    .expect("read .github/workflows/perf.yml");
    // `go_e2e_count` downloads only the shards' test counts, never a build.
    let mut publishing = jobs
        .iter()
        .filter(|(id, _)| !["bin_e2e", "go_e2e_count"].contains(id))
        .map(|(id, body)| (format!("heph.yml {id}"), without_comments(body)))
        .collect::<Vec<_>>();
    publishing.push(("perf.yml".to_owned(), without_comments(&perf)));
    for (job, body) in publishing {
        for line in body.lines().map(str::trim) {
            let Some(pattern) = line.strip_prefix("pattern:") else {
                continue;
            };
            assert!(
                pattern.trim().trim_matches('"').starts_with("heph"),
                "{job} downloads `{}`, which is not limited to `heph*` and so \
                 can pick up the test-only cdylib",
                pattern.trim()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Structural checks over the parsed workflow.
//
// The checks above predate these and read the text; everything below asks
// about transitive `needs:`, step conditions and step `env:`, which a line
// scanner gets wrong in the silent direction. So it parses the file with
// `serde_yaml` instead, and every helper returns `Option` so a test that
// cannot find what it looks for fails with its own message.

use serde_yaml::{Mapping, Value};
use std::collections::BTreeSet;

fn parsed() -> Value {
    serde_yaml::from_str(&workflow()).expect("heph.yml parses as YAML")
}

fn jobs_map(wf: &Value) -> Option<&Mapping> {
    wf.get("jobs").and_then(Value::as_mapping)
}

fn job<'a>(wf: &'a Value, id: &str) -> Option<&'a Value> {
    jobs_map(wf)?.get(id)
}

fn job_ids(wf: &Value) -> Vec<String> {
    jobs_map(wf)
        .map(|jobs| {
            jobs.keys()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A job's direct `needs:`, in either YAML form.
fn needs_of(job: &Value) -> Vec<String> {
    match job.get("needs") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Sequence(many)) => many
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every job `id` waits for, directly or through another job.
fn needs_closure(wf: &Value, id: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = job(wf, id).map(needs_of).unwrap_or_default();
    while let Some(dep) = stack.pop() {
        if seen.insert(dep.clone()) {
            stack.extend(job(wf, &dep).map(needs_of).unwrap_or_default());
        }
    }
    seen
}

/// A job's steps, with `parallel:` groups flattened into their members.
fn steps(job: &Value) -> Vec<&Value> {
    fn walk<'a>(seq: &'a [Value], out: &mut Vec<&'a Value>) {
        for step in seq {
            match step.get("parallel").and_then(Value::as_sequence) {
                Some(group) => walk(group, out),
                None => out.push(step),
            }
        }
    }
    let mut out = Vec::new();
    if let Some(seq) = job.get("steps").and_then(Value::as_sequence) {
        walk(seq, &mut out);
    }
    out
}

/// A string field of a step or job, or `""`.
fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn env_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get("env")?.get(key)?.as_str()
}

/// The first step whose `run:` contains `needle`.
fn step_running<'a>(job: &'a Value, needle: &str) -> Option<&'a Value> {
    steps(job)
        .into_iter()
        .find(|step| field(step, "run").contains(needle))
}

/// The step whose whole `run:` is exactly `command`.
fn step_running_exactly<'a>(job: &'a Value, command: &str) -> Option<&'a Value> {
    steps(job)
        .into_iter()
        .find(|step| field(step, "run").trim() == command)
}

/// The `os/arch` of each `strategy.matrix.include` entry.
fn coords(job: &Value) -> Vec<String> {
    job.get("strategy")
        .and_then(|s| s.get("matrix"))
        .and_then(|m| m.get("include"))
        .and_then(Value::as_sequence)
        .map(|entries| {
            entries
                .iter()
                .map(|e| format!("{}/{}", field(e, "os"), field(e, "arch")))
                .collect()
        })
        .unwrap_or_default()
}

/// What the fake `gh` answers when the gate lists the PRs stacked on this one.
#[derive(Debug, Clone, Copy)]
enum Lookup {
    /// `gh` prints this (what the real one prints after `--jq`: `#12 #34`, or
    /// nothing for no children).
    Children(&'static str),
    /// `gh` exits 1, as on an API outage or a token without `pull-requests`.
    Fails,
}

/// What a run of the gate script did.
#[derive(Debug)]
struct GateRun {
    run: String,
    reason: String,
    /// The fake `gh`'s argv, one per line, or `None` when it was never called.
    gh_args: Option<String>,
}

/// Runs the gate's `Decide` step as Actions would (`bash -e -o pipefail`),
/// with `env` as its whole environment and a fake `gh` first on `PATH`.
fn run_gate(script: &str, env: &[(&str, &str)], lookup: Lookup) -> GateRun {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("github_output");
    let gh_args = dir.path().join("gh_args");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("creating the fake gh's directory");
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > \"$FAKE_GH_ARGS\"\n\
         [ -n \"$FAKE_GH_FAIL\" ] && exit 1\n\
         printf '%s' \"$FAKE_GH_CHILDREN\"\n",
    )
    .expect("writing the fake gh");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
        .expect("making the fake gh executable");
    let script_path = dir.path().join("decide.sh");
    std::fs::write(&script_path, script).expect("writing the gate script");

    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut cmd = std::process::Command::new("bash");
    cmd.args(["--noprofile", "--norc", "-e", "-o", "pipefail"])
        .arg(&script_path)
        .env_clear()
        .env("PATH", path)
        .env("GITHUB_OUTPUT", &output)
        .env("FAKE_GH_ARGS", &gh_args)
        .envs(env.iter().copied());
    match lookup {
        Lookup::Children(children) => cmd.env("FAKE_GH_CHILDREN", children),
        Lookup::Fails => cmd.env("FAKE_GH_FAIL", "1"),
    };
    let status = cmd.output().expect("running the gate script with bash");
    assert!(
        status.status.success(),
        "the gate script failed for {env:?}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let written = std::fs::read_to_string(&output).expect("reading $GITHUB_OUTPUT");
    // A missing key reads as "", which the caller's assertions reject.
    let out = |key: &str| {
        written
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
            .to_string()
    };
    GateRun {
        run: out("run"),
        reason: out("reason"),
        gh_args: std::fs::read_to_string(&gh_args).ok(),
    }
}

/// One row of the gate's decision table.
struct Case {
    what: &'static str,
    event: &'static str,
    base: &'static str,
    default: &'static str,
    forced: bool,
    lookup: Lookup,
    builds: bool,
    /// Whether the gate should ask GitHub which PRs are stacked on this one.
    asks: bool,
}

/// The gate's decision, case by case, from the script itself.
///
/// The shape checks above prove the gate *reads* the base and the label; this
/// proves what it does with them. Actions concurrency is limited: one run is
/// more jobs than the account can run at once, so every extra build queues. A
/// stack builds its bottom (it targets master, its checks are required) and
/// its top (nothing is stacked on it, and its tree holds every layer), and
/// skips every layer between. A gate that builds a middle layer costs a run
/// per push; one that skips the top leaves the stack untested; one that skips
/// a master-targeting PR lets its required checks conclude `skipped`, which
/// branch protection reads as a pass — so those never wait on the API at all.
#[test]
fn the_gate_script_decides_who_builds() {
    let wf = parsed();
    let gate = job(&wf, "gate").expect("`heph.yml` has a `gate` job");
    let decide = steps(gate)
        .into_iter()
        .find(|s| field(s, "id") == "decide")
        .expect("the `gate` job has a step with `id: decide`");
    assert_eq!(
        env_of(decide, "FORCED"),
        Some("${{ contains(github.event.pull_request.labels.*.name, 'ci/force-ci') }}"),
        "`FORCED` must be exactly whether the PR's *current* labels include \
         `ci/force-ci` — on `unlabeled` too, where the payload's label list \
         no longer has it."
    );
    assert_eq!(
        env_of(decide, "HEAD"),
        Some("${{ github.event.pull_request.head.ref }}"),
        "the PRs stacked on this one are the ones based on its *head* branch"
    );
    assert_eq!(
        gate.get("permissions")
            .and_then(|p| p.get("pull-requests"))
            .and_then(Value::as_str),
        Some("read"),
        "the gate lists open PRs, so its token needs `pull-requests: read`; \
         without it every lookup fails and every middle layer builds"
    );
    let script = field(decide, "run");

    let stacked = |what, forced, lookup, builds| Case {
        what,
        event: "pull_request",
        base: "feat/base",
        default: "master",
        forced,
        lookup,
        builds,
        asks: !forced,
    };
    let unstacked = |what, event, base, default, builds| Case {
        what,
        event,
        base,
        default,
        forced: false,
        // Would make it skip, if the gate asked.
        lookup: Lookup::Children("#2"),
        builds,
        asks: false,
    };
    let cases = [
        unstacked("a push to master", "push", "", "master", true),
        unstacked("a master PR", "pull_request", "master", "master", true),
        unstacked(
            "a default branch not named master",
            "pull_request",
            "main",
            "main",
            true,
        ),
        Case {
            forced: true,
            ..unstacked(
                "a master PR with the label",
                "pull_request",
                "master",
                "master",
                true,
            )
        },
        stacked("the top of a stack", false, Lookup::Children(""), true),
        stacked("a middle layer", false, Lookup::Children("#7"), false),
        stacked(
            "a layer under two PRs",
            false,
            Lookup::Children("#7 #8"),
            false,
        ),
        stacked("a forced middle layer", true, Lookup::Children("#7"), true),
        stacked("a failed lookup", false, Lookup::Fails, true),
        Case {
            base: "main",
            ..stacked(
                "a base merely named like a trunk",
                false,
                Lookup::Children("#7"),
                false,
            )
        },
        Case {
            base: "master-next",
            ..stacked(
                "a base that starts with the default's name",
                false,
                Lookup::Children("#7"),
                false,
            )
        },
    ];
    for case in cases {
        let Case {
            what,
            event,
            base,
            default,
            forced,
            lookup,
            builds,
            asks,
        } = case;
        let forced_s = if forced { "true" } else { "false" };
        let got = run_gate(
            script,
            &[
                ("EVENT", event),
                ("BASE", base),
                ("HEAD", "feat/this"),
                ("DEFAULT_BRANCH", default),
                ("FORCED", forced_s),
                ("GH_TOKEN", "fake"),
                ("GH_REPO", "owner/repo"),
            ],
            lookup,
        );
        let GateRun {
            run,
            reason,
            gh_args,
        } = got;
        assert_eq!(
            run,
            builds.to_string(),
            "{what}: event={event} base={base} default={default} forced={forced} \
             lookup={lookup:?} should {}build; the gate said: {reason}",
            if builds { "" } else { "not " }
        );
        assert!(!reason.is_empty(), "{what}: the gate gave no reason");
        assert_eq!(
            gh_args.is_some(),
            asks,
            "{what}: the gate {} GitHub for the PRs stacked on it",
            if asks { "should ask" } else { "must not ask" }
        );
        if let Some(args) = gh_args {
            let args: Vec<&str> = args.lines().collect();
            assert!(
                args.windows(2).any(|w| w == ["--base", "feat/this"])
                    && args.windows(2).any(|w| w == ["--state", "open"]),
                "{what}: the gate must list the *open* PRs based on its head \
                 branch. It ran: gh {args:?}"
            );
        }
        if !builds {
            assert!(
                reason.contains("is stacked on it") && reason.contains("ci/force-ci"),
                "{what}: a skip must name the PR stacked on this one, whose run \
                 to read, and the label that overrides it. Got: {reason}"
            );
        }
    }
}

const PLATFORMS: [&str; 3] = ["darwin/arm64", "linux/amd64", "linux/arm64"];

/// The jobs this layout added hang off `gate` like every other. Also covered
/// by `every_job_is_behind_the_gate`; stated through the parsed `needs:` here
/// so a block-list `needs:` (which the text parser reads as empty) cannot
/// slip past both.
#[test]
fn go_e2e_jobs_are_behind_the_gate() {
    let wf = parsed();
    for id in ["go_e2e_build", "go_e2e", "go_e2e_count"] {
        assert!(job(&wf, id).is_some(), "no `{id}` job");
        assert!(
            needs_closure(&wf, id).contains("gate"),
            "`{id}` does not wait for `gate`, so it runs on stacked PRs"
        );
    }
}

/// Lint runs on every supported platform, each leg in the `lint` job, and no
/// test job lints.
///
/// Lint was once the first step of the darwin and linux/arm64 Test legs, to
/// save a macOS slot. That serialized a ~7 min warm lint ahead of the tests, and
/// `Test darwin/arm64` became the run's critical path (28 min, against 19 min
/// for every other check).
#[test]
fn every_platform_lints_in_its_own_job() {
    let wf = parsed();
    let lint = job(&wf, "lint").expect("a `lint` job");
    let mut legs = coords(lint);
    legs.sort();
    assert_eq!(
        legs, PLATFORMS,
        "the `lint` job has one leg per supported platform"
    );
    assert!(
        step_running_exactly(lint, "lint").is_some(),
        "the `lint` job runs the `lint` script"
    );

    let test = job(&wf, "test").expect("a `test` job");
    assert!(
        step_running_exactly(test, "lint").is_none(),
        "`test` runs `lint`, which puts it on that test job's critical path; \
         lint belongs to the `lint` job"
    );
}

/// Every platform tests the workspace minus exactly `bin-e2e` and
/// `plugingo-e2e`, and the `go_e2e` jobs run `plugingo-e2e`. A subset for
/// darwin and linux/arm64 — the crates with platform-conditional code — was
/// tried, and darwin still ran nearly the whole suite while every other crate
/// went untested there.
#[test]
fn test_plus_go_e2e_covers_the_workspace() {
    let wf = parsed();
    let test = job(&wf, "test").expect("a `test` job");
    let mut legs = coords(test);
    legs.sort();
    assert_eq!(legs, PLATFORMS, "`test` has one leg per supported platform");
    let step = step_running_exactly(test, "tst").expect("`test` runs `tst`");
    let scope = env_of(step, "TST_SCOPE").expect("`test` sets TST_SCOPE");
    let words: Vec<&str> = scope.split_whitespace().collect();
    assert!(
        words.contains(&"--workspace"),
        "`test` is the workspace: {scope}"
    );
    assert!(
        !words.contains(&"-p"),
        "`test` selects no single crate: {scope}"
    );
    let excluded: BTreeSet<&str> = words
        .windows(2)
        .filter(|w| w.first() == Some(&"--exclude"))
        .filter_map(|w| w.get(1).copied())
        .collect();
    assert_eq!(
        excluded,
        BTreeSet::from(["bin-e2e", "plugingo-e2e"]),
        "`test` must exclude only bin-e2e (its own job) and plugingo-e2e (go_e2e)"
    );

    let build = job(&wf, "go_e2e_build").expect("a `go_e2e_build` job");
    let archive = step_running(build, "nextest archive").expect("go_e2e_build archives");
    assert!(
        field(archive, "run").contains("-p plugingo-e2e"),
        "go_e2e_build archives plugingo-e2e"
    );
    let shards = job(&wf, "go_e2e").expect("a `go_e2e` job");
    assert!(
        step_running(shards, "--archive-file").is_some(),
        "go_e2e runs the archive"
    );
    for id in ["go_e2e_build", "go_e2e"] {
        let runs_on = job(&wf, id).map(|j| field(j, "runs-on")).unwrap_or("");
        assert_eq!(runs_on, "ubuntu-latest", "`{id}` runs on linux/amd64");
    }
}

/// The two Go smoke tests run as their own invocation with HEPH_E2E_DEVENV,
/// which `devenv_host_toolchain` needs or it returns early and passes
/// vacuously. They are skipped on linux/amd64 only, which runs all of
/// `plugingo-e2e` in `go_e2e`.
#[test]
fn smoke_step_sets_heph_e2e_devenv() {
    let wf = parsed();
    let test = job(&wf, "test").expect("a `test` job");
    let smoke =
        step_running(test, "--test devenv_host_toolchain").expect("`test` runs the Go smoke tests");
    assert!(
        field(smoke, "if").contains("!(matrix.os == 'linux' && matrix.arch == 'amd64')"),
        "the smoke tests skip linux/amd64 and nothing else: {}",
        field(smoke, "if")
    );
    let run = field(smoke, "run");
    assert!(
        run.contains("-p plugingo-e2e") && run.contains("--test testmain"),
        "{run}"
    );
    assert_eq!(
        env_of(smoke, "HEPH_E2E_DEVENV"),
        Some("1"),
        "devenv_host_toolchain skips itself without HEPH_E2E_DEVENV=1"
    );
    assert_ne!(
        run.trim(),
        "tst",
        "the smoke tests are not part of the `tst` call"
    );
}

/// The shard list, the partition's denominator and the count check agree.
#[test]
fn go_e2e_shards_agree_on_their_count() {
    let wf = parsed();
    let shards = job(&wf, "go_e2e").expect("a `go_e2e` job");
    let list = shards
        .get("strategy")
        .and_then(|s| s.get("matrix"))
        .and_then(|m| m.get("shard"))
        .and_then(Value::as_sequence)
        .expect("go_e2e has a `matrix.shard` list");
    let n = list.len();
    let expected: Vec<u64> = (1..=n as u64).collect();
    let got: Vec<u64> = list.iter().filter_map(Value::as_u64).collect();
    assert_eq!(got, expected, "shards are numbered 1..=N");

    let run = field(
        step_running(shards, "--partition").expect("go_e2e partitions the archive"),
        "run",
    );
    assert!(
        run.contains(&format!("\"slice:$SHARD/{n}\"")),
        "go_e2e partitions with `slice:$SHARD/{n}` (not `count:`, which puts \
         every one-test binary on shard 1): {run}"
    );
    assert!(
        run.contains("--no-tests=fail"),
        "an empty shard fails: {run}"
    );
    assert!(
        field(shards, "name").ends_with(&format!("/{n}")),
        "the check name shows the shard count"
    );

    let count = job(&wf, "go_e2e_count").expect("a `go_e2e_count` job");
    let check = step_running(count, "EXPECTED").expect("go_e2e_count compares counts");
    assert_eq!(env_of(check, "SHARDS"), Some(n.to_string().as_str()));
    let needs = needs_of(count);
    assert!(
        needs.iter().any(|d| d == "go_e2e") && needs.iter().any(|d| d == "go_e2e_build"),
        "go_e2e_count waits for the shards and the build: {needs:?}"
    );
}

/// `plugingo-e2e` is excluded from `test`'s doctest pass as well as its
/// nextest pass, and the `go_e2e` jobs run no doctests. That loses nothing only
/// while the crate has no library, and so no doctests.
#[test]
fn go_e2e_drops_no_doctests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/plugingo-e2e");
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read plugingo-e2e's Cargo.toml");
    assert!(
        !root.join("src/lib.rs").exists() && !manifest.contains("[lib]"),
        "plugingo-e2e has a library now, so it can have doctests, and nothing \
         runs them: `test` excludes the crate from `cargo test --doc` too. Run \
         its doctests somewhere (e.g. a `cargo test --doc -p plugingo-e2e` step \
         in go_e2e_build)."
    );
}

/// Every nextest run in CI uses the `ci` profile: a per-test timeout, so a
/// hang is a named TIMEOUT, and no retries, so a flake is a failure.
#[test]
fn ci_runs_the_nextest_ci_profile() {
    let wf = parsed();
    assert_eq!(
        env_of(&wf, "NEXTEST_PROFILE"),
        Some("ci"),
        "the workflow sets NEXTEST_PROFILE=ci for every job"
    );
    let config =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".config/nextest.toml"))
            .expect("read .config/nextest.toml");
    let (_, ci) = config
        .split_once("\n[profile.ci]\n")
        .expect("nextest.toml has a [profile.ci]");
    let ci = ci.split("\n[").next().unwrap_or(ci);
    let settings: Vec<&str> = ci
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert!(
        settings.contains(&"retries = 0"),
        "the ci profile retries nothing: {settings:?}"
    );
    assert!(
        settings.iter().any(|l| l.starts_with("slow-timeout")),
        "the ci profile has a slow-timeout: {settings:?}"
    );
}

/// Jobs whose result says something about the code rather than about the
/// artifacts. Publishing must never wait on one of them.
const CHECK_JOBS: [&str; 8] = [
    "lint",
    "test",
    "go_e2e_build",
    "go_e2e",
    "go_e2e_count",
    "bin_e2e_warm",
    "bin_e2e",
    "abi",
];

/// The step publishes to, or authenticates against, the artifacts repo.
fn touches_the_release(step: &Value) -> bool {
    let run = field(step, "run");
    let token = step
        .get("env")
        .and_then(Value::as_mapping)
        .is_some_and(|env| {
            env.values()
                .filter_map(Value::as_str)
                .any(|v| v.contains("HEPH_ARTIFACTS_PAT"))
        });
    run.contains("gh release") || field(step, "uses").contains("action-gh-release") || token
}

/// Every job but `release` that touches the artifacts repo.
fn publishing_jobs(wf: &Value) -> Vec<String> {
    job_ids(wf)
        .into_iter()
        .filter(|id| id != "release")
        .filter(|id| job(wf, id).is_some_and(|j| steps(j).into_iter().any(touches_the_release)))
        .collect()
}

/// Publishing waits only on building: no job that uploads to the pre-release
/// reaches a test, lint or bin-e2e job through `needs:`. A tester's flake used
/// to hold every platform's artifacts back.
#[test]
fn publishing_does_not_wait_on_tests() {
    let wf = parsed();
    let mut publishers = publishing_jobs(&wf);
    publishers.sort();
    assert_eq!(
        publishers,
        ["build", "manifests", "prerelease_create"],
        "the jobs that publish to the pre-release changed; check the new one \
         against this test's rule before adding it here"
    );
    for id in &publishers {
        let waits_on: Vec<String> = needs_closure(&wf, id)
            .into_iter()
            .filter(|dep| CHECK_JOBS.contains(&dep.as_str()))
            .collect();
        assert!(
            waits_on.is_empty(),
            "`{id}` publishes to the pre-release but waits for {waits_on:?}, so \
             a red or slow check holds the artifacts back"
        );
    }
}

/// Only `release` makes a release mainline, and it waits for everything.
#[test]
fn only_release_promotes_to_mainline() {
    let wf = parsed();
    for id in job_ids(&wf).into_iter().filter(|id| id != "release") {
        let Some(j) = job(&wf, &id) else { continue };
        for step in steps(j) {
            let with = step.get("with");
            let promotes_with_action = with.is_some_and(|w| {
                w.get("prerelease").and_then(Value::as_bool) == Some(false)
                    || w.get("make_latest").is_some()
            });
            let promotes_with_cli = field(step, "run")
                .split_whitespace()
                .any(|w| w == "--latest" || w == "--latest=true" || w == "--prerelease=false");
            assert!(
                !promotes_with_action && !promotes_with_cli,
                "`{id}` promotes a release to mainline; only `release` may"
            );
        }
    }

    let release = job(&wf, "release").expect("a `release` job");
    assert!(
        field(release, "if").contains("refs/heads/master"),
        "release runs on master only"
    );
    let promotes = steps(release).into_iter().any(|s| {
        s.get("with").is_some_and(|w| {
            w.get("prerelease").and_then(Value::as_bool) == Some(false)
                && w.get("make_latest").and_then(Value::as_bool) == Some(true)
        })
    });
    assert!(
        promotes,
        "release promotes (prerelease: false, make_latest: true)"
    );
    let needs = needs_of(release);
    for required in [
        "manifests",
        "lint",
        "test",
        "go_e2e",
        "go_e2e_count",
        "bin_e2e",
    ] {
        assert!(
            needs.iter().any(|n| n == required),
            "release does not wait for `{required}`, so mainline can move past \
             a red check. needs: {needs:?}"
        );
    }
}

/// Every asset family the old single `Pre-release` job published still has an
/// upload step: the CLI in both flavours, heph-bench, the four plugin cdylibs
/// and heph-govet — which the go plugin downloads from
/// `releases/download/<version>/heph-govet_<os>_<arch>`. The manifests and
/// their checksums go up through `publish-plugin-manifests.sh`, which
/// `every_build_publishes_the_manifests_after_its_cdylibs` and
/// `tests/root/publish_manifests.rs` cover.
#[test]
fn every_release_asset_has_an_uploader() {
    let wf = parsed();
    let uploads: String = job_ids(&wf)
        .into_iter()
        .filter_map(|id| job(&wf, &id))
        .flat_map(steps)
        .map(|s| field(s, "run"))
        .filter(|run| run.contains("gh release upload"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut families = vec![
        "\"heph_${OS}_${ARCH}\"".to_string(),
        "\"heph_debug_${OS}_${ARCH}\"".to_string(),
        "\"heph-bench_${OS}_${ARCH}\"".to_string(),
        "dist/heph-govet_*".to_string(),
    ];
    for plugin in ["go", "gha", "oci", "devenv"] {
        families.push(format!(
            "\"heph-{plugin}-plugin_${{OS}}_${{ARCH}}.${{EXT}}\""
        ));
    }
    let missing: Vec<&String> = families
        .iter()
        .filter(|f| !uploads.contains(f.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "no `gh release upload` publishes {missing:?}"
    );
}

const PUBLISH_MANIFESTS: &str = "bash .github/workflows/publish-plugin-manifests.sh";

/// A platform's plugins are installable the moment that platform is built:
/// each `build` leg republishes the manifests itself instead of leaving it to
/// `manifests`, which waits for the slowest platform. The step comes after the
/// leg's own upload, because the script builds the manifests from the
/// release's asset listing — run before it, the leg's platform is missing from
/// what it publishes, and the convergence argument in the script's header
/// (every leg uploads before its first listing) no longer holds.
///
/// `manifests` runs it too, and must: it is the only run that no leg can
/// overlap, and the one whose failure fails a check.
#[test]
fn every_build_publishes_the_manifests_after_its_cdylibs() {
    let wf = parsed();
    let build = job(&wf, "build").expect("a `build` job");
    let leg_steps = steps(build);
    let upload = leg_steps
        .iter()
        .position(|s| field(s, "name") == "Publish to the pre-release")
        .expect("the build leg uploads its own files");
    let at = leg_steps
        .iter()
        .position(|s| field(s, "run").trim() == PUBLISH_MANIFESTS)
        .expect("the build leg publishes the plugin manifests");
    assert!(
        at > upload,
        "the build leg publishes the manifests before its own cdylibs are on \
         the release, so its platform is missing from them"
    );
    let step = leg_steps.get(at).expect("the step just found");
    assert_eq!(
        field(step, "if"),
        "needs.prerelease_create.outputs.publish == 'true'"
    );
    // Without devenv the leg has no `go` to build the generator with; without
    // `continue-on-error` an API outage fails a build that succeeded; without a
    // timeout a hung request holds the leg until the job's 90 minutes.
    assert!(field(step, "shell").starts_with("devenv shell"));
    assert_eq!(
        step.get("continue-on-error").and_then(Value::as_bool),
        Some(true)
    );
    assert!(step.get("timeout-minutes").is_some());
    assert_eq!(
        env_of(step, "EXPECT_PLATFORMS"),
        Some("${{ matrix.os }}_${{ matrix.arch }}"),
        "a leg must expect its own platform, or a lagging listing that lacks \
         it reads as current"
    );

    let manifests = job(&wf, "manifests").expect("a `manifests` job");
    let last = step_running_exactly(manifests, PUBLISH_MANIFESTS)
        .expect("`manifests` republishes the plugin manifests after every build");
    assert!(
        last.get("continue-on-error").is_none(),
        "`manifests` is the backstop; its publish must fail the job"
    );
    assert!(
        needs_of(manifests).iter().any(|n| n == "build"),
        "`manifests` must wait for every build leg, or a leg can overlap it"
    );
    let mut legs: Vec<String> = coords(build).iter().map(|c| c.replace('/', "_")).collect();
    legs.sort();
    let expected: Vec<&str> = env_of(last, "EXPECT_PLATFORMS")
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    assert_eq!(
        expected, legs,
        "`manifests` must expect exactly the build matrix's platforms, sorted, \
         or it marks a release complete with one missing"
    );
}

/// `heph-govet` goes up from the job that holds its artifact and that every
/// build leg waits for, so it is on the release before any manifest names the
/// go plugin.
#[test]
fn heph_govet_is_published_before_any_build() {
    let wf = parsed();
    let create = job(&wf, "prerelease_create").expect("a `prerelease_create` job");
    assert!(needs_of(create).iter().any(|n| n == "govet"));
    assert!(
        step_running(create, "dist/heph-govet_*").is_some(),
        "`prerelease_create` publishes heph-govet"
    );
    let build = job(&wf, "build").expect("a `build` job");
    assert!(needs_of(build).iter().any(|n| n == "prerelease_create"));
}

/// Without the upload secret (a fork PR) publishing is skipped, not failed:
/// `prerelease_create` reports `publish=false`, every upload step is
/// conditional on it, and the token lives only in step-level env (accepted
/// exemption E4).
#[test]
fn publishing_is_skipped_not_failed_without_the_secret() {
    let wf = parsed();
    let create = job(&wf, "prerelease_create").expect("a `prerelease_create` job");
    let step = step_running(create, "publish=").expect("prerelease_create sets `publish`");
    assert_eq!(
        env_of(step, "GH_TOKEN"),
        Some("${{ secrets.HEPH_ARTIFACTS_PAT }}")
    );
    let run = field(step, "run");
    let (absent, present) = run
        .split_once("publish=false")
        .expect("prerelease_create can report publish=false");
    assert!(
        absent.contains("[ -z \"$GH_TOKEN\" ]") && present.contains("exit 0"),
        "publish=false is decided by the token being empty, and exits cleanly"
    );

    assert!(
        env_of(&wf, "GH_TOKEN").is_none(),
        "no workflow-level release token"
    );
    for id in job_ids(&wf) {
        let Some(j) = job(&wf, &id) else { continue };
        let job_env = j
            .get("env")
            .map(|e| serde_yaml::to_string(e).unwrap_or_default())
            .unwrap_or_default();
        assert!(
            !job_env.contains("HEPH_ARTIFACTS_PAT"),
            "`{id}` puts HEPH_ARTIFACTS_PAT in job-level env; it belongs in the \
             upload step's env only"
        );
        if id == "prerelease_create" || id == "release" {
            continue;
        }
        for s in steps(j).into_iter().filter(|s| touches_the_release(s)) {
            assert!(
                field(s, "if").contains("needs.prerelease_create.outputs.publish == 'true'"),
                "`{id}` has an upload step that does not skip without the secret: {:?}",
                field(s, "name")
            );
            assert!(
                needs_of(j).iter().any(|n| n == "prerelease_create"),
                "`{id}` reads needs.prerelease_create but does not need it"
            );
        }
    }
}

/// A run uses at most four macOS jobs — Build, Lint, Test and Binary E2E — so
/// one run fits GitHub's five concurrent macOS slots. It used five or six,
/// which queued its own jobs. Three, with Lint folded into Test, put lint on
/// the darwin critical path. `perfbench` is excluded: it runs only on master
/// pushes or with `ci/perf-test`.
#[test]
fn a_run_uses_at_most_four_macos_jobs() {
    let wf = parsed();
    let is_mac = |v: &Value| {
        field(v, "os") == "darwin" || {
            let runs_on = field(v, "runs-on");
            runs_on.contains("macos") || runs_on.contains("MACOS_RUNNER")
        }
    };
    let mut mac_jobs = Vec::new();
    for id in job_ids(&wf).into_iter().filter(|id| id != "perfbench") {
        let Some(j) = job(&wf, &id) else { continue };
        let entries = j
            .get("strategy")
            .and_then(|s| s.get("matrix"))
            .and_then(|m| m.get("include"))
            .and_then(Value::as_sequence);
        match entries {
            Some(entries) => {
                mac_jobs.extend(entries.iter().filter(|e| is_mac(e)).map(|_| id.clone()))
            }
            None if is_mac(j) => mac_jobs.push(id.clone()),
            None => {}
        }
    }
    assert!(
        mac_jobs.len() <= 4,
        "a run starts {} macOS jobs ({mac_jobs:?}); the free tier runs five at \
         once, so a run would queue behind its own jobs",
        mac_jobs.len()
    );
}

/// heph-bench is skipped on PRs without `ci/perf-test`, but every push builds
/// it: perf baselines (N-1) are downloaded from master's releases, so a master
/// build without it would leave the next PR's perf run with nothing to compare.
#[test]
fn master_always_builds_heph_bench() {
    let wf = parsed();
    let build = job(&wf, "build").expect("a `build` job");
    let expr = env_of(build, "BUILD_BENCH").expect("build sets BUILD_BENCH");
    let inner = expr
        .trim()
        .strip_prefix("${{")
        .and_then(|e| e.strip_suffix("}}"))
        .expect("BUILD_BENCH is an expression")
        .trim();
    assert!(
        !inner.contains("&&"),
        "BUILD_BENCH must be a plain disjunction, so its push arm cannot be \
         narrowed: {inner}"
    );
    let arms: Vec<&str> = inner.split("||").map(str::trim).collect();
    assert!(
        arms.contains(&"github.event_name != 'pull_request'"),
        "BUILD_BENCH is not true for every push: {inner}"
    );
    assert!(
        arms.iter().any(|a| a.contains("'ci/perf-test'")),
        "BUILD_BENCH must be true for a PR labelled ci/perf-test, which runs \
         perfbench against this build: {inner}"
    );

    let build_step = step_running(build, "TARGETS=").expect("build composes TARGETS");
    let run = field(build_step, "run");
    let adds_bench = run
        .split_once("if [ \"$BUILD_BENCH\" = \"true\" ]; then")
        .and_then(|(_, rest)| rest.split_once("fi"))
        .is_some_and(|(then, _)| then.trim() == "TARGETS=\"$TARGETS --bin heph-bench -p bench\"");
    assert!(
        adds_bench,
        "the Build step adds heph-bench to TARGETS when BUILD_BENCH is true"
    );
    let perf = job(&wf, "perfbench").expect("a `perfbench` job");
    assert!(
        field(perf, "if").contains("'ci/perf-test'"),
        "perfbench and BUILD_BENCH key on the same label"
    );
}
