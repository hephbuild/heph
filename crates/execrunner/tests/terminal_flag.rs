//! The terminal flag reaches the runner host from both spawn paths.
//!
//! A runner that moves the target behind another transport decides on
//! `RunnerCtx::tty` whether to hand a terminal through (`docker exec -t`), so
//! the places that know must say so: the io spawns' `ctty`, the foreground
//! spawn's PTY slave — whose `Spec` says `ctty: false`, because that path does
//! its own `setsid`/`TIOCSCTTY` outside `proc_exec` — and, with no slave, the
//! foreground spawn's inherited fds.
//!
//! Its own binary because it installs a runner host, which is process-global;
//! the crate's unit tests rely on none being installed.

use execrunner::foreground::{ForegroundCommand, spawn_foreground};
use execrunner::{
    PathPolicy, PrepareOutcome, RunnerHost, RunnerRef, SpecRewrite, install_host, output,
    spawn_io_with_path,
};
use hcore::hasync::{Cancellable, StdCancellationToken};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hproc::proc_exec;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const UNSET: u8 = 2;

/// Answers one request id, so two tests in one process do not read each
/// other's record, and keeps the last `tty` it was handed.
struct RecordingHost {
    request_id: &'static str,
    tty: AtomicU8,
    /// Fail the `prepare` after recording, so nothing is spawned: for a path
    /// whose child this test has no way to wait on, or must not start.
    refuse: bool,
}

impl RecordingHost {
    fn install(request_id: &'static str) -> Arc<Self> {
        Self::install_with(request_id, false)
    }

    fn install_refusing(request_id: &'static str) -> Arc<Self> {
        Self::install_with(request_id, true)
    }

    fn install_with(request_id: &'static str, refuse: bool) -> Arc<Self> {
        let host = Arc::new(Self {
            request_id,
            tty: AtomicU8::new(UNSET),
            refuse,
        });
        install_host(host.clone());
        host
    }

    fn recorded(&self) -> Option<bool> {
        match self.tty.swap(UNSET, Ordering::SeqCst) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl RunnerHost for RecordingHost {
    fn owns(&self, request_id: &str) -> bool {
        request_id == self.request_id
    }

    fn alive(&self) -> bool {
        true
    }

    async fn prepare(
        &self,
        _request_id: &str,
        _addr: &Addr,
        rewrite: SpecRewrite,
        tty: bool,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<PrepareOutcome> {
        self.tty.store(u8::from(tty), Ordering::SeqCst);
        if self.refuse {
            anyhow::bail!("recorded; refusing so nothing is spawned");
        }
        Ok(PrepareOutcome {
            rewrite,
            supplies_environment: false,
        })
    }
}

fn addr() -> Addr {
    Addr::new(PkgBuf::from("tools"), "runner".to_string(), BTreeMap::new())
}

/// A child that exits at once, by a path both Linux and macOS have (`/bin/true`
/// is `/usr/bin/true` on macOS).
const EXIT_NOW: (&str, &[&str]) = ("/bin/sh", &["-c", "exit 0"]);

fn exit_now_args() -> Vec<OsString> {
    EXIT_NOW.1.iter().map(OsString::from).collect()
}

/// A spec as the exec driver builds it: on a PTY slave with `ctty` for a
/// `--shell`, on pipes without it for a captured run.
fn spec(ctty: bool, slave: std::os::fd::OwnedFd) -> proc_exec::Spec {
    proc_exec::Spec {
        program: PathBuf::from(EXIT_NOW.0),
        args: exit_now_args(),
        env: vec![],
        cwd: PathBuf::from("/"),
        stdin: if ctty {
            proc_exec::StdioSpec::Fd(slave)
        } else {
            proc_exec::StdioSpec::Null
        },
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: true,
        ctty,
    }
}

/// A build target's `--shell` goes through `spawn_io_with_path` — the exec
/// driver's spawn — with `ctty` on the spec. A captured run says the opposite.
#[tokio::test]
async fn the_drivers_spawn_reports_its_ctty() {
    let host = RecordingHost::install_refusing("tty-spawn-io");
    let addr = addr();
    let ctoken = StdCancellationToken::new();

    for ctty in [true, false] {
        let (_master, slave) = hproc::pty::open_pty().expect("open_pty");
        let refused = spawn_io_with_path(
            RunnerRef::target("tty-spawn-io", &addr),
            spec(ctty, slave),
            &PathPolicy::default(),
            &ctoken,
        )
        .await;
        assert!(refused.is_err(), "the host refuses after recording");
        assert_eq!(
            host.recorded(),
            Some(ctty),
            "the runner must be told whether the target owns a terminal"
        );
    }
}

/// The same through `output`, which runs the child to completion — the other
/// entry point that derives the flag from the spec.
#[tokio::test]
async fn the_output_spawn_reports_its_ctty() {
    let host = RecordingHost::install("tty-io");
    let addr = addr();
    let ctoken = StdCancellationToken::new();

    for ctty in [true, false] {
        // Held open so the slave does not hang up under the tool.
        let (_master, slave) = hproc::pty::open_pty().expect("open_pty");
        let out = output(
            RunnerRef::target("tty-io", &addr),
            spec(ctty, slave),
            &ctoken,
        )
        .await
        .expect("run the child");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            host.recorded(),
            Some(ctty),
            "the runner must be told whether the target owns a terminal"
        );
    }
}

/// An entrypoint on the PTY heph owns: the slave travels on the command, and
/// the spec the runner sees says `ctty: false`, so the flag must come from the
/// slave.
#[tokio::test]
async fn the_foreground_spawn_on_a_slave_reports_a_terminal() {
    let host = RecordingHost::install("tty-fg");
    let addr = addr();
    let ctoken = StdCancellationToken::new();

    let (_master, slave) = hproc::pty::open_pty().expect("open_pty");
    let cmd = ForegroundCommand::new(EXIT_NOW.0, exit_now_args(), vec![], "/").with_slave(slave);
    let (child, _) = spawn_foreground(
        RunnerRef::target("tty-fg", &addr),
        cmd,
        &PathPolicy::default(),
        &ctoken,
    )
    .await
    .expect("prepare");
    let child = child.expect("spawn the child");

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (stops, _stopped) = tokio::sync::mpsc::unbounded_channel();
    let (_continue, continues) = tokio::sync::mpsc::unbounded_channel();
    child
        .wait_pty(rx, stops, continues, &ctoken)
        .await
        .expect("wait");

    assert_eq!(
        host.recorded(),
        Some(true),
        "a spawn on a PTY slave runs on a terminal, whatever the placeholder spec says"
    );
}

/// Set in the re-executed child of
/// [`an_inherited_spawn_is_a_terminal_only_when_all_three_fds_are`].
const CHILD_ENV: &str = "HEPH_TERMINAL_FLAG_CHILD";
const CHILD_TEST: &str = "inherited_stdio_child";

const SAW_TERMINAL: i32 = 10;
const SAW_NO_TERMINAL: i32 = 11;
const NEVER_ASKED: i32 = 12;
const SPAWNED: i32 = 13;

/// The child half of the inherit test: a no-op unless the parent re-executed
/// this binary with a PTY slave on some of its stdio. It reports what the
/// runner host was told through its exit code, before anything is spawned.
#[tokio::test]
async fn inherited_stdio_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let host = RecordingHost::install_refusing("tty-inherit");
    let addr = addr();
    let ctoken = StdCancellationToken::new();
    // No slave: the tool would inherit this process's own fds, as `--raw` does.
    let cmd = ForegroundCommand::new(EXIT_NOW.0, exit_now_args(), vec![], "/");
    let refused = spawn_foreground(
        RunnerRef::target("tty-inherit", &addr),
        cmd,
        &PathPolicy::default(),
        &ctoken,
    )
    .await
    .is_err();
    let code = match (refused, host.recorded()) {
        (false, _) => SPAWNED,
        (true, Some(true)) => SAW_TERMINAL,
        (true, Some(false)) => SAW_NO_TERMINAL,
        (true, None) => NEVER_ASKED,
    };
    std::process::exit(code);
}

/// `--raw`: the tool inherits heph's own fds, and is on a terminal only when
/// all three of them are. One redirected stream means bytes the caller wants
/// as bytes, so the runner must not be told to allocate a terminal.
///
/// A child process per case, because the rule reads the process's own fds 0-2
/// and a test that `dup2`ed onto them would change them under every other test
/// thread in this binary.
#[test]
fn an_inherited_spawn_is_a_terminal_only_when_all_three_fds_are() {
    let exe = std::env::current_exe().expect("current_exe");
    // Which of (stdin, stdout, stderr) are on the slave; the rest are /dev/null.
    let cases = [
        ([true, true, true], SAW_TERMINAL),
        ([false, true, true], SAW_NO_TERMINAL),
        ([true, false, true], SAW_NO_TERMINAL),
        ([true, true, false], SAW_NO_TERMINAL),
    ];
    for (on_slave, want) in cases {
        // The master stays open for the whole case so the slave never hangs up.
        let (_master, slave) = hproc::pty::open_pty().expect("open_pty");
        let stdio = |on: bool| {
            if on {
                std::process::Stdio::from(slave.try_clone().expect("dup the slave"))
            } else {
                std::process::Stdio::null()
            }
        };
        let [stdin, stdout, stderr] = on_slave;
        let status = std::process::Command::new(&exe)
            .args([CHILD_TEST, "--exact", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdin(stdio(stdin))
            .stdout(stdio(stdout))
            .stderr(stdio(stderr))
            .status()
            .expect("re-execute the test binary");
        assert_eq!(
            status.code(),
            Some(want),
            "with (stdin, stdout, stderr) on a terminal = {on_slave:?}"
        );
    }
}
