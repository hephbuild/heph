//! `heph log` through the built binary: what reaches stdout and what
//! reaches stderr, the exit code, `-n`, and `-f` against a run in another
//! heph (a closed stdout, a ctrl-c). The engine side is
//! `crates/e2e/tests/e2e/log_cmd.rs`; these are the CLI's.

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(120);

/// A workspace with its own `HOME`, on the `bash` driver.
struct Ws {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Ws {
    fn new() -> Self {
        let ws = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            home: tempfile::tempdir().expect("home"),
        };
        std::fs::write(
            ws.root().join(".hephconfig"),
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  - builtin: exec\n  - builtin: bash\n",
        )
        .expect("write config");
        std::fs::create_dir_all(ws.root().join("p")).expect("mkdir p");
        ws
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")));
        cmd.args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("HEPH_CWD", self.root())
            .env("HEPH_NO_SELF_UPDATE", "1")
            .env("HEPH_DISABLE_TELEMETRY", "1")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        let child = self
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn heph");
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || drop(tx.send(child.wait_with_output())));
        let res = rx.recv_timeout(DEADLINE);
        if res.is_err() {
            kill(pid, libc::SIGKILL);
        }
        res.expect("heph did not finish").expect("wait heph")
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.root().join(name), "").expect("touch");
    }
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn kill(pid: u32, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(pid).expect("pid");
    // SAFETY: kill(2) takes plain integers and touches no memory of ours.
    unsafe { libc::kill(pid, sig) };
}

/// Wait for `child` to exit, killing it past the deadline.
fn wait_exit(child: &mut Child, what: &str) -> ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        let late = Instant::now() > deadline;
        if late {
            drop(child.kill());
        }
        assert!(!late, "{what} did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every line `r` yields, as it comes.
fn lines(r: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// Read `rx` until a line containing `want`, returning everything read.
fn until(rx: &mpsc::Receiver<String>, want: &str) -> String {
    let deadline = Instant::now() + DEADLINE;
    let mut seen = String::new();
    while !seen.contains(want) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
            }
            Err(_gone) => assert!(seen.contains(want), "never saw {want:?}; saw:\n{seen}"),
        }
    }
    seen
}

const LOG: &[u8] = b"l1\nl2\n\x1b[31mred\x1b[0m\nl4\nl5\n";

/// `//p:t` prints [`LOG`], an escape sequence included; `//p:nonl` a log
/// with no final newline; `//p:never` is never built; `//p:g` prints
/// `g-1`, waits for `{gates}/g.1`, prints `g-2` and `g-3`, and waits for
/// `{gates}/g.2`.
fn build_file(gates: &str) -> String {
    format!(
        r#"target(name = "t", driver = "bash", out = "t.txt", run = "printf 'l1\\nl2\\n\\033[31mred\\033[0m\\nl4\\nl5\\n'; printf x > t.txt")
target(name = "nonl", driver = "bash", out = "n.txt", run = "printf 'a\\nb'; printf x > n.txt")
target(name = "never", driver = "bash", out = "v.txt", run = "printf x > v.txt")
target(name = "g", driver = "bash", out = "g.txt", run = "echo g-1; while [ ! -e {gates}/g.1 ]; do sleep 0.02; done; echo g-2; echo g-3; while [ ! -e {gates}/g.2 ]; do sleep 0.02; done; printf x > g.txt")
"#
    )
}

fn workspace() -> Ws {
    let ws = Ws::new();
    let root = ws.root().display().to_string();
    std::fs::write(ws.root().join("p/BUILD"), build_file(&root)).expect("write BUILD");
    ws
}

/// spec `log_cmd_stdout_is_the_log`: stdout holds the log bytes exactly,
/// control bytes included (I13), the status line is on stderr only, and
/// `-n 3` prints the last three lines.
#[test]
fn log_cmd_stdout_is_the_log() {
    let ws = workspace();
    let run = ws.run(&["run", "//p:t"]);
    assert!(run.status.success(), "heph run: {}", describe(&run));

    let out = ws.run(&["log", "//p:t"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(out.stdout, LOG, "stdout is the log: {}", describe(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("//p:t · cached"), "the status line: {err}");

    let out = ws.run(&["log", "-n", "3", "//p:t"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        out.stdout,
        b"\x1b[31mred\x1b[0m\nl4\nl5\n",
        "the last three lines: {}",
        describe(&out)
    );
}

/// spec `log_cmd_edges`: `-n 0`, `-n` larger than the log, a log with no
/// trailing newline, an unknown address against a known one with no log,
/// `| head -1` with `-f` (stops on EPIPE), `-f` on a pending target stopped
/// by ctrl-c, `--current -f`.
#[test]
fn log_cmd_edges() {
    let ws = workspace();
    for addr in ["//p:t", "//p:nonl"] {
        let run = ws.run(&["run", addr]);
        assert!(run.status.success(), "heph run {addr}: {}", describe(&run));
    }

    // `-n 0`: a log was found, and none of it printed.
    let out = ws.run(&["log", "-n", "0", "//p:t"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(out.stdout, b"", "-n 0");
    // `-n` over the log's length: all of it.
    let out = ws.run(&["log", "-n", "100", "//p:t"]);
    assert_eq!(out.stdout, LOG, "-n 100: {}", describe(&out));
    // No final newline: the last line is still a line, printed as it is.
    let out = ws.run(&["log", "//p:nonl"]);
    assert_eq!(out.stdout, b"a\nb", "{}", describe(&out));
    let out = ws.run(&["log", "-n", "1", "//p:nonl"]);
    assert_eq!(out.stdout, b"b", "{}", describe(&out));

    // Never built, and unknown: the same answer, since nothing is resolved.
    for addr in ["//p:never", "//p:nope"] {
        let out = ws.run(&["log", addr]);
        assert_eq!(out.status.code(), Some(1), "{addr}: {}", describe(&out));
        assert_eq!(out.stdout, b"", "{addr}: nothing on stdout");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(&format!("no log for {addr}")), "{addr}: {err}");
    }

    // `-f` on a target nothing runs: it waits, and ctrl-c stops it with
    // nothing printed, exit 1.
    let mut f = ws
        .cmd(&["log", "-f", "//p:never"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heph log -f");
    let err = lines(f.stderr.take().expect("stderr"));
    until(&err, "waiting for a run to start");
    kill(f.id(), libc::SIGINT);
    let status = wait_exit(&mut f, "heph log -f after ctrl-c");
    assert_eq!(status.code(), Some(1), "{status}");
    until(&err, "stopped before a run started");
    let mut stdout = Vec::new();
    f.stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut stdout)
        .expect("read stdout");
    assert_eq!(stdout, b"", "nothing printed");

    // `heph log -f | head -1` on a run in another heph: once the reader is
    // gone, the next write fails and `-f` stops, though the run goes on.
    let mut run = ws
        .cmd(&["run", "//p:g"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn heph run");
    let mut f = ws
        .cmd(&["log", "-f", "//p:g"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heph log -f");
    let err = lines(f.stderr.take().expect("stderr"));
    // Read here, not on a thread: dropping the reader must close the pipe
    // before the run writes again.
    let mut out = BufReader::new(f.stdout.take().expect("stdout"));
    let mut first = String::new();
    out.read_line(&mut first).expect("read the first line");
    assert_eq!(first, "g-1\n");
    drop(out);
    ws.touch("g.1");
    let status = wait_exit(&mut f, "heph log -f with its stdout closed");
    let seen: Vec<String> = err.try_iter().collect();
    assert!(status.success(), "{status}: {seen:?}");
    assert!(
        run.try_wait().expect("try_wait").is_none(),
        "-f stopped on the closed pipe, not at the run's end"
    );
    ws.touch("g.2");
    let status = wait_exit(&mut run, "heph run //p:g");
    assert!(status.success(), "heph run //p:g: {status}");

    // `--current -f` on a target built at its current inputs: that run's
    // log, read through once.
    let out = ws.run(&["log", "--current", "-f", "//p:g"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(out.stdout, b"g-1\ng-2\ng-3\n", "{}", describe(&out));
}
