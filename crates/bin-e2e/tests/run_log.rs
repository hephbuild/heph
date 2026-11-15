//! A run's live log, through the shipped binary and a real cdylib driver.
//!
//! `crates/e2e/tests/e2e/run_log.rs` covers the engine side in-process; what
//! only a `dlopen` has is the seam: `ManagedRunRequest.log_capture` reaching a
//! plugin's own copy of the SDK, and its writes crossing back as frames the
//! host appends to `<home>/runs/<id>/<n>.log` while the run lasts. The test
//! cdylib's `stream` driver refuses a run without `log_capture`, writes a
//! stdout and a stderr line per phase, and waits on a gate file between
//! phases, so the test reads the live file mid-run.

mod common;

use common::{BASE_CONFIG, Dist, Workspace, describe, sha256_file, wait_output, wait_until};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

/// What the `stream` driver writes, phase by phase, interleaved in the order
/// it writes them.
const PHASES: [&str; 2] = ["out-1\nerr-1\n", "out-2\nerr-2\n"];

/// How long the live file may take to show a phase's lines.
const LIVE_DEADLINE: Duration = Duration::from_secs(60);

/// Every live log under `<home>/runs/`.
fn live_logs(home: &Path) -> Vec<PathBuf> {
    let Ok(engines) = std::fs::read_dir(home.join("runs")) else {
        return vec![];
    };
    engines
        .flatten()
        .filter_map(|e| std::fs::read_dir(e.path()).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect()
}

/// A workspace loading the test cdylib, with `//app:ok` and `//app:fail` on
/// its `stream` driver.
fn workspace(dist: &Dist) -> Workspace {
    let ws = Workspace::new().expect("workspace");
    let dylib = dist.test_plugin("entrypoint");
    assert!(dylib.is_file(), "missing {}", dylib.display());
    let manifest = ws.root().join("test-entrypoint-plugin.json");
    let sum = sha256_file(&dylib).expect("hash test cdylib");
    common::write_manifest(&manifest, "entrypoint_test", &dylib, Some(&sum))
        .expect("write manifest");
    ws.config(&format!("{BASE_CONFIG}  - path: {}\n", manifest.display()))
        .expect("write config");
    ws.write(
        "app/BUILD",
        concat!(
            "target(name = \"ok\", driver = \"entrypoint_test.stream\")\n",
            "target(name = \"fail\", driver = \"entrypoint_test.stream\")\n",
        ),
    )
    .expect("write BUILD");
    ws
}

/// A `heph log -f` started mid-run. Its stdout is read on the test thread, so
/// a write it could not make (EPIPE) or a follow that never ends fails the
/// test instead of hiding in a helper thread. A watchdog SIGKILLs it past
/// [`common::RUN_DEADLINE`], which ends a blocked read with EOF.
struct Follower {
    child: Child,
    stdout: ChildStdout,
    seen: Vec<u8>,
    stop: mpsc::Sender<()>,
    watchdog: JoinHandle<()>,
}

impl Follower {
    fn spawn(ws: &Workspace, dist: &Dist, addr: &str) -> Self {
        let mut child = ws
            .cmd(dist, &["log", "-f", addr])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn heph log -f");
        let stdout = child.stdout.take().expect("heph log -f stdout");
        let pid = i32::try_from(child.id()).expect("pid");
        let (stop, rx) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if rx.recv_timeout(common::RUN_DEADLINE) == Err(RecvTimeoutError::Timeout) {
                // SAFETY: a plain syscall on integers; the child is reaped
                // only after this thread is joined, so the pid is still ours.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
        Self {
            child,
            stdout,
            seen: Vec::new(),
            stop,
            watchdog,
        }
    }

    /// Read until it has printed as many bytes as `want`, which they must be.
    fn expect(&mut self, want: &str) {
        let mut buf = [0u8; 256];
        while self.seen.len() < want.len() {
            let n = self.stdout.read(&mut buf).expect("read heph log -f");
            assert!(
                n > 0,
                "heph log -f closed stdout at {:?}, before {want:?}",
                String::from_utf8_lossy(&self.seen)
            );
            self.seen
                .extend_from_slice(buf.get(..n).unwrap_or_default());
        }
        assert_eq!(
            String::from_utf8_lossy(&self.seen),
            want,
            "heph log -f prints the chunks in order"
        );
    }

    /// Read to EOF and reap it; `stdout` is everything it printed.
    fn finish(self) -> Output {
        let Self {
            child,
            mut stdout,
            mut seen,
            stop,
            watchdog,
        } = self;
        let read = stdout.read_to_end(&mut seen);
        drop(stop);
        watchdog.join().expect("watchdog");
        let mut out = child.wait_with_output().expect("reap heph log -f");
        out.stdout = seen;
        assert!(
            read.is_ok(),
            "read heph log -f: {read:?}\n{}",
            describe(&out)
        );
        out
    }
}

/// Run `name` and walk it through its phases: after each, the one live file
/// holds exactly what was written so far, then its gate opens. With `follow`,
/// a `heph log -f` started once phase 1 is live must print each phase before
/// its gate opens. Returns the finished run, and the follower's.
fn run_through_phases(
    ws: &Workspace,
    dist: &Dist,
    name: &str,
    follow: bool,
) -> (Output, Option<Output>) {
    let home = ws.root().join(".heph");
    let addr = format!("//app:{name}");
    let child = ws
        .cmd(dist, &["run", &addr])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heph");

    let mut want = String::new();
    let mut live: Option<PathBuf> = None;
    let mut follower: Option<Follower> = None;
    for (phase, lines) in (1..).zip(PHASES) {
        want.push_str(lines);
        wait_until(
            &format!("{addr}: the live log to hold phase {phase}"),
            LIVE_DEADLINE,
            || {
                let logs = live_logs(&home);
                let [log] = logs.as_slice() else {
                    return false;
                };
                std::fs::read_to_string(log).is_ok_and(|s| s == want)
            },
        );
        let logs = live_logs(&home);
        // The same file across phases: it grew, it was not replaced.
        if let Some(first) = &live {
            assert_eq!(&logs, std::slice::from_ref(first), "{addr}: one live file");
        }
        live = logs.into_iter().next();
        if follow {
            follower
                .get_or_insert_with(|| Follower::spawn(ws, dist, &addr))
                .expect(&want);
        }
        ws.write(format!("app/{name}.{phase}"), "")
            .expect("open the gate");
    }
    let out = wait_output(child, common::RUN_DEADLINE).expect("heph run");
    let left = live_logs(&home);
    assert!(
        left.is_empty(),
        "{addr}: the live file outlived its run: {left:?}\n{}",
        describe(&out)
    );
    (out, follower.map(Follower::finish))
}

/// **A real cdylib driver given `log_capture` streams its log live.** Its
/// interleaved stdout and stderr reach one live file under `<home>/runs/` in
/// write order while the run lasts; the file grows in place, and is gone once
/// the run ends. A `heph log -f` started mid-run prints each chunk as it lands
/// and exits 0 when the run ends; after a success, `heph log` prints the same
/// bytes from the cache entry's `Log`. On failure, the slot's log is the same
/// bytes.
#[test]
fn log_capture_streams_live() {
    let dist = Dist::locate();
    let ws = workspace(&dist);
    let full: String = PHASES.concat();

    let (out, followed) = run_through_phases(&ws, &dist, "ok", true);
    assert!(out.status.success(), "//app:ok: {}", describe(&out));
    let followed = followed.expect("a follower");
    assert!(
        followed.status.success(),
        "heph log -f //app:ok: {}",
        describe(&followed)
    );
    assert_eq!(
        String::from_utf8_lossy(&followed.stdout),
        full,
        "heph log -f //app:ok prints the run's whole log, and nothing after it"
    );

    let shown = ws
        .run(&dist, &["log", "//app:ok"])
        .expect("heph log //app:ok");
    assert!(shown.status.success(), "{}", describe(&shown));
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout),
        full,
        "heph log prints the cached run's log, and only it, on stdout"
    );

    let (out, _) = run_through_phases(&ws, &dist, "fail", false);
    assert!(!out.status.success(), "//app:fail must fail");
    let inspect = ws
        .run(&dist, &["inspect", "failure", "--json", "//app:fail"])
        .expect("inspect failure");
    assert!(inspect.status.success(), "{}", describe(&inspect));
    let view: serde_json::Value =
        serde_json::from_slice(&inspect.stdout).expect("inspect failure --json");
    let log = view
        .get("log")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("no slot log: {view}"));
    assert_eq!(
        std::fs::read_to_string(log).expect("read the slot log"),
        full,
        "the failure slot keeps the bytes the live file held"
    );
}
