//! The telemetry hand-off as a process: a command spools its event and exits,
//! and the detached `heph __telemetry-flush` it started outlives it and sends.
//!
//! Only reachable here: the flusher is the shipped binary re-executing itself
//! in a new session after the parent exited, and the opt-outs are decided in
//! `main`, which no linked test runs. Nothing here can reach PostHog: every run
//! that could send — telemetry on, or opted out with a spool left over (which
//! is sent once) — points `HEPH_TELEMETRY_ENDPOINT` at a loopback address — a
//! sink this test serves, or a port nothing listens on.

mod common;

use common::{BASE_CONFIG, Dist, Workspace, describe};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The env vars heph reads as "this is CI" (`telemetry::CI_ENV_VARS`). CI
/// itself sets some of them, and on CI telemetry flushes synchronously instead
/// of handing off, so every run here clears them all.
const CI_ENV_VARS: &[&str] = &[
    "CI",
    "CONTINUOUS_INTEGRATION",
    "BUILD_NUMBER",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "CIRCLECI",
    "TRAVIS",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "BUILDKITE",
    "DRONE",
    "APPVEYOR",
    "BITBUCKET_BUILD_NUMBER",
    "TF_BUILD",
    "CODEBUILD_BUILD_ID",
];

/// A loopback port nothing listens on (discard): a flush to it fails at once.
const DEAD_ENDPOINT: &str = "http://127.0.0.1:9";

/// How many spooled events make a flush due (`telemetry::FLUSH_AT_EVENTS`).
const FLUSH_AT_EVENTS: usize = 50;

/// `ws.cmd` with telemetry on, off CI, pointed at `endpoint`.
fn telemetry_cmd(ws: &Workspace, dist: &Dist, args: &[&str], endpoint: &str) -> Command {
    let mut cmd = ws.cmd(dist, args);
    cmd.env_remove("HEPH_DISABLE_TELEMETRY")
        .env_remove("XDG_CONFIG_HOME")
        .env("HEPH_TELEMETRY_ENDPOINT", endpoint)
        .stdin(Stdio::null());
    for var in CI_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd
}

fn spool(ws: &Workspace) -> PathBuf {
    ws.home().join(".config/heph/telemetry-spool.db")
}

/// `heph tool telemetry status --json`, parsed.
fn status(ws: &Workspace, dist: &Dist, endpoint: &str) -> serde_json::Value {
    let out = telemetry_cmd(
        ws,
        dist,
        &["tool", "telemetry", "status", "--json"],
        endpoint,
    )
    .output()
    .expect("spawn heph tool telemetry status");
    assert!(out.status.success(), "{}", describe(&out));
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("{e}: {}", describe(&out)))
        .expect("status --json prints one JSON object")
}

/// One HTTP request off `stream`: its body, by `Content-Length`.
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("read request");
        assert!(n > 0, "connection closed mid-headers");
        buf.extend_from_slice(chunk.get(..n).expect("chunk"));
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(buf.get(..header_end).expect("head")).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .ok_or_else(|| format!("no content-length in:\n{head}"))
        .expect("a content-length header")
        .trim()
        .parse()
        .expect("content-length");
    while buf.len() < header_end + len {
        let n = stream.read(&mut chunk).expect("read body");
        assert!(n > 0, "connection closed mid-body");
        buf.extend_from_slice(chunk.get(..n).expect("chunk"));
    }
    buf.get(header_end..header_end + len)
        .expect("body")
        .to_vec()
}

/// A PostHog stand-in on loopback. Answers every request `200`, handing each
/// body to the test — except the first, which it holds unanswered until the
/// test releases it.
struct Sink {
    url: String,
    bodies: mpsc::Receiver<Vec<u8>>,
    release_first: mpsc::Sender<()>,
}

impl Sink {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind sink");
        let url = format!("http://{}", listener.local_addr().expect("sink addr"));
        let (body_tx, bodies) = mpsc::channel();
        let (release_first, release_rx) = mpsc::channel::<()>();
        // Detached: it blocks in `accept` until the test process exits.
        std::thread::spawn(move || {
            let mut first = true;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let body = read_request(&mut stream);
                if body_tx.send(body).is_err() {
                    return;
                }
                if std::mem::take(&mut first) && release_rx.recv().is_err() {
                    return;
                }
                let reply = br#"{"status":1}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                drop(stream.write_all(head.as_bytes()));
                drop(stream.write_all(reply));
            }
        });
        Self {
            url,
            bodies,
            release_first,
        }
    }
}

/// `(pid, ppid, command)` for every process on the host.
fn processes() -> Vec<(u32, u32, String)> {
    let out = Command::new("ps")
        // `-ww`: never truncate the command line to a terminal width.
        .args(["-A", "-ww", "-o", "pid=,ppid=,command="])
        .output()
        .expect("spawn ps");
    assert!(out.status.success(), "{}", describe(&out));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            Some((pid, ppid, it.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

/// The whole hand-off: the command that makes the spool due exits without
/// waiting, and the flusher it started — still running after it, holding a
/// request this test has not answered — sends the spooled events, starts no
/// supervisor, and once answered records a successful flush.
#[test]
fn the_flusher_outlives_the_command_and_sends_the_spool() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let sink = Sink::start();

    // Each command spools one event; the one that brings the spool to a full
    // batch finds the flush due and starts the flusher.
    for i in 1..=FLUSH_AT_EVENTS {
        let started = Instant::now();
        let out = telemetry_cmd(&ws, &dist, &["version"], &sink.url)
            .output()
            .expect("spawn heph version");
        assert!(out.status.success(), "run {i}: {}", describe(&out));
        // The sink holds the flusher's request until after this returns, so a
        // command that waited on its flusher would sit out the flusher's 30 s
        // deadline here.
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "run {i} waited for the flusher: {:?}",
            started.elapsed()
        );
    }

    let body = sink
        .bodies
        .recv_timeout(Duration::from_secs(30))
        .expect("the flusher never posted to the sink");
    let batch: serde_json::Value = serde_json::from_slice(&body).expect("batch json");
    let events = batch["batch"].as_array().expect("a /batch/ request");
    assert_eq!(events.len(), FLUSH_AT_EVENTS, "one full batch: {batch}");
    for e in events {
        assert_eq!(e["event"], "cli_command", "{e}");
        assert_eq!(e["properties"]["command"], "version", "{e}");
    }

    // Every command has exited; the flusher is still running, blocked on the
    // request it is waiting to have answered. It must have started no child of
    // its own — no `__supervisor` sidecar in particular.
    let ws_name = ws
        .root()
        .file_name()
        .expect("workspace dir name")
        .to_string_lossy()
        .into_owned();
    let procs = processes();
    let flusher = procs
        .iter()
        .find(|(_, _, cmd)| cmd.contains("__telemetry-flush") && cmd.contains(&ws_name))
        .unwrap_or_else(|| panic!("no flusher for {ws_name} among:\n{procs:#?}"));
    let children: Vec<_> = procs
        .iter()
        .filter(|(_, ppid, _)| *ppid == flusher.0)
        .collect();
    assert!(
        children.is_empty(),
        "the flusher started children: {children:?}"
    );

    sink.release_first.send(()).expect("release the flusher");

    // Answered, the flusher deletes what it sent and records the outcome.
    // Polling `status` spools one event per poll (sent by the same flusher if
    // it is still draining), so `pending_events` ends at most at the poll
    // count — never back at the full batch.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut polls = 0;
    let last = loop {
        let s = status(&ws, &dist, &sink.url);
        polls += 1;
        if s["last_flush_ok"] == true {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the flusher never recorded a successful flush: {s:#}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(last["last_flush_ms"].is_i64(), "{last:#}");
    assert!(
        last["pending_events"].as_u64().expect("pending") <= polls,
        "the sent batch is still spooled: {last:#}"
    );
    assert_eq!(last["endpoint_overridden"], true, "{last:#}");
    assert_eq!(last["flush_mode"], "detached", "{last:#}");
}

/// `heph __telemetry-flush` with the kill switch set and nothing spooled is
/// inert: it exits 0, prints nothing, and leaves nothing under `HOME` — no
/// config dir, no spool, no lock — because it is dispatched before logging,
/// clap or the supervisor, and an opted-out flush with no spool on disk
/// creates nothing.
#[test]
fn a_disabled_flusher_does_nothing() {
    let dist = Dist::locate();
    let home = tempfile::tempdir().expect("home tempdir");
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let out = Command::new(dist.heph())
        .arg("__telemetry-flush")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .env("HEPH_TELEMETRY_ENDPOINT", DEAD_ENDPOINT)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .stdin(Stdio::null())
        .output()
        .expect("spawn heph __telemetry-flush");

    assert!(out.status.success(), "{}", describe(&out));
    assert!(out.stdout.is_empty(), "{}", describe(&out));
    assert!(out.stderr.is_empty(), "{}", describe(&out));
    for dir in [home.path(), cwd.path()] {
        let left: Vec<_> = std::fs::read_dir(dir)
            .expect("ls")
            .map(|e| e.expect("entry").path())
            .collect();
        assert!(left.is_empty(), "the flusher wrote {left:?}");
    }
}

/// Spool one event with telemetry on (to a dead loopback endpoint; one event
/// never makes a flush due).
fn seed_spool(ws: &Workspace, dist: &Dist) {
    let out = telemetry_cmd(ws, dist, &["version"], DEAD_ENDPOINT)
        .output()
        .expect("spawn heph version");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(spool(ws).exists(), "no spool after a telemetry-on run");
}

/// The spool's files (it and its `-wal`/`-shm` sidecars) still on disk.
fn spool_files_left(path: &Path) -> Vec<String> {
    let dir = path.parent().expect("config dir");
    std::fs::read_dir(dir)
        .expect("ls config dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("telemetry-spool.db"))
        .collect()
}

/// Wait for the detached opt-out flusher to delete the spool — within its 35 s
/// watchdog, whatever its send did.
fn await_spool_gone(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let left = spool_files_left(path);
        if left.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "spool left behind: {left:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Opting out with the kill switch sends what was spooled before — recorded
/// while telemetry was on — once, through the detached flusher, then deletes
/// the spool. The opted-out command does not wait for the send.
#[test]
fn the_env_opt_out_sends_the_spool_once_then_deletes_it() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    seed_spool(&ws, &dist);
    let sink = Sink::start();
    // Answer the flusher's request as soon as it arrives.
    sink.release_first.send(()).expect("release");

    // `ws.cmd` sets `HEPH_DISABLE_TELEMETRY`; the endpoint keeps the hand-off
    // on loopback.
    let mut cmd = ws.cmd(&dist, &["version"]);
    cmd.env_remove("XDG_CONFIG_HOME")
        .env("HEPH_TELEMETRY_ENDPOINT", &sink.url)
        .stdin(Stdio::null());
    for var in CI_ENV_VARS {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn heph version");
    assert!(out.status.success(), "{}", describe(&out));

    let body = sink
        .bodies
        .recv_timeout(Duration::from_secs(30))
        .expect("the opt-out flusher never posted to the sink");
    let batch: serde_json::Value = serde_json::from_slice(&body).expect("batch json");
    let events = batch["batch"].as_array().expect("a /batch/ request");
    assert_eq!(events.len(), 1, "the one seeded event: {batch}");
    let event = events.first().expect("one event");
    assert_eq!(event["properties"]["command"], "version", "{batch}");
    await_spool_gone(&spool(&ws));
}

/// Opting out in the workspace config with the endpoint unreachable: the one
/// send fails, and the spool is deleted all the same.
#[test]
fn the_config_opt_out_deletes_the_spool_when_the_send_fails() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    seed_spool(&ws, &dist);

    ws.config(&format!("{BASE_CONFIG}telemetry:\n  enabled: false\n"))
        .expect("config");
    let out = telemetry_cmd(&ws, &dist, &["version"], DEAD_ENDPOINT)
        .output()
        .expect("spawn heph version");
    assert!(out.status.success(), "{}", describe(&out));
    await_spool_gone(&spool(&ws));
}
