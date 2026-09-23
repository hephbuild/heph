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
//! # The environment is the agent's, not a declaration
//!
//! Nothing here says what the environment *contains*, and that is the point of
//! agent mode: the agent is a process the launch put inside the environment, so
//! its own `environ` **is** the environment. Targets are built from it (see
//! [`crate::agent`]) rather than from a map somebody had to transcribe into
//! `runner.json`.
//!
//! That is sound for the cache because a consumer names the runner *target*,
//! whose hashout is one of its hashed inputs — so whatever the runner provides
//! is already in the consumer's cache key by construction. A declared copy
//! would add a second thing to keep in sync with the first, and re-deriving the
//! environment somewhere it is not is exactly the drift the fingerprint exists
//! to catch.
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
use anyhow::Context as _;
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

/// How much of the launch command's output to keep for a failure message.
/// A tail, because the useful part of a nix or docker failure is the end.
const LAUNCH_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// argv that runs a command inside the environment. The agent invocation is
    /// appended to it.
    pub launch: Vec<String>,
    /// Working directory for the *launch* command — not for targets, which get
    /// their own sandbox cwd from the request. `devenv shell` resolves its
    /// environment relative to where it runs, so a runner target that is not in
    /// the devenv root has to say where the root is.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// A live agent.
struct Session {
    socket: PathBuf,
    /// The write end of the agent's stdin, held open for the session's life.
    ///
    /// This is the parent-death channel, and it is the only teardown that works
    /// unconditionally: the OS closes descriptors at process exit whether or
    /// not destructors run, so the agent sees EOF and exits even when heph
    /// panics, calls `process::exit`, or is `SIGKILL`ed. `shutdown` is the
    /// tidy path; this is the one that cannot be skipped.
    _keepalive: Option<std::process::ChildStdin>,
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

/// A started agent, remembered for teardown.
///
/// Kept separately from `slots` because teardown runs from `Drop`, which cannot
/// await the per-key async lock a slot sits behind. A plain list of pgids is
/// all the killpg needs and it can be taken synchronously.
#[derive(Debug, Clone)]
struct Live {
    pgid: i32,
    socket: PathBuf,
}

pub struct SessionRunner {
    /// The directory under which this user's private socket directory is
    /// made: [`SOCKET_ROOT`] outside tests. See [`socket_path`].
    socket_root: PathBuf,
    /// One slot per key, so two targets racing to first-use the same session
    /// start one agent rather than two — while two *different* sessions still
    /// start concurrently.
    ///
    /// The slot holds `None` after a failed start rather than a cached error: a
    /// transient failure (a network blip during nix evaluation) must not
    /// poison every remaining target in the process.
    slots: Mutex<HashMap<SessionKey, SessionSlot>>,
    /// Every agent this runner has started, for [`ExecRunner::shutdown`].
    live: Mutex<Vec<Live>>,
}

impl Default for SessionRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRunner {
    /// Sockets go under [`SOCKET_ROOT`].
    pub fn new() -> Self {
        Self::with_socket_root(PathBuf::from(SOCKET_ROOT))
    }

    /// Sockets go under `socket_root` instead, so a test does not share the
    /// real per-user directory with every other heph on the machine.
    pub fn with_socket_root(socket_root: PathBuf) -> Self {
        Self {
            socket_root,
            slots: Mutex::new(HashMap::new()),
            live: Mutex::new(Vec::new()),
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

        // Short and fixed-width, for `sun_path`. The pid disambiguates two heph
        // processes sharing a directory; the digest, two sessions in one.
        let name = format!("{}-{}.sock", short_digest(ctx), std::process::id());
        let socket = socket_path(&self.socket_root, &name)
            .with_context(|| format!("runner {}: place the agent socket", ctx.addr))?;

        let (program, args) = build_launch_argv(cfg, &exe, &socket)?;

        // `std::process::Command`, not `proc_exec`, and the reason is the whole
        // shape of this function.
        //
        // `proc_exec`'s only public wait is `spawn_wait`, which parks a blocking
        // task for the child's whole life. A session agent outlives every
        // request, so that task never completes — and tokio's runtime shutdown
        // *joins* blocking threads, so the runtime would wait for the agent
        // while the agent waits to be killed by teardown. That is a deadlock on
        // exit, and it is not fixable by ordering: by the time the runtime is
        // shutting down, `Engine::drop` has already run.
        //
        // `Child::try_wait` is non-blocking and needs no thread at all, which
        // removes the dependency entirely.
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args)
            .current_dir(
                cfg.cwd.as_ref().map(PathBuf::from).unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                }),
            )
            .env(SOCK_ENV, &socket)
            // Piped, emphatically not inherited. An inherited descriptor makes
            // the agent a co-owner of heph's own stdout and stderr, and the
            // agent outlives the request that started it — so anything reading
            // heph's output to EOF waits on the *agent*, forever. That is a
            // hang with no error; it cost a 30-minute CI timeout to find.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The parent-death channel. heph holds the write end; the agent
            // reads it and exits on EOF. The OS closes descriptors at process
            // exit whether or not destructors run, so this survives a panic, a
            // `process::exit`, and a `SIGKILL` — none of which any teardown
            // hook would see.
            .stdin(std::process::Stdio::piped());

        use std::os::unix::process::CommandExt as _;
        // Its own session, so one target's cancellation cannot reach another's
        // and the agent's killpg reaches its whole tree. Declared outside the
        // `unsafe` below so each block holds exactly one unsafe operation.
        let new_session = || {
            // SAFETY: async-signal-safe, and the only call in the post-fork
            // window.
            if unsafe { libc::setsid() } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: `pre_exec`'s contract is that the closure be
        // async-signal-safe. This one is a single `setsid` — no allocation, no
        // locks, nothing that can deadlock against a lock held across the fork.
        unsafe { cmd.pre_exec(new_session) };

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "runner {}: start the session agent via {:?}: {e}",
                ctx.addr,
                cfg.launch
            )
        })?;
        let pgid = child.id() as i32;
        let keepalive = child.stdin.take();

        // Drained continuously, not just on failure: a piped agent that fills
        // the pipe blocks in `write(2)`, so reading only on failure would
        // deadlock exactly the environments that are chatty at startup. The
        // tail is bounded — this is a diagnostic, not a log.
        let tail = Arc::new(Mutex::new(String::new()));
        for stream in [
            child.stdout.take().map(TailSource::Out),
            child.stderr.take().map(TailSource::Err),
        ]
        .into_iter()
        .flatten()
        {
            let tail = Arc::clone(&tail);
            std::thread::spawn(move || stream.drain_into(&tail));
        }

        let child = Arc::new(Mutex::new(child));
        if let Ok(mut live) = self.live.lock() {
            live.push(Live {
                pgid,
                socket: socket.clone(),
            });
        }

        await_socket(&socket, &child, &tail, ctx)
            .await
            .inspect_err(|_failed| {
                // Reap the half-started environment rather than leaving it
                // behind for the rest of the process's life.
                if pgid > 0 {
                    // SAFETY: a pgid this process created via `setsid`.
                    unsafe { libc::killpg(pgid, libc::SIGKILL) };
                }
                if let Ok(mut c) = child.lock() {
                    _ = c.wait();
                }
            })?;

        Ok(Session {
            socket,
            _keepalive: keepalive,
            pgid,
        })
    }
}

/// One of the agent's output streams, drained into the shared tail.
enum TailSource {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl TailSource {
    fn drain_into(self, tail: &Arc<Mutex<String>>) {
        use std::io::Read as _;
        let mut reader: Box<dyn std::io::Read> = match self {
            TailSource::Out(o) => Box::new(o),
            TailSource::Err(e) => Box::new(e),
        };
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                return;
            }
            let Ok(mut buf) = tail.lock() else { return };
            let Some(fresh) = chunk.get(..n) else { return };
            buf.push_str(&String::from_utf8_lossy(fresh));
            if buf.len() > LAUNCH_TAIL_BYTES {
                let cut = buf.len() - LAUNCH_TAIL_BYTES;
                // Trim on a char boundary; a split UTF-8 sequence would panic
                // the drain and lose the rest of the output.
                let cut = (cut..buf.len())
                    .find(|i| buf.is_char_boundary(*i))
                    .unwrap_or(buf.len());
                buf.replace_range(..cut, "");
            }
        }
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

/// The launch command's captured output, for a diagnostic.
fn launch_output(tail: &Arc<Mutex<String>>) -> String {
    match tail.lock() {
        Ok(buf) if buf.trim().is_empty() => "(it printed nothing)".to_string(),
        Ok(buf) => format!("\n{}", buf.trim_end()),
        Err(_poisoned) => "(unavailable)".to_string(),
    }
}

/// The launch argv, for a diagnostic — re-read from the config rather than
/// threaded through, since this is a cold error path.
fn cfg_launch_hint(ctx: &RunnerCtx<'_>) -> Vec<String> {
    serde_json::from_value::<SessionConfig>(ctx.config.clone())
        .map(|c| c.launch)
        .unwrap_or_default()
}

/// Where agent sockets go.
///
/// Not heph's home: its depth is wherever the user keeps the repository, and
/// `sun_path` is 104 bytes on macOS (108 on Linux) — a nested worktree is
/// enough to overflow it. Not `$TMPDIR` either: a macOS one is
/// `/var/folders/xy/<~30 chars>/T/`, most of the budget on its own, and it is
/// absent in a plugin's environment. A literal `/tmp` is short, present, and
/// the same on every supported platform.
///
/// It is shared, so sockets go in a per-user subdirectory that
/// [`ensure_private_dir`] vets: a directory someone else pre-created there
/// would otherwise hand them the socket every target's stdio is passed through.
const SOCKET_ROOT: &str = "/tmp";

/// macOS `sun_path` is 104 bytes, Linux 108; the smaller is the budget, so the
/// behavior is the same on both.
const SUN_PATH_MIN: usize = 104;

/// The socket named `name`, in this user's private directory under `root`.
fn socket_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let dir = root.join(format!("heph-{}", rustix::process::geteuid().as_raw()));
    let socket = dir.join(name);
    if !fits_sun_path(&socket) {
        // Only reachable with a deep test root; the real one is `/tmp`.
        anyhow::bail!(
            "agent socket path is {} bytes, over the {SUN_PATH_MIN}-byte unix-socket limit: \
             {socket:?}",
            socket.as_os_str().len()
        );
    }
    ensure_private_dir(&dir)?;
    Ok(socket)
}

/// Checked rather than discovered as an `ENAMETOOLONG` from `bind`, or — worse
/// on some libcs — a silent truncation that makes two sessions share a socket.
fn fits_sun_path(socket: &Path) -> bool {
    socket.as_os_str().as_encoded_bytes().len() < SUN_PATH_MIN
}

/// Create `dir` as `0700`, or accept it only if it already is a real directory
/// owned by this user that no one else can enter.
///
/// It lives in a world-writable parent, so its existence proves nothing: it
/// could be another user's, or a symlink to one. `symlink_metadata` rather
/// than `metadata` for exactly that reason.
fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create agent socket dir {dir:?}")),
    }
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("inspect agent socket dir {dir:?}"))?;
    let euid = rustix::process::geteuid().as_raw();
    if !meta.file_type().is_dir() || meta.uid() != euid || meta.mode() & 0o077 != 0 {
        anyhow::bail!(
            "agent socket dir {dir:?} is not a private directory of this user (want a directory \
             owned by uid {euid} with mode 0700; found {}, uid {}, mode {:o}). Remove it and \
             retry.",
            if meta.file_type().is_symlink() {
                "a symlink"
            } else if meta.file_type().is_dir() {
                "a directory"
            } else {
                "a file"
            },
            meta.uid(),
            meta.mode() & 0o7777,
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
async fn await_socket(
    socket: &Path,
    child: &Arc<Mutex<std::process::Child>>,
    tail: &Arc<Mutex<String>>,
    ctx: &RunnerCtx<'_>,
) -> anyhow::Result<()> {
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
        // Checked *after* the connect, so an agent that started and exited in
        // the same tick is still noticed as started.
        //
        // This is the difference between "the container image cannot run the
        // agent" reported in a second and a build that looks hung for the whole
        // handshake window. The launch command's own output is already on
        // heph's stderr — it inherits it — so the message points there rather
        // than trying to recover it.
        let exited = child
            .lock()
            .ok()
            .and_then(|mut c| c.try_wait().ok().flatten())
            .is_some();
        if exited {
            anyhow::bail!(
                "runner {}: the session agent exited before it started listening.\n  launch: \
                 {:?}\n  output: {}\nA container runner most often fails here because the \
                 image cannot execute the heph binary — heph is mounted in from the host, so a \
                 macOS binary cannot run in a Linux image.",
                ctx.addr,
                cfg_launch_hint(ctx),
                launch_output(tail),
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "runner {}: the session agent did not start within {}s.\n  launch: {:?}\n  \
                 output: {}",
                ctx.addr,
                HANDSHAKE_TIMEOUT.as_secs(),
                cfg_launch_hint(ctx),
                launch_output(tail),
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

    /// The environment is the agent's `environ`, which this process cannot see.
    /// The agent composes the final `PATH` from what the client forwards; a
    /// fallback reinstated here would arrive ahead of it.
    fn supplies_environment(&self) -> bool {
        true
    }

    /// Kill every agent this runner started.
    ///
    /// The sessions are also behind `slots`, but teardown runs from `Drop` and
    /// cannot await the per-key async lock those sit behind — hence the flat
    /// `live` list, which a synchronous caller can take.
    fn shutdown(&self) {
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        for session in live.drain(..) {
            if session.pgid > 0 {
                // SAFETY: a pgid this process created via `setsid`.
                unsafe { libc::killpg(session.pgid, libc::SIGTERM) };
            }
            _ = std::fs::remove_file(&session.socket);
        }
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
            cwd: None,
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

    #[test]
    fn sockets_go_in_a_private_per_user_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::Builder::new()
            .prefix("hs")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let socket = socket_path(root.path(), "ab12cd34-999.sock").expect("placed");

        assert!(fits_sun_path(&socket), "{socket:?}");
        let dir = socket.parent().expect("parent");
        assert_eq!(dir.parent(), Some(root.path()));
        let mode = std::fs::metadata(dir).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        // Reused, not refused, the second time.
        socket_path(root.path(), "ab12cd34-1000.sock").expect("reused");
    }

    /// Caught here rather than as an `ENAMETOOLONG` from `bind` — or, on some
    /// libcs, a silent truncation that makes two sessions share a socket.
    #[test]
    fn an_overlong_socket_path_is_refused_with_the_limit() {
        let deep = PathBuf::from(format!("/nonexistent/{}", "x".repeat(120)));
        let err = socket_path(&deep, "ab12cd34-999.sock").expect_err("must refuse");
        assert!(format!("{err:#}").contains("104"), "{err:#}");
        assert!(!deep.exists(), "nothing is created for a refused path");
    }

    /// `/tmp` is shared: a directory anyone else can enter would let them at the
    /// socket every target's stdio passes through.
    #[test]
    fn a_short_socket_dir_others_can_enter_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("open");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let err = ensure_private_dir(&dir).expect_err("must refuse");
        assert!(format!("{err:#}").contains("mode 777"), "{err:#}");
    }

    #[test]
    fn a_symlinked_short_socket_dir_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("real");
        std::fs::create_dir(&target).expect("mkdir");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = ensure_private_dir(&link).expect_err("must refuse");
        assert!(format!("{err:#}").contains("a symlink"), "{err:#}");
    }

    #[test]
    fn an_empty_launch_argv_is_rejected() {
        let cfg = SessionConfig {
            launch: vec![],
            cwd: None,
        };
        build_launch_argv(&cfg, Path::new("/heph"), Path::new("/s.sock"))
            .expect_err("an empty launch argv has no command to run");
    }
}
