//! `heph session`: what a second terminal sees of a running heph.
//!
//! Process-level by nature: a session is how one heph *process* is watched
//! from another, a lock wait names the holder's pid (one per process), and
//! the guarantees are about how a process ends — its exit code, a signal, a
//! `SIGKILL` no in-process test can survive. The recording's own logic is
//! unit-tested in `crates/tui` and `src/commands/session`.

mod common;

use common::session::{
    Bg, SHORT, id_in, last_end, list_entry, lock_free, recorded_dirs, sessions_dir,
};
use common::{Dist, Workspace, describe, signal, wait_until};

/// `exit3` is an entrypoint, so heph exits with its tool's code — 3, which no
/// failure of heph's own produces; `exit125` exits with the code heph's own
/// failures use, which `attach` must say is the session's. `long` is killed or interrupted before it
/// ends; `held` is built by two
/// runs at once, the second waiting on the first's result lock, which it
/// announces only past the engine's 5s notice.
const BUILD: &str = r#"
target(name = "ok", driver = "bash", run = "true", cache = False)
target(name = "fail", driver = "bash", run = "exit 1", cache = False)
target(name = "exit3", driver = "bash", entrypoint = True, run = "sleep 2; exit 3")
target(name = "exit125", driver = "bash", entrypoint = True, run = "sleep 2; exit 125")
target(name = "slowfail", driver = "bash", run = "sleep 4; exit 1", cache = False)
target(name = "long", driver = "bash", run = "sleep 60", cache = False)
target(name = "held", driver = "bash", run = "sleep 12")
"#;

fn workspace() -> Workspace {
    let ws = Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", BUILD).expect("write BUILD");
    ws
}

/// The session a finished, captured run recorded.
fn session_of(ws: &Workspace, out: &std::process::Output) -> std::path::PathBuf {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let id = id_in(&stderr);
    assert!(id.is_some(), "no session announced\n{}", describe(out));
    sessions_dir(ws.home()).join(id.unwrap_or_default())
}

/// However the process ends — success, a failed target, Ctrl-C, an error
/// before anything builds — the last record is `End` with its exit code and
/// the lock is free; a `SIGKILL` leaves no `End`, and `list` says `lost`.
#[test]
fn end_written_on_every_exit() {
    let dist = Dist::locate();
    let ws = workspace();

    for (addr, code) in [("//pkg:ok", 0), ("//pkg:fail", 1), ("//pkg:missing", 1)] {
        let out = ws.run(&dist, &["run", addr]).expect("run");
        assert_eq!(out.status.code(), Some(code), "{addr}\n{}", describe(&out));
        let dir = session_of(&ws, &out);
        let end = last_end(&dir).unwrap_or_else(|| panic!("{addr}: no End last"));
        assert_eq!(end["exit_code"], code, "{addr}: {end}");
        assert_eq!(end["panicked"], false, "{addr}: {end}");
        assert!(lock_free(&dir), "{addr}: lock still held");
    }

    // Ctrl-C: SIGINT to the process mid-target.
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:long"]);
    run.wait_executing("//pkg:long");
    signal(run.pid(), libc::SIGINT);
    let status = run.wait();
    let end = last_end(&run.dir)
        .unwrap_or_else(|| panic!("Ctrl-C: no End last ({status})\n{}", run.stderr()));
    assert_ne!(status.code(), Some(0), "{}", run.stderr());
    if let Some(code) = status.code() {
        assert_eq!(end["exit_code"], code, "{end}");
    }
    assert!(lock_free(&run.dir), "Ctrl-C: lock still held");
    let entry = list_entry(&ws, &dist, &run.id);
    assert_eq!(entry["state"], "failed", "{entry}");

    // SIGKILL: nothing runs at exit, so no End — and the lock went with it.
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:long"]);
    run.wait_executing("//pkg:long");
    signal(run.pid(), libc::SIGKILL);
    run.wait();
    assert!(last_end(&run.dir).is_none(), "an End after SIGKILL");
    assert!(lock_free(&run.dir), "SIGKILL: lock still held");
    let entry = list_entry(&ws, &dist, &run.id);
    assert_eq!(entry["state"], "lost", "{entry}");
}

/// Two runs build the same target: the second waits on the first's result
/// lock, and both `show` and `list --json` name the session holding it.
#[test]
fn lock_wait_names_the_holding_session() {
    let dist = Dist::locate();
    let ws = workspace();

    let mut holder = Bg::spawn(&ws, &dist, &["run", "//pkg:held"]);
    holder.wait_executing("//pkg:held");
    let mut waiter = Bg::spawn(&ws, &dist, &["run", "//pkg:held"]);

    let held_by = format!("held by session {}", holder.id);
    let mut show = String::new();
    wait_until("the waiter to name its holder", SHORT, || {
        let out = ws
            .run(&dist, &["session", "show", &waiter.id])
            .expect("session show");
        assert!(out.status.success(), "{}", describe(&out));
        show = String::from_utf8_lossy(&out.stdout).into_owned();
        show.contains(&held_by)
    });
    assert!(show.contains("Stuck on"), "{show}");
    assert!(show.contains("//pkg:held"), "{show}");

    let entry = list_entry(&ws, &dist, &waiter.id);
    let blocked = entry["blocked_on"]
        .as_array()
        .expect("blocked_on is a list");
    assert!(
        blocked.iter().any(|b| b["kind"] == "lock"
            && b["addr"] == "//pkg:held"
            && b["holder_session"] == holder.id.as_str()),
        "{entry}"
    );
    assert_eq!(entry["state"], "waiting", "{entry}");

    // The waiter's own line-backend notice names the holder's session too.
    let notice = format!("held by session {} (pid {})", holder.id, holder.pid());
    wait_until("the waiter's own notice to name its holder", SHORT, || {
        waiter.stderr().contains(&notice)
    });

    assert!(holder.wait().success(), "{}", holder.stderr());
    assert!(waiter.wait().success(), "{}", waiter.stderr());
}

/// `attach` without a terminal follows in line form and exits as the session
/// did; an unknown id and a lost session are its own errors, 125.
#[test]
fn attach_exit_codes() {
    let dist = Dist::locate();
    let ws = workspace();

    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:exit3"]);
    let out = ws
        .run(&dist, &["session", "attach", &run.id])
        .expect("attach");
    assert_eq!(out.status.code(), Some(3), "{}", describe(&out));
    assert_eq!(run.wait().code(), Some(3), "{}", run.stderr());

    // A mirrored 125 reads like attach's own failure, so attach says whose it
    // is — on stderr, leaving stdout clean.
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:exit125"]);
    let out = ws
        .run(&dist, &["session", "attach", &run.id])
        .expect("attach");
    assert_eq!(out.status.code(), Some(125), "{}", describe(&out));
    assert_eq!(run.wait().code(), Some(125), "{}", run.stderr());
    let note = format!("heph: session {} exited 125\n", run.id);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&note),
        "no {note:?} on stderr\n{}",
        describe(&out)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout not clean\n{}",
        describe(&out)
    );

    let out = ws
        .run(&dist, &["session", "attach", "zzzzzzzzzz"])
        .expect("attach");
    assert_eq!(out.status.code(), Some(125), "{}", describe(&out));

    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:long"]);
    run.wait_executing("//pkg:long");
    signal(run.pid(), libc::SIGKILL);
    run.wait();
    let out = ws
        .run(&dist, &["session", "attach", &run.id])
        .expect("attach");
    assert_eq!(out.status.code(), Some(125), "{}", describe(&out));
}

/// `inflight` gets the live run's in-flight report through the session
/// directory — no signal — and the run goes on to exit with its own code; an
/// ended session cannot answer, 125.
#[test]
fn inflight_round_trip() {
    let dist = Dist::locate();
    let ws = workspace();

    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:slowfail"]);
    run.wait_executing("//pkg:slowfail");
    let out = ws
        .run(&dist, &["session", "inflight", &run.id])
        .expect("inflight");
    assert!(out.status.success(), "{}", describe(&out));
    let report = String::from_utf8_lossy(&out.stdout);
    let written = std::fs::read_to_string(run.dir.join("inflight.txt")).expect("inflight.txt");
    assert!(!written.trim().is_empty(), "an empty report");
    assert_eq!(report.trim_end(), written.trim_end());
    assert!(
        !run.dir.join("inflight.request").exists(),
        "the request was not consumed"
    );
    // The failed build's own code, recorded as such.
    assert_eq!(run.wait().code(), Some(1), "{}", run.stderr());
    assert_eq!(last_end(&run.dir).expect("an End")["exit_code"], 1);

    let out = ws
        .run(&dist, &["session", "inflight", &run.id])
        .expect("inflight");
    assert_eq!(out.status.code(), Some(125), "{}", describe(&out));
}

/// `HEPH_NO_SESSION=1`: no directory, no start line.
#[test]
fn no_session_env() {
    let dist = Dist::locate();
    let ws = workspace();

    let out = ws
        .cmd(&dist, &["run", "//pkg:ok"])
        .env("HEPH_NO_SESSION", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        id_in(&String::from_utf8_lossy(&out.stderr)).is_none(),
        "{}",
        describe(&out)
    );
    let sessions = sessions_dir(ws.home());
    assert!(
        std::fs::read_dir(&sessions).map_or(true, |mut d| d.next().is_none()),
        "a session was recorded under {}",
        sessions.display()
    );

    // The same run without it records one: the check above can see one.
    let out = ws.run(&dist, &["run", "//pkg:ok"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(last_end(&session_of(&ws, &out)).is_some());
}

/// The shipped binary records where it ran: the session row carries the
/// workspace root and the directory heph was started in. `list` uses them:
/// inside a workspace it shows that workspace's sessions, `--all` every one
/// under the same `HOME`.
#[test]
fn list_filters_by_workspace() {
    let dist = Dist::locate();
    let a = workspace();
    let b = workspace();
    // One `HOME` for both, so both sessions land in one sessions directory.
    let home = a.home();
    let run_in = |ws: &Workspace, cwd: &std::path::Path, args: &[&str]| {
        let child = ws
            .cmd(&dist, args)
            .current_dir(cwd)
            .env("HEPH_CWD", cwd)
            .env("HOME", home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        common::wait_output(child, common::RUN_DEADLINE).expect("run")
    };
    let canonical = |p: &std::path::Path| std::fs::canonicalize(p).expect("canonicalize");

    // Started in a package directory, so `cwd` and the root differ.
    let mut ids = Vec::new();
    for ws in [&a, &b] {
        let cwd = ws.root().join("pkg");
        let out = run_in(ws, &cwd, &["run", "//pkg:ok"]);
        assert!(out.status.success(), "{}", describe(&out));
        let id = id_in(&String::from_utf8_lossy(&out.stderr));
        assert!(id.is_some(), "no session announced\n{}", describe(&out));
        let id = id.unwrap_or_default();
        let (root, at) = recorded_dirs(&sessions_dir(home).join(&id));
        for (field, got, want) in [
            ("workspace_root", root, ws.root()),
            ("cwd", at, cwd.as_path()),
        ] {
            assert!(got.is_some(), "{field} recorded");
            assert_eq!(
                canonical(std::path::Path::new(&got.unwrap_or_default())),
                canonical(want),
                "{field}"
            );
        }
        ids.push(id);
    }

    let listed = |all: bool| -> Vec<String> {
        let mut args = vec!["session", "list", "--json"];
        if all {
            args.push("--all");
        }
        let out = run_in(&a, a.root(), &args);
        assert!(out.status.success(), "{}", describe(&out));
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).expect("list --json");
        let mut ids: Vec<String> = list
            .as_array()
            .unwrap_or_else(|| panic!("list --json is a bare array: {list}"))
            .iter()
            .filter_map(|s| s["id"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        ids
    };
    let [in_a, in_b]: [String; 2] = ids.try_into().expect("two sessions");
    assert_eq!(listed(false), vec![in_a.clone()], "only this workspace's");
    let mut both = vec![in_a, in_b];
    both.sort();
    assert_eq!(listed(true), both, "--all lists every workspace's");

    // Outside a workspace (the temp `HOME` has no `.hephconfig` above it),
    // `list` widens on its own and says so — on stderr only, so `--json`
    // stdout still parses as one document.
    let out = run_in(&a, home, &["session", "list", "--json"]);
    assert!(out.status.success(), "{}", describe(&out));
    const NOTE: &str = "heph: not in a workspace; listing the sessions of every workspace";
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(NOTE),
        "the note leaked onto stdout\n{}",
        describe(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(NOTE),
        "no scope note on stderr\n{}",
        describe(&out)
    );
    let list: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}\n{}", describe(&out)));
    let mut widened: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("list --json is a bare array: {list}"))
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    widened.sort();
    assert_eq!(widened, both, "outside a workspace lists every workspace's");
}

/// `run --raw` sets up no session: no directory, and nothing on stderr — no
/// start line, no "records no session" warning — beside the tool's bytes.
#[test]
fn raw_records_no_session() {
    let dist = Dist::locate();
    let ws = workspace();
    ws.write(
        "raw/BUILD",
        r#"target(name = "tool", driver = "bash", entrypoint = True, run = "echo raw-tool-bytes")"#,
    )
    .expect("write BUILD");

    let out = ws
        .cmd(&dist, &["run", "//raw:tool", "--raw"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "raw-tool-bytes\n",
        "{}",
        describe(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "{}",
        describe(&out)
    );
    let sessions = sessions_dir(ws.home());
    assert!(
        std::fs::read_dir(&sessions).map_or(true, |mut d| d.next().is_none()),
        "a session was recorded under {}",
        sessions.display()
    );
}
