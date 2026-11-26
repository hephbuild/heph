use super::*;
use crate::fixtures::{args, row};
use crate::{Style, never_cancelled};
use std::time::Instant;

fn jq(code: &str, doc: &str) -> (String, anyhow::Result<()>) {
    let filter = Jq::compile(code).unwrap();
    let mut out = Vec::new();
    let result = filter.run(doc.as_bytes().to_vec(), &mut out, &never_cancelled);
    (String::from_utf8(out).unwrap(), result)
}

#[test]
fn jq_results() {
    // A string raw, a number, an object compact; one per line.
    let (out, r) = jq(r#".[] "#, r#"["a", 1, {"k": [1, 2]}, null]"#);
    r.unwrap();
    assert_eq!(out, "a\n1\n{\"k\":[1,2]}\nnull\n");
    // Zero results: empty stdout, success.
    let (out, r) = jq("empty", "[1]");
    r.unwrap();
    assert_eq!(out, "");
    // A string with a newline and an ESC is printed as is.
    let (out, r) = jq(".s", r#"{"s": "line1\nline2\u001b[31m"}"#);
    r.unwrap();
    assert_eq!(out, "line1\nline2\x1b[31m\n");
    // Through the printer, over a list document.
    let args = args(&["--jq", ".[] | .addr"]);
    let mut out = Vec::new();
    args.printer_to(&mut out, Style::Pretty)
        .list_iter((0..2).map(row), &never_cancelled)
        .unwrap();
    assert_eq!(out, b"//p:t0\n//p:t1\n");
}

#[test]
fn jq_runtime_error_exits_2() {
    let (out, r) = jq(
        r#".[] | if . == 2 then error("two") else . end"#,
        "[1, 2, 3]",
    );
    assert_eq!(out, "1\n", "results before the error stay");
    let err = r.unwrap_err();
    assert_eq!(crate::exit_code(&err), Some(2));
    assert!(format!("{err:#}").contains("two"), "{err:#}");
}

#[test]
fn jq_is_pure() {
    for code in [
        "env",
        "$ENV",
        "$ENV.HOME",
        "halt",
        "halt_error",
        "halt_error(1)",
        "input",
        "inputs",
    ] {
        let err = Jq::compile(code).unwrap_err();
        assert!(err.contains("unknown"), "{code}: {err}");
        assert!(err.contains("stdin or the environment"), "{code}: {err}");
    }
    // What stays.
    for code in ["now", "debug", "1 | stderr"] {
        Jq::compile(code).unwrap();
    }
}

#[test]
fn jq_stops_on_cancellation() {
    // Produces forever, produces nothing forever, and a long reduce.
    for code in [
        "repeat(.)",
        "last(repeat(1))",
        "reduce range(1e12) as $i (0; .+1)",
    ] {
        let filter = Jq::compile(code).unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        let cancelled = move || Instant::now() >= deadline;
        let mut out = std::io::sink();
        let started = Instant::now();
        let err = filter.run(b"1".to_vec(), &mut out, &cancelled).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<OutputError>(),
                Some(OutputError::Cancelled)
            ),
            "{code}: {err:#}"
        );
        assert_eq!(crate::exit_code(&err), Some(130));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{code} did not stop"
        );
    }
}

#[test]
fn jq_dot_equals_json() {
    #[derive(serde::Serialize, crate::Record)]
    struct Wide {
        big: u64,
        huge: u128,
        nan: f64,
        neg: i64,
        float: f64,
        by_number: std::collections::BTreeMap<u32, &'static str>,
        nested: serde_json::Value,
        text: &'static str,
    }
    let value = Wide {
        big: u64::MAX,
        huge: u128::MAX,
        nan: f64::NAN,
        neg: i64::MIN,
        float: 1.5,
        by_number: [(2, "b"), (10, "a")].into_iter().collect(),
        nested: serde_json::json!({"z": 1, "a": [true, null]}),
        text: "quote \" and \\ and é",
    };
    let print = |flags: &[&str]| {
        let args = crate::JsonArgs::<Wide>::try_parse_from(flags).unwrap();
        let mut out = Vec::new();
        args.printer_to(&mut out, Style::Compact)
            .record(&value, &never_cancelled)
            .unwrap();
        String::from_utf8(out).unwrap()
    };
    assert_eq!(print(&["--jq", "."]), print(&["--json"]));

    // A list: the compact document without its layout newlines.
    let list = |flags: &[&str]| {
        let args = args(flags);
        let mut out = Vec::new();
        args.printer_to(&mut out, Style::Compact)
            .list_iter((0..3).map(row), &never_cancelled)
            .unwrap();
        String::from_utf8(out).unwrap()
    };
    let json = list(&["--fields", "all"]).replace('\n', "");
    assert_eq!(list(&["--fields", "all", "--jq", "."]), format!("{json}\n"));
}

#[test]
fn compile_errors_name_the_column() {
    let err = Jq::compile(".foo | bar").unwrap_err();
    assert!(err.contains("unknown function `bar/0`"), "{err}");
    assert!(err.contains("column 8"), "{err}");
    assert!(err.contains("\n  .foo | bar\n         ^^^"), "{err}");

    let err = Jq::compile(".[").unwrap_err();
    assert!(err.contains("column"), "{err}");

    let err = Jq::compile("\"abc").unwrap_err();
    assert!(err.contains("closing delimiter for `\"`"), "{err}");

    let err = Jq::compile(".a |\n  nope").unwrap_err();
    assert!(err.contains("line 2, column 3"), "{err}");
}

/// An eval thread that panics mid-filter drops its sender without `Done`:
/// the results before it stay on stdout, and the run fails with exit 2
/// instead of passing a short output off as whole.
#[test]
fn jq_eval_panic_is_a_runtime_error() {
    let (tx, rx) = mpsc::sync_channel::<Msg>(CHANNEL_DEPTH);
    let thread = std::thread::spawn(move || {
        tx.send(Msg::Line(b"first\n".to_vec())).unwrap();
        panic!("a bug in the filter's evaluation");
    });
    let mut out = Vec::new();
    let err = drain(&rx, &mut out, &never_cancelled).unwrap_err();
    assert!(thread.join().is_err(), "the thread panicked");
    assert_eq!(out, b"first\n");
    assert!(
        matches!(err.downcast_ref(), Some(OutputError::JqRuntime(m)) if m.contains("panicked")),
        "{err:#}"
    );
    assert_eq!(crate::exit_code(&err), Some(2));

    // The control: a filter that ends says so, and the run succeeds.
    let (out, r) = jq(".[]", "[1, 2]");
    r.unwrap();
    assert_eq!(out, "1\n2\n");
}
