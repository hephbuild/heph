//! The exec-runner seam: the single chokepoint every subprocess heph spawns
//! passes through.
//!
//! Before this crate, seven call sites reached straight into
//! [`hproc::proc_exec`] and built their own [`proc_exec::Spec`]. They now go
//! through [`spawn`] / [`output`], which take the runner's *address* first and
//! the spec second. What a runner does is rewrite the spec — argv, env, cwd —
//! and hand it back; the host then spawns it exactly as before.
//!
//! **A runner is a spec rewrite, not a process abstraction.** That constraint
//! is what keeps this cheap: [`spawn`] returns a real [`proc_exec::Handle`], so
//! the bounded drain, the PTY line discipline, the `DRAIN_DEADLINE` handling,
//! the `setsid`/`killpg` contract and supervisor registration are untouched and
//! not re-derived on a new transport. It also means every runner must be
//! expressible as "spawn a different local process". A true remote-execution
//! runner does not fit; the escape hatch, if it is ever needed, is the one an
//! agent runner already uses — a small local client process standing in for the
//! remote one.
//!
//! # Who resolves the runner
//!
//! Resolving a runner address means reading the target's output, which is
//! `Engine::result_addr` — and the engine already depends on this crate through
//! the drivers. So the resolver cannot be named here: it is an installed trait
//! object ([`RunnerHost`]), handed over at engine construction the same way
//! `hproc`'s supervisor sink is installed.
//!
//! A cdylib statically links its **own** copy of this crate, so its
//! [`RunnerHost`] slot is a different global from the host binary's. An
//! uninstalled host with a non-local runner is a hard error, never a silent
//! fall back to a local spawn — see [`prepare`].

pub mod agent;
pub mod config;
pub mod registry;
pub mod session;

use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use hproc::proc_exec;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Which runner an exec goes through, and the request it is resolved in.
///
/// Carries an *address*, never a config: the config lives in the runner
/// target's output and is read by the host (see [`RunnerHost`]). `request_id`
/// scopes that resolution to the in-flight request, so the host can reach the
/// right engine state.
#[derive(Debug, Clone, Copy)]
pub struct RunnerRef<'a> {
    pub request_id: &'a str,
    /// `None` is the builtin `local` runner: spawn directly on this host.
    pub addr: Option<&'a Addr>,
}

impl RunnerRef<'static> {
    /// The builtin `local` runner — an identity rewrite.
    ///
    /// Takes no request scope, because nothing resolves: the request id exists
    /// only so the host can reach the right engine state when it has a runner
    /// target to read, and a local runner has none. Six of the seven call sites
    /// this crate replaced are internal host-tool invocations that are always
    /// local, and threading a request id through them would be churn for a
    /// value nobody reads.
    pub fn local() -> Self {
        Self {
            request_id: "",
            addr: None,
        }
    }
}

impl<'a> RunnerRef<'a> {
    /// A runner named by target address.
    pub fn target(request_id: &'a str, addr: &'a Addr) -> Self {
        Self {
            request_id,
            addr: Some(addr),
        }
    }

    /// Whether this is the builtin `local` runner (no resolution, no rewrite).
    pub fn is_local(&self) -> bool {
        self.addr.is_none()
    }
}

/// The part of a [`proc_exec::Spec`] a runner may rewrite.
///
/// Deliberately not the whole `Spec`. Stdio is live [`std::os::fd::OwnedFd`]s
/// that must never cross a plugin boundary, and `setsid`/`ctty` are the
/// caller's process-group contract with the supervisor, not the runner's to
/// change. Splitting the rewritable half out means the seam carries plain data
/// and the descriptors stay where they were opened.
///
/// `args`/`env`/`cwd` stay in OS-string form end to end: a non-UTF-8 env value
/// or path is legal on every supported target, and lossy conversion here would
/// corrupt it invisibly.
#[derive(Debug, Clone)]
pub struct SpecRewrite {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

/// How `PATH` is composed when the target may not be the one supplying it.
///
/// `PATH` is the one variable a runner and its target both always have an
/// opinion about, and it is a *list*, so "who wins" is the wrong question — the
/// answer is an order:
///
/// ```text
/// PATH = prefix ++ what the target declared ++ what the runner provides
/// ```
///
/// - **prefix** is the target's own tools. They lead, so a target that declares
///   a tool gets *that* one even when the environment it runs in ships another.
/// - **the target's** own `env`/`pass_env` next: it asked for it explicitly.
/// - **the runner's** last: the environment is the base, not an override.
///
/// [`PathPolicy::fallback`] is what a *local* spawn falls back to when none of
/// the three produced anything — the driver's sandbox `PATH`. It is deliberately
/// not part of the composition: a driver that injected it unconditionally would
/// put `/usr/bin` ahead of the environment the target asked to run in, and a
/// host-installed tool would quietly shadow the runner's.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    /// Entries that lead wherever the target runs — its declared tools.
    pub prefix: Vec<OsString>,
    /// Used only when nothing else provides a `PATH`, and never under a runner
    /// that supplies an environment of its own.
    pub fallback: Option<OsString>,
}

/// The `PATH` key, as an `OsStr` comparison target.
fn is_path_key(k: &OsString) -> bool {
    k == "PATH"
}

fn get_env(env: &[(OsString, OsString)], key: &str) -> Option<OsString> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// Join `PATH` fragments in order, dropping empties and repeats.
///
/// Deduplicated because the fragments genuinely overlap — a runner's `PATH` and
/// a driver's fallback both tend to end in `/usr/bin` — and a `PATH` that grows
/// a duplicate per exec is a real cost on every `execvp` the target makes.
pub fn join_path(fragments: impl IntoIterator<Item = OsString>) -> Option<OsString> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<OsString> = Vec::new();
    for fragment in fragments {
        for entry in std::env::split_paths(&fragment) {
            if entry.as_os_str().is_empty() {
                continue;
            }
            if seen.insert(entry.clone()) {
                out.push(entry.into_os_string());
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    std::env::join_paths(out).ok()
}

impl SpecRewrite {
    /// Split the rewritable half out of a spec, leaving the descriptors and the
    /// process-group flags behind.
    fn split(spec: &proc_exec::Spec) -> Self {
        Self {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
        }
    }

    /// Apply a rewritten half back onto the spec it came from.
    fn apply(self, spec: &mut proc_exec::Spec) {
        spec.program = self.program;
        spec.args = self.args;
        spec.env = self.env;
        spec.cwd = self.cwd;
    }
}

/// Resolves a runner address and rewrites the spec it applies to.
///
/// Implemented by the engine (which can read a runner target's output) and
/// installed into this crate's process-global slot with [`install_host`]. Kept
/// as a trait so this crate depends on nothing above `hproc`/`hcore`/`hmodel` —
/// the engine already depends on the drivers that call [`spawn`], so naming it
/// here would close a dependency cycle.
#[async_trait::async_trait]
pub trait RunnerHost: Send + Sync {
    /// Rewrite `rewrite` for the runner at `addr`, within request `request_id`.
    ///
    /// May start a session (an agent runner holds a live environment), so it is
    /// async and may be slow on first use for a given runner.
    /// Whether this host can resolve `request_id`.
    ///
    /// A process can hold more than one engine — the test harness builds many,
    /// and reopening a workspace builds a second over the same root — so the
    /// installed hosts are a list, not a slot, and a request is routed to the
    /// engine that owns it. Request ids are process-unique, so at most one host
    /// answers.
    fn owns(&self, request_id: &str) -> bool;

    /// Whether the engine behind this host is still alive. Dead hosts are
    /// pruned on the next install so a long-lived process (the test binary)
    /// does not accumulate them.
    fn alive(&self) -> bool;

    async fn prepare(
        &self,
        request_id: &str,
        addr: &Addr,
        rewrite: SpecRewrite,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<PrepareOutcome>;
}

/// What a runner did, beyond the rewrite itself.
#[derive(Debug)]
pub struct PrepareOutcome {
    pub rewrite: SpecRewrite,
    /// The runner puts the target in an environment of its own, so the driver's
    /// fallback `PATH` must not be reinstated here — see [`PathPolicy`].
    ///
    /// True for an agent runner, whose environment is not visible from this
    /// process at all: it is the agent's `environ`, and the agent composes the
    /// final `PATH` from what this side sends it.
    pub supplies_environment: bool,
}

/// The installed hosts, newest last.
///
/// A list rather than a `OnceLock` slot because a process can hold several
/// engines: every `WorkspaceBuilder::build` in the test suite makes one, and
/// `reopen` makes a second over the same root deliberately. With a single slot
/// the first engine to install would silently answer for all of them, and a
/// target in the second engine would resolve its runner against the first
/// engine's request registry — which fails, confusingly, as "request is no
/// longer live". Routing by request id is what makes many engines behave the
/// way one does.
static HOSTS: OnceLock<std::sync::Mutex<Vec<Arc<dyn RunnerHost>>>> = OnceLock::new();

fn hosts() -> &'static std::sync::Mutex<Vec<Arc<dyn RunnerHost>>> {
    HOSTS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Install a runner host. Called once per engine, and once per loaded cdylib by
/// the plugin SDK (each cdylib has its own copy of this static).
pub fn install_host(host: Arc<dyn RunnerHost>) {
    let Ok(mut list) = hosts().lock() else {
        tracing::error!("exec-runner host registry poisoned; runner resolution will fail");
        return;
    };
    list.retain(|h| h.alive());
    list.push(host);
}

/// Whether any runner host has been installed in *this* copy of the crate.
pub fn host_installed() -> bool {
    hosts().lock().map(|l| !l.is_empty()).unwrap_or(false)
}

/// The host that owns `request_id`, if one is installed here.
fn host_for(request_id: &str) -> Option<Arc<dyn RunnerHost>> {
    let list = hosts().lock().ok()?;
    list.iter()
        .rev()
        .find(|h| h.owns(request_id))
        .map(Arc::clone)
}

/// Resolve the runner and rewrite the spec in place.
///
/// A local runner returns immediately, having touched nothing — that is the
/// default for every target in an unconfigured workspace, so it must stay free.
///
/// A non-local runner with no installed host is an **error**, never a fall back
/// to a local spawn. Falling back would run the target outside the environment
/// its cache key already claims, which is a silently wrong build — the worst
/// outcome this system can produce. (`hproc`'s supervisor tracker degrades to a
/// no-op when uninstalled; that is right for a best-effort reaper and wrong
/// here.)
async fn prepare(
    runner: RunnerRef<'_>,
    spec: &mut proc_exec::Spec,
    path: &PathPolicy,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    let Some(addr) = runner.addr else {
        // Nothing supplies an environment, so the target's own `PATH` is the
        // whole of it and the driver's fallback stands behind it — the plain
        // local spawn, unchanged.
        let declared = get_env(&spec.env, "PATH");
        compose_path(&mut spec.env, path, declared, true);
        return Ok(());
    };

    let host = host_for(runner.request_id).ok_or_else(|| {
        if host_installed() {
            anyhow::anyhow!(
                "target requests exec runner {} but no installed runner host owns request '{}'. \
                 The runner is resolved while the target that named it is executing, so this \
                 means the request was dropped mid-execution.",
                addr.format(),
                runner.request_id,
            )
        } else {
            anyhow::anyhow!(
                "target requests exec runner {} but no runner host is installed in this \
                 component. A plugin cdylib links its own copy of the exec-runner crate and must \
                 be handed the host's runner registry at load time; a plugin built against an \
                 older SDK will not be.",
                addr.format(),
            )
        }
    })?;

    // Captured before the runner touches it, so what the *target* declared can
    // still be told apart from what the runner provides afterwards. Without the
    // distinction there is no order to compose: a `PATH` in the env on the far
    // side could equally be either.
    let declared = get_env(&spec.env, "PATH");

    let outcome = host
        .prepare(runner.request_id, addr, SpecRewrite::split(spec), ctoken)
        .await?;
    let supplies_environment = outcome.supplies_environment;
    outcome.rewrite.apply(spec);

    // Whatever the runner put there, minus what was already there — i.e. the
    // runner's own contribution.
    let provided = get_env(&spec.env, "PATH").filter(|now| Some(now) != declared.as_ref());
    compose_path(
        &mut spec.env,
        path,
        declared,
        // An agent runner's environment lives in the agent, not here, so there
        // is nothing to compose against yet and the fallback must not be
        // reinstated: it would put the driver's `/usr/bin` ahead of the
        // environment the target asked to run in. The agent finishes the job.
        !supplies_environment,
    );
    if let Some(provided) = provided {
        let so_far = get_env(&spec.env, "PATH");
        if let Some(joined) = join_path(so_far.into_iter().chain(std::iter::once(provided))) {
            set_path(&mut spec.env, joined);
        }
    }
    Ok(())
}

/// `prefix ++ declared`, plus the fallback when `use_fallback` and nothing else
/// produced anything.
fn compose_path(
    env: &mut Vec<(OsString, OsString)>,
    path: &PathPolicy,
    declared: Option<OsString>,
    use_fallback: bool,
) {
    let composed = join_path(path.prefix.iter().cloned().chain(declared));
    let composed = match composed {
        Some(p) => Some(p),
        None if use_fallback => path.fallback.clone(),
        None => None,
    };
    // A `None` says nothing about PATH: leave whatever is there (a runner may
    // have just set it) rather than inserting an empty one, which would make
    // every `execvp` in the target fail with a confusing ENOENT.
    if let Some(p) = composed {
        set_path(env, p);
    }
}

fn set_path(env: &mut Vec<(OsString, OsString)>, value: OsString) {
    if let Some(slot) = env.iter_mut().find(|(k, _)| is_path_key(k)) {
        slot.1 = value;
    } else {
        env.push((OsString::from("PATH"), value));
    }
}

/// Batch run under `runner`: spawn, capture stdout/stderr, wait, return.
///
/// The runner equivalent of [`proc_exec::output`].
pub async fn output(
    runner: RunnerRef<'_>,
    spec: proc_exec::Spec,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<std::process::Output> {
    output_with_path(runner, spec, &PathPolicy::default(), ctoken).await
}

/// [`output`], with the caller's `PATH` composition.
pub async fn output_with_path(
    runner: RunnerRef<'_>,
    mut spec: proc_exec::Spec,
    path: &PathPolicy,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<std::process::Output> {
    prepare(runner, &mut spec, path, ctoken).await?;
    Ok(proc_exec::output(spec, ctoken).await?)
}

/// Streaming spawn under `runner`, returning a real [`proc_exec::Handle`].
///
/// The runner equivalent of [`proc_exec::spawn`]. The returned handle is the
/// same type the caller would have got from a direct spawn, so every existing
/// drain/wait/cancel path applies unchanged.
///
/// Returns the spawn's `io::Error` untouched inside the `anyhow` chain, so a
/// caller that inspects [`std::io::ErrorKind::NotFound`] to render a
/// "not on PATH" diagnostic still can — see [`spawn_io`] for the variant that
/// keeps the error typed.
pub async fn spawn(
    runner: RunnerRef<'_>,
    spec: proc_exec::Spec,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<proc_exec::Handle> {
    let (handle, _) = spawn_io(runner, spec, ctoken).await?;
    Ok(handle?)
}

/// Like [`spawn`], but the spawn failure stays a [`std::io::Error`] so the
/// caller can branch on its `kind()`.
///
/// Two error channels, deliberately: resolving the runner can fail (a missing
/// runner target, an unreachable session) and that is an `anyhow` error the
/// caller has nothing useful to add to; the `execve` itself failing is the
/// caller's to describe, because only it knows what PATH it configured and what
/// the program was for. The rewritten spec travels back with the error so the
/// diagnostic names the program that was *actually* spawned rather than the one
/// the caller asked for — under a runner those differ.
pub async fn spawn_io(
    runner: RunnerRef<'_>,
    spec: proc_exec::Spec,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::io::Result<proc_exec::Handle>, SpawnedAs)> {
    spawn_io_with_path(runner, spec, &PathPolicy::default(), ctoken).await
}

/// [`spawn_io`], with the caller's `PATH` composition — see [`PathPolicy`].
///
/// The exec driver is the caller that needs it: it is the one that knows which
/// `PATH` entries are the target's declared tools and which are only its own
/// sandbox default.
pub async fn spawn_io_with_path(
    runner: RunnerRef<'_>,
    mut spec: proc_exec::Spec,
    path: &PathPolicy,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::io::Result<proc_exec::Handle>, SpawnedAs)> {
    prepare(runner, &mut spec, path, ctoken).await?;
    let spawned_as = SpawnedAs {
        program: spec.program.clone(),
        cwd: spec.cwd.clone(),
    };
    Ok((proc_exec::spawn(spec), spawned_as))
}

/// What [`spawn_io`] actually tried to execute, for the caller's error message.
#[derive(Debug, Clone)]
pub struct SpawnedAs {
    pub program: PathBuf,
    pub cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;

    fn addr() -> Addr {
        Addr::new(PkgBuf::from("tools"), "runner".to_string(), BTreeMap::new())
    }

    fn spec(program: &str) -> proc_exec::Spec {
        proc_exec::Spec {
            program: PathBuf::from(program),
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

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn path_of(env: &[(OsString, OsString)]) -> Option<String> {
        get_env(env, "PATH").map(|v| v.to_string_lossy().into_owned())
    }

    fn policy(prefix: &[&str], fallback: Option<&str>) -> PathPolicy {
        PathPolicy {
            prefix: prefix.iter().map(|p| os(p)).collect(),
            fallback: fallback.map(os),
        }
    }

    /// The order the whole feature rests on: the target's tools, then what the
    /// target declared, then the environment it runs in.
    #[test]
    fn the_targets_tools_lead_and_what_it_declared_follows() {
        let mut env = vec![(os("PATH"), os("/declared"))];
        compose_path(
            &mut env,
            &policy(&["/tools"], Some("/fallback")),
            Some(os("/declared")),
            true,
        );
        assert_eq!(path_of(&env).as_deref(), Some("/tools:/declared"));
    }

    /// The driver's sandbox `PATH` is a fallback, not a contribution: it applies
    /// when nothing else produced one, and never ahead of what did.
    #[test]
    fn the_fallback_applies_only_when_nothing_else_did() {
        let mut env = vec![];
        compose_path(&mut env, &policy(&[], Some("/usr/bin:/bin")), None, true);
        assert_eq!(path_of(&env).as_deref(), Some("/usr/bin:/bin"));

        let mut env = vec![];
        compose_path(
            &mut env,
            &policy(&["/tools"], Some("/usr/bin:/bin")),
            None,
            true,
        );
        assert_eq!(
            path_of(&env).as_deref(),
            Some("/tools"),
            "the fallback must not follow the target's own tools onto PATH"
        );
    }

    /// A runner that supplies an environment gets no fallback: reinstating
    /// `/usr/bin` here would arrive ahead of the environment the target asked to
    /// run in, and a host-installed tool would quietly shadow the runner's.
    #[test]
    fn a_runner_that_supplies_an_environment_gets_no_fallback() {
        let mut env = vec![];
        compose_path(&mut env, &policy(&[], Some("/usr/bin:/bin")), None, false);
        assert_eq!(path_of(&env), None);
    }

    /// The regression the local branch nearly shipped: composing from the
    /// policy alone would have replaced a target's whole `PATH` with its tool
    /// dir, dropping the driver's sandbox `PATH` on every target that declares
    /// a tool and no runner.
    #[tokio::test]
    async fn a_local_spawn_keeps_the_path_it_already_had_behind_its_tools() {
        let mut spec = spec("/bin/true");
        spec.env.push((os("PATH"), os("/usr/local/bin:/usr/bin")));
        let ctoken = StdCancellationToken::new();
        let policy = policy(&["/sandbox/bin"], Some("/fallback"));
        prepare(RunnerRef::local(), &mut spec, &policy, &ctoken)
            .await
            .expect("local prepare");
        assert_eq!(
            path_of(&spec.env).as_deref(),
            Some("/sandbox/bin:/usr/local/bin:/usr/bin")
        );
    }

    #[test]
    fn joining_a_path_drops_empties_and_repeats() {
        let joined = join_path([os("/a:/b"), os(""), os("/b:/c")]);
        assert_eq!(
            joined.map(|v| v.to_string_lossy().into_owned()).as_deref(),
            Some("/a:/b:/c")
        );
        assert_eq!(join_path([os(""), os("")]), None);
    }

    #[test]
    fn local_runner_is_local() {
        assert!(RunnerRef::local().is_local());
        let a = addr();
        assert!(!RunnerRef::target("req", &a).is_local());
    }

    /// The rewritable half must carry the fields a runner changes and leave the
    /// descriptors and process-group flags behind — those are the caller's
    /// contract with the supervisor, not the runner's to touch.
    #[test]
    fn split_and_apply_round_trips_the_rewritable_half() {
        let mut s = spec("/bin/echo");
        s.setsid = true;
        s.args = vec![OsString::from("hello")];

        let mut half = SpecRewrite::split(&s);
        half.program = PathBuf::from("/usr/bin/devenv");
        half.args = vec![OsString::from("shell"), OsString::from("--")];
        half.env.push((OsString::from("K"), OsString::from("V")));
        half.apply(&mut s);

        assert_eq!(s.program, PathBuf::from("/usr/bin/devenv"));
        assert_eq!(s.args, vec![OsString::from("shell"), OsString::from("--")]);
        assert_eq!(s.env, vec![(OsString::from("K"), OsString::from("V"))]);
        // Untouched by the rewrite.
        assert!(s.setsid);
    }

    /// Non-UTF-8 argv and env survive a rewrite. Legal on every supported
    /// target, and lossy conversion at the seam would corrupt it invisibly.
    #[test]
    fn rewritable_half_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![0xff, 0xfe, b'x']);
        let mut s = spec("/bin/echo");
        s.args = vec![raw.clone()];
        s.env = vec![(OsString::from("K"), raw.clone())];

        let half = SpecRewrite::split(&s);
        assert_eq!(half.args, vec![raw.clone()]);
        assert_eq!(half.env, vec![(OsString::from("K"), raw)]);
    }

    /// A non-local runner with no installed host must fail loudly. Falling back
    /// to a local spawn would run the target outside the environment its cache
    /// key claims.
    #[tokio::test]
    async fn non_local_runner_without_host_is_an_error() {
        // This test owns the process-global only in the sense that it never
        // installs one; `install_host` is never called anywhere in this crate's
        // suite, so the slot stays empty.
        let a = addr();
        let ctoken = StdCancellationToken::new();
        let err = output(RunnerRef::target("req", &a), spec("/bin/true"), &ctoken)
            .await
            .expect_err("must not fall back to a local spawn");
        let msg = format!("{err:#}");
        assert!(msg.contains("no runner host is installed"), "{msg}");
        assert!(msg.contains("//tools:runner"), "{msg}");
    }

    /// The local path must not require a host, must not consult one, and must
    /// spawn exactly what it was given.
    #[tokio::test]
    async fn local_runner_spawns_unchanged() {
        let ctoken = StdCancellationToken::new();
        let out = output(RunnerRef::local(), spec("/bin/echo"), &ctoken)
            .await
            .expect("spawn /bin/echo");
        assert!(out.status.success());
    }
}
