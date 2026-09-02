//! Behavioural tests for `scripts/coverage-report.py`.
//!
//! `tests/coverage_gate.rs` asserts that `cov` *invokes* the floors. This file
//! asserts they work, which is a different claim and the one that matters:
//! nothing downstream of `cov` can fail — Codecov is informational and the job
//! gates nothing — so this script is the last thing standing between a broken
//! collection and a plausible number published as a drop. A gate that only
//! checks the flag is spelled correctly leaves that property resting on
//! nothing.
//!
//! Two behaviours are covered, both with failure modes that are silent by
//! construction:
//!
//!   - **the floors and the canary**, which must fire on an empty or tiny
//!     report and must *not* fire on a healthy one;
//!   - **`--strip-cfg-test`**, which deletes lines from the denominator. Too
//!     little and 39% of this tree's lines are test modules counted as covered
//!     production code; too much and it silently removes real code, which moves
//!     coverage in the flattering direction. Both directions are asserted.
//!
//! Fixtures are written into a `TempDir` rather than pointed at the real tree:
//! the parse has to be exercised against inputs that do not exist here yet
//! (a `DA:` with a checksum field, an unbalanced module, an absolute path).

use std::path::Path;
use std::process::{Command, Output};

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Run the script over `lcov`, with `source_root` as the tree its paths are
/// relative to.
fn run(dir: &Path, lcov: &str, args: &[&str]) -> Output {
    let lcov_path = dir.join("in.info");
    std::fs::write(&lcov_path, lcov).expect("write lcov fixture");
    Command::new("python3")
        .arg(repo_root().join("scripts/coverage-report.py"))
        .arg(&lcov_path)
        .arg("--source-root")
        .arg(dir)
        .args(args)
        .output()
        .expect("run coverage-report.py")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A minimal well-formed record for `path` with the given `line,hits` pairs.
fn record(path: &str, lines: &[(u32, u32)]) -> String {
    let mut out = format!("SF:{path}\n");
    for (line, hits) in lines {
        out.push_str(&format!("DA:{line},{hits}\n"));
    }
    out.push_str("end_of_record\n");
    out
}

#[test]
fn an_empty_report_fails_rather_than_reporting_zero_percent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run(dir.path(), "", &[]);

    assert!(
        !out.status.success(),
        "an lcov with no source files exited 0. That publishes 'collection \
         broke' as '0% coverage', and Codecov cannot tell the two apart."
    );
    assert!(
        stderr(&out).contains("not the same thing as 0% coverage"),
        "the empty-report failure does not say what happened:\n{}",
        stderr(&out)
    );
}

/// A file listed with only excluded lines must not survive as a phantom entry.
#[test]
fn a_record_left_with_no_lines_is_dropped_entirely() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("only_tests.rs"),
        "#[cfg(test)]\nmod tests {\n    fn a() {}\n}\n",
    )
    .expect("write source");

    let out = run(
        dir.path(),
        &record("only_tests.rs", &[(2, 1), (3, 1)]),
        &["--strip-cfg-test"],
    );

    assert!(
        !out.status.success(),
        "a report whose every line was excluded exited 0 with nothing left to \
         measure:\n{}",
        stdout(&out)
    );
}

#[test]
fn the_floors_fire_and_say_which_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = record("src/a.rs", &[(1, 1), (2, 0)]);

    let out = run(
        dir.path(),
        &lcov,
        &["--min-files", "10", "--min-lines", "500"],
    );
    assert!(!out.status.success(), "the floors did not fire");
    let err = stderr(&out);
    assert!(
        err.contains("only 1 source files") && err.contains("only 2 instrumented lines"),
        "the floor failure does not name both breaches:\n{err}"
    );

    // The table still has to be printed, or a failure cannot be told apart
    // from a drop without re-running.
    assert!(
        stdout(&out).contains("TOTAL"),
        "the table was not printed on the failing path, so there is nothing to \
         tell 'collection broke' from 'a crate stopped being built':\n{}",
        stdout(&out)
    );
}

#[test]
fn the_floors_pass_at_the_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run(
        dir.path(),
        &record("src/a.rs", &[(1, 1), (2, 0)]),
        &["--min-files", "1", "--min-lines", "2"],
    );
    assert!(
        out.status.success(),
        "a report exactly at the floors was rejected, so the floors are \
         off-by-one and will red a healthy run:\n{}",
        stderr(&out)
    );
}

/// The canary is the guard a size floor cannot be: a floor is satisfiable by
/// build-script noise, a named file that must have executed is not.
#[test]
fn require_covered_distinguishes_absent_from_unhit_from_covered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = format!(
        "{}{}",
        record("src/hit.rs", &[(1, 3), (2, 1)]),
        record("src/cold.rs", &[(1, 0), (2, 0)])
    );

    let absent = run(dir.path(), &lcov, &["--require-covered", "src/gone.rs"]);
    assert!(!absent.status.success(), "an absent canary passed");
    assert!(
        stderr(&absent).contains("absent from the report entirely"),
        "{}",
        stderr(&absent)
    );

    let unhit = run(dir.path(), &lcov, &["--require-covered", "src/cold.rs"]);
    assert!(
        !unhit.status.success(),
        "a canary present with zero hits passed — which is exactly the shape of \
         a run where the binaries were built but never executed"
    );
    assert!(
        stderr(&unhit).contains("0 of 2 lines covered"),
        "{}",
        stderr(&unhit)
    );

    let covered = run(dir.path(), &lcov, &["--require-covered", "src/hit.rs"]);
    assert!(
        covered.status.success(),
        "a covered canary was rejected:\n{}",
        stderr(&covered)
    );
}

/// The same file appears once per binary that linked it. A line hit by only one
/// of those records is covered.
#[test]
fn duplicate_records_for_one_file_are_accumulated_not_overwritten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = format!(
        "{}{}",
        record("src/a.rs", &[(1, 0), (2, 0)]),
        record("src/a.rs", &[(1, 4), (2, 0)])
    );

    let out = run(dir.path(), &lcov, &["--require-covered", "src/a.rs"]);
    assert!(
        out.status.success(),
        "a line hit only by the second record was reported as unhit, so \
         whichever binary parsed last decides the number:\n{}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("      1/2"),
        "expected 1 of 2 lines covered after accumulation:\n{}",
        stdout(&out)
    );
}

/// lcov's `DA:` allows a third checksum field. Dropping those records would
/// shrink the denominator silently.
#[test]
fn da_records_with_a_checksum_field_are_parsed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = "SF:src/a.rs\nDA:1,2,abc123\nDA:2,0,def456\nDA:oops,1\nend_of_record\n";

    let out = run(dir.path(), lcov, &["--min-lines", "2"]);
    assert!(
        out.status.success(),
        "checksum-bearing DA records were dropped, or the malformed one \
         crashed the parse:\n{}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("      1/2"),
        "expected exactly the two well-formed lines:\n{}",
        stdout(&out)
    );
}

#[test]
fn strip_cfg_test_removes_the_test_module_and_keeps_production_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.rs"),
        // 1 pub fn real() -> u32 {
        // 2     1
        // 3 }
        // 4
        // 5 #[cfg(test)]
        // 6 mod tests {
        // 7     #[test]
        // 8     fn t() {}
        // 9 }
        "pub fn real() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    )
    .expect("write source");

    let lcov = record("lib.rs", &[(1, 1), (2, 0), (7, 1), (8, 1)]);
    let out_lcov = dir.path().join("out.info");
    let out = run(
        dir.path(),
        &lcov,
        &[
            "--strip-cfg-test",
            "--out-lcov",
            out_lcov.to_str().expect("utf-8 path"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let filtered = std::fs::read_to_string(&out_lcov).expect("read filtered lcov");
    assert!(
        filtered.contains("DA:1,1") && filtered.contains("DA:2,0"),
        "production lines were deleted from the denominator, which moves \
         coverage in the flattering direction:\n{filtered}"
    );
    assert!(
        !filtered.contains("DA:7,") && !filtered.contains("DA:8,"),
        "#[cfg(test)] lines survived; they are 39% of this tree and they invert \
         patch coverage:\n{filtered}"
    );
    assert!(
        filtered.contains("LF:2") && filtered.contains("LH:1"),
        "the LF/LH counters were not recomputed, so the report disagrees with \
         its own DA records:\n{filtered}"
    );
    assert!(
        stdout(&out).contains("2 lines in #[cfg(test)] modules excluded"),
        "the exclusion is invisible in the output:\n{}",
        stdout(&out)
    );
}

/// The reason this is not grcov's `--excl-start`/`--excl-stop`: a
/// `#[cfg(test)] mod foo;` declaration opens no brace, and a regex range would
/// run to the next column-0 `}` — deleting production code. 14 sites in this
/// tree are of that shape.
#[test]
fn a_cfg_test_declaration_or_item_excludes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.rs"),
        // 1 #[cfg(test)]
        // 2 mod helpers;
        // 3
        // 4 pub fn real() -> u32 {
        // 5     1
        // 6 }
        "#[cfg(test)]\nmod helpers;\n\npub fn real() -> u32 {\n    1\n}\n",
    )
    .expect("write source");

    let out_lcov = dir.path().join("out.info");
    let out = run(
        dir.path(),
        &record("lib.rs", &[(4, 1), (5, 1)]),
        &[
            "--strip-cfg-test",
            "--out-lcov",
            out_lcov.to_str().expect("utf-8 path"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let filtered = std::fs::read_to_string(&out_lcov).expect("read filtered lcov");
    assert!(
        filtered.contains("DA:4,1") && filtered.contains("DA:5,1"),
        "a `#[cfg(test)] mod foo;` declaration swallowed the code after it — \
         this is precisely the failure the regex approach has:\n{filtered}"
    );
}

/// An unterminated module must exclude nothing rather than run to EOF.
#[test]
fn an_unclosed_cfg_test_module_excludes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn real() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n",
    )
    .expect("write source");

    let out_lcov = dir.path().join("out.info");
    let out = run(
        dir.path(),
        &record("lib.rs", &[(1, 1)]),
        &[
            "--strip-cfg-test",
            "--out-lcov",
            out_lcov.to_str().expect("utf-8 path"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        std::fs::read_to_string(&out_lcov)
            .expect("read filtered lcov")
            .contains("DA:1,1"),
        "an unclosed module excluded to EOF"
    );
}

/// Function records are keyed by line but their execution counts are keyed by
/// name, so dropping one without the other leaves a count for a function that
/// is no longer in the report.
#[test]
fn function_records_inside_a_test_module_are_dropped_with_their_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn real() {\n}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n",
    )
    .expect("write source");

    let lcov = "SF:lib.rs\nFN:1,real\nFN:5,tests::t\nFNDA:2,real\nFNDA:9,tests::t\n\
                DA:1,2\nDA:5,9\nend_of_record\n";
    let out_lcov = dir.path().join("out.info");
    let out = run(
        dir.path(),
        lcov,
        &[
            "--strip-cfg-test",
            "--out-lcov",
            out_lcov.to_str().expect("utf-8 path"),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let filtered = std::fs::read_to_string(&out_lcov).expect("read filtered lcov");
    assert!(
        !filtered.contains("tests::t"),
        "a test function's FNDA outlived its FN record:\n{filtered}"
    );
    assert!(
        filtered.contains("FNF:1") && filtered.contains("FNH:1"),
        "the function counters were not recomputed:\n{filtered}"
    );
}

/// Reporting units are what the table is actionable by; a blank one is a row
/// nobody can act on.
#[test]
fn reporting_units_are_named_for_every_path_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = format!(
        "{}{}{}",
        record("crates/engine/src/a.rs", &[(1, 1)]),
        record("src/b.rs", &[(1, 1)]),
        record("/nix/store/x/c.rs", &[(1, 1)])
    );

    let out = run(dir.path(), &lcov, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let table = stdout(&out);

    for unit in ["crates/engine", "heph", "<external>"] {
        assert!(
            table.contains(unit),
            "no `{unit}` row in the table — an absolute path that escapes \
             grcov's `--ignore '/*'` would render as a blank row:\n{table}"
        );
    }
}

/// `summary.json` is the agent surface: it has to be stable enough that two
/// runs of the same tree diff to nothing.
#[test]
fn summary_json_is_stable_and_carries_no_timestamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lcov = record("src/a.rs", &[(1, 1), (2, 0)]);
    let json = dir.path().join("summary.json");

    let mut renders = Vec::new();
    for _ in 0..2 {
        let out = run(
            dir.path(),
            &lcov,
            &[
                "--json",
                json.to_str().expect("utf-8 path"),
                "--label",
                "linux/amd64",
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        renders.push(std::fs::read_to_string(&json).expect("read summary.json"));
    }

    assert_eq!(
        renders.first(),
        renders.last(),
        "summary.json is not reproducible across runs, so it cannot be diffed"
    );
    let rendered = renders.first().expect("one render");
    assert!(
        rendered.contains("\"line_coverage\": 50.0")
            && rendered.contains("\"label\": \"linux/amd64\""),
        "summary.json does not carry the totals it is read for:\n{rendered}"
    );
}
