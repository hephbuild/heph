//! Guards on the coverage *configuration* — what gets measured, and whether a
//! broken measurement can reach Codecov looking like a real one.
//!
//! Coverage is the one number in this repo that cannot fail loudly on its own.
//! `codecov.yml` marks every status `informational`, and the `coverage` job is
//! deliberately absent from `release`'s `needs:` — both on purpose, so a
//! percentage never blocks a merge. The consequence is that a report which
//! measured almost nothing publishes as a plausible number and reads as a drop,
//! not as a broken pipeline. Codecov will not complain and CI will not go red.
//!
//! So the checks live in `cov` itself, and this file's job is to keep them
//! there. Five axes, each with a failure that is silent by construction:
//!
//!   - **what is compiled** — an env `RUSTFLAGS` *replaces* the configured
//!     flags rather than merging with them, so the instrumented build can
//!     silently lose `--cfg tokio_unstable` and measure a differently
//!     configured tokio than the one that ships;
//!   - **what is instrumented** — without an explicit target, cargo applies
//!     `-Cinstrument-coverage` to build scripts and proc-macros too, so rustc
//!     itself starts writing profiles and build-script lines land in the report
//!     as covered code;
//!   - **what is measured** — `cov` must run the suite `tst` runs, and must
//!     strip `#[cfg(test)]` modules, which are 39% of this tree's lines;
//!   - **what can go red** — every guard in `cov` must still be wired to an
//!     `exit`, not merely mentioned;
//!   - **whether the report reaches Codecov intact** — a searched-for upload, a
//!     shrugged-off failure, or a flag that no longer matches `codecov.yml`.
//!
//! Cheap on purpose: file reads and string work, no build. The *behaviour* of
//! the floors is tested in `tests/coverage_report.rs`, which runs the real
//! script — a gate that only checks a flag is spelled correctly leaves the
//! whole property resting on nothing.

mod common;

use common::{
    devenv_script, job_needs, job_run_steps, job_steps, matrix_coords, matrix_include, repo_root,
    script_legs, without_comments, workflow_jobs,
};

/// The values of `key` across a job body's `matrix.include` entries.
///
/// Local rather than in `tests/common`: only this gate needs it, and an unused
/// `pub fn` there is a `dead_code` error in the *other* test binary — with no
/// way out, since `clippy::allow_attributes` bans `#[allow]` and an `#[expect]`
/// would be unfulfilled in the binary that does use it.
fn matrix_values(body: &str, key: &str) -> Vec<String> {
    matrix_include(body)
        .iter()
        .filter_map(|entry| {
            entry
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
        .collect()
}

fn devenv() -> String {
    std::fs::read_to_string(repo_root().join("devenv.nix")).expect("read devenv.nix")
}

fn workflow() -> String {
    std::fs::read_to_string(repo_root().join(".github/workflows/heph.yml"))
        .expect("read .github/workflows/heph.yml")
}

fn codecov() -> String {
    std::fs::read_to_string(repo_root().join("codecov.yml")).expect("read codecov.yml")
}

/// The `cov` script body, comments stripped.
///
/// Stripped because this script *documents* the traps it avoids, naming the
/// very flags and constructs asserted on below. Prose must be able to neither
/// satisfy an assertion nor violate one.
fn cov() -> String {
    without_comments(&devenv_script(&devenv(), "cov"))
}

/// The quoted entries of `build.rustflags` in `.cargo/config.toml`.
///
/// Scanned for quoted strings rather than split on commas: a flag may contain
/// one (`-Clink-args=-a,-b`), and splitting would yield two garbage halves that
/// then match nothing and quietly weaken the assertion below.
fn config_rustflags() -> Vec<String> {
    let config = std::fs::read_to_string(repo_root().join(".cargo/config.toml"))
        .expect("read .cargo/config.toml");
    let (_, rest) = config
        .split_once("rustflags = [")
        .expect(".cargo/config.toml defines `build.rustflags`");
    let (list, _) = rest
        .split_once(']')
        .expect("the `build.rustflags` array is closed");

    let mut flags = Vec::new();
    let mut chars = list.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut flag = String::new();
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            flag.push(c);
        }
        flags.push(flag);
    }
    flags
}

/// Whether a line containing `needle` is followed by an `exit` within `window`
/// lines.
///
/// The point of this whole file in one helper. A check that is present but no
/// longer wired to a non-zero exit is worse than an absent one, because it
/// still reads like a guard — and asserting the noun (`profraw_count`,
/// `grcov.log`) cannot see that, because those names survive in the very
/// `echo` lines that report the number. That version of the test below existed,
/// passed over a `cov` with every check deleted, and was worthless.
fn guarded_by_exit(script: &str, needle: &str, window: usize) -> bool {
    let lines: Vec<&str> = script.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .any(|(at, _)| {
            lines
                .iter()
                .skip(at)
                .take(window)
                .any(|line| line.trim_start().starts_with("exit "))
        })
}

/// `cov` must carry the configured rustflags over, not replace them.
///
/// Cargo takes its extra flags from exactly one source and never merges them,
/// so a bare `RUSTFLAGS=-Cinstrument-coverage` drops everything
/// `.cargo/config.toml` sets. `build.rs`'s `frame_pointers()` exists because of
/// the same trap seen from the other side, and the two halves fail very
/// differently: losing `-Cforce-frame-pointers=yes` reddens `pprof_dump`'s own
/// test, while losing `--cfg tokio_unstable` compiles fine and quietly measures
/// a build that never ships.
///
/// The assertion is that `cov` *reads* the list rather than restating it. A
/// hand-copied superset satisfies "every flag is present" on the day it is
/// written and silently stops being one the next time a flag is added here.
#[test]
fn cov_carries_over_the_configured_rustflags() {
    let cov = cov();
    let configured = config_rustflags();

    assert!(
        !configured.is_empty(),
        ".cargo/config.toml has no `build.rustflags`, so this test proves \
         nothing. If they moved, point `cov` and this test at the new home."
    );

    assert!(
        cov.contains(".cargo/config.toml"),
        "`cov` does not read `.cargo/config.toml`. An env `RUSTFLAGS` replaces \
         the configured flags wholesale, so the instrumented build would lose \
         {configured:?} — `--cfg tokio_unstable` among them, which compiles \
         fine and measures a differently-configured tokio than the one that \
         ships."
    );

    // Whole shell words, not substrings: `.cargo/config.toml` splits its flags
    // (`"-C", "target-cpu=…"`), and a substring check would find `-C` inside
    // `-Cinstrument-coverage` and fail over nothing.
    let words: Vec<&str> = cov
        .split_whitespace()
        .map(|w| w.trim_matches('"'))
        .collect();
    for flag in &configured {
        assert!(
            !words.contains(&flag.as_str()),
            "`cov` restates `{flag}` instead of reading it out of \
             `.cargo/config.toml`. A hand-copied list is a superset only until \
             the next flag is added there, and nothing goes red when it stops \
             being one."
        );
    }
}

/// `cov` must build for an explicit target.
///
/// Cargo only splits host units from target units when a target is named.
/// Without one, `-Cinstrument-coverage` also applies to build scripts and to
/// `htspec-derive` — a proc-macro that *rustc itself* dlopens — so every rustc
/// invocation in the workspace registers a profile writer and drops a
/// `.profraw`. The test profiles end up buried under thousands of files, and
/// build-script lines are reported as covered code.
#[test]
fn cov_separates_host_units_from_target_units() {
    let cov = cov();
    assert!(
        cov.contains("CARGO_BUILD_TARGET") || cov.contains("--target "),
        "`cov` names no target, so cargo applies `-Cinstrument-coverage` to \
         build scripts and proc-macros as well. `htspec-derive` is dlopen'd by \
         rustc, so every compile in the workspace would then write its own \
         .profraw and build-script lines would count as covered code."
    );
}

/// `cov` must measure the suite that gates this repo, not a copy of it.
///
/// `tst` is three invocations, and the two feature-gated ones are the whole
/// coverage of the stabby transport. A `cov` that restated the selection could
/// drop them, and "the transport is at 0%" is indistinguishable from "the
/// transport is untested". A restated `--exclude` is worse still: it shrinks
/// the denominator, so coverage *rises*.
#[test]
fn cov_measures_the_same_suite_as_tst() {
    let cov = cov();

    // First word only. The invariant is "cov delegates to tst", and matching
    // the whole line would go red on a redirect change that preserves it.
    assert!(
        cov.lines()
            .any(|line| line.split_whitespace().next() == Some("tst")),
        "`cov` does not invoke `tst` for its unfiltered run, so the report \
         describes a package selection maintained separately from the one that \
         actually gates this repo. Drift there is invisible: the number simply \
         stops covering a crate."
    );

    for restated in ["--workspace", "--exclude", "--features"] {
        assert!(
            !cov.contains(restated),
            "`cov` restates `{restated}`, so it has its own package/feature \
             selection alongside `tst`'s. The two will diverge, and a narrower \
             selection makes coverage go *up*."
        );
    }
}

/// `#[cfg(test)]` modules must be stripped from what Codecov reads.
///
/// 39% of this tree's Rust lines are inside one, and source-based coverage
/// instruments them along with the code they test. No path-based exclusion can
/// reach them — they live inside production source files — so if this stops
/// happening, nothing else in the pipeline notices. The effect is worse than an
/// inflated headline: `.claude/testing.md` requires every change to ship with a
/// test, so patch coverage — the number a reviewer reads — comes out flattering
/// in proportion to how much test code the PR added.
#[test]
fn cov_strips_cfg_test_modules_from_the_uploaded_report() {
    assert!(
        cov().contains("--strip-cfg-test"),
        "`cov` no longer strips `#[cfg(test)]` modules, so ~39% of this tree's \
         lines are counted as covered production code and patch coverage on a \
         PR rises with the amount of test code it adds."
    );

    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    let (_, body) = jobs
        .iter()
        .find(|(id, _)| legs.contains(id))
        .expect("the coverage leg is a job");
    assert!(
        !without_comments(body).contains("lcov.raw.info"),
        "the CI upload points at grcov's raw report rather than the filtered \
         one, so the stripping happens and is then thrown away."
    );
}

/// Every guard in `cov` must still be wired to an exit.
///
/// This is the only place in the whole pipeline that can fail. Codecov is
/// informational, the job gates nothing, and `fail_ci_if_error` catches upload
/// *transport* errors only — a valid, tiny, entirely wrong lcov sails through
/// all three.
#[test]
fn every_guard_in_cov_is_wired_to_a_non_zero_exit() {
    let cov = cov();

    let guards = [
        (
            "[ \"$profraw_count\" -eq 0 ]",
            "no profile was written at all — instrumentation never reached the \
             test binaries, which is not the same thing as 0% coverage",
        ),
        (
            "-empty",
            "a zero-length profile, which llvm-profdata skips in complete \
             silence, so the report comes out smaller than the truth and \
             entirely plausible",
        ),
        (
            "[ -n \"$strays\" ]",
            "a child with a cleared environment wrote its profile somewhere \
             else — counters missing from the report, and an undeclared output \
             if it landed in a sandbox",
        ),
        (
            "[ \"$tests_run\" -eq 0 ]",
            "the suite ran zero tests; a cargo filter matching nothing still \
             exits 0",
        ),
        (
            "grep -qiE",
            "grcov skipped profiles it could not parse and exited 0 anyway, \
             which yields a smaller, entirely plausible number",
        ),
        (
            "llvm-profdata",
            "grcov fell back to some other LLVM, which reads a subset of the \
             profiles rather than failing",
        ),
    ];

    for (guard, why) in guards {
        assert!(
            guarded_by_exit(&cov, guard, 12),
            "`cov` has no `{guard}` guard followed by an `exit`. Without it, \
             {why} — and nothing downstream can notice, because Codecov is \
             informational and this job gates nothing."
        );
    }

    // The floors live in the report script; `tests/coverage_report.rs` proves
    // they fire. Here it is only that `cov` still asks for them.
    for floor in ["--min-files", "--min-lines", "--require-covered"] {
        assert!(
            cov.contains(floor),
            "`cov` no longer passes `{floor}`, so a report far too small to be \
             a measurement would be published as a number."
        );
    }
}

/// The coverage job must cover both supported OSes.
///
/// Coverage is per-line, so a report describes whichever OS ran the suite.
/// `crates/proc/src/proc_exec/imp_macos.rs` is ~790 lines a Linux run does not
/// compile: it emits no coverage mapping, never appears in the lcov, and
/// Codecov's patch status on a PR touching only that file reports "nothing to
/// cover" — a green check over entirely unmeasured code. One leg per platform,
/// and `fail-fast: false` so a red leg does not cancel the other into a
/// `cancelled` that says nothing about the platform you did not see.
#[test]
fn the_coverage_job_covers_both_supported_oses() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");

    assert_eq!(
        legs.len(),
        1,
        "expected exactly one job to run the `cov` script — the matrix. Found \
         {legs:?}"
    );

    let (id, body) = jobs
        .iter()
        .find(|(id, _)| legs.contains(id))
        .expect("the coverage leg is a job");
    let body = without_comments(body);

    assert!(
        body.lines()
            .any(|line| line.replace(' ', "") == "fail-fast:false"),
        "the `{id}` matrix does not set `fail-fast: false`. The first red leg \
         would cancel the other, and a two-OS report is exactly the case where \
         the platform you did not see is the one that mattered."
    );

    let coords = matrix_coords(&body);
    assert!(
        coords.iter().any(|c| c.starts_with("linux/")),
        "the `{id}` matrix has no linux leg; found {coords:?}"
    );
    assert!(
        coords.iter().any(|c| c.starts_with("darwin/")),
        "the `{id}` matrix has no darwin leg, so every `cfg(target_os = \
         \"macos\")` path — `crates/proc/src/proc_exec/imp_macos.rs` most of \
         all — is absent from the report rather than uncovered in it, and \
         Codecov reports \"nothing to cover\" over it. Found {coords:?}"
    );
}

/// The matrix's Codecov flags and `codecov.yml` must agree.
///
/// Nothing else binds them, and both directions of drift are silent because
/// Codecov applies its config with no diagnostic. Rename a `matrix.flag` and
/// Codecov creates an *unmanaged* flag with default rules — no `carryforward` —
/// so the first run that skips that leg reports its history as a cliff. And
/// `after_n_builds` must equal the leg count: too high and the PR comment never
/// arrives, too low and it is posted on a partial merge, before the other
/// platform has uploaded.
#[test]
fn the_matrix_flags_match_codecov_yml() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    let (id, body) = jobs
        .iter()
        .find(|(id, _)| legs.contains(id))
        .expect("the coverage leg is a job");
    let body = without_comments(body);

    assert!(
        body.contains("flags: ${{ matrix.flag }}"),
        "the `{id}` legs do not upload under a per-leg Codecov flag, so the \
         merged report cannot be split back by platform — which is the only \
         reason there are two legs."
    );

    let mut declared = matrix_values(&body, "flag");
    declared.sort();
    assert_eq!(
        declared.len(),
        matrix_coords(&body).len(),
        "not every `{id}` leg declares a `flag:`; an upload without one lands \
         in the unflagged pool and the OS axis is lost in the merge"
    );

    let codecov = codecov();
    let (_, rest) = codecov
        .split_once("individual_flags:")
        .expect("codecov.yml declares `individual_flags`");
    let mut configured: Vec<String> = rest
        .lines()
        .map_while(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                return Some(None);
            }
            trimmed
                .strip_prefix("- name: ")
                .map(|name| Some(name.trim().to_string()))
        })
        .flatten()
        .collect();
    configured.sort();

    assert_eq!(
        declared, configured,
        "the coverage matrix uploads flags {declared:?} but codecov.yml manages \
         {configured:?}. A flag Codecov does not manage gets default rules — no \
         `carryforward` — so the next run that skips that leg reports its \
         history as a cliff, with no diagnostic anywhere."
    );

    let after_n = codecov
        .split_once("after_n_builds:")
        .and_then(|(_, rest)| rest.lines().next())
        .and_then(|line| line.trim().parse::<usize>().ok())
        .expect("codecov.yml sets `after_n_builds` to a number");
    assert_eq!(
        after_n,
        declared.len(),
        "codecov.yml waits for {after_n} uploads but the matrix has {} legs. \
         Too high and the PR comment never arrives; too low and it is posted \
         before the other platform uploaded, on a partial merge.",
        declared.len()
    );
}

/// A coverage leg must not be able to conclude `success` without measuring.
///
/// The same two one-line routes `lint_gate` guards: `continue-on-error: true`
/// turns a failure into a success, and an `if:` on the step skips it entirely.
/// Here they are worse than on `lint`, because `cov` *is* the only thing that
/// can fail — skip it and an unmeasured commit publishes whatever Codecov's
/// carry-forward has.
#[test]
fn a_coverage_leg_cannot_report_success_without_measuring() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    assert!(
        !legs.is_empty(),
        "no job in the workflow runs the `cov` script at all"
    );

    for (id, body) in jobs.iter().filter(|(id, _)| legs.contains(id)) {
        let body = without_comments(body);

        assert!(
            !body
                .lines()
                .any(|line| line.trim().starts_with("continue-on-error:")),
            "the `{id}` job has a `continue-on-error:`, which turns a failed \
             measurement into a job that concludes `success` — and a failed \
             measurement is the only signal this feature has."
        );

        let cov_step = job_steps(&body)
            .into_iter()
            .find(|step| job_run_steps(step).contains(&"cov"))
            .unwrap_or_else(|| panic!("`{id}` has a step running `cov`"));

        assert!(
            !cov_step.lines().any(|line| line.trim().starts_with("if:")),
            "the Coverage step of `{id}` carries an `if:`. A skipped step does \
             not fail its job, so the leg concludes `success` having measured \
             nothing.\n{cov_step}"
        );
    }
}

/// The upload must name its file and must fail loudly.
///
/// Left to auto-discovery the action can pick up an unrelated report — or none,
/// which is an error only by default configuration. And an upload failure that
/// is shrugged off leaves the PR comment showing the *previous* commit's
/// number, which is indistinguishable from "coverage did not move".
#[test]
fn the_codecov_upload_is_pinned_and_fails_loudly() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    assert!(
        !legs.is_empty(),
        "no job in the workflow runs the `cov` script at all"
    );

    let (id, body) = jobs
        .iter()
        .find(|(id, _)| legs.contains(id))
        .expect("the coverage leg is a job");
    let body = without_comments(body);

    let upload = job_steps(&body)
        .into_iter()
        .find(|step| step.contains("codecov/codecov-action"))
        .unwrap_or_else(|| panic!("`{id}` has a step using codecov/codecov-action"));

    for (needle, why) in [
        (
            "files: coverage/lcov.info",
            "the action searches the workspace instead, and can upload an \
             unrelated report",
        ),
        (
            "disable_search: true",
            "the action keeps searching even with `files:` set, and may upload \
             something else alongside the report",
        ),
        (
            "fail_ci_if_error: true",
            "a 404 or a timeout is shrugged off, leaving the PR comment on the \
             previous commit's number — which reads as 'coverage did not move'",
        ),
    ] {
        assert!(
            upload.contains(needle),
            "the Codecov upload in `{id}` is missing `{needle}`: {why}.\n{upload}"
        );
    }
}

/// Coverage must stay advisory.
///
/// Every rationale in `cov`, in `codecov.yml` and in this file rests on it: the
/// checks live in the script *because* nothing downstream gates. Flip either
/// half and a 90-minute, fork-fragile job becomes a merge and release gate on a
/// number — which is how coverage stops being a measurement and starts being a
/// thing to farm.
#[test]
fn coverage_stays_advisory() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    assert!(!legs.is_empty(), "no job in the workflow runs `cov`");

    let (_, release) = jobs
        .iter()
        .find(|(id, _)| *id == "release")
        .expect("the workflow defines a `release` job");
    let needs = job_needs(release);
    for leg in &legs {
        assert!(
            !needs.contains(leg),
            "`release` now depends on `{leg}`, making an advisory 90-minute job \
             a release gate. If that is intended, the rationale in `cov`, \
             `codecov.yml` and this file all have to change with it — every one \
             of them argues from 'nothing downstream can fail'. Found needs: \
             {needs:?}"
        );
    }

    let settings = codecov()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        settings.matches("informational: true").count(),
        4,
        "codecov.yml no longer marks all four statuses (project and patch, in \
         both the default rules and the per-flag rules) `informational: true`, \
         so Codecov can set a failing check on a percentage."
    );
}

/// `cleanup` must wait for the coverage legs.
///
/// `cleanup` deletes the `repo` artifact that every compiling job downloads as
/// its first step, and the coverage legs are the longest in the graph. Without
/// this entry `cleanup` can fire while a leg is still queued, and
/// `download-artifact` hard-fails with an error that points at artifacts rather
/// than at coverage. It is not a gate — nothing runs after `cleanup`.
#[test]
fn cleanup_waits_for_the_coverage_legs() {
    let workflow = workflow();
    let jobs = workflow_jobs(&workflow);
    let legs = script_legs(&jobs, "cov");
    assert!(
        !legs.is_empty(),
        "no job in the workflow runs the `cov` script at all, so the check \
         below would pass over an empty set"
    );

    let (_, cleanup) = jobs
        .iter()
        .find(|(id, _)| *id == "cleanup")
        .expect("the workflow defines a `cleanup` job");
    let needs = job_needs(cleanup);

    let missing: Vec<&&str> = legs.iter().filter(|leg| !needs.contains(leg)).collect();
    assert!(
        missing.is_empty(),
        "`cleanup` does not depend on {missing:?}, so it can delete the `repo` \
         artifact while a coverage leg is still waiting to download it. Found \
         needs: {needs:?}"
    );
}

/// Every path `codecov.yml` excludes must still exist.
///
/// An `ignore:` glob is invisible when it stops matching. A rename turns one
/// into a no-op that quietly re-adds a permanently-0% crate to the denominator;
/// an over-broad one excludes live code and inflates the number. Neither shows
/// up anywhere — Codecov applies the config silently.
#[test]
fn every_codecov_ignore_still_points_at_something() {
    let root = repo_root();
    let config = codecov();

    let (_, rest) = config
        .split_once("\nignore:\n")
        .expect("codecov.yml has an `ignore:` list");

    let mut checked = 0;
    for line in rest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(entry) = trimmed.strip_prefix("- ") else {
            // The list ends at the first line that is not an entry or a comment.
            if trimmed.is_empty() {
                continue;
            }
            break;
        };
        let pattern = entry.trim().trim_matches('"');
        // Only the fixed prefix matters — the part before the first glob is
        // what a rename breaks.
        let prefix = pattern
            .split_once("**")
            .map_or(pattern, |(prefix, _)| prefix)
            .trim_end_matches('/');
        if prefix.is_empty() || prefix.contains('*') {
            continue;
        }
        assert!(
            root.join(prefix).exists(),
            "codecov.yml ignores `{pattern}`, but `{prefix}` does not exist. \
             The glob matches nothing now: either it was renamed (and the \
             crate is silently back in the report) or it is gone (and the \
             entry is dead)."
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no ignore entry was checked, so this test proves nothing about \
         codecov.yml's exclusions"
    );
}
