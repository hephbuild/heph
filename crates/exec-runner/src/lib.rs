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

pub mod agent;

use hcore::hasync::Cancellable;
use hproc::proc_exec::{self, Handle, Spec};
use std::ffi::OsString;
use std::process::Output;
use std::sync::Arc;

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
    /// Names the environment defines as **shell functions** rather than
    /// binaries. Diagnostics only, and the difference between a dead end and a
    /// recoverable error: a target calling one of these otherwise fails with
    /// "not found in PATH", which sends the reader hunting for a missing
    /// package that is not missing.
    pub shell_functions: Vec<String>,
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
    /// The program is not a binary in this environment, but the environment
    /// *does* define it as a shell function — which a snapshot runner cannot
    /// provide. Named separately because the fix is different in kind: not
    /// "install it" but "this runner mode cannot express it".
    ShellFunctionNotABinary {
        program: String,
        runner: String,
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
            SpawnError::ShellFunctionNotABinary { program, runner } => write!(
                f,
                "spawn child process {program:?}: runner `{runner}` defines {program:?} as a shell \
                 function, not a binary on PATH. The snapshot runner cannot provide shell \
                 functions — call the underlying command directly, or set `mode = \"session\"` on \
                 the runner target."
            ),
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
            SpawnError::SessionDied { .. } | SpawnError::ShellFunctionNotABinary { .. } => None,
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

/// One file from the runner target's artifacts, handed to [`ExecRunner::open`].
///
/// Bytes rather than a path: the artifact **is** the description of the
/// environment (§4.7 — "the canonicalized `ExecSessionSpec` *is* the runner
/// target's output artifact"), it is small by construction, and passing bytes
/// keeps `open` a pure parse of content the cache key already covers rather
/// than a filesystem read the key knows nothing about.
#[derive(Debug, Clone)]
pub struct RunnerArtifact {
    /// Path within the artifact tree.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// What a runner needs to open a session.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    /// Content-addressed identity of this environment — the runner target's
    /// hashouts. Two runner targets with byte-identical artifacts are the same
    /// environment and share a session, which is the intended behaviour.
    pub key: String,
    /// The runner target's address, for diagnostics only.
    pub runner_addr: String,
    /// The runner target's output artifacts.
    pub artifacts: Vec<RunnerArtifact>,
}

/// An environment in which target processes are created.
///
/// Registered on the engine under a name, and selected by the *driver name of
/// the runner target*: a plugin that exports a `devenv` driver (which builds
/// the environment artifact) exports a `devenv` runner alongside it (which
/// parses that artifact). One name, two halves — the half that produces the
/// description, and the half that reads it.
#[async_trait::async_trait]
pub trait ExecRunner: Send + Sync {
    /// Acquire the session for `req.key`.
    ///
    /// Called at most once per distinct key per engine — the pool single-flights
    /// it — and never on the per-target path. It may be slow: a cold devenv
    /// evaluation is tens of seconds.
    ///
    /// **Must be a pure function of `req`.** Anything it reads that is not in
    /// `req` is unhashed input: `open` runs after `hashin` is computed, and not
    /// at all on a fully-cached build, so a value it discovers cannot reach the
    /// key and cannot be validated on the build where a stale artifact is
    /// served.
    async fn open(
        &self,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>>;
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
                shell_functions: Vec::new(),
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

/// The environment a runner declares, in the same four shapes a target has.
///
/// The split is not cosmetic — it is where the cache key is drawn:
///
/// | | resolved | in the key |
/// |---|---|---|
/// | `env` | at snapshot time, literal | **yes** |
/// | `pass_env` | at snapshot time, from the host | **yes** — the *value* is baked in |
/// | `runtime_env` | at spawn, literal | the declaration only |
/// | `runtime_pass_env` | at spawn, from the host | the *name* only |
///
/// The property that matters is the last row. `pass_env` bakes a host value
/// into the environment's description, so changing that value correctly re-keys
/// every target built in it. `runtime_pass_env` bakes only the *name*, and reads
/// the value at spawn — so an `SSH_AUTH_SOCK` or a `DOCKER_HOST` that differs
/// per machine and per login reaches the process without ever reaching a cache
/// key. Using the wrong one of those two is the difference between a shared
/// cache that works and one that serves a machine its neighbour's build.
///
/// The declarations themselves are hashed either way, because they live in the
/// runner target's artifact and that artifact *is* the environment's identity.
/// That is the honest line: heph will not pretend a different environment is the
/// same one, but it will not put ambient host state in the key either.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub struct SessionEnv {
    /// Literal variables, resolved when the environment was captured.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Host variables whose **values** were captured with the environment.
    ///
    /// Already folded into `env` by the time a session is built; kept separately
    /// so diagnostics can say where a value came from.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Literal variables applied at spawn rather than captured.
    #[serde(default)]
    pub runtime_env: Vec<(String, String)>,
    /// Host variables read at spawn. Only the name is ever hashed.
    ///
    /// `"*"` passes the whole host environment, exactly as a target's
    /// `runtime_pass_env` does — an escape hatch that puts the developer's
    /// entire ambient environment underneath the target's own.
    #[serde(default)]
    pub runtime_pass_env: Vec<String>,
}

impl SessionEnv {
    /// Host variables this runner passes through, resolved **now**.
    ///
    /// The **weakest** layer — weaker than the environment the runner captured.
    /// That ordering is deliberate and differs from a target's own keys, where
    /// `runtime_pass_env` sits on top of `env`.
    ///
    /// The reason: a runner exists to *provide* an environment. If a passed-
    /// through host variable outranked the captured one, `runtime_pass_env =
    /// ["*"]` would silently replace that environment with the developer's own
    /// — a devenv `CC` quietly becoming whatever `CC` the login shell had. The
    /// build would still be keyed as though it ran in devenv. CI caught exactly
    /// this: a runner declaring `CC=clang` and `"*"` produced `CC=gcc`, because
    /// the runner ran on a machine whose environment set `CC`.
    ///
    /// A runner that genuinely wants to override its captured environment says
    /// so with `runtime_env`, which is explicit and outranks it.
    pub fn host_layer(&self) -> Vec<(OsString, OsString)> {
        let mut out: Vec<(OsString, OsString)> = Vec::new();
        if self.runtime_pass_env.iter().any(|n| n == "*") {
            out.extend(std::env::vars_os());
        } else {
            for name in &self.runtime_pass_env {
                if let Some(v) = std::env::var_os(name) {
                    out.push((OsString::from(name), v));
                }
            }
        }
        out
    }

    /// What the runner declared literally: `env` (captured) then `runtime_env`
    /// (applied at spawn), the latter winning. Both outrank the host layer and
    /// the captured environment, and both lose to the target's own entries.
    pub fn declared_layer(&self) -> Vec<(OsString, OsString)> {
        let mut out: Vec<(OsString, OsString)> =
            Vec::with_capacity(self.env.len() + self.runtime_env.len());
        for (k, v) in self.env.iter().chain(self.runtime_env.iter()) {
            let (k, v) = (OsString::from(k), OsString::from(v));
            if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
        out
    }
}

/// A `Direct` session: processes are forked from this process, with a base
/// environment applied beneath the caller's own.
///
/// This is the shape a devenv env-snapshot runner produces — the environment is
/// a set of variables, and nothing else about process creation changes. It is
/// also the reason `prepare` is the seam: there is no process to own here, only
/// a spec to transform.
#[derive(Debug)]
pub struct EnvSession {
    base_env: Vec<(OsString, OsString)>,
    /// The runner's declared `runtime_env` / `runtime_pass_env`, resolved on
    /// every `prepare` rather than once at open — a host variable that changes
    /// mid-build is meant to be seen.
    declared: SessionEnv,
    caps: SessionCaps,
    description: SessionDescription,
}

impl EnvSession {
    pub fn new(
        base_env: Vec<(OsString, OsString)>,
        caps: SessionCaps,
        description: SessionDescription,
    ) -> Self {
        Self::with_declared(base_env, SessionEnv::default(), caps, description)
    }

    /// [`Self::new`] with the runner's declared environment, whose `runtime_*`
    /// parts are resolved at each spawn.
    pub fn with_declared(
        base_env: Vec<(OsString, OsString)>,
        declared: SessionEnv,
        caps: SessionCaps,
        description: SessionDescription,
    ) -> Self {
        Self {
            declared,
            base_env,
            caps,
            description,
        }
    }
}

#[async_trait::async_trait]
impl ExecSession for EnvSession {
    /// Merge `base_env` **underneath** the spec's own entries.
    ///
    /// The caller wins on a collision: by the time a driver hands over a spec it
    /// has already resolved the target's `env` / `pass_env` / `runtime_env` and
    /// the sandbox's `$OUT`/`$SRC` routing, and none of that may be silently
    /// replaced by an environment the target did not write.
    ///
    /// Pre-sized and built as a `Vec` rather than routed through a
    /// `HashMap<String, String>`: the latter would clone every key and value and
    /// then convert each again into `OsString`, which for a 60–150-variable dev
    /// shell is hundreds of allocations per executed target.
    fn prepare(&self, mut spec: Spec) -> Result<Spec, SpawnError> {
        // Weakest to strongest:
        //   host passthrough  <  captured environment  <  declared literals  <  the target
        //
        // The first two are the order CI corrected: a `"*"` passthrough must not
        // be able to replace the environment the runner was chosen to provide.
        let host = self.declared.host_layer();
        let declared = self.declared.declared_layer();
        let mut env =
            Vec::with_capacity(host.len() + self.base_env.len() + declared.len() + spec.env.len());
        for (k, v) in host
            .iter()
            .chain(self.base_env.iter())
            .chain(declared.iter())
        {
            // The target always wins, and within the runner's own layers the
            // later one does. No key appears twice — a duplicate would leave
            // which value the child sees up to `execve`.
            if spec.env.iter().any(|(sk, _)| sk == k) {
                continue;
            }
            if let Some(slot) = env
                .iter_mut()
                .find(|(ek, _): &&mut (OsString, OsString)| ek == k)
            {
                slot.1 = v.clone();
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
        env.append(&mut spec.env);
        spec.env = env;
        Ok(spec)
    }

    fn base_env(&self) -> Option<&[(OsString, OsString)]> {
        Some(&self.base_env)
    }

    fn caps(&self) -> &SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &SessionDescription {
        &self.description
    }
}

#[cfg(test)]
mod env_session_tests {
    use super::*;
    use std::path::PathBuf;

    fn session(base: &[(&str, &str)]) -> EnvSession {
        EnvSession::new(
            base.iter()
                .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
                .collect(),
            SessionCaps {
                pty: true,
                max_concurrent: None,
                identity: Identity::Pinned {
                    by: "test".to_string(),
                },
            },
            SessionDescription {
                runner: "test".to_string(),
                shell_functions: Vec::new(),
                key: "k".to_string(),
                summary: "test".to_string(),
            },
        )
    }

    fn spec_with(env: &[(&str, &str)]) -> Spec {
        Spec {
            program: PathBuf::from("/bin/true"),
            args: vec![],
            env: env
                .iter()
                .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
                .collect(),
            cwd: PathBuf::from("/"),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Null,
            stderr: proc_exec::StdioSpec::Null,
            setsid: false,
            ctty: false,
        }
    }

    fn get<'a>(spec: &'a Spec, key: &str) -> Option<&'a OsString> {
        spec.env.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn base_env_is_applied_where_the_caller_said_nothing() {
        let out = session(&[("PATH", "/nix/bin")])
            .prepare(spec_with(&[]))
            .expect("prepare");
        assert_eq!(get(&out, "PATH"), Some(&OsString::from("/nix/bin")));
    }

    /// The caller wins. A driver has already resolved the target's own `env` and
    /// the sandbox's `$OUT`/`$SRC` routing by this point; an environment the
    /// target never wrote must not silently replace any of it.
    #[test]
    fn the_callers_entries_win_over_the_base() {
        let out = session(&[("PATH", "/nix/bin"), ("CC", "clang")])
            .prepare(spec_with(&[("PATH", "/sandbox/bin:/nix/bin")]))
            .expect("prepare");
        assert_eq!(
            get(&out, "PATH"),
            Some(&OsString::from("/sandbox/bin:/nix/bin"))
        );
        // …and a base entry the caller did not mention still arrives.
        assert_eq!(get(&out, "CC"), Some(&OsString::from("clang")));
    }

    /// One entry per key: a duplicate would leave which value the child sees up
    /// to `execve`, which is not a decision to leave to chance.
    #[test]
    fn no_duplicate_keys_survive_the_merge() {
        let out = session(&[("A", "base")])
            .prepare(spec_with(&[("A", "caller")]))
            .expect("prepare");
        assert_eq!(out.env.iter().filter(|(k, _)| k == "A").count(), 1);
    }
}

#[cfg(test)]
mod shell_function_diag_tests {
    use super::*;

    /// M1's one real limitation, made recoverable. Without this the reader sees
    /// "not found in PATH" and goes looking for a package that is not missing.
    #[test]
    fn a_shell_function_says_so_rather_than_not_found() {
        let msg = SpawnError::ShellFunctionNotABinary {
            program: "fmt-all".to_string(),
            runner: "//:devenv".to_string(),
        }
        .to_string();
        assert!(msg.contains("shell function"), "{msg}");
        assert!(msg.contains("//:devenv"), "{msg}");
        // Names the way out, not just the problem.
        assert!(msg.contains(r#"mode = "session""#), "{msg}");
    }
}

/// How a `Wrap` session gets the environment to the *inner* process.
///
/// The distinction is not cosmetic: `chroot`, `bwrap` and `nix develop
/// --command` exec the inner program in their own process, so it inherits the
/// spec's environment. `docker exec` does not — the environment we set belongs
/// to the `docker` CLI on this side of the socket, and the container process
/// sees none of it. A wrapper of the second kind that used `Inherit` would run
/// every target with an environment it never asked for and no error to show
/// for it.
#[derive(Debug, Clone)]
pub enum WrapEnv {
    /// The wrapper execs the inner program; the spec's env carries through.
    Inherit,
    /// The wrapper creates the process elsewhere. Each variable is rendered
    /// into the wrapper's own argv using this template, where `{K}` and `{V}`
    /// are replaced — e.g. `["-e", "{K}={V}"]` for `docker exec`.
    Args(Vec<String>),
}

/// A `Wrap` session: the process is created by a wrapper command — a container
/// exec, a `chroot`, a `nix develop --command`.
///
/// Still a pure spec transformation, which is the whole reason `prepare` is the
/// seam: nothing here owns a process, so `proc_exec` keeps its synchronous
/// spawn, its `Handle` invariants and its stdio handling untouched.
#[derive(Debug)]
pub struct WrapSession {
    prefix_argv: Vec<OsString>,
    env_mode: WrapEnv,
    base_env: Vec<(OsString, OsString)>,
    caps: SessionCaps,
    description: SessionDescription,
}

impl WrapSession {
    pub fn new(
        prefix_argv: Vec<OsString>,
        env_mode: WrapEnv,
        base_env: Vec<(OsString, OsString)>,
        caps: SessionCaps,
        description: SessionDescription,
    ) -> anyhow::Result<Self> {
        if prefix_argv.is_empty() {
            anyhow::bail!("a Wrap session needs a wrapper command; `prefix_argv` is empty");
        }
        Ok(Self {
            prefix_argv,
            env_mode,
            base_env,
            caps,
            description,
        })
    }

    fn merged_env(&self, spec_env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
        let mut env = Vec::with_capacity(self.base_env.len() + spec_env.len());
        for (k, v) in &self.base_env {
            if !spec_env.iter().any(|(sk, _)| sk == k) {
                env.push((k.clone(), v.clone()));
            }
        }
        env.extend_from_slice(spec_env);
        env
    }
}

#[async_trait::async_trait]
impl ExecSession for WrapSession {
    fn prepare(&self, mut spec: Spec) -> Result<Spec, SpawnError> {
        let merged = self.merged_env(&spec.env);

        // prefix[0] becomes the program; everything else, then the env args (if
        // this wrapper needs them), then the original program and its args.
        // `split_first` rather than an index: the constructor already rejects an
        // empty prefix, but a slice that "cannot be empty" is worth expressing
        // in the type rather than in a comment.
        let (wrapper, rest) = self
            .prefix_argv
            .split_first()
            .ok_or_else(|| SpawnError::Io(std::io::Error::other("empty wrapper command")))?;
        let mut args: Vec<OsString> = rest.to_vec();
        if let WrapEnv::Args(template) = &self.env_mode {
            for (k, v) in &merged {
                for part in template {
                    args.push(OsString::from(
                        part.replace("{K}", &k.to_string_lossy())
                            .replace("{V}", &v.to_string_lossy()),
                    ));
                }
            }
        }
        args.push(spec.program.clone().into_os_string());
        args.append(&mut spec.args);

        spec.program = std::path::PathBuf::from(wrapper);
        spec.args = args;
        // The wrapper itself still needs an environment to run in — a `PATH` to
        // find `docker` with, at minimum — so the merged env goes on the spec
        // either way. Under `Args` it reaches the inner process through argv
        // instead, and this is what the wrapper process sees.
        spec.env = merged;
        Ok(spec)
    }

    fn base_env(&self) -> Option<&[(OsString, OsString)]> {
        match self.env_mode {
            // The environment reaches the inner process, so reporting it is
            // honest.
            WrapEnv::Inherit => Some(&self.base_env),
            // It does not describe what the inner process sees — the wrapper
            // decides that. Saying `None` makes a caller degrade explicitly
            // rather than print a confident, wrong answer.
            WrapEnv::Args(_) => None,
        }
    }

    fn caps(&self) -> &SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &SessionDescription {
        &self.description
    }
}

#[cfg(test)]
mod wrap_session_tests {
    use super::*;
    use std::path::PathBuf;

    fn caps() -> SessionCaps {
        SessionCaps {
            pty: false,
            max_concurrent: None,
            identity: Identity::Pinned {
                by: "img@sha256:abc".to_string(),
            },
        }
    }

    fn desc() -> SessionDescription {
        SessionDescription {
            runner: "//:ctr".to_string(),
            shell_functions: vec![],
            key: "k".to_string(),
            summary: "container".to_string(),
        }
    }

    fn spec() -> Spec {
        Spec {
            program: PathBuf::from("cargo"),
            args: vec![OsString::from("build")],
            env: vec![(OsString::from("MINE"), OsString::from("1"))],
            cwd: PathBuf::from("/ws"),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Null,
            stderr: proc_exec::StdioSpec::Null,
            setsid: false,
            ctty: false,
        }
    }

    fn argv(s: &Spec) -> Vec<String> {
        std::iter::once(s.program.to_string_lossy().into_owned())
            .chain(s.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    #[test]
    fn the_wrapper_becomes_the_program_and_the_original_follows() {
        let s = WrapSession::new(
            vec![OsString::from("chroot"), OsString::from("/root")],
            WrapEnv::Inherit,
            vec![(OsString::from("CC"), OsString::from("clang"))],
            caps(),
            desc(),
        )
        .expect("wrap");
        let out = s.prepare(spec()).expect("prepare");
        assert_eq!(argv(&out), vec!["chroot", "/root", "cargo", "build"]);
        // Base env still applies underneath the caller's own.
        assert!(out.env.iter().any(|(k, v)| k == "CC" && v == "clang"));
        assert!(out.env.iter().any(|(k, v)| k == "MINE" && v == "1"));
    }

    /// `docker exec` does not inherit our environment, so it has to be rendered
    /// into the wrapper's argv or the inner process never sees it.
    #[test]
    fn args_mode_renders_the_environment_into_the_wrappers_argv() {
        let s = WrapSession::new(
            vec![
                OsString::from("docker"),
                OsString::from("exec"),
                OsString::from("-i"),
            ],
            WrapEnv::Args(vec!["-e".to_string(), "{K}={V}".to_string()]),
            vec![(OsString::from("CC"), OsString::from("clang"))],
            caps(),
            desc(),
        )
        .expect("wrap");
        let out = s.prepare(spec()).expect("prepare");
        let a = argv(&out);
        assert_eq!(a[0], "docker");
        assert!(
            a.windows(2).any(|w| w[0] == "-e" && w[1] == "CC=clang"),
            "{a:?}"
        );
        assert!(
            a.windows(2).any(|w| w[0] == "-e" && w[1] == "MINE=1"),
            "{a:?}"
        );
        // The inner command still comes last, after everything the wrapper needs.
        assert_eq!(&a[a.len() - 2..], &["cargo", "build"]);
    }

    /// A `Wrap` runner whose environment lives inside the container cannot
    /// honestly answer "what will this process see" — so it says so rather than
    /// reporting the host-side map as if it were the answer.
    #[test]
    fn args_mode_reports_no_enumerable_environment() {
        let s = WrapSession::new(
            vec![OsString::from("docker")],
            WrapEnv::Args(vec!["-e".to_string(), "{K}={V}".to_string()]),
            vec![(OsString::from("CC"), OsString::from("clang"))],
            caps(),
            desc(),
        )
        .expect("wrap");
        assert!(s.base_env().is_none());
    }

    #[test]
    fn an_empty_wrapper_command_is_rejected() {
        assert!(
            WrapSession::new(vec![], WrapEnv::Inherit, vec![], caps(), desc()).is_err(),
            "a Wrap session with nothing to wrap with is not a session"
        );
    }
}

/// An `Agent` session: processes are forked by a long-lived helper living
/// inside the environment — a `devenv shell` held open for the build.
///
/// Like the other modes, this is a **pure spec transformation**: the spec is
/// rewritten to run a client that heph spawns as its own ordinary child. See
/// [`agent`] for why that indirection exists rather than an overridden `spawn`.
pub struct AgentSession {
    /// The heph binary, which is also the client (`__runner-exec`).
    client_bin: std::path::PathBuf,
    socket: std::path::PathBuf,
    base_env: Vec<(OsString, OsString)>,
    caps: SessionCaps,
    description: SessionDescription,
    teardown: std::sync::Mutex<Option<TeardownJob>>,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("socket", &self.socket)
            .field("runner", &self.description.runner)
            .finish_non_exhaustive()
    }
}

impl AgentSession {
    pub fn new(
        client_bin: std::path::PathBuf,
        socket: std::path::PathBuf,
        base_env: Vec<(OsString, OsString)>,
        caps: SessionCaps,
        description: SessionDescription,
        teardown: Option<TeardownJob>,
    ) -> Self {
        Self {
            client_bin,
            socket,
            base_env,
            caps,
            description,
            teardown: std::sync::Mutex::new(teardown),
        }
    }
}

#[async_trait::async_trait]
impl ExecSession for AgentSession {
    fn prepare(&self, mut spec: Spec) -> Result<Spec, SpawnError> {
        // Base env under the caller's own, exactly as `Direct` does. The client
        // forwards its own environment to the agent, which applies it with
        // `env_clear` — so the devenv shell's ambient variables never reach the
        // target. That is the correction the design's M2 analysis needed: an
        // agent that let its own environment through would put the developer's
        // `GOFLAGS` into every build, unhashed, under a lockfile-pinned key.
        let mut env = Vec::with_capacity(self.base_env.len() + spec.env.len());
        for (k, v) in &self.base_env {
            if !spec.env.iter().any(|(sk, _)| sk == k) {
                env.push((k.clone(), v.clone()));
            }
        }
        env.append(&mut spec.env);

        let mut args: Vec<OsString> = vec![
            OsString::from("__runner-exec"),
            OsString::from("--socket"),
            self.socket.clone().into_os_string(),
            OsString::from("--"),
        ];
        args.push(spec.program.clone().into_os_string());
        args.append(&mut spec.args);

        spec.program = self.client_bin.clone();
        spec.args = args;
        spec.env = env;
        Ok(spec)
    }

    fn base_env(&self) -> Option<&[(OsString, OsString)]> {
        Some(&self.base_env)
    }

    fn caps(&self) -> &SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &SessionDescription {
        &self.description
    }

    fn teardown(&self) -> Option<TeardownJob> {
        self.teardown.lock().ok().and_then(|mut t| t.take())
    }
}

#[cfg(test)]
mod agent_session_tests {
    use super::*;
    use std::path::PathBuf;

    fn session() -> AgentSession {
        AgentSession::new(
            PathBuf::from("/usr/bin/heph"),
            PathBuf::from("/run/agent.sock"),
            vec![(OsString::from("CC"), OsString::from("clang"))],
            SessionCaps {
                pty: true,
                max_concurrent: None,
                identity: Identity::Asserted {
                    why: "live shell".to_string(),
                },
            },
            SessionDescription {
                runner: "//:devenv".to_string(),
                shell_functions: vec!["fmt-all".to_string()],
                key: "k".to_string(),
                summary: "session".to_string(),
            },
            None,
        )
    }

    fn spec() -> Spec {
        Spec {
            program: PathBuf::from("cargo"),
            args: vec![OsString::from("build")],
            env: vec![(OsString::from("MINE"), OsString::from("1"))],
            cwd: PathBuf::from("/ws"),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Null,
            stderr: proc_exec::StdioSpec::Null,
            setsid: false,
            ctty: false,
        }
    }

    /// The rewrite heph then spawns as an ordinary child — which is what keeps
    /// `Handle`, the drain and the PTY untouched.
    #[test]
    fn the_spec_becomes_a_client_invocation_wrapping_the_original() {
        let out = session().prepare(spec()).expect("prepare");
        assert_eq!(out.program, PathBuf::from("/usr/bin/heph"));
        let args: Vec<String> = out
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "__runner-exec",
                "--socket",
                "/run/agent.sock",
                "--",
                "cargo",
                "build"
            ]
        );
        // `--` matters: a target's own argv must never be read as client flags.
        assert!(args.iter().any(|a| a == "--"));
    }

    #[test]
    fn the_environment_is_merged_with_the_caller_winning() {
        let mut s = spec();
        s.env.push((OsString::from("CC"), OsString::from("gcc")));
        let out = session().prepare(s).expect("prepare");
        let cc: Vec<_> = out.env.iter().filter(|(k, _)| k == "CC").collect();
        assert_eq!(cc.len(), 1, "no duplicate keys reach execve");
        assert_eq!(cc[0].1, OsString::from("gcc"), "the caller wins");
        assert!(out.env.iter().any(|(k, _)| k == "MINE"));
    }

    /// Teardown is handed over once — a second caller must not be able to run
    /// `docker rm` (or kill the shell) a second time.
    #[test]
    fn teardown_is_handed_over_at_most_once() {
        let s = AgentSession::new(
            PathBuf::from("/usr/bin/heph"),
            PathBuf::from("/run/a.sock"),
            vec![],
            session().caps.clone(),
            session().description.clone(),
            Some(Box::new(|| Ok(()))),
        );
        assert!(s.teardown().is_some());
        assert!(s.teardown().is_none());
    }
}

#[cfg(test)]
mod session_env_tests {
    use super::*;
    use std::path::PathBuf;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, x)| ((*k).to_string(), (*x).to_string()))
            .collect()
    }

    fn get<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsString> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[test]
    fn literal_env_is_applied() {
        let se = SessionEnv {
            env: pairs(&[("CC", "clang")]),
            ..Default::default()
        };
        assert_eq!(
            get(&se.declared_layer(), "CC"),
            Some(&OsString::from("clang"))
        );
    }

    /// `runtime_env` sits above the captured `env`, mirroring the order a
    /// target's own keys are applied in.
    #[test]
    fn runtime_env_wins_over_captured_env() {
        let se = SessionEnv {
            env: pairs(&[("MODE", "captured")]),
            runtime_env: pairs(&[("MODE", "runtime")]),
            ..Default::default()
        };
        let l = se.declared_layer();
        assert_eq!(get(&l, "MODE"), Some(&OsString::from("runtime")));
        assert_eq!(
            l.iter().filter(|(k, _)| k == "MODE").count(),
            1,
            "a duplicate key would leave the child's value up to execve"
        );
    }

    /// The property the split exists for: the *value* is read at spawn, so it
    /// never reaches a cache key. Only the name was ever hashed.
    #[test]
    fn runtime_pass_env_reads_the_host_at_spawn() {
        // SAFETY: process-global mutation, and the name is unique to this test,
        // so no other test observes or races it.
        unsafe {
            std::env::set_var("heph_TEST_RUNNER_RT_PASS", "from_host");
        }
        let se = SessionEnv {
            runtime_pass_env: vec!["heph_TEST_RUNNER_RT_PASS".to_string()],
            ..Default::default()
        };
        assert_eq!(
            get(&se.host_layer(), "heph_TEST_RUNNER_RT_PASS"),
            Some(&OsString::from("from_host"))
        );
    }

    #[test]
    fn a_runtime_pass_env_name_absent_from_the_host_is_simply_not_set() {
        let se = SessionEnv {
            runtime_pass_env: vec!["heph_TEST_RUNNER_DEFINITELY_UNSET".to_string()],
            ..Default::default()
        };
        assert!(get(&se.host_layer(), "heph_TEST_RUNNER_DEFINITELY_UNSET").is_none());
    }

    /// `"*"` is the same escape hatch a target has: the whole host environment,
    /// underneath everything the target set itself.
    #[test]
    fn a_wildcard_passes_the_whole_host_environment() {
        // SAFETY: as above — a name used by this test alone.
        unsafe {
            std::env::set_var("heph_TEST_RUNNER_WILDCARD", "yes");
        }
        let se = SessionEnv {
            runtime_pass_env: vec!["*".to_string()],
            ..Default::default()
        };
        assert_eq!(
            get(&se.host_layer(), "heph_TEST_RUNNER_WILDCARD"),
            Some(&OsString::from("yes"))
        );
    }

    /// The ordering CI corrected, and the reason it is not a target's.
    ///
    /// A runner exists to *provide* an environment. If a passed-through host
    /// variable outranked the captured one, `runtime_pass_env = ["*"]` would
    /// silently replace that environment with the developer's own — and the
    /// build would still be keyed as though it ran in the runner's. This failed
    /// on both Linux legs, where the CI image sets `CC`: a runner declaring
    /// `CC=clang` produced `CC=gcc`.
    #[test]
    fn the_captured_environment_outranks_a_host_passthrough() {
        // SAFETY: process-global, and the name is this test's alone.
        unsafe {
            std::env::set_var("heph_TEST_RUNNER_CAPTURED", "from_host");
        }
        let session = EnvSession::with_declared(
            vec![(
                OsString::from("heph_TEST_RUNNER_CAPTURED"),
                OsString::from("from_capture"),
            )],
            SessionEnv {
                runtime_pass_env: vec!["*".to_string()],
                ..Default::default()
            },
            caps(),
            desc(),
        );
        let out = session.prepare(bare_spec()).expect("prepare");
        assert_eq!(
            get(&out.env, "heph_TEST_RUNNER_CAPTURED"),
            Some(&OsString::from("from_capture")),
            "a `*` passthrough must not replace what the runner captured",
        );
    }

    /// …and `runtime_env` is how a runner overrides its own capture, because it
    /// is explicit rather than whatever the host happened to have.
    #[test]
    fn runtime_env_can_override_the_capture_when_the_host_cannot() {
        let session = EnvSession::with_declared(
            vec![(OsString::from("MODE"), OsString::from("captured"))],
            SessionEnv {
                runtime_env: pairs(&[("MODE", "explicit")]),
                ..Default::default()
            },
            caps(),
            desc(),
        );
        let out = session.prepare(bare_spec()).expect("prepare");
        assert_eq!(get(&out.env, "MODE"), Some(&OsString::from("explicit")));
    }

    /// Above all of it, the target. A runner that could overwrite `$OUT`, `$SRC`
    /// or a target's own `env` would silently change what the target builds.
    #[test]
    fn the_target_still_wins_over_everything_the_runner_declares() {
        // SAFETY: as above.
        unsafe {
            std::env::set_var("heph_TEST_RUNNER_COLLIDE", "from_host");
        }
        let session = EnvSession::with_declared(
            vec![(
                OsString::from("heph_TEST_RUNNER_COLLIDE"),
                OsString::from("from_capture"),
            )],
            SessionEnv {
                runtime_env: pairs(&[("heph_TEST_RUNNER_COLLIDE", "from_runner")]),
                runtime_pass_env: vec!["*".to_string()],
                ..Default::default()
            },
            caps(),
            desc(),
        );
        let mut spec = bare_spec();
        spec.env = vec![(
            OsString::from("heph_TEST_RUNNER_COLLIDE"),
            OsString::from("from_target"),
        )];
        let out = session.prepare(spec).expect("prepare");
        assert_eq!(
            get(&out.env, "heph_TEST_RUNNER_COLLIDE"),
            Some(&OsString::from("from_target"))
        );
        assert_eq!(
            out.env
                .iter()
                .filter(|(k, _)| k == "heph_TEST_RUNNER_COLLIDE")
                .count(),
            1
        );
    }

    fn caps() -> SessionCaps {
        SessionCaps {
            pty: false,
            max_concurrent: None,
            identity: Identity::Pinned {
                by: "t".to_string(),
            },
        }
    }

    fn desc() -> SessionDescription {
        SessionDescription {
            runner: "//:r".to_string(),
            shell_functions: vec![],
            key: "k".to_string(),
            summary: String::new(),
        }
    }

    fn bare_spec() -> Spec {
        Spec {
            program: PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            cwd: PathBuf::from("/"),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Null,
            stderr: proc_exec::StdioSpec::Null,
            setsid: false,
            ctty: false,
        }
    }
}
