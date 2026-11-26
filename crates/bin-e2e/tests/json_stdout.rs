//! `--json`, `--fields` and `--jq` at the process boundary: what reaches the
//! stdout a script reads, and what a closed one does to the walk.
//!
//! Neither has an in-process form. The TUI engages only when stderr is a
//! terminal, so a linked test always takes the line backend and could not show
//! that the JSON route keeps every other byte off stdout (I1); and a broken
//! pipe is a `head` exiting under a real process (rule 4).

mod common;

use anyhow::Context as _;
use common::pty::{Ended, Pty};
use common::{Dist, Workspace};
use serde_json::Value;
use std::io::{BufRead as _, Read as _};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// A release binary on a shared CI runner.
const DEADLINE: Duration = Duration::from_secs(120);

/// A plain target, and a `copy` target so `codegen status` has something to
/// list (the unwritten `.gitignore` section, before any run).
const BUILD: &str = r#"
target(name = "a", driver = "bash", run = "true")
target(
    name = "gen",
    driver = "bash",
    run = "printf v1 > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#;

/// `s` as one single-quoted bash word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `heph <args>` with stderr on a PTY and stdout redirected to a file, so heph
/// sees a terminal and, but for the output flag, would start its TUI. Asserts
/// it exits 0; returns what reached the file, and how many cursor-position
/// queries the terminal got (the TUI's inline viewport asks one as it starts).
fn on_terminal(ws: &Workspace, dist: &Dist, args: &[&str]) -> (String, usize) {
    let out = ws.root().join(".stdout");
    let words: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    // A regular file, not a pipe: a stdout that feeds another process already
    // keeps the TUI off (`tui::should_use_tui`), which would make this vacuous.
    let script = format!(
        "exec \"$HEPH\" {} > {}",
        words.join(" "),
        sh_quote(&out.to_string_lossy()),
    );
    let home = ws.home().to_string_lossy().into_owned();
    let mut pty = Pty::script(dist, ws.root(), &script, &[("HOME", &home)]);
    let ended = pty.wait_exit(DEADLINE);
    let stdout = std::fs::read_to_string(&out).expect("read the captured stdout");
    assert_eq!(
        ended,
        Ended::Code(0),
        "heph {args:?}\n--- stdout ---\n{stdout}\n--- terminal ---\n{}",
        pty.seen()
    );
    (stdout, pty.dsr_queries())
}

/// The whole of stdout is one JSON document, with nothing before or after it
/// (`serde_json::from_str` rejects trailing non-whitespace).
fn document(args: &[&str], stdout: &str) -> Value {
    serde_json::from_str(stdout)
        .with_context(|| format!("stdout of heph {args:?} is not only JSON: {stdout:?}"))
        .expect("one JSON document")
}

/// Each `--jq` result printed as a raw string, one per line: no blank line,
/// no escape sequence, no control byte a TUI or a log line would have left.
fn raw_lines<'a>(args: &[&str], stdout: &'a str) -> Vec<&'a str> {
    assert!(
        stdout.ends_with('\n'),
        "heph {args:?}: unterminated --jq output {stdout:?}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    for line in &lines {
        assert!(
            !line.is_empty() && !line.chars().any(char::is_control),
            "heph {args:?}: {line:?} is not a raw --jq result\n{stdout:?}"
        );
    }
    lines
}

/// The keys of an object, sorted.
fn keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v
        .as_object()
        .with_context(|| format!("not an object: {v}"))
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn elements(args: &[&str], doc: &Value) -> Vec<Value> {
    doc.as_array()
        .with_context(|| format!("heph {args:?} printed no array: {doc}"))
        .expect("an array")
        .clone()
}

/// Run `args` on a terminal and check what every JSON route shares: exit 0,
/// and the TUI kept off (the progress display goes with `--json`). Returns
/// stdout.
fn json_run(ws: &Workspace, dist: &Dist, args: &[&str]) -> String {
    let (stdout, dsr) = on_terminal(ws, dist, args);
    assert_eq!(dsr, 0, "heph {args:?} started the TUI on stderr");
    stdout
}

/// I1: under each of `--json`, `--fields` and `--jq` alone, with stderr on a
/// terminal, stdout carries the JSON (or the `--jq` results) and nothing else.
/// `inspect def` takes no `--fields` (D11).
#[test]
fn stdout_is_only_json() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write("p/BUILD", BUILD).expect("write BUILD");
    // A recorded session for `session list` to list.
    let run = ws.run(&dist, &["run", "//p:a"]).expect("run //p:a");
    assert!(run.status.success(), "{}", common::describe(&run));
    let recorded = common::session::id_in(&String::from_utf8_lossy(&run.stderr))
        .expect("the run announced its session");

    // The harness is real: the same query, without an output flag, starts the
    // TUI on this terminal.
    let (_, dsr) = on_terminal(&ws, &dist, &["query", "-e", "//..."]);
    assert!(dsr > 0, "the TUI never engaged on the terminal");

    // query: an array of rows, one row per target.
    let args = ["query", "-e", "//...", "--json"];
    let rows = elements(&args, &document(&args, &json_run(&ws, &dist, &args)));
    let addrs: Vec<&str> = rows.iter().filter_map(|r| r["addr"].as_str()).collect();
    assert!(addrs.contains(&"//p:a"), "{rows:?}");
    assert_eq!(addrs.len(), rows.len(), "a row without an addr: {rows:?}");

    let args = ["query", "-e", "//...", "--fields", "addr"];
    let only = elements(&args, &document(&args, &json_run(&ws, &dist, &args)));
    assert_eq!(only.len(), rows.len(), "{only:?}");
    assert!(only.iter().all(|r| keys(r) == ["addr"]), "{only:?}");

    let args = ["query", "-e", "//...", "--jq", ".[].addr"];
    let stdout = json_run(&ws, &dist, &args);
    assert_eq!(raw_lines(&args, &stdout), addrs);

    // inspect def: one document.
    let args = ["inspect", "def", "//p:a", "--json"];
    let def = document(&args, &json_run(&ws, &dist, &args));
    assert_eq!(def["target_def"]["addr"], "//p:a", "{def}");

    let args = ["inspect", "def", "//p:a", "--jq", ".target_def.addr"];
    let stdout = json_run(&ws, &dist, &args);
    assert_eq!(raw_lines(&args, &stdout), ["//p:a"]);

    // session list: the commands above recorded sessions too, so only the
    // first run's is pinned.
    let args = ["session", "list", "--json"];
    let sessions = elements(&args, &document(&args, &json_run(&ws, &dist, &args)));
    assert!(
        sessions.iter().any(|s| s["id"] == recorded.as_str()),
        "{recorded} not listed: {sessions:?}"
    );

    let args = ["session", "list", "--fields", "id"];
    let ids = elements(&args, &document(&args, &json_run(&ws, &dist, &args)));
    assert!(ids.iter().all(|s| keys(s) == ["id"]), "{ids:?}");
    assert!(ids.iter().any(|s| s["id"] == recorded.as_str()), "{ids:?}");

    let args = ["session", "list", "--jq", ".[].id"];
    let stdout = json_run(&ws, &dist, &args);
    let lines = raw_lines(&args, &stdout);
    assert!(lines.contains(&recorded.as_str()), "{lines:?}");

    // codegen status: one document, with the stale `.gitignore` section in it.
    let args = ["tool", "codegen", "status", "--json"];
    let status = document(&args, &json_run(&ws, &dist, &args));
    let paths: Vec<&str> = status["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items: {status}"))
        .iter()
        .filter_map(|i| i["path"].as_str())
        .collect();
    assert!(paths.contains(&".gitignore"), "{status}");

    let args = ["tool", "codegen", "status", "--fields", "items"];
    let items = document(&args, &json_run(&ws, &dist, &args));
    assert_eq!(keys(&items), ["items"], "{items}");

    let args = ["tool", "codegen", "status", "--jq", ".items[].path"];
    let stdout = json_run(&ws, &dist, &args);
    assert_eq!(raw_lines(&args, &stdout), paths);
}

/// How a measured run ended: its exit code, the CPU time it used, and stdout.
struct Measured {
    code: i32,
    cpu: Duration,
    stdout: Vec<u8>,
    stderr: String,
}

/// Reap `pid` with `wait4`, for the CPU time of that one process (the test
/// binary's other children are not counted); past `DEADLINE` it is killed.
fn reap(pid: i32) -> (i32, Duration) {
    let started = Instant::now();
    loop {
        let mut status = 0;
        // SAFETY: `rusage` is plain integers, for which all-zero is valid.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals for the call's duration.
        let reaped = unsafe { libc::wait4(pid, &raw mut status, libc::WNOHANG, &raw mut usage) };
        if reaped == pid {
            assert!(libc::WIFEXITED(status), "heph ended by a signal: {status}");
            let cpu = |t: libc::timeval| {
                Duration::from_secs(u64::try_from(t.tv_sec).unwrap_or_default())
                    + Duration::from_micros(u64::try_from(t.tv_usec).unwrap_or_default())
            };
            return (
                libc::WEXITSTATUS(status),
                cpu(usage.ru_utime) + cpu(usage.ru_stime),
            );
        }
        assert_eq!(reaped, 0, "wait4: {}", std::io::Error::last_os_error());
        let late = started.elapsed() > DEADLINE;
        if late {
            common::signal(pid, libc::SIGKILL);
        }
        assert!(!late, "heph did not exit within {DEADLINE:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `heph <args>` with stdout read to the end, or, with `first_line`, read up
/// to its first newline and then closed, as `| head -1` does.
fn measured(ws: &Workspace, dist: &Dist, args: &[&str], first_line: bool) -> Measured {
    let err_path = ws.root().join(".stderr");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let mut child = ws
        .cmd(dist, args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(err)
        .spawn()
        .expect("spawn heph");
    let pid = i32::try_from(child.id()).expect("pid");
    let pipe = child.stdout.take().expect("piped stdout");
    // On a thread, so that a hung heph is killed by `reap` rather than
    // blocking the read forever.
    let reader = std::thread::spawn(move || {
        let mut pipe = std::io::BufReader::new(pipe);
        let mut buf = Vec::new();
        let read = if first_line {
            pipe.read_until(b'\n', &mut buf)
        } else {
            pipe.read_to_end(&mut buf)
        };
        read.expect("read heph's stdout");
        // Dropping `pipe` here closes the read end.
        buf
    });
    let (code, cpu) = reap(pid);
    let stdout = reader.join().expect("stdout reader");
    // `reap` waited on it; the handle has nothing left to do.
    drop(child);
    Measured {
        code,
        cpu,
        stdout,
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    }
}

/// Rounds of the walk's window the workspace holds: the walk of the whole of it
/// costs this many windows of package evaluation, a walk cut short after its
/// first row about one.
const ROUNDS: usize = 8;

/// Rule 4: a closed stdout (`| head -1`) cancels the request, so the walk stops
/// instead of evaluating every package, and the exit is 0.
///
/// Each package's BUILD file spins long enough that evaluating it dominates
/// what a run costs, and the walk evaluates `2 × cpus` packages at a time
/// (`discovery_concurrency`). The whole walk is measured first, for scale; the
/// one cut short must cost a fraction of it. CPU time rather than wall time,
/// so a loaded runner slows both alike.
#[test]
fn broken_pipe_stops_the_walk() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let window = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) * 2;
    let packages = window * ROUNDS;
    // `a` sorts first and is cheap: its row is ready at once, and the write
    // after it finds the pipe closed.
    ws.write(
        "a/BUILD",
        "target(name = \"t\", driver = \"bash\", run = \"true\")\n",
    )
    .expect("write a/BUILD");
    let spin = "def spin():\n    x = 0\n    for i in range(2000000):\n        x += i\n    return x\n\nspin()\ntarget(name = \"t\", driver = \"bash\", run = \"true\")\n";
    for i in 0..packages {
        ws.write(Path::new(&format!("p{i:04}")).join("BUILD"), spin)
            .expect("write BUILD");
    }
    let args = ["query", "-e", "//...", "--fields", "addr"];

    let cut = measured(&ws, &dist, &args, true);
    assert_eq!(cut.code, 0, "| head -1 exit\n{}", cut.stderr);
    assert_eq!(cut.stdout, b"[\n", "{}", cut.stderr);

    let whole = measured(&ws, &dist, &args, false);
    assert_eq!(whole.code, 0, "whole walk exit\n{}", whole.stderr);
    let rows: Vec<Value> = serde_json::from_slice(&whole.stdout).expect("whole walk is JSON");
    let targets = rows
        .iter()
        .filter(|r| r["addr"].as_str().is_some_and(|a| a.ends_with(":t")))
        .count();
    assert_eq!(targets, packages + 1, "the whole walk missed packages");

    assert!(
        cut.cpu * 3 < whole.cpu,
        "the walk ran on after stdout closed: {:?} of CPU cut short vs {:?} whole, \
         {packages} packages in windows of {window}",
        cut.cpu,
        whole.cpu,
    );
}
