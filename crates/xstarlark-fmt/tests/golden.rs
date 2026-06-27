//! Golden tests ported 1:1 from the Go reference's `fmt_test.go`. Each fixture
//! in `testdata/` is `input` + `----\n` + `expected`; formatting `input` must
//! produce `expected`, and re-formatting must be idempotent.

use std::fs;
use std::path::PathBuf;

use xstarlark_fmt::{Config, FmtError, format};

fn testdata_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

#[test]
fn golden() {
    let cfg = Config { indent_size: 2 };
    let mut count = 0;
    let mut failures: Vec<String> = Vec::new();
    for entry in fs::read_dir(testdata_dir()).expect("read testdata") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let content = fs::read_to_string(&path).expect("read fixture");
        // Fixtures explicitly marked work-in-progress are skipped, as in Go.
        if content.contains("# skip") {
            continue;
        }
        let (input, expected) = content
            .split_once("----\n")
            .unwrap_or_else(|| panic!("fixture {name} missing ---- separator"));

        let got = format(&name, input, cfg)
            .unwrap_or_else(|e| panic!("fixture {name} failed to format: {e}"));
        if got != expected {
            failures.push(format!(
                "=== {name}: output mismatch ===\n--- got ---\n{got}\n--- want ---\n{expected}"
            ));
            continue;
        }

        // Idempotency: formatting the formatted output must be a no-op.
        let again = format(&name, &got, cfg)
            .unwrap_or_else(|e| panic!("fixture {name} failed to re-format: {e}"));
        if again != got {
            failures.push(format!(
                "=== {name}: not idempotent ===\n--- once ---\n{got}\n--- twice ---\n{again}"
            ));
        }
        count += 1;
    }
    assert!(count > 0 || !failures.is_empty(), "no fixtures ran");
    assert!(
        failures.is_empty(),
        "{} fixture(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn skip_file_directive_at_start() {
    let src = "# heph:fmt skip-file\ntarget(  name='a'  )\n";
    match format("t.build", src, Config::default()) {
        Err(FmtError::Skip) => {}
        other => panic!("expected Skip, got {other:?}"),
    }
}

#[test]
fn skip_file_directive_later_does_not_skip() {
    let src = "target(name = 'a')\n# heph:fmt skip-file\n";
    let got = format("t.build", src, Config::default()).expect("should format");
    assert!(got.contains("# heph:fmt skip-file"));
}

#[test]
fn invalid_indent_rejected() {
    let err = format("t.build", "a = 1\n", Config { indent_size: 0 }).unwrap_err();
    assert!(matches!(err, FmtError::Config(_)));
}
