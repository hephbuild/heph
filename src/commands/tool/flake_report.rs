//! Aggregates repeated `cargo test` runs into a per-test pass/fail report.
//!
//! Built for the nightly flake-detection job: run the suite N times, capture
//! each run's stdout, then point this at the directory of captures. It
//! parses the default `cargo test` text harness output (no nightly-only
//! `--format json`, since the workspace pins a stable toolchain) and reports
//! which tests failed how often, so a flake is a number instead of a hunch.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Context;
use serde::Serialize;

#[derive(clap::Args)]
#[command(
    override_usage = "heph tool flake-report --logs-dir <DIR>\n       heph tool flake-report --logs-dir <DIR> --json-out <PATH>"
)]
pub struct Args {
    /// Directory containing one `cargo test` stdout capture per run (any
    /// filenames; every file in the directory is treated as one run).
    #[arg(long, value_name = "DIR")]
    pub logs_dir: PathBuf,
    /// Also write the full per-test report as JSON to this path.
    #[arg(long, value_name = "PATH")]
    pub json_out: Option<PathBuf>,
    /// Exit with a non-zero status if any test failed in some runs but not
    /// all (i.e. is flaky rather than consistently broken or consistently
    /// passing). Off by default: the nightly job reports, it doesn't gate.
    #[arg(long)]
    pub fail_on_flake: bool,
}

pub fn execute(args: &Args) -> anyhow::Result<()> {
    let mut run_names = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&args.logs_dir)
        .with_context(|| format!("reading logs dir {}", args.logs_dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .with_context(|| format!("listing logs dir {}", args.logs_dir.display()))?;
    entries.sort();

    let mut agg = Aggregator::default();
    for path in &entries {
        if !path.is_file() {
            continue;
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        agg.add_run(&text);
        run_names.push(path.display().to_string());
    }

    if run_names.is_empty() {
        anyhow::bail!(
            "no run logs found in {} — nothing to aggregate",
            args.logs_dir.display()
        );
    }

    let report = agg.finish(run_names);

    println!("{}", report.to_markdown());

    if let Some(json_out) = &args.json_out {
        let json = serde_json::to_string_pretty(&report).context("serializing JSON report")?;
        std::fs::write(json_out, json)
            .with_context(|| format!("writing {}", json_out.display()))?;
    }

    if args.fail_on_flake && report.tests.iter().any(TestOutcome::is_flaky) {
        anyhow::bail!("flaky test(s) found — see report above");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Failed,
    Ignored,
}

/// One `test <name> ... <status>` line from a single `cargo test` binary's
/// output, keyed by `<binary>::<name>` so identically-named tests in
/// different crates/binaries don't collide.
fn parse_run(text: &str) -> Vec<(String, Status)> {
    let mut current_binary = "unknown".to_string();
    let mut out = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("Running ") {
            if let Some(binary) = binary_short_name(rest) {
                current_binary = binary;
            }
            continue;
        }
        // `   Doc-tests <crate>` has no `Running ` line of its own — without
        // this, every doctest result would be misattributed to whichever
        // regular test binary happened to run last.
        if let Some(krate) = trimmed.strip_prefix("Doc-tests ") {
            current_binary = format!("doctest:{}", krate.trim());
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("test ") else {
            continue;
        };
        // Cargo's periodic liveness line reuses the `test ` prefix but has no
        // ` ... <status>` suffix (`test foo::bar has been running for over 60
        // seconds`), so it's already excluded by the `rsplit_once` below.
        let Some((name, status)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let status = if status == "ok" {
            Status::Ok
        } else if status == "FAILED" {
            Status::Failed
        } else if status.starts_with("ignored") {
            Status::Ignored
        } else {
            continue;
        };
        out.push((format!("{current_binary}::{name}"), status));
    }

    out
}

/// `Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)`
/// or `Running tests/foo.rs (target/debug/deps/foo-abcdef1234567890)` ->
/// the pre-hash binary name (`core`, `foo`). Returns `None` for lines that
/// don't match cargo's `Running ... (path)` shape (e.g. `Running` steps for
/// non-test build output should never reach here, but be defensive).
fn binary_short_name(after_running: &str) -> Option<String> {
    let open = after_running.rfind('(')?;
    let close = after_running.rfind(')')?;
    if close < open {
        return None;
    }
    let path = after_running.get(open + 1..close)?;
    let basename = path.rsplit('/').next().unwrap_or(path);
    // Cargo appends a 16-hex-digit hash: `<name>-0123456789abcdef`.
    match basename.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            Some(name.to_string())
        }
        _ => Some(basename.to_string()),
    }
}

#[derive(Default)]
struct Aggregator {
    counts: BTreeMap<String, Counts>,
    runs_seen: usize,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    ok: u32,
    failed: u32,
    ignored: u32,
}

impl Aggregator {
    fn add_run(&mut self, run_text: &str) {
        self.runs_seen += 1;
        for (key, status) in parse_run(run_text) {
            let counts = self.counts.entry(key).or_default();
            match status {
                Status::Ok => counts.ok += 1,
                Status::Failed => counts.failed += 1,
                Status::Ignored => counts.ignored += 1,
            }
        }
    }

    fn finish(self, run_names: Vec<String>) -> Report {
        let mut tests: Vec<TestOutcome> = self
            .counts
            .into_iter()
            .map(|(key, c)| {
                let seen = c.ok + c.failed;
                let failure_rate = if seen == 0 {
                    0.0
                } else {
                    f64::from(c.failed) / f64::from(seen)
                };
                TestOutcome {
                    key,
                    ok: c.ok,
                    failed: c.failed,
                    ignored: c.ignored,
                    seen,
                    failure_rate,
                }
            })
            .collect();

        // Worst offenders first; break ties deterministically by name so the
        // report is stable run to run.
        tests.sort_by(|a, b| {
            b.failure_rate
                .partial_cmp(&a.failure_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.failed.cmp(&a.failed))
                .then_with(|| a.key.cmp(&b.key))
        });

        Report {
            runs: self.runs_seen,
            run_names,
            tests,
        }
    }
}

#[derive(Serialize)]
struct TestOutcome {
    key: String,
    ok: u32,
    failed: u32,
    ignored: u32,
    /// `ok + failed`; excludes `ignored` and excludes runs where the test
    /// binary never reached this test (e.g. it aborted earlier in the run).
    seen: u32,
    failure_rate: f64,
}

impl TestOutcome {
    /// Failed in some runs but not all — the signature of a flake rather
    /// than a consistently broken or consistently passing test.
    fn is_flaky(&self) -> bool {
        self.failed > 0 && self.failed < self.seen
    }

    /// Failed in every run it appeared in, *and* appeared in every run —
    /// i.e. actually broken, not just unlucky about which runs it survived
    /// to reach. `total_runs` is [`Report::runs`].
    fn is_consistently_failing(&self, total_runs: usize) -> bool {
        self.seen > 0 && self.failed == self.seen && self.seen as usize == total_runs
    }

    /// This test's binary didn't reach it in every run (e.g. an earlier test
    /// crashed the process), so `failed`/`seen` undercounts the true
    /// picture — a 100% failure rate here is not the same confidence as one
    /// backed by every run.
    fn has_reduced_visibility(&self, total_runs: usize) -> bool {
        (self.seen as usize) < total_runs
    }
}

#[derive(Serialize)]
struct Report {
    runs: usize,
    run_names: Vec<String>,
    tests: Vec<TestOutcome>,
}

impl Report {
    fn to_markdown(&self) -> String {
        let flaky: Vec<&TestOutcome> = self.tests.iter().filter(|t| t.is_flaky()).collect();
        let always_failing: Vec<&TestOutcome> = self
            .tests
            .iter()
            .filter(|t| t.is_consistently_failing(self.runs))
            .collect();
        let reduced_visibility: Vec<&TestOutcome> = self
            .tests
            .iter()
            .filter(|t| t.has_reduced_visibility(self.runs))
            .collect();

        const WRITE_TO_STRING_CANNOT_FAIL: &str = "writeln! to a String cannot fail";

        let mut out = String::new();
        writeln!(
            out,
            "# Flake report ({} run{}, {} distinct tests)",
            self.runs,
            if self.runs == 1 { "" } else { "s" },
            self.tests.len()
        )
        .expect(WRITE_TO_STRING_CANNOT_FAIL);
        writeln!(
            out,
            "\n{} flaky, {} consistently failing, {} with reduced visibility.\n",
            flaky.len(),
            always_failing.len(),
            reduced_visibility.len()
        )
        .expect(WRITE_TO_STRING_CANNOT_FAIL);

        if flaky.is_empty() {
            out.push_str("No flaky tests.\n");
        } else {
            out.push_str("| Test | Failed/Seen | Rate |\n|---|---|---|\n");
            for t in &flaky {
                writeln!(
                    out,
                    "| {} | {}/{} | {:.0}% |",
                    t.key,
                    t.failed,
                    t.seen,
                    t.failure_rate * 100.0
                )
                .expect(WRITE_TO_STRING_CANNOT_FAIL);
            }
        }

        if !always_failing.is_empty() {
            out.push_str("\nConsistently failing (not flaky — broken every run):\n");
            for t in &always_failing {
                writeln!(out, "- {} ({}/{})", t.key, t.failed, t.seen)
                    .expect(WRITE_TO_STRING_CANNOT_FAIL);
            }
        }

        if !reduced_visibility.is_empty() {
            out.push_str(
                "\nReduced visibility (binary likely aborted before reaching these in some \
                 runs — failure counts below are undercounts, not full confidence):\n",
            );
            for t in &reduced_visibility {
                writeln!(out, "- {} (seen {}/{} runs)", t.key, t.seen, self.runs)
                    .expect(WRITE_TO_STRING_CANNOT_FAIL);
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_ALL_PASS: &str = "\
   Compiling core v0.1.0
     Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)

running 2 tests
test hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded ... ok
test htplatform::tests::test_arch_mapping ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.69s
";

    const RUN_ONE_FAILS: &str = "\
     Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)

running 2 tests
test hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded ... FAILED
test htplatform::tests::test_arch_mapping ... ok

failures:

---- hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded stdout ----
thread panicked at src/hmemoizer.rs:123

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.69s
";

    #[test]
    fn parse_run_keys_tests_by_binary_and_name() {
        let parsed = parse_run(RUN_ALL_PASS);
        assert_eq!(
            parsed,
            vec![
                (
                    "core::hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded"
                        .to_string(),
                    Status::Ok
                ),
                (
                    "core::htplatform::tests::test_arch_mapping".to_string(),
                    Status::Ok
                ),
            ]
        );
    }

    #[test]
    fn parse_run_does_not_double_count_the_failures_recap_block() {
        // The `---- name stdout ----` recap line must not be mistaken for a
        // `test ... ok`/`FAILED` line.
        let parsed = parse_run(RUN_ONE_FAILS);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            (
                "core::hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded".to_string(),
                Status::Failed
            )
        );
    }

    #[test]
    fn parse_run_skips_the_running_for_over_60_seconds_notice() {
        let text = "\
     Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)
test slow::tests::takes_a_while has been running for over 60 seconds
test slow::tests::takes_a_while ... ok
";
        let parsed = parse_run(text);
        assert_eq!(
            parsed,
            vec![("core::slow::tests::takes_a_while".to_string(), Status::Ok)]
        );
    }

    #[test]
    fn binary_short_name_strips_the_cargo_hash_suffix() {
        assert_eq!(
            binary_short_name("unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)"),
            Some("core".to_string())
        );
        assert_eq!(
            binary_short_name("tests/foo.rs (target/debug/deps/foo-abcdef0123456789)"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn a_test_that_fails_in_some_runs_but_not_all_is_reported_as_flaky() {
        let mut agg = Aggregator::default();
        agg.add_run(RUN_ALL_PASS);
        agg.add_run(RUN_ONE_FAILS);
        agg.add_run(RUN_ALL_PASS);
        let report = agg.finish(vec!["a".into(), "b".into(), "c".into()]);

        let flaky = report
            .tests
            .iter()
            .find(|t| {
                t.key == "core::hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded"
            })
            .expect("test present");
        assert!(flaky.is_flaky());
        assert_eq!(flaky.failed, 1);
        assert_eq!(flaky.seen, 3);
        assert!((flaky.failure_rate - 1.0 / 3.0).abs() < 1e-9);

        let stable = report
            .tests
            .iter()
            .find(|t| t.key == "core::htplatform::tests::test_arch_mapping")
            .expect("test present");
        assert!(!stable.is_flaky());
        assert_eq!(stable.failed, 0);
    }

    #[test]
    fn a_test_that_fails_every_run_is_consistently_failing_not_flaky() {
        let mut agg = Aggregator::default();
        agg.add_run(RUN_ONE_FAILS);
        agg.add_run(RUN_ONE_FAILS);
        let report = agg.finish(vec!["a".into(), "b".into()]);

        let t = report
            .tests
            .iter()
            .find(|t| {
                t.key == "core::hmemoizer::tests::a_cell_with_waiters_and_no_driver_is_stranded"
            })
            .expect("test present");
        assert!(!t.is_flaky());
        assert_eq!(t.failed, 2);
        assert_eq!(t.seen, 2);

        let md = report.to_markdown();
        assert!(md.contains("No flaky tests."));
        assert!(md.contains("Consistently failing"));
    }

    #[test]
    fn markdown_report_sorts_by_failure_rate_descending() {
        let mostly_fails = "\
     Running unittests src/lib.rs (target/debug/deps/foo-c48417a792d7e9e5)
test always_fails ... FAILED
test rarely_fails ... ok
";
        let mut agg = Aggregator::default();
        for _ in 0..3 {
            agg.add_run(mostly_fails);
        }
        agg.add_run(
            "\
     Running unittests src/lib.rs (target/debug/deps/foo-c48417a792d7e9e5)
test always_fails ... FAILED
test rarely_fails ... FAILED
",
        );
        let report = agg.finish(vec!["1".into(), "2".into(), "3".into(), "4".into()]);

        // always_fails: 4/4 failed (rate 1.0, consistently failing).
        // rarely_fails: 1/4 failed (rate 0.25, flaky).
        assert_eq!(report.tests[0].key, "foo::always_fails");
        assert_eq!(report.tests[1].key, "foo::rarely_fails");

        let md = report.to_markdown();
        let flaky_idx = md.find("foo::rarely_fails").expect("listed as flaky");
        let failing_idx = md
            .find("foo::always_fails")
            .expect("listed as consistently failing");
        assert!(
            flaky_idx < failing_idx,
            "flaky table should render before the consistently-failing list"
        );
    }

    #[test]
    fn doctest_results_are_attributed_to_the_doctest_crate_not_the_last_binary() {
        // Real `cargo test` section order: unit tests, then integration test
        // binaries, then doctests — with no `Running ` line of their own.
        let text = "\
     Running unittests src/lib.rs (target/debug/deps/heph-6f1c18785b1a867e)
test a::unit_test ... ok

     Running tests/supervisor.rs (target/debug/deps/supervisor-ef8fc1ebcf07feca)
test integration_test ... ok

   Doc-tests heph
test src/lib.rs - foo (line 12) ... FAILED
";
        let parsed = parse_run(text);
        assert_eq!(
            parsed,
            vec![
                ("heph::a::unit_test".to_string(), Status::Ok),
                ("supervisor::integration_test".to_string(), Status::Ok),
                (
                    "doctest:heph::src/lib.rs - foo (line 12)".to_string(),
                    Status::Failed
                ),
            ]
        );
    }

    #[test]
    fn two_binaries_with_an_identically_named_test_do_not_collide() {
        let text = "\
     Running unittests src/lib.rs (target/debug/deps/foo-c48417a792d7e9e5)
test shared_name ... FAILED

     Running unittests src/lib.rs (target/debug/deps/bar-3e33b7047d1b4596)
test shared_name ... ok
";
        let parsed = parse_run(text);
        assert_eq!(
            parsed,
            vec![
                ("foo::shared_name".to_string(), Status::Failed),
                ("bar::shared_name".to_string(), Status::Ok),
            ]
        );
    }

    #[test]
    fn a_test_missing_from_some_runs_has_reduced_visibility_not_full_confidence() {
        // Simulates a binary that aborted before reaching `late_test` in 1 of
        // 3 runs: two runs report it FAILED, one run's text never mentions it
        // at all (as if the process crashed before printing that line).
        let full_run = "\
     Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)
test late_test ... FAILED
";
        let crashed_run = "\
     Running unittests src/lib.rs (target/debug/deps/core-c48417a792d7e9e5)
";
        let mut agg = Aggregator::default();
        agg.add_run(full_run);
        agg.add_run(full_run);
        agg.add_run(crashed_run);
        let report = agg.finish(vec!["a".into(), "b".into(), "c".into()]);

        let t = report
            .tests
            .iter()
            .find(|t| t.key == "core::late_test")
            .expect("test present");
        assert_eq!(t.failed, 2);
        assert_eq!(t.seen, 2);
        assert!(t.has_reduced_visibility(report.runs));
        // 2/2 observed runs failed, but it was only observed in 2 of 3 total
        // runs — must NOT be reported as "consistently failing" (which
        // claims every run), even though failed == seen.
        assert!(!t.is_consistently_failing(report.runs));

        let md = report.to_markdown();
        assert!(
            !md.contains("Consistently failing"),
            "a test seen in only 2/3 runs must not be labelled consistently failing:\n{md}"
        );
        assert!(md.contains("Reduced visibility"));
        assert!(md.contains("core::late_test (seen 2/3 runs)"));
    }

    #[test]
    fn execute_on_an_empty_logs_dir_returns_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = Args {
            logs_dir: dir.path().to_path_buf(),
            json_out: None,
            fail_on_flake: false,
        };
        let err = execute(&args).expect_err("an empty logs dir has nothing to aggregate");
        assert!(err.to_string().contains("no run logs found"));
    }

    #[test]
    fn execute_ignores_subdirectories_and_writes_the_json_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("run-1.log"), RUN_ALL_PASS).expect("write run log");
        std::fs::create_dir(dir.path().join("not-a-run")).expect("create subdir");
        std::fs::write(
            dir.path().join("not-a-run").join("run-2.log"),
            RUN_ONE_FAILS,
        )
        .expect("write nested log (must be ignored)");

        let json_out = dir.path().join("report.json");
        let args = Args {
            logs_dir: dir.path().to_path_buf(),
            json_out: Some(json_out.clone()),
            fail_on_flake: false,
        };
        execute(&args).expect("subdirectories are skipped, not read as a run");

        let json = std::fs::read_to_string(&json_out).expect("json report written");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        // Only run-1.log (RUN_ALL_PASS) should have been aggregated — proves
        // the subdirectory and its nested log were not read as a run.
        assert_eq!(value["runs"], 1);
        let tests = value["tests"].as_array().expect("tests array");
        assert!(
            tests.iter().all(|t| t["failed"].as_u64() == Some(0)),
            "only the all-pass run should have been aggregated: {value}"
        );
    }

    #[test]
    fn execute_fail_on_flake_gates_on_flakiness_not_on_any_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("run-1.log"), RUN_ALL_PASS).expect("write run log");
        std::fs::write(dir.path().join("run-2.log"), RUN_ONE_FAILS).expect("write run log");

        let not_gated = Args {
            logs_dir: dir.path().to_path_buf(),
            json_out: None,
            fail_on_flake: false,
        };
        execute(&not_gated).expect("fail_on_flake=false must succeed despite a flaky test");

        let gated = Args {
            logs_dir: dir.path().to_path_buf(),
            json_out: None,
            fail_on_flake: true,
        };
        let err = execute(&gated).expect_err("fail_on_flake=true must fail on a flaky test");
        assert!(err.to_string().contains("flaky"));
    }

    #[test]
    fn json_report_uses_the_field_names_the_workflow_greps_for_with_jq() {
        // .github/workflows/flake-detect.yml does
        // `jq -r '.tests[] | select(.failed > 0) | .key'` against this JSON —
        // a silent field rename here would pass every Rust test while
        // breaking that CI step with no red build to catch it.
        let mut agg = Aggregator::default();
        agg.add_run(RUN_ONE_FAILS);
        let report = agg.finish(vec!["a".into()]);

        let value = serde_json::to_value(&report).expect("serializable");
        assert!(value["runs"].is_number());
        let test = &value["tests"][0];
        assert!(test["key"].is_string());
        assert!(test["failed"].is_number());
        assert!(test["seen"].is_number());
    }
}
