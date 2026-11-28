//! The working-set journal at the process boundary: what a CI cache snapshot
//! knows about a heph that exited, and about one that was killed.
//!
//! `crates/e2e/tests/e2e/cache_snapshot.rs` covers the journal in-process,
//! where a kill can only be simulated by stripping the trailer. What only the
//! shipped binary has is the real exit: the trailer written by `Drop for
//! Journal` as the process ends, none at all under `SIGKILL`, the lock the OS
//! releases with the dead process, and the lock's fd staying out of the
//! targets heph spawns (std opens close-on-exec).

mod common;

use anyhow::Context as _;
use common::{Dist, RUN_DEADLINE, Workspace, describe, signal, wait_output, wait_until};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long the slow target sleeps: long enough to be killed mid-run.
const SLOW_SECS: u32 = 120;

/// How long the build may take to reach the slow target's run.
const START_DEADLINE: Duration = Duration::from_secs(120);

/// The trailer `Drop for Journal` writes, and its last line.
const TRAILER: &str = r#"{"t":"Trailer"}"#;

/// Every journal a reader would read: `<root>/.heph/cache/working-set/*.log`.
fn journals(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join(".heph/cache/working-set")) else {
        return vec![];
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect()
}

fn the_journal(root: &Path) -> (PathBuf, String) {
    let found = journals(root);
    assert_eq!(found.len(), 1, "expected one journal, found {found:?}");
    let path = found.into_iter().next().expect("one journal");
    let text = std::fs::read_to_string(&path).expect("read the journal");
    (path, text)
}

/// `heph tool cache snapshot --json`: its exit code and its document.
fn snapshot(ws: &Workspace, dist: &Dist, snap: &Path) -> (i32, serde_json::Value) {
    let child = ws
        .cmd(dist, &["tool", "cache", "snapshot", "--json"])
        .env("HEPH_CACHE_SNAPSHOT", snap)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heph tool cache snapshot");
    let out = wait_output(child, Duration::from_secs(120)).expect("heph tool cache snapshot");
    let doc = serde_json::from_slice(&out.stdout)
        .with_context(|| describe(&out))
        .expect("stdout is one JSON document");
    (out.status.code().expect("an exit code"), doc)
}

/// The paths `pid` has open.
#[cfg(target_os = "linux")]
fn open_paths(pid: i32) -> Vec<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("list the target's fds")
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .map(|p| p.display().to_string())
        .collect()
}

/// The paths `pid` has open.
#[cfg(target_os = "macos")]
fn open_paths(pid: i32) -> Vec<String> {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-p", &pid.to_string(), "-Fn"])
        .output()
        .expect("lsof is part of macOS");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(str::to_string)
        .collect()
}

/// D13: `heph run` exits with a clean trailer; `kill -9` mid-request leaves
/// none, and `heph tool cache snapshot` then reports an incomplete run
/// instead of waiting on the dead process; the journal's lock fd is not
/// inherited by the target heph runs.
#[test]
fn the_shipped_binary_writes_the_trailer_and_kill_leaves_none() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let snap_parent = tempfile::tempdir().expect("snapshot parent");
    let snap = snap_parent.path().join("snap");
    let pid_file = ws.home().join("slow.pid");
    ws.write(
        "pkg/BUILD",
        &format!(
            "target(name = \"a\", driver = \"bash\", run = [\"echo a > $OUT\"], out = \"a.txt\")\n\
             target(name = \"slow\", driver = \"bash\", deps = [\"//pkg:a\"], \
             run = [\"echo $$ > {pid}; sleep {SLOW_SECS}; echo s > $OUT\"], out = \"s.txt\")\n",
            pid = pid_file.display(),
        ),
    )
    .expect("BUILD");

    // A clean exit: the journal ends with its trailer.
    let out = ws
        .cmd(&dist, &["run", "//pkg:a"])
        .env("HEPH_CACHE_SNAPSHOT", &snap)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(anyhow::Error::from)
        .and_then(|c| wait_output(c, RUN_DEADLINE))
        .expect("heph run //pkg:a");
    assert!(out.status.success(), "{}", describe(&out));
    let (path, text) = the_journal(ws.root());
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines
            .first()
            .is_some_and(|l| l.contains(r#""magic":"heph-working-set""#)),
        "{}: {text}",
        path.display()
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""t":"Revision""#) && l.contains(r#""name":"a""#)),
        "{}: //pkg:a is not recorded: {text}",
        path.display()
    );
    assert_eq!(lines.last(), Some(&TRAILER), "{}: {text}", path.display());
    let (code, doc) = snapshot(&ws, &dist, &snap);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["complete"], true, "{doc}");
    assert!(journals(ws.root()).is_empty(), "the journal is consumed");

    // Killed mid-request: `//pkg:a`'s hit opened the journal, then the slow
    // target ran until heph was killed.
    let stderr = std::fs::File::create(ws.home().join("slow.stderr")).expect("stderr file");
    let mut child = ws
        .cmd(&dist, &["run", "//pkg:slow"])
        .env("HEPH_CACHE_SNAPSHOT", &snap)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn heph run //pkg:slow");
    let heph_pid = i32::try_from(child.id()).expect("pid");
    wait_until("the slow target to start", START_DEADLINE, || {
        std::fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    });
    let target_pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid file")
        .trim()
        .parse()
        .expect("a pid");
    let (journal, _) = the_journal(ws.root());
    let open = open_paths(target_pid);
    assert!(
        !open.is_empty(),
        "no open files listed for the target {target_pid}"
    );
    assert!(
        !open.iter().any(|p| p.contains("working-set")),
        "the target inherited the journal: {open:?}"
    );

    signal(heph_pid, libc::SIGKILL);
    child.wait().expect("reap heph");
    let text = std::fs::read_to_string(&journal).expect("read the killed journal");
    assert!(
        !text.lines().any(|l| l == TRAILER),
        "{}: a killed heph wrote a trailer: {text}",
        journal.display()
    );

    // The dead process's lock went with it: no wait, no exit 3, and the run
    // reads as incomplete.
    let (code, doc) = snapshot(&ws, &dist, &snap);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["complete"], false, "{doc}");
    let incomplete = doc["incomplete"].as_array().expect("incomplete is a list");
    assert!(
        incomplete
            .iter()
            .any(|i| i["reason"].as_str().is_some_and(|r| r.contains("trailer"))),
        "{doc}"
    );
}
