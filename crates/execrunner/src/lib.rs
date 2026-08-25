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
    ) -> anyhow::Result<SpecRewrite>;
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
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<()> {
    let Some(addr) = runner.addr else {
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

    let rewritten = host
        .prepare(runner.request_id, addr, SpecRewrite::split(spec), ctoken)
        .await?;
    rewritten.apply(spec);
    Ok(())
}

/// Batch run under `runner`: spawn, capture stdout/stderr, wait, return.
///
/// The runner equivalent of [`proc_exec::output`].
pub async fn output(
    runner: RunnerRef<'_>,
    mut spec: proc_exec::Spec,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<std::process::Output> {
    prepare(runner, &mut spec, ctoken).await?;
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
    mut spec: proc_exec::Spec,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::io::Result<proc_exec::Handle>, SpawnedAs)> {
    prepare(runner, &mut spec, ctoken).await?;
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
