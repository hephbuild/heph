//! Guards on `.github/workflows/stack-status.yml`, which lets a stack merge on
//! its top's run.
//!
//! `gh stack merge` holds every layer of a stack to the `master` ruleset's
//! required checks. The gate in `heph.yml` skips the middle layers, so they
//! have none. `tools/stack-status` posts the top's results onto them; its own
//! `go test` covers what it posts and when. Here: that the workflow still
//! reaches it. Each way this breaks fails quietly, as a stack that will not
//! merge, long after the change that broke it:
//!
//!   - `workflow_run` matches workflows by `name:`, so renaming `heph.yml`'s
//!     `name: CI` stops every mirror;
//!   - without `statuses: write` every post is a 403;
//!   - the program's tests run in `heph.yml` (the workflow that runs it comes
//!     after, on the default branch only), so dropping that step leaves it
//!     untested.

use serde_yaml::Value;
use std::path::Path;

fn load(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{name} parses as YAML: {e}"))
}

fn steps<'a>(wf: &'a Value, job: &str) -> &'a [Value] {
    wf.get("jobs")
        .and_then(|j| j.get(job))
        .and_then(|j| j.get("steps"))
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("job `{job}` has steps"))
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

#[test]
fn the_mirror_follows_ci_and_can_post_statuses() {
    let wf = load("stack-status.yml");
    let ci = load("heph.yml");
    let ci_name = str_of(&ci, "name");
    assert!(!ci_name.is_empty(), "heph.yml has a `name:`");
    let followed: Vec<&str> = wf
        .get("on")
        .and_then(|o| o.get("workflow_run"))
        .and_then(|w| w.get("workflows"))
        .and_then(Value::as_sequence)
        .map(|s| s.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert_eq!(
        followed,
        [ci_name],
        "`workflow_run` matches workflows by `name:`; renaming heph.yml without this silently stops every mirror"
    );
    assert_eq!(
        wf.get("permissions")
            .and_then(|p| p.get("statuses"))
            .and_then(Value::as_str),
        Some("write"),
    );
    let cond = wf
        .get("jobs")
        .and_then(|j| j.get("mirror"))
        .map(|j| str_of(j, "if"))
        .unwrap_or_default();
    assert!(
        cond.contains("github.event.workflow_run.event == 'pull_request'")
            && cond.contains("base.ref != github.event.repository.default_branch"),
        "pushes and master-targeting PRs must not take a runner: {cond}"
    );
    let runs = steps(&wf, "mirror").iter().any(|s| {
        str_of(s, "working-directory") == "tools/stack-status" && str_of(s, "run") == "go run ."
    });
    assert!(runs, "the mirror job runs `go run .` in tools/stack-status");
}

#[test]
fn the_mirrors_tests_run_in_ci() {
    let ci = load("heph.yml");
    let tested = steps(&ci, "govet").iter().any(|s| {
        str_of(s, "working-directory") == "tools/stack-status"
            && str_of(s, "run").starts_with("go test")
    });
    assert!(
        tested,
        "heph.yml's `govet` job runs `go test` in tools/stack-status"
    );
}
