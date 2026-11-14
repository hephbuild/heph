//! Reading a heph session from the outside, as a second terminal would: the
//! session directory under the run's `HOME` (`~/.heph/sessions/<id>/`), its
//! records, and its lock — with nothing linked from the workspace.

use super::{Dist, Workspace, describe};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// What the line backend prints on stderr when a session starts.
const START_LINE: &str = "heph: session ";

/// The session id a run announced on stderr, if it did.
pub fn id_in(stderr: &str) -> Option<String> {
    let rest = stderr.split(START_LINE).nth(1)?;
    let id: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    (!id.is_empty()).then_some(id)
}

/// Where sessions of runs with `home` as `HOME` land.
pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".heph").join("sessions")
}

/// Every complete record of a session's `events.ndjson`, in order.
pub fn records(dir: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("events.ndjson")).unwrap_or_default();
    let complete = text.rsplit_once('\n').map_or("", |(done, _)| done);
    complete
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The `End` record, if it is the last one.
pub fn last_end(dir: &Path) -> Option<serde_json::Value> {
    let session = records(dir).pop()?.get("session")?.clone();
    (session.get("type")? == "End").then_some(session)
}

/// Whether a build event of `kind` for `addr` was recorded.
pub fn recorded(dir: &Path, kind: &str, addr: &str) -> bool {
    records(dir).iter().any(|r| {
        r.pointer("/kind/type").is_some_and(|t| t == kind)
            && r.pointer("/kind/addr").is_some_and(|a| a == addr)
    })
}

/// Whether the session's lock is free: the owner is gone, or released it.
/// The probe is what `FLock::is_path_held` does — take it, and let go.
pub fn lock_free(dir: &Path) -> bool {
    let file = std::fs::File::open(dir.join("lock")).expect("open the session's lock");
    // SAFETY: a live fd for the duration of both calls.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if taken {
        // SAFETY: as above.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
    taken
}

/// `heph session list --json`'s entry for `id`, which must be listed.
pub fn list_entry(ws: &Workspace, dist: &Dist, id: &str) -> serde_json::Value {
    let out = ws
        .run(dist, &["session", "list", "--json"])
        .expect("session list");
    assert!(out.status.success(), "{}", describe(&out));
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).expect("list --json");
    let entry = list
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .and_then(|all| all.iter().find(|s| s.get("id").is_some_and(|i| i == id)));
    assert!(entry.is_some(), "{id} is not listed: {list}");
    entry.cloned().unwrap_or_default()
}

/// A heph run in the background, with no terminal (the line backend), its
/// output going to files the test reads while it runs.
pub struct Bg {
    child: Child,
    stderr: PathBuf,
    logs: tempfile::TempDir,
    pub id: String,
    pub dir: PathBuf,
}

/// How long a run takes to announce its session, or a viewer to answer.
pub const SHORT: Duration = Duration::from_secs(60);

impl Bg {
    /// Spawn `heph <args>` and wait for its session's start line.
    pub fn spawn(ws: &Workspace, dist: &Dist, args: &[&str]) -> Self {
        let logs = tempfile::tempdir().expect("logs tempdir");
        let stderr = logs.path().join("stderr");
        let child = ws
            .cmd(dist, args)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(logs.path().join("stdout")).expect("stdout file"))
            .stderr(std::fs::File::create(&stderr).expect("stderr file"))
            .spawn()
            .expect("spawn heph");
        let mut bg = Self {
            child,
            stderr,
            logs,
            id: String::new(),
            dir: PathBuf::new(),
        };
        let started = Instant::now();
        bg.id = loop {
            if let Some(id) = id_in(&bg.stderr()) {
                break id;
            }
            assert!(
                started.elapsed() < SHORT && matches!(bg.child.try_wait(), Ok(None)),
                "heph {} announced no session\n{}",
                args.join(" "),
                bg.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        bg.dir = sessions_dir(ws.home()).join(&bg.id);
        bg
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("pid fits")
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    pub fn stdout(&self) -> String {
        std::fs::read_to_string(self.logs.path().join("stdout")).unwrap_or_default()
    }

    /// Wait until the run has recorded `ExecuteStart` for `addr`: the target is
    /// running, and the session is live.
    pub fn wait_executing(&self, addr: &str) {
        super::wait_until(&format!("{addr} to start executing"), SHORT, || {
            recorded(&self.dir, "ExecuteStart", addr)
        });
    }

    /// Wait for the run to exit, and return its status; past
    /// [`super::RUN_DEADLINE`] it is killed and the test fails.
    pub fn wait(&mut self) -> std::process::ExitStatus {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("wait on heph") {
                return status;
            }
            let late = started.elapsed() > super::RUN_DEADLINE;
            if late {
                drop(self.child.kill());
            }
            assert!(
                !late,
                "heph still running after {:?}\n{}",
                super::RUN_DEADLINE,
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Bg {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}
