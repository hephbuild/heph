//! The output flags at the process boundary, on `heph query`: what reaches
//! stdout, what reaches stderr, and the exit code, for the cases the spec
//! pins at the CLI (`bad_jq_fails_before_the_engine`,
//! `incomplete_list_is_unclosed`, `jq_runtime_error_exits_2`).
//!
//! Drives the built `heph` binary: what is under test is `main`'s exit code
//! and the bytes on each stream. `codegen_json_cli.rs` covers the same rules
//! on codegen's `--check` exit code.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// What the BUILD file of package `q` fails with as soon as it is evaluated.
const EVALUATED: &str = "this BUILD file was evaluated";

/// How stderr names that failure: if it shows up, the provider walked `q`.
const BROKEN: &str = "q/BUILD";

/// A workspace with two targets in `p` and, when `broken`, a package `q`
/// whose BUILD file fails to evaluate.
fn workspace(broken: bool) -> tempfile::TempDir {
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ws.path().join(".hephconfig2"),
        "plugins:\n  - builtin: buildfile\n  - builtin: exec\n  - builtin: bash\n",
    )
    .expect("write config");
    std::fs::create_dir_all(ws.path().join("p")).expect("mkdir p");
    std::fs::write(
        ws.path().join("p/BUILD"),
        "target(name = \"a\", driver = \"bash\", run = \"true\")\n\
         target(name = \"b\", driver = \"bash\", run = \"true\")\n",
    )
    .expect("write p/BUILD");
    if broken {
        std::fs::create_dir_all(ws.path().join("q")).expect("mkdir q");
        std::fs::write(
            ws.path().join("q/BUILD"),
            format!("fail(\"{EVALUATED}\")\n"),
        )
        .expect("write q/BUILD");
    }
    ws
}

fn run(ws: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_heph"))
        .args(args)
        .current_dir(ws)
        .env("HOME", home)
        .env("HEPH_CWD", ws)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph")
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// I2: a filter that does not parse, an unknown function, a removed builtin
/// and an unknown field each exit 2 with stdout empty, before anything
/// starts: the broken BUILD file is never evaluated, and no session is
/// recorded under `HOME`.
#[test]
fn bad_jq_fails_before_the_engine() {
    let ws = workspace(true);
    let home = tempfile::tempdir().expect("home");

    // The control: the same query with a good filter does evaluate it.
    let control = run(
        ws.path(),
        home.path(),
        &["query", "-e", "//...", "--jq", "."],
    );
    assert!(
        String::from_utf8_lossy(&control.stderr).contains(BROKEN),
        "{}",
        describe(&control)
    );

    let home = tempfile::tempdir().expect("home");
    for flags in [
        &["--jq", ".[] | select("][..],
        &["--jq", "no_such_function"],
        &["--jq", "env"],
        &["--jq", "$ENV"],
        &["--fields", "nope"],
    ] {
        let mut args = vec!["query", "-e", "//..."];
        args.extend(flags);
        let out = run(ws.path(), home.path(), &args);
        let what = format!("{flags:?}\n{}", describe(&out));
        assert_eq!(out.status.code(), Some(2), "{what}");
        assert!(out.stdout.is_empty(), "{what}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(BROKEN), "the provider ran: {what}");
        assert!(!stderr.is_empty(), "no reason given: {what}");
    }
    assert!(
        !home.path().join(".heph").exists(),
        "a session or engine state was created under HOME"
    );
}

/// D10: a list the walk could not complete is left without its `]`, the
/// command exits non-zero, and stderr says why: a package that failed to
/// evaluate under the default keep-going, and a mid-stream error under
/// `--fail-fast`.
#[test]
fn incomplete_list_is_unclosed() {
    let ws = workspace(true);
    for extra in [&[][..], &["--fail-fast"]] {
        let home = tempfile::tempdir().expect("home");
        let mut args = vec!["query", "-e", "//...", "--json"];
        args.extend(extra);
        let out = run(ws.path(), home.path(), &args);
        let what = format!("{extra:?}\n{}", describe(&out));
        assert_ne!(out.status.code(), Some(0), "{what}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with('['), "the list was started: {what}");
        assert!(!stdout.trim_end().ends_with(']'), "closed: {what}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&stdout).is_err(),
            "a JSON reader took the short list as whole: {what}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(BROKEN),
            "stderr does not say why: {what}"
        );
    }

    // The control: the whole walk closes it.
    let ws = workspace(false);
    let home = tempfile::tempdir().expect("home");
    let out = run(ws.path(), home.path(), &["query", "-e", "//...", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}\n{}", describe(&out)));
    // Beside the two targets, the BUILD-file check targets of `p`.
    for addr in ["//p:a", "//p:b"] {
        assert!(
            rows.iter().any(|r| r["addr"] == addr),
            "{addr}: {}",
            describe(&out)
        );
    }
}

/// An output flag on a command with no JSON form is a usage error (2), and
/// stderr says what to use instead, from the command's `NO_JSON` entry.
#[test]
fn no_json_command_names_what_to_use_instead() {
    let ws = workspace(false);
    let home = tempfile::tempdir().expect("home");
    for flags in [&["--json"][..], &["--fields", "addr"], &["--jq", "."]] {
        let mut args = vec!["log", "//p:a"];
        args.extend(flags);
        let out = run(ws.path(), home.path(), &args);
        let what = format!("{flags:?}\n{}", describe(&out));
        assert_eq!(out.status.code(), Some(2), "{what}");
        assert!(out.stdout.is_empty(), "{what}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("`heph log` has no JSON output")
                && stderr.contains("`heph inspect failure --json` has the log's path"),
            "{what}"
        );
    }
}

/// `tool build-fmt -` writes the formatted source to stdout, so an output
/// flag there is refused as a usage error (2), with stdout untouched.
#[test]
fn stdin_build_fmt_refuses_output_flags_with_2() {
    let ws = workspace(false);
    let home = tempfile::tempdir().expect("home");
    for flags in [&["--json"][..], &["--fields", "checked"], &["--jq", "."]] {
        let mut args = vec!["tool", "build-fmt", "-"];
        args.extend(flags);
        let out = run(ws.path(), home.path(), &args);
        let what = format!("{flags:?}\n{}", describe(&out));
        assert_eq!(out.status.code(), Some(2), "{what}");
        assert!(out.stdout.is_empty(), "{what}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("has no JSON form"),
            "{what}"
        );
    }
}

/// `--fields help` into a pipe the reader already closed exits 0, as every
/// other output path does on a closed pipe.
#[test]
fn fields_help_on_a_closed_pipe_exits_0() {
    let ws = workspace(false);
    let home = tempfile::tempdir().expect("home");
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_heph"))
        .args(["query", "--fields", "help"])
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("HEPH_CWD", ws.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .expect("run heph");
    assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
}

/// A filter that fails while it runs exits 2, and the results it printed
/// before the error stay on stdout.
#[test]
fn jq_runtime_error_exits_2() {
    let ws = workspace(false);
    let home = tempfile::tempdir().expect("home");
    let out = run(
        ws.path(),
        home.path(),
        &[
            "query",
            "-e",
            "//...",
            "--jq",
            r#".[] | if .name == "b" then error("boom") else .addr end"#,
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "//p:a\n",
        "{}",
        describe(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("boom"),
        "{}",
        describe(&out)
    );
}
