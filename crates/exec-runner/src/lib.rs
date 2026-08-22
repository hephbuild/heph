//! The exec-runner contract: *in what environment is a target's process created?*
//!
//! Design: `docs/EXEC_RUNNERS.md`. This crate carries **Phase 0** — the seam and
//! the `local` session. The `ExecRunner`/pool/`open` side (Phase 1) is deliberately
//! not here yet: it has no caller until `runner =` exists, and an API with no
//! caller is an API that is wrong.
//!
//! ## Why `prepare`, and not a process factory
//!
//! For the two exec modes that matter first — `Direct` (a devenv env snapshot)
//! and `Wrap` (a container, a chroot) — a session is a **pure transformation of
//! the [`Spec`]**: merge a base environment underneath the caller's own, prepend
//! an argv prefix. The host then calls the ordinary [`proc_exec::spawn`] /
//! [`proc_exec::output`] on the result.
//!
//! Making [`ExecSession::prepare`] the seam, rather than trait-objectifying
//! process creation, is what keeps all of the following untouched:
//!
//! - `proc_exec::spawn` stays **synchronous**. There is no suspension point
//!   between the fork and the caller receiving the `Handle`, so a cancellation
//!   cannot land there and orphan a child that was never registered with the
//!   supervisor. An `async fn spawn` would open exactly that window, and under
//!   fail-fast it is the common cancellation shape, not an exotic one.
//! - `Handle`'s "the spawn is the API" invariant (waiting and draining must not
//!   share a task) and its OS-divergent reader-termination discipline stay
//!   concrete rather than erased behind a trait object.
//! - PTY allocation stays where it is. A `StdioSpec::Pty` variant looked
//!   attractive until it had to carry `termios` — `openpty(3)` on macOS leaves
//!   the slave's termios unspecified, which is why `pluginexec::pty::inherit_termios`
//!   exists — and until it needed a return path for the master. What actually has
//!   to move for a non-forking runner is fd *transport*, not pty allocation.
//!
//! ## Both `spawn` and `output`, deliberately
//!
//! Six of heph's eight process-creation sites are [`proc_exec::output`], not
//! `spawn`, and **`output` cannot be built on `spawn`**: it drains with
//! `DrainCapacity::Unbounded` because nothing consumes the channel until the
//! wait returns, where `spawn` is bounded at 512 KiB for backpressure. A
//! `go list` emitting more than that, routed through `spawn` + wait, wedges on
//! darwin and passes on linux. So the batch/streaming drain policy stays inside
//! `hproc`, and this trait exposes both.

use hcore::hasync::Cancellable;
use hproc::proc_exec::{self, Handle, Spec};
use std::process::Output;
use std::ffi::OsString;

/// How well-pinned a session's environment is. Diagnostics only — it never
/// changes whether heph caches a target. Choosing a weakly-pinned environment
/// is the user's decision; heph's job is to report the tradeoff, not to
/// override `cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The environment is described by content the cache key already covers
    /// (a lockfile resolving to store paths, an image digest).
    Pinned { by: String },
    /// The environment is asserted rather than observed. A stale hit is
    /// possible where a `Pinned` environment would have missed.
    Asserted { why: String },
}

impl Identity {
    pub fn is_pinned(&self) -> bool {
        matches!(self, Identity::Pinned { .. })
    }
}

/// Capabilities of one acquired session.
///
/// Per-**session**, not per-runner: one runner can open a `Direct` session that
/// allocates a pty trivially and an `Agent` session that can only do so over an
/// fd-passing socket, so a runner-level answer would be wrong for one of them.
#[derive(Debug, Clone)]
pub struct SessionCaps {
    /// Whether a controlling terminal can be allocated — gates `--shell`.
    pub pty: bool,
    /// Max concurrent processes this session serves. `None` = the engine's
    /// worker pool is the only cap.
    ///
    /// Whoever enforces this must do so at **admission**, before the engine's
    /// worker permit is taken — never as a second semaphore under a held
    /// permit. The latter converts a per-session cap into global starvation:
    /// with 24 workers and a cap of 1, 24 targets take every permit while 23
    /// park inside the session, and an unrelated local target then waits for a
    /// permit that will not free.
    pub max_concurrent: Option<usize>,
    pub identity: Identity,
}

/// What a human sees for this session in `heph inspect` and the in-flight report.
#[derive(Debug, Clone)]
pub struct SessionDescription {
    /// The runner that opened it (`local`, or a runner target's addr).
    pub runner: String,
    /// Content-addressed key this session was opened for. Empty for `local`,
    /// which contributes nothing to any cache key.
    pub key: String,
    /// One line for a human: what this environment is.
    pub summary: String,
}

/// Where a `PATH` came from, so a "not found" can say which one it searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSource {
    /// The driver's own `path` option (or its built-in default).
    Driver,
    /// Supplied by the session's base environment.
    Session { runner: String },
}

/// Typed because two distinct callers match on the outcome: the exec driver, to
/// render a diagnostic naming *which* PATH was searched; and (in Phase 1) the
/// session pool, to decide whether a failure poisons the session.
/// `io::ErrorKind` cannot carry either — it has no variant for "the session is
/// gone", and a non-forking session reports a missing program as a protocol
/// error rather than `NotFound`.
#[derive(Debug)]
pub enum SpawnError {
    ProgramNotFound {
        program: String,
        /// The `PATH` that was actually searched.
        path: String,
        /// Which layer supplied that `PATH` — the difference between "fix your
        /// `.hephconfig`" and "fix your runner", which are different files.
        source: PathSource,
        /// The child's working directory. A missing cwd fails the same syscall
        /// with the same errno, so naming it saves a wrong-turn diagnosis.
        cwd: String,
        io: std::io::Error,
    },
    SessionDied {
        key: String,
        reason: String,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::ProgramNotFound {
                program,
                path,
                source,
                cwd,
                io,
            } => match source {
                PathSource::Driver => write!(
                    f,
                    "spawn child process {program:?}: {io} — not found in the driver's sandbox \
                     PATH ({path}). This PATH is set by the driver's `path` option in .hephconfig \
                     and is independent of the invoking shell's PATH — a program on your \
                     interactive PATH can still be missing here. Also check that the working \
                     directory {cwd:?} exists."
                ),
                PathSource::Session { runner } => write!(
                    f,
                    "spawn child process {program:?}: {io} — not found in the PATH provided by \
                     runner `{runner}` ({path}). The driver's own `path` option is not applied \
                     when a runner supplies PATH, so a program on the host is not reachable here \
                     — declare it as a `tools =` dep, or add it to the runner's environment. Also \
                     check that the working directory {cwd:?} exists."
                ),
            },
            SpawnError::SessionDied { key, reason } => write!(
                f,
                "exec session {key} died before the process could be created: {reason}"
            ),
            SpawnError::Io(e) => write!(f, "spawn child process: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SpawnError::Io(e) | SpawnError::ProgramNotFound { io: e, .. } => Some(e),
            SpawnError::SessionDied { .. } => None,
        }
    }
}

impl From<std::io::Error> for SpawnError {
    fn from(e: std::io::Error) -> Self {
        SpawnError::Io(e)
    }
}

/// Cleanup a session hands back for the host to run.
///
/// **Synchronous on purpose**, and not an `async fn close`. The weak argument is
/// "`Drop` cannot await" — which does not hold on its own, since the pool owns
/// the session and an aborted build never drops it. The real reasons:
///
/// 1. An async teardown detached at exit (`tokio::spawn(close())`) is silently
///    dropped when the runtime is already shutting down — which is the common
///    case for "heph is exiting". It works in tests and leaks in production.
/// 2. The mechanism that actually guarantees completion before process exit is
///    `sandbox_cleaner`'s `bg_pending` accounting, and it is `FnOnce`-shaped.
/// 3. That lane may not move onto tokio at all, per the macOS cross-thread waker
///    hazard (`docs/RCA_MACOS_WAKER.md`).
///
/// Teardown is `docker rm` / `kill` — blocking calls that belong on a dedicated
/// thread. It returns `Result` so a failure is a `warn!` rather than silence.
pub type TeardownJob = Box<dyn FnOnce() -> anyhow::Result<()> + Send + 'static>;

/// One acquired exec environment, shared by every target that resolved to it.
#[async_trait::async_trait]
pub trait ExecSession: Send + Sync {
    /// Transform a spec so the process is created in this environment: merge
    /// [`base_env`](Self::base_env) **underneath** the spec's own entries, and
    /// prepend any argv prefix.
    ///
    /// The caller's entries win. A driver has already resolved the target's
    /// `env` / `pass_env` / `runtime_env` and the sandbox's `$OUT`/`$SRC`
    /// routing by this point; the session supplies the floor beneath them.
    fn prepare(&self, spec: Spec) -> Result<Spec, SpawnError>;

    /// Streaming creation. Bounded drain, for a caller that consumes output
    /// concurrently with the child.
    fn spawn(&self, spec: Spec) -> Result<Handle, SpawnError> {
        Ok(proc_exec::spawn(self.prepare(spec)?)?)
    }

    /// Batch creation. Unbounded drain — see the module docs for why this is
    /// not `spawn` plus a wait.
    async fn output(
        &self,
        spec: Spec,
        cancel: &(dyn Cancellable + Send + Sync),
    ) -> Result<Output, SpawnError> {
        Ok(proc_exec::output(self.prepare(spec)?, cancel).await?)
    }

    /// The environment every process here starts from, or `None` when it cannot
    /// be enumerated host-side (a container's environment lives inside the
    /// container). Callers that report "where did this PATH entry come from"
    /// must degrade explicitly rather than pretend an empty environment.
    fn base_env(&self) -> Option<&[(OsString, OsString)]>;

    fn caps(&self) -> &SessionCaps;

    fn describe(&self) -> &SessionDescription;

    /// See [`TeardownJob`]. `None` when there is nothing to tear down.
    fn teardown(&self) -> Option<TeardownJob> {
        None
    }
}

/// The zero-configuration session: the process is created exactly as it would
/// have been before exec runners existed.
///
/// `prepare` is the identity function, and that is load-bearing rather than
/// lazy. `local` contributes **nothing** to any cache key, which is only sound
/// while it is byte-for-byte today's behaviour — the invariant is that
/// *`local` is the zero-configuration session; any configuration on it must
/// reach the key*. Adding a `base_env` or a `path` here without also making it
/// hash is a silent wrong-build.
#[derive(Debug)]
pub struct LocalSession {
    caps: SessionCaps,
    description: SessionDescription,
}

impl Default for LocalSession {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSession {
    pub fn new() -> Self {
        Self {
            caps: SessionCaps {
                pty: true,
                max_concurrent: None,
                // Honest rather than flattering: `local` is in truth the
                // *least*-pinned environment in the system — the host's
                // `/usr/bin`, plus a `path` option that is not hashed. It is
                // absent from the cache key for compatibility (no existing
                // artifact may be invalidated by exec runners shipping), not
                // because it is well-pinned. A user who sees no warning here
                // should not read that as a guarantee.
                identity: Identity::Asserted {
                    why: "the host environment; not described by any cache key".to_string(),
                },
            },
            description: SessionDescription {
                runner: "local".to_string(),
                key: String::new(),
                summary: "host process, no environment applied".to_string(),
            },
        }
    }
}

#[async_trait::async_trait]
impl ExecSession for LocalSession {
    fn prepare(&self, spec: Spec) -> Result<Spec, SpawnError> {
        Ok(spec)
    }

    fn base_env(&self) -> Option<&[(OsString, OsString)]> {
        None
    }

    fn caps(&self) -> &SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &SessionDescription {
        &self.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec(program: &str) -> Spec {
        Spec {
            program: PathBuf::from(program),
            args: vec![],
            env: vec![(OsString::from("A"), OsString::from("1"))],
            cwd: PathBuf::from("/"),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Null,
            stderr: proc_exec::StdioSpec::Null,
            setsid: false,
            ctty: false,
        }
    }

    /// `local` must not perturb the spec at all. If this ever fails, every
    /// cached artifact built before exec runners shipped is suspect: `local`
    /// contributes nothing to `hashin` precisely because it changes nothing.
    #[test]
    fn local_prepare_is_the_identity() {
        let s = LocalSession::new();
        let before = spec("/bin/echo");
        let after = s.prepare(spec("/bin/echo")).expect("prepare");

        assert_eq!(after.program, before.program);
        assert_eq!(after.args, before.args);
        assert_eq!(after.env, before.env);
        assert_eq!(after.cwd, before.cwd);
        assert_eq!(after.setsid, before.setsid);
        assert_eq!(after.ctty, before.ctty);
    }

    #[test]
    fn local_reports_no_environment_rather_than_an_empty_one() {
        // `Some(&[])` would read as "this environment is empty", which is a
        // different and false claim.
        assert!(LocalSession::new().base_env().is_none());
    }

    #[test]
    fn local_has_nothing_to_tear_down() {
        assert!(LocalSession::new().teardown().is_none());
    }

    #[test]
    fn program_not_found_names_which_path_it_searched() {
        let nf = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let driver = SpawnError::ProgramNotFound {
            program: "cargo".to_string(),
            path: "/usr/bin:/bin".to_string(),
            source: PathSource::Driver,
            cwd: "/ws".to_string(),
            io: nf(),
        }
        .to_string();
        assert!(driver.contains("driver's sandbox PATH"), "{driver}");
        assert!(driver.contains("`path` option in .hephconfig"), "{driver}");

        let session = SpawnError::ProgramNotFound {
            program: "cargo".to_string(),
            path: "/nix/store/x/bin".to_string(),
            source: PathSource::Session {
                runner: "//:devenv".to_string(),
            },
            cwd: "/ws".to_string(),
            io: nf(),
        }
        .to_string();
        assert!(session.contains("runner `//:devenv`"), "{session}");
        assert!(session.contains("tools ="), "{session}");
    }
}
