//! The agent protocol, end to end, minus the process boundary.
//!
//! In production the agent is `heph __runner-agent` and the client is
//! `heph __runner-exec`, so a fully faithful test needs the release binary and
//! belongs in `bin-e2e`. What is testable here is everything that actually
//! carries risk: the `SCM_RIGHTS` handshake, `dup2` onto the target's stdio,
//! exit-status and signal fidelity, and — the one nobody would notice
//! regressing — that a target dies when its client goes away.
//!
//! These drive the same `exec_via_agent` the real client calls. A test that
//! reimplemented the handshake would prove the test right, not the client.

#![expect(
    clippy::panic,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use execrunner::agent::{
    ExecOutcome, ExecRequest, exec_via_agent, serve_for_test, start_via_agent,
};
use std::ffi::OsString;
use std::io::{Read as _, Seek as _};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// An agent serving on a socket in a temp dir, for the life of the test.
struct Agent {
    _dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Agent {
    fn start() -> Agent {
        let dir = tempfile::tempdir().expect("tempdir");
        // Short, because macOS caps `sun_path` at 104 bytes and a temp dir is
        // most of that already.
        let socket = dir.path().join("a.sock");
        let s = socket.clone();
        std::thread::spawn(move || {
            // Runs until the test process exits.
            let _ = serve_for_test(&s);
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Agent { _dir: dir, socket };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("agent did not start listening on {socket:?}");
    }
}

fn sh(script: &str) -> ExecRequest {
    ExecRequest {
        program: PathBuf::from("/bin/sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        env: vec![(OsString::from("XR_ENV"), OsString::from("from-request"))],
        cwd: PathBuf::from("/"),
        ctty: false,
    }
}

/// stdout as a real file, so the assertion is on bytes the target wrote through
/// the descriptor the agent was handed — not on anything the protocol relayed.
fn capture() -> std::fs::File {
    tempfile::tempfile().expect("tempfile")
}

fn read_back(mut f: std::fs::File) -> String {
    f.rewind().expect("rewind");
    let mut s = String::new();
    f.read_to_string(&mut s).expect("read");
    s
}

fn run(agent: &Agent, req: &ExecRequest, out: &std::fs::File) -> ExecOutcome {
    let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
    exec_via_agent(
        &agent.socket,
        req,
        [devnull.as_fd(), out.as_fd(), out.as_fd()],
    )
    .expect("exec via agent")
}

/// The descriptors are *passed*: the target writes straight into the file the
/// caller opened, with nothing relaying the bytes.
#[test]
fn output_reaches_the_passed_descriptor() {
    let agent = Agent::start();
    let out = capture();
    let outcome = run(&agent, &sh("echo hello-from-the-target"), &out);
    assert_eq!(outcome, ExecOutcome::Exited(0));
    assert!(
        read_back(out).contains("hello-from-the-target"),
        "the target's stdout must arrive on the passed descriptor"
    );
}

/// The agent `env_clear`s and applies exactly what the client forwarded. If it
/// let a target inherit its own environment instead, the developer's ambient
/// state would reach every build unhashed, under a fingerprint-pinned key.
#[test]
fn the_target_gets_the_agents_environment_with_the_requests_on_top() {
    // The agent is the process the launch put *inside* the environment, so its
    // own `environ` is the environment — that is what agent mode is for. The
    // request is the target's own, and wins where the two disagree.
    //
    // SAFETY: set before the agent thread forks anything; this test process is
    // the only writer.
    unsafe { std::env::set_var("XR_AGENT_ONLY", "from-the-environment") };
    // SAFETY: as above. `sh()` puts `XR_ENV=from-request` in the request, so
    // this is the collision case.
    unsafe { std::env::set_var("XR_ENV", "from-the-environment") };
    let agent = Agent::start();
    let out = capture();
    let outcome = run(
        &agent,
        &sh("echo \"env=$XR_ENV agent_only=${XR_AGENT_ONLY:-absent}\""),
        &out,
    );
    assert_eq!(outcome, ExecOutcome::Exited(0));
    let got = read_back(out);
    assert!(
        got.contains("env=from-request"),
        "the target's own value must win over the environment's; got {got:?}"
    );
    assert!(
        got.contains("agent_only=from-the-environment"),
        "the environment the agent lives in must reach the target; got {got:?}"
    );
    // SAFETY: as above.
    unsafe { std::env::remove_var("XR_AGENT_ONLY") };
    // SAFETY: as above.
    unsafe { std::env::remove_var("XR_ENV") };
}

/// `PATH` is a list, so neither side simply wins: the target's entries lead and
/// the environment's follow. A target that declares a tool gets *that* one even
/// when the environment it runs in ships another by the same name.
#[test]
fn the_targets_path_entries_lead_and_the_environments_follow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let from_target = dir.path().join("target-bin");
    let from_env = dir.path().join("env-bin");
    for (d, who) in [(&from_target, "target"), (&from_env, "environment")] {
        std::fs::create_dir_all(d).expect("mkdir");
        let tool = d.join("xr-tool");
        std::fs::write(&tool, format!("#!/bin/sh\necho from-the-{who}\n")).expect("write");
        std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
    }

    // The agent's own PATH is the environment's half.
    // SAFETY: set before the agent thread forks anything; this test process is
    // the only writer.
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                from_env.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
    };
    let agent = Agent::start();

    let out = capture();
    let mut req = sh("xr-tool");
    req.env.push((
        OsString::from("PATH"),
        OsString::from(from_target.as_os_str()),
    ));
    let outcome = run(&agent, &req, &out);
    assert_eq!(outcome, ExecOutcome::Exited(0));
    let got = read_back(out);
    assert!(
        got.contains("from-the-target"),
        "the target's own PATH entry must lead; got {got:?}"
    );
}

#[test]
fn exit_codes_round_trip() {
    let agent = Agent::start();
    for code in [0, 1, 42, 127] {
        let out = capture();
        let outcome = run(&agent, &sh(&format!("exit {code}")), &out);
        assert_eq!(outcome, ExecOutcome::Exited(code), "exit {code}");
    }
}

/// A target killed by a signal must be reported as signalled, not as an exit
/// code that merely encodes one — heph's `ExitStatus` distinguishes them and so
/// does every consumer of it.
#[test]
fn a_signalled_target_is_reported_as_signalled() {
    let agent = Agent::start();
    let out = capture();
    let outcome = run(&agent, &sh("kill -TERM $$"), &out);
    assert_eq!(outcome, ExecOutcome::Signaled(libc::SIGTERM));
}

#[test]
fn the_requests_cwd_is_honoured() {
    let agent = Agent::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = capture();
    let mut req = sh("pwd");
    req.cwd = dir.path().to_path_buf();
    let outcome = run(&agent, &req, &out);
    assert_eq!(outcome, ExecOutcome::Exited(0));
    // macOS resolves /var → /private/var, so compare the final component.
    let want = dir
        .path()
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    assert!(read_back(out).contains(&want));
}

/// A program that does not exist must fail the request rather than hanging or
/// reporting success.
#[test]
fn a_missing_program_fails_the_request() {
    let agent = Agent::start();
    let out = capture();
    let mut req = sh("true");
    req.program = PathBuf::from("/definitely/not/here/xr9f2c");
    let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
    let outcome = exec_via_agent(
        &agent.socket,
        &req,
        [devnull.as_fd(), out.as_fd(), out.as_fd()],
    )
    .expect("protocol must still complete");
    match outcome {
        ExecOutcome::Failed(msg) => assert!(msg.contains("xr9f2c"), "{msg}"),
        other => panic!("expected a Failed outcome, got {other:?}"),
    }
}

/// **The one that matters.** heph killing the client does not kill the target —
/// different session, different process tree. The agent watches for the
/// client's socket closing and escalates; if it does not, a cancelled build
/// leaves compilers running inside a sandbox the cleaner is deleting.
///
/// Asserts the process is *gone*, not that a call returned.
#[test]
fn a_target_dies_when_its_client_disconnects() {
    let agent = Agent::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let pidfile = dir.path().join("pid");
    let out = capture();

    // Records its pid, then outlives any plausible test run. If cancellation
    // never reaches it, it is still alive at the assertion below.
    let req = sh(&format!("echo $$ > {}; sleep 300", pidfile.display()));

    let pid = {
        let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
        // Dropping this socket at the end of the scope is the whole test: it is
        // exactly what the agent sees when heph SIGKILLs the client.
        let _conn = start_via_agent(
            &agent.socket,
            &req,
            [devnull.as_fd(), out.as_fd(), out.as_fd()],
        )
        .expect("handshake");

        wait_for_pid(&pidfile).expect("the target should have started")
    };

    assert!(
        wait_until_gone(pid, Duration::from_secs(30)),
        "the target (pid {pid}) survived its client disconnecting; a cancelled \
         build would leave it writing into a sandbox being deleted"
    );
}

/// Two targets in flight at once must each get their own descriptors. If the
/// agent let one request's stdio leak into the next fork, heph's reader would
/// never see EOF on the first — the stray-descendant hang `setsid` exists to
/// prevent, reintroduced through another door.
#[test]
fn concurrent_targets_do_not_share_descriptors() {
    let agent = Agent::start();
    let a = capture();
    let b = capture();

    let devnull = std::fs::File::open("/dev/null").expect("/dev/null");
    let ca = start_via_agent(
        &agent.socket,
        &sh("sleep 0.2; echo AAA"),
        [devnull.as_fd(), a.as_fd(), a.as_fd()],
    )
    .expect("handshake a");
    let cb = start_via_agent(
        &agent.socket,
        &sh("echo BBB"),
        [devnull.as_fd(), b.as_fd(), b.as_fd()],
    )
    .expect("handshake b");

    // Drain both replies so neither is cancelled by our own disconnect.
    let (ra, rb) = (await_reply(ca), await_reply(cb));
    assert_eq!(ra, ExecOutcome::Exited(0));
    assert_eq!(rb, ExecOutcome::Exited(0));

    let (sa, sb) = (read_back(a), read_back(b));
    assert!(sa.contains("AAA") && !sa.contains("BBB"), "a got {sa:?}");
    assert!(sb.contains("BBB") && !sb.contains("AAA"), "b got {sb:?}");
}

fn await_reply(mut conn: std::os::unix::net::UnixStream) -> ExecOutcome {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len).expect("reply length");
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    conn.read_exact(&mut body).expect("reply body");
    ExecOutcome::decode(&body[..]).expect("decode reply")
}

/// Read a pid a target wrote, waiting for the file to appear.
fn wait_for_pid(path: &Path) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(pid) = s.trim().parse::<i32>()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Whether `pid` is gone, polled to a deadline.
fn wait_until_gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        // SAFETY: signal 0 only probes for the process's existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A session whose launch command dies must fail immediately, not wait out the
/// handshake window.
///
/// Found by an example: a container image that cannot execute the agent left
/// `docker run` exiting on the spot, and the build then sat for the full
/// handshake timeout — minutes of looking hung with nothing reporting why. The
/// launch here exits before the agent it was asked to run ever starts.
///
/// `/bin/sh -c 'exit 1'` rather than `/bin/false`: `/bin/sh` is the one binary
/// this repo's suites treat as present everywhere (see `proc_exec`'s own
/// tests), and `/bin/false` is genuinely absent on some of the hosts here.
#[test]
fn a_session_whose_launch_dies_fails_fast() {
    use execrunner::registry::{RunnerCtx, RunnerRegistry};
    use execrunner::session::SessionRunner;
    use hcore::hasync::StdCancellationToken;
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut reg = RunnerRegistry::default();
    reg.register(Arc::new(SessionRunner::new(dir.path().to_path_buf())))
        .expect("register");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let started = Instant::now();
    let err = rt.block_on(async {
        let ctoken = StdCancellationToken::new();
        let config = serde_json::json!({ "launch": ["/bin/sh", "-c", "exit 1"] });
        let ctx = RunnerCtx {
            addr: "//x:runner",
            fingerprint: "fp",
            config: &config,
            ctoken: &ctoken,
        };
        let runner = reg.get("session").expect("session runner").clone();
        runner
            .prepare(
                &ctx,
                execrunner::SpecRewrite {
                    program: PathBuf::from("/bin/echo"),
                    args: vec![],
                    env: vec![],
                    cwd: PathBuf::from("/"),
                },
            )
            .await
            .expect_err("a launch that exits must not be waited on")
    });

    let msg = format!("{err:#}");
    assert!(msg.contains("exited before it started listening"), "{msg}");
    assert!(msg.contains("/bin/sh"), "must name the launch: {msg}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "must fail fast, took {:?}",
        started.elapsed()
    );
}
