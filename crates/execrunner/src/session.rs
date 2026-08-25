//! The builtin `session` runner: hold an environment open, run targets in it.
//!
//! Its config is one field — the argv that runs a command *inside* the
//! environment:
//!
//! ```json
//! { "runner": "session", "config": { "launch": ["devenv", "shell", "--"] } }
//! ```
//!
//! The runner appends `heph __runner-agent --socket S` to that, so the agent
//! ends up living inside whatever `launch` sets up. Everything after is the
//! protocol in [`crate::agent`].
//!
//! The consequence is worth stating: **a plugin that only wants agent mode
//! needs no runner code at all.** It writes a `runner.json` naming `session`
//! with the right `launch` prefix, and the mechanics — descriptor passing,
//! cancellation, signal fidelity, session pooling — are shared. A plugin only
//! writes its own runner when it has a lifecycle of its own to manage, which is
//! why the OCI runner exists and the devenv one does not.

use crate::SpecRewrite;
use crate::agent::{AGENT_SUBCOMMAND, CLIENT_SUBCOMMAND, SOCK_ENV};
use crate::registry::{ExecRunner, RunnerCtx};
use hproc::proc_exec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for a freshly-launched agent to answer.
///
/// A cold `devenv shell` evaluates nix, which is minutes on an empty store —
/// hence generous. It is a backstop against a launch command that will never
/// answer, not a latency budget; the cancellation token is what a user's Ctrl-C
/// travels through.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// argv that runs a command inside the environment. The agent invocation is
    /// appended to it.
    pub launch: Vec<String>,
}

/// A live agent.
struct Session {
    socket: PathBuf,
    /// Held for the session's lifetime. Dropping a `proc_exec::Handle` reaps
    /// its child, so this field *is* the session's lifetime.
    ///
    /// Behind a `Mutex` because the handle is `Send` but not `Sync` on macOS
    /// (it owns an `mpsc::Receiver`), and a session is shared across every
    /// target using it. Never locked — the mutex is here to make the handle
    /// shareable, not to coordinate anything.
    _agent: Mutex<proc_exec::Handle>,
    pgid: i32,
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.pgid > 0 {
            // SAFETY: a pgid this process created via `setsid`.
            unsafe { libc::killpg(self.pgid, libc::SIGTERM) };
        }
        _ = std::fs::remove_file(&self.socket);
    }
}

/// Sessions, keyed by `(runner address, fingerprint)`.
///
/// Not the fingerprint alone. It is author-supplied in a hand-written runner,
/// so two unrelated runners declaring the same string would otherwise share one
/// environment — and the second one's targets would run somewhere the build
/// never described. Keying on the address too costs nothing and removes the
/// class.
type SessionKey = (String, String);

/// One lazily-started session, behind a per-key async lock.
type SessionSlot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

pub struct SessionRunner {
    /// Where agent sockets live. Handed over rather than discovered.
    ///
    /// `sun_path` is 104 bytes on macOS (108 on Linux) and a macOS `$TMPDIR` is
    /// `/var/folders/xy/<~30 chars>/T/`, so a socket under it plus a digest
    /// overflows — `bind` fails, or silently truncates and two sessions collide
    /// on one socket. heph controls the depth of its own state directory, so it
    /// supplies one. (`$TMPDIR` is also simply absent in a plugin's
    /// environment, which is the other half of the rule.)
    socket_dir: PathBuf,
    /// One slot per key, so two targets racing to first-use the same session
    /// start one agent rather than two — while two *different* sessions still
    /// start concurrently.
    ///
    /// The slot holds `None` after a failed start rather than a cached error: a
    /// transient failure (a network blip during nix evaluation) must not
    /// poison every remaining target in the process.
    slots: Mutex<HashMap<SessionKey, SessionSlot>>,
}

impl SessionRunner {
    pub fn new(socket_dir: PathBuf) -> Self {
        Self {
            socket_dir,
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn slot(
        &self,
        key: SessionKey,
    ) -> anyhow::Result<Arc<tokio::sync::Mutex<Option<Arc<Session>>>>> {
        let mut slots = self
            .slots
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("exec-runner session table poisoned"))?;
        Ok(Arc::clone(slots.entry(key).or_default()))
    }

    async fn session(
        &self,
        ctx: &RunnerCtx<'_>,
        cfg: &SessionConfig,
    ) -> anyhow::Result<Arc<Session>> {
        let key = (ctx.addr.to_string(), ctx.fingerprint.to_string());
        let slot = self.slot(key.clone())?;
        let mut guard = slot.lock().await;
        if let Some(live) = guard.as_ref() {
            return Ok(Arc::clone(live));
        }
        let started = Arc::new(self.launch(ctx, cfg).await?);
        *guard = Some(Arc::clone(&started));
        Ok(started)
    }

    async fn launch(&self, ctx: &RunnerCtx<'_>, cfg: &SessionConfig) -> anyhow::Result<Session> {
        if cfg.launch.is_empty() {
            anyhow::bail!(
                "runner {}: the `session` runner needs a `launch` argv — the command that runs \
                 something inside the environment, e.g. [\"devenv\", \"shell\", \"--\"]",
                ctx.addr
            );
        }

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("runner {}: locate the heph binary: {e}", ctx.addr))?;

        std::fs::create_dir_all(&self.socket_dir).map_err(|e| {
            anyhow::anyhow!(
                "runner {}: create agent socket dir {:?}: {e}",
                ctx.addr,
                self.socket_dir
            )
        })?;

        // Short and fixed-width, for `sun_path`. The pid disambiguates two heph
        // processes sharing a home directory; the digest, two sessions in one.
        let socket =
            self.socket_dir
                .join(format!("{}-{}.sock", short_digest(ctx), std::process::id()));
        assert_socket_path_fits(&socket)?;

        let (program, args) = build_launch_argv(cfg, &exe, &socket)?;

        // `setsid` so the supervisor sidecar's killpg reaps the agent *and* the
        // environment wrapper around it on a hard shutdown.
        let spec = proc_exec::Spec {
            program,
            args,
            // The agent inherits heph's environment plus the socket path. Its
            // own environment is never what a target gets — the agent
            // `env_clear`s and applies exactly what the client forwards.
            env: std::env::vars_os()
                .chain(std::iter::once((
                    OsString::from(SOCK_ENV),
                    socket.clone().into_os_string(),
                )))
                .collect(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            stdin: proc_exec::StdioSpec::Null,
            // Inherited so a failing `devenv shell` says why on heph's stderr
            // rather than into a pipe nobody drains.
            stdout: proc_exec::StdioSpec::Inherit,
            stderr: proc_exec::StdioSpec::Inherit,
            setsid: true,
            ctty: false,
        };

        let agent = proc_exec::spawn(spec).map_err(|e| {
            anyhow::anyhow!(
                "runner {}: start the session agent via {:?}: {e}",
                ctx.addr,
                cfg.launch
            )
        })?;
        let pgid = agent.pid();

        await_socket(&socket, ctx).await.inspect_err(|_failed| {
            // Reap the half-started environment rather than leaving it behind
            // for the rest of the process's life.
            if pgid > 0 {
                // SAFETY: a pgid this process created via `setsid`.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        })?;

        Ok(Session {
            socket,
            _agent: Mutex::new(agent),
            pgid,
        })
    }
}

/// `<launch...> <heph> __runner-agent --socket <path>`.
fn build_launch_argv(
    cfg: &SessionConfig,
    exe: &Path,
    socket: &Path,
) -> anyhow::Result<(PathBuf, Vec<OsString>)> {
    let mut it = cfg.launch.iter();
    let head = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty `launch` argv"))?;
    let mut args: Vec<OsString> = it.map(OsString::from).collect();
    args.push(exe.as_os_str().to_os_string());
    args.push(OsString::from(AGENT_SUBCOMMAND));
    args.push(OsString::from("--socket"));
    args.push(socket.as_os_str().to_os_string());
    Ok((PathBuf::from(head), args))
}

/// A short, stable name for this runner's socket.
fn short_digest(ctx: &RunnerCtx<'_>) -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = xxhash_rust::xxh3::Xxh3Default::new();
    ctx.addr.hash(&mut h);
    ctx.fingerprint.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// macOS `sun_path` is 104 bytes, Linux 108. Checked rather than discovered as
/// an `ENAMETOOLONG` from `bind`, or — worse on some libcs — a silent
/// truncation that makes two sessions share one socket.
fn assert_socket_path_fits(socket: &Path) -> anyhow::Result<()> {
    const SUN_PATH_MIN: usize = 104;
    let len = socket.as_os_str().as_encoded_bytes().len();
    if len >= SUN_PATH_MIN {
        anyhow::bail!(
            "agent socket path is {len} bytes, over the {SUN_PATH_MIN}-byte unix-socket limit \
             (macOS; Linux allows 108): {socket:?}. Move heph's home directory somewhere shorter."
        );
    }
    Ok(())
}

/// Wait for the agent to start answering.
///
/// Polls rather than watching the directory, because the interesting failure is
/// the launch command dying, not the file appearing late. Cancellation is
/// checked every tick: a cold environment start is the single most likely
/// moment for a user to press Ctrl-C, and a timeout alone would ignore it for
/// minutes.
async fn await_socket(socket: &Path, ctx: &RunnerCtx<'_>) -> anyhow::Result<()> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if ctx.ctoken.is_cancelled() {
            anyhow::bail!(
                "runner {}: cancelled while starting the session agent",
                ctx.addr
            );
        }
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "runner {}: the session agent did not start within {}s. Its launch command's \
                 output is on heph's stderr above.",
                ctx.addr,
                HANDSHAKE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[async_trait::async_trait]
impl ExecRunner for SessionRunner {
    fn name(&self) -> &str {
        "session"
    }

    async fn prepare(
        &self,
        ctx: &RunnerCtx<'_>,
        mut rewrite: SpecRewrite,
    ) -> anyhow::Result<SpecRewrite> {
        let cfg: SessionConfig = serde_json::from_value(ctx.config.clone())
            .map_err(|e| anyhow::anyhow!("runner {}: parse session config: {e}", ctx.addr))?;
        let session = self.session(ctx, &cfg).await?;

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("runner {}: locate the heph binary: {e}", ctx.addr))?;

        // The target's own program becomes an argument of the client, which is
        // what heph forks in its place. `program` moves because it is the path
        // `execve` resolves, not merely argv[0].
        let mut args: Vec<OsString> = Vec::with_capacity(rewrite.args.len() + 3);
        args.push(OsString::from(CLIENT_SUBCOMMAND));
        args.push(OsString::from("--"));
        args.push(rewrite.program.clone().into_os_string());
        args.append(&mut rewrite.args);
        rewrite.args = args;
        rewrite.program = exe;

        // The one control value that rides the environment; the agent strips
        // this exact key before `execve`.
        rewrite.env.retain(|(k, _)| k != SOCK_ENV);
        rewrite.env.push((
            OsString::from(SOCK_ENV),
            session.socket.clone().into_os_string(),
        ));
        Ok(rewrite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_argv_appends_the_agent_invocation() {
        let cfg = SessionConfig {
            launch: vec!["devenv".to_string(), "shell".to_string(), "--".to_string()],
        };
        let (program, args) = build_launch_argv(
            &cfg,
            Path::new("/usr/local/bin/heph"),
            Path::new("/run/x.sock"),
        )
        .expect("argv");
        assert_eq!(program, PathBuf::from("devenv"));
        assert_eq!(
            args,
            vec![
                OsString::from("shell"),
                OsString::from("--"),
                OsString::from("/usr/local/bin/heph"),
                OsString::from("__runner-agent"),
                OsString::from("--socket"),
                OsString::from("/run/x.sock"),
            ]
        );
    }

    /// Caught here rather than as an `ENAMETOOLONG` from `bind` — or, on some
    /// libcs, a silent truncation that makes two sessions share a socket.
    #[test]
    fn an_overlong_socket_path_is_refused_with_the_limit() {
        let long = PathBuf::from(format!("/tmp/{}/agent.sock", "x".repeat(120)));
        let err = assert_socket_path_fits(&long).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("104"), "{msg}");
    }

    #[test]
    fn a_short_socket_path_is_accepted() {
        assert_socket_path_fits(Path::new("/tmp/h/ab12cd34-999.sock")).expect("fits");
    }

    #[test]
    fn an_empty_launch_argv_is_rejected() {
        let cfg = SessionConfig { launch: vec![] };
        build_launch_argv(&cfg, Path::new("/heph"), Path::new("/s.sock"))
            .expect_err("an empty launch argv has no command to run");
    }
}
