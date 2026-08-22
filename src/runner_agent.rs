//! The two hidden subcommands behind `Agent` exec sessions.
//!
//! - `heph __runner-agent --socket <path> [--prelude <file>]` runs *inside* the
//!   environment (a `devenv shell`) and forks every target's process from there.
//! - `heph __runner-exec --socket <path> -- <argv…>` is the per-target client
//!   heph spawns as its own ordinary child, which hands the agent its own stdio
//!   descriptors and exits with the child's status.
//!
//! Both are parsed before clap, like `__supervisor`, so neither drags the CLI
//! or a tokio runtime into a path that must stay small.
//!
//! See `hexec_runner::agent` for why the client exists at all rather than heph
//! talking to the agent directly.

use hexec_runner::agent::{self, ExecReply, ExecRequest};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process::ExitCode;

pub enum HiddenCommand {
    Agent {
        socket: PathBuf,
        prelude: Option<PathBuf>,
    },
    Exec {
        socket: PathBuf,
        argv: Vec<String>,
    },
}

/// Recognize the hidden invocations without clap.
pub fn parse(mut args: impl Iterator<Item = String>) -> Option<HiddenCommand> {
    match args.next()?.as_str() {
        "__runner-agent" => {
            let mut socket = None;
            let mut prelude = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--socket" => socket = args.next().map(PathBuf::from),
                    "--prelude" => prelude = args.next().map(PathBuf::from),
                    _ => return None,
                }
            }
            Some(HiddenCommand::Agent {
                socket: socket?,
                prelude,
            })
        }
        "__runner-exec" => {
            let mut socket = None;
            let mut argv = Vec::new();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--socket" => socket = args.next().map(PathBuf::from),
                    // Everything after `--` is the target's own command, which
                    // must never be read as a flag of ours.
                    "--" => {
                        argv.extend(args.by_ref());
                        break;
                    }
                    _ => return None,
                }
            }
            if argv.is_empty() {
                return None;
            }
            Some(HiddenCommand::Exec {
                socket: socket?,
                argv,
            })
        }
        _ => None,
    }
}

pub fn run(cmd: HiddenCommand) -> ExitCode {
    match cmd {
        HiddenCommand::Agent { socket, prelude } => run_agent(&socket, prelude.as_deref()),
        HiddenCommand::Exec { socket, argv } => run_client(&socket, argv),
    }
}

/// Client: hand the agent this process's own stdio and the command to run, then
/// exit with whatever the child exited with.
fn run_client(socket: &std::path::Path, argv: Vec<String>) -> ExitCode {
    let req = ExecRequest {
        argv,
        // This process's environment IS the composed one — heph's driver built
        // it and `proc_exec` applied it with `env_clear`. Forwarding it means
        // the agent can `env_clear` on its side too, so the shell's own ambient
        // variables never reach the target.
        env: std::env::vars().collect(),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        setsid: true,
    };

    // 0/1/2 are already wired to the target's pipes or PTY by heph. Passing the
    // descriptors themselves, rather than proxying bytes, is what keeps the
    // bounded drain and the PTY line discipline out of this path entirely.
    let stdio: [RawFd; 3] = [0, 1, 2];

    match agent::request(socket, &req, stdio) {
        Ok(ExecReply::Exited { code }) => match code {
            Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
            // Killed by a signal. 128+n is the shell convention; the exact
            // signal is not recoverable through `ExitCode`, so report the
            // generic "died on a signal" rather than inventing success.
            None => ExitCode::from(137),
        },
        Ok(ExecReply::Error { message }) => {
            eprintln!("exec agent: {message}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("exec agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Agent: serve requests until the listener is closed.
fn run_agent(socket: &std::path::Path, prelude: Option<&std::path::Path>) -> ExitCode {
    let listener = match agent::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("exec agent: bind {}: {e}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    let prelude = prelude.and_then(|p| std::fs::read_to_string(p).ok());

    // Removed on the way out so a clean exit does not leave a socket file that
    // the next session would have to recognize as stale.
    let _cleanup = SocketCleanup(socket.to_path_buf());

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let prelude = prelude.clone();
        // One thread per request: a target's process can run for minutes, and a
        // single-threaded agent would serialize the whole build behind it —
        // which is the opposite of what a shared session is for.
        std::thread::spawn(move || {
            // A failed request is reported to its own client over the socket;
            // there is nothing useful for the agent to do with it here, and a
            // panic would take the whole session down for one target.
            drop(agent::serve_one(conn, &move |req, stdio| {
                exec_and_wait(req, stdio, prelude.as_deref())
            }));
        });
    }
    ExitCode::SUCCESS
}

struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.0));
    }
}

/// Fork/exec the request in this process's environment and wait for it.
fn exec_and_wait(
    req: ExecRequest,
    stdio: [RawFd; 3],
    prelude: Option<&str>,
) -> anyhow::Result<Option<i32>> {
    use std::os::unix::process::CommandExt as _;

    // With a prelude, the command runs through bash so the environment's shell
    // functions are defined first — which is the whole reason a devenv user
    // reaches for `mode = "session"` over a snapshot.
    let mut cmd = match prelude {
        Some(p) => {
            let mut c = std::process::Command::new("bash");
            c.arg("-c")
                .arg(format!("{p}\n\"$@\""))
                .arg("--")
                .args(&req.argv);
            c
        }
        None => {
            let (program, rest) = req
                .argv
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("empty argv"))?;
            let mut c = std::process::Command::new(program);
            c.args(rest);
            c
        }
    };

    cmd.current_dir(&req.cwd);
    // `env_clear` is the point: this agent lives inside the devenv shell, so
    // inheriting its environment would put the developer's ambient variables
    // into every build — unhashed, under a key that reports as lockfile-pinned.
    cmd.env_clear().envs(req.env.iter().map(|(k, v)| (k, v)));

    let setsid = req.setsid;
    // SAFETY: the closure runs in the forked child before `exec`, where the only
    // calls it makes — `dup2` and `setsid` — are async-signal-safe and touch
    // just that child's own descriptor table and session. The descriptors came
    // from `recvmsg` and are open in this process, so they survive the fork.
    unsafe {
        cmd.pre_exec(move || redirect_and_detach(stdio, setsid));
    }

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "spawn {:?} inside the exec session: {e}",
            req.argv.first().map_or("", String::as_str)
        )
    })?;
    let status = child.wait()?;
    Ok(status.code())
}

/// The child's pre-exec setup: point 0/1/2 at the descriptors the client sent,
/// and start a new session so heph's supervisor can reap the whole tree.
///
/// Split out so each `unsafe` call states its own safety condition rather than
/// hiding behind one block around the whole closure.
fn redirect_and_detach(stdio: [RawFd; 3], setsid: bool) -> std::io::Result<()> {
    for (target, src) in stdio.iter().enumerate() {
        // SAFETY: async-signal-safe, and in the forked child `target` (0, 1 or
        // 2) is ours to replace. `src` was received over the socket and is open.
        let rc = unsafe { libc::dup2(*src, target as RawFd) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if setsid {
        // SAFETY: async-signal-safe; the child is not already a process-group
        // leader, having just been forked.
        let rc = unsafe { libc::setsid() };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(v: &[&str]) -> Option<HiddenCommand> {
        parse(v.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn an_ordinary_invocation_is_not_a_hidden_command() {
        assert!(parse_args(&["run", "//x:y"]).is_none());
        assert!(parse_args(&[]).is_none());
    }

    #[test]
    fn the_agent_form_parses() {
        match parse_args(&["__runner-agent", "--socket", "/s.sock"]) {
            Some(HiddenCommand::Agent { socket, prelude }) => {
                assert_eq!(socket, PathBuf::from("/s.sock"));
                assert!(prelude.is_none());
            }
            other => panic!("expected an agent command, got {}", other.is_some()),
        }
    }

    /// Everything after `--` belongs to the target. Without this, a target
    /// running `foo --socket bar` would silently retarget the client.
    #[test]
    fn the_targets_own_flags_are_not_read_as_ours() {
        match parse_args(&[
            "__runner-exec",
            "--socket",
            "/s.sock",
            "--",
            "cargo",
            "--socket",
            "nope",
        ]) {
            Some(HiddenCommand::Exec { socket, argv }) => {
                assert_eq!(socket, PathBuf::from("/s.sock"));
                assert_eq!(argv, vec!["cargo", "--socket", "nope"]);
            }
            other => panic!("expected an exec command, got {}", other.is_some()),
        }
    }

    #[test]
    fn an_exec_with_no_command_is_rejected() {
        assert!(parse_args(&["__runner-exec", "--socket", "/s.sock", "--"]).is_none());
    }
}
