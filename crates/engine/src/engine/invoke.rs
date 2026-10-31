//! `Engine::invoke`: how an entrypoint target — one whose def has
//! [`TargetDef::entrypoint`](crate::engine::driver::targetdef::TargetDef::entrypoint)
//! set, from `entrypoint = True` on exec/bash or any driver's equivalent —
//! runs.
//!
//! An entrypoint is *invoked*, not built: it runs through its driver's
//! ordinary `run` (or `run_shell`) in its own sandbox, with
//! [`RunOptions`](crate::engine::driver::RunOptions) that hand it the
//! caller's cwd (per [`Workdir`]), environment, args and terminal. It is never
//! cached and never anyone's input.
//!
//! `invoke` resolves the def, refuses undeclared args before any dep builds
//! (N5), builds the closure through the ordinary `result_addr`, drains the
//! cache writes those builds queued (E15), then runs the shared sandbox block
//! ([`Engine::run_in_sandbox`]) with an invocation lease (R11, R12).
//!
//! What it deliberately leaves out, against `execute`: the request memoizer
//! (two calls in one process are two runs), the per-address execute lock, the
//! worker permit, and every cache write of the entrypoint's own (N1, N10).
//! The engine installs no signal handler: the caller hands in the relay's
//! receiver and the dispositions (R9).

use crate::engine::Engine;
use crate::engine::InteractiveWrapper;
use crate::engine::driver::targetdef::Workdir;
use crate::engine::driver::{
    Disposition, Foreground, Handover, RunOptions, RunStdio, Signal, SignalReceiver,
};
use crate::engine::execute::{SandboxLease, SandboxRun};
use crate::engine::invoke_sandbox::{BUILD_SANDBOX_PREFIX, InvokeSandbox, invoke_parent};
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use anyhow::Context;
use hcore::process::Exit;
use hmodel::htaddr::Addr;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// What the CLI asks for: an address, and everything about the caller.
///
/// `#[non_exhaustive]`: built with [`InvokeRequest::new`] and the `with_*`
/// setters. Not `Clone` (the signal receiver moves into the run); `Debug`
/// prints the environment's keys only.
#[non_exhaustive]
pub struct InvokeRequest {
    pub addr: Addr,
    /// The trailing args after `--`, in order. Refused unless the target
    /// declared it takes them.
    pub args: Vec<OsString>,
    /// The caller's raw cwd, captured at startup; may be outside the
    /// workspace. Where a `workdir = "caller"` tool runs.
    pub cwd: PathBuf,
    /// The caller's whole environment, byte for byte: the bottom layer of the
    /// tool's, under the target's own env. `HEPH_ROOT` is appended to it.
    pub env: Vec<(OsString, OsString)>,
    /// `heph run --shell`: the driver's `run_shell` instead of `run`.
    pub shell: bool,
    /// Signals the caller relays to the tool while it runs (R9). A closed
    /// channel is "no more signals", never a cancel.
    pub signals: SignalReceiver,
    /// Dispositions the tool gets before it execs: what the caller recorded
    /// at startup.
    pub dispositions: Vec<(Signal, Disposition)>,
    /// The open-file soft limit the tool execs with: what the caller recorded
    /// at startup, before heph raised its own (R8). `None` leaves heph's.
    pub nofile: Option<u64>,
    /// The signals blocked in the tool at exec: the caller's recorded mask.
    pub blocked: Vec<Signal>,
    /// Wraps the driver's run, as for a build: the CLI pauses its TUI here.
    /// The streams it hands in are ignored — the tool inherits heph's fds.
    pub wrapper: Option<InteractiveWrapper>,
    /// Run once the closure is materialized, right before the driver spawns
    /// the tool: where the CLI stops treating Ctrl-C as "cancel" (R9).
    pub handover: Option<Handover>,
}

impl InvokeRequest {
    /// A request with no args, no shell, a closed signal channel, no
    /// dispositions and no wrapper.
    pub fn new(addr: Addr, cwd: impl Into<PathBuf>, env: Vec<(OsString, OsString)>) -> Self {
        let (_closed, signals) = crate::engine::driver::signal_channel();
        Self {
            addr,
            args: Vec::new(),
            cwd: cwd.into(),
            env,
            shell: false,
            signals,
            dispositions: Vec::new(),
            nofile: None,
            blocked: Vec::new(),
            wrapper: None,
            handover: None,
        }
    }

    pub fn with_nofile(mut self, nofile: Option<u64>) -> Self {
        self.nofile = nofile;
        self
    }

    pub fn with_blocked(mut self, blocked: Vec<Signal>) -> Self {
        self.blocked = blocked;
        self
    }

    pub fn with_handover(mut self, handover: Handover) -> Self {
        self.handover = Some(handover);
        self
    }

    pub fn with_args(mut self, args: Vec<OsString>) -> Self {
        self.args = args;
        self
    }

    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }

    pub fn with_signals(mut self, signals: SignalReceiver) -> Self {
        self.signals = signals;
        self
    }

    pub fn with_dispositions(mut self, dispositions: Vec<(Signal, Disposition)>) -> Self {
        self.dispositions = dispositions;
        self
    }

    pub fn with_wrapper(mut self, wrapper: InteractiveWrapper) -> Self {
        self.wrapper = Some(wrapper);
        self
    }
}

impl std::fmt::Debug for InvokeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keys only: a value is the caller's, and may be a token.
        let env_keys: Vec<_> = self.env.iter().map(|(k, _)| k).collect();
        f.debug_struct("InvokeRequest")
            .field("addr", &self.addr)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env_keys", &env_keys)
            .field("shell", &self.shell)
            .field("dispositions", &self.dispositions)
            .field("nofile", &self.nofile)
            .field("blocked", &self.blocked)
            .field("wrapper", &self.wrapper.is_some())
            .field("handover", &self.handover.is_some())
            .finish_non_exhaustive()
    }
}

/// How an invocation that got past resolution ended (R4). A failure *before*
/// the target resolved to an entrypoint is `invoke`'s `Err` instead.
///
/// For the binary: `Exited(Exit::Code(n))` exits `n` with no box (0
/// included); `Exited(Exit::Signal(s))` re-raises `s`; `Failed` prints its box
/// and exits 125; `invoke`'s `Err` keeps today's exit 1.
#[derive(Debug)]
#[non_exhaustive]
pub enum InvokeOutcome {
    /// The tool ran, and this is how it ended.
    Exited(Exit),
    /// heph failed after the target resolved to an entrypoint: a dep build,
    /// a refused arg, materializing the closure, credentials, the spawn
    /// itself (ENOENT/EACCES), or a cancellation before the tool exited. The
    /// error has no exit status of the tool's in it.
    Failed(anyhow::Error),
}

/// An entrypoint named as some target's input (N2).
#[derive(Debug, thiserror::Error)]
#[error(
    "{entrypoint} is an entrypoint (`entrypoint = True`) and cannot be an input{}: an entrypoint runs in \
     the caller's working directory and environment, so a build step depending on it could run \
     something outside any sandbox. Depend on what it runs instead",
    consumer_clause(.consumer.as_ref(), .origin_id)
)]
pub struct EntrypointAsInput {
    pub entrypoint: Addr,
    pub origin_id: String,
    pub consumer: Option<Addr>,
}

fn consumer_clause(consumer: Option<&Addr>, origin_id: &str) -> String {
    match consumer {
        Some(c) => format!(" (of {c}, as `{origin_id}`)"),
        None => format!(" (as `{origin_id}`)"),
    }
}

impl EntrypointAsInput {
    /// Name the consumer on an [`EntrypointAsInput`] that does not yet, and
    /// pass any other error through.
    pub(crate) fn name_consumer(e: anyhow::Error, consumer: &Addr) -> anyhow::Error {
        match e.downcast::<EntrypointAsInput>() {
            Ok(mut refused) => {
                refused.consumer.get_or_insert_with(|| consumer.clone());
                refused.into()
            }
            Err(e) => e,
        }
    }
}

/// An entrypoint reached by a build: a direct `result_addr`, a multi-target
/// selection, or anything else that would execute it (N2, R14).
#[derive(Debug, thiserror::Error)]
#[error(
    "{addr} is an entrypoint (`entrypoint = True`){}: it is invoked on its own with `heph run {addr}`, \
     never built, and never as part of a multi-target selection. Select it alone, or select what it runs",
    selection_clause(*.selected)
)]
pub struct EntrypointNotBuildable {
    pub addr: Addr,
    /// How many targets the selection that reached it resolved to, once the
    /// walk knows (R18); `None` for a direct build of the address.
    pub selected: Option<usize>,
}

fn selection_clause(selected: Option<usize>) -> String {
    match selected {
        Some(1) => ", the only target the selection resolved to".to_string(),
        Some(n) => format!(", one of the {n} targets the selection resolved to"),
        None => String::new(),
    }
}

impl EntrypointNotBuildable {
    /// Name the selection's size on an [`EntrypointNotBuildable`] in `e`'s
    /// chain, and leave any other error alone.
    pub(crate) fn name_selection(mut e: anyhow::Error, selected: usize) -> anyhow::Error {
        if let Some(refused) = e.downcast_mut::<EntrypointNotBuildable>() {
            refused.selected = Some(selected);
        }
        e
    }
}

/// Trailing args for an entrypoint that did not declare `args = True` (N5).
#[derive(Debug, thiserror::Error)]
#[error(
    "{addr} takes no arguments, but was given {count}. Declare `args = True` on it to pass the \
     arguments after `--` to the program"
)]
pub struct UndeclaredArgs {
    pub addr: Addr,
    pub count: usize,
}

impl Engine {
    /// Whether `addr` is an entrypoint: its def's attribute. Resolves (parses)
    /// its def, which builds nothing.
    pub async fn is_entrypoint(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<bool> {
        let def = Arc::clone(self).get_def(rs.clone(), addr).await?;
        Ok(def.target_def.entrypoint.is_some())
    }

    /// Refuse an entrypoint reached by a multi-target selection, before
    /// anything of it builds.
    pub(crate) async fn refuse_entrypoint_in_selection(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<()> {
        if self.is_entrypoint(rs, addr).await? {
            return Err(EntrypointNotBuildable {
                addr: addr.clone(),
                selected: None,
            }
            .into());
        }
        Ok(())
    }

    /// Run `req.addr`, an entrypoint: see the module docs. `Err` is a failure
    /// before the target resolved to an entrypoint; everything after is an
    /// [`InvokeOutcome`].
    pub async fn invoke(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        req: InvokeRequest,
    ) -> anyhow::Result<InvokeOutcome> {
        let addr = req.addr.clone();
        rs.track_dep(&addr).map_err(anyhow::Error::new)?;
        let rs = rs.with_parent(addr.clone());

        let spec = Arc::clone(&self)
            .get_spec_no_track(rs.clone(), &addr)
            .await?;
        let def = Arc::clone(&self)
            .get_def_no_track(rs.clone(), &addr)
            .await?;
        let target = Arc::clone(&def.target_def);
        let Some(entrypoint) = target.entrypoint else {
            anyhow::bail!(
                "{addr} is not an entrypoint; declare `entrypoint = True` on an exec or bash \
                 target to invoke it"
            );
        };

        let res = self
            .invoke_resolved(&rs, &spec, target, entrypoint, req)
            .await;
        Ok(match res {
            Ok(Ok(())) => InvokeOutcome::Exited(Exit::Code(0)),
            // Only the driver's own run is read for a process exit.
            Ok(Err(e)) => match hplugin::error::process_exit(&e) {
                Some(exit) if exit.is_failure_status() => InvokeOutcome::Exited(exit),
                // A status no failed process can have (a plugin's code 0 or
                // 300, a SIGSTOP): heph cannot exit with it, so it is heph's
                // failure, with the driver's message.
                Some(exit) => InvokeOutcome::Failed(e.context(format!(
                    "the driver reported the impossible exit status {exit:?} for a failed run"
                ))),
                None => InvokeOutcome::Failed(e),
            },
            Err(e) => InvokeOutcome::Failed(e),
        })
    }

    /// Everything after resolution. The outer `Err` is heph's; the inner one
    /// is the driver's run.
    async fn invoke_resolved(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        spec: &crate::engine::EngineTargetSpec,
        target: Arc<crate::engine::driver::targetdef::TargetDef>,
        entrypoint: crate::engine::driver::targetdef::Entrypoint,
        req: InvokeRequest,
    ) -> anyhow::Result<anyhow::Result<()>> {
        let addr = &req.addr;
        // After the def resolves, before any dep builds: a flagless release
        // script must not swallow a mistyped `--dry-run` and run for real.
        if !req.args.is_empty() && !entrypoint.accepts_args {
            return Err(UndeclaredArgs {
                addr: addr.clone(),
                count: req.args.len(),
            }
            .into());
        }
        // `<pkg>/__invoke/__target_x/` is also where the build sandbox of
        // `//<pkg>/__invoke:x` lives: an invocation must never land in it.
        if addr.name.starts_with(BUILD_SANDBOX_PREFIX) {
            anyhow::bail!(
                "{addr}: an entrypoint's name cannot start with `{BUILD_SANDBOX_PREFIX}` (heph's \
                 build sandboxes use that prefix); rename the target"
            );
        }
        let driver = self
            .drivers_by_name
            .get(&spec.driver)
            .with_context(|| format!("driver not found: {}", spec.driver))?
            .clone();

        // The closure. Expanding the inputs is what refuses an entrypoint
        // among them (N2); linking selects the runtime ones and their outputs.
        let expanded = Arc::clone(self)
            .expanded_inputs_for(rs.clone(), addr)
            .await?;
        let linked = Arc::clone(self)
            .link(rs.clone(), Arc::clone(&target))
            .await?;

        // Build everything: the runtime inputs for their artifacts, and every
        // other input (hash-only deps, `${read://…}` producers) because the
        // invocation depends on them having been built — they are just not
        // wired in.
        let fail_fast = rs.fail_fast();
        let hash_only = expanded.iter().filter(|i| !i.runtime).map(|input| {
            enclose::enclose!((Arc::clone(self) => engine, rs) async move {
                engine
                    .result_addr(
                        rs,
                        &input.r#ref.r#ref,
                        OutputMatcher::All,
                        &ResultOptions::default(),
                    )
                    .await?;
                anyhow::Ok(())
            })
        });
        let (inputs, hash_only) = futures::future::join(
            Arc::clone(self).inputs_result_exec(rs.clone(), &linked.inputs),
            crate::engine::fanout::join_all_failable(hash_only, fail_fast),
        )
        .await;
        let inputs = inputs?;
        hash_only?;

        // The process may exit as soon as the tool does, without the teardown
        // that would otherwise flush what these builds queued: drain it now,
        // so a cold call leaves its deps cached (E15). The local cache only:
        // the request's other background work — remote uploads above all,
        // which can take minutes — runs on beside the tool and is drained as
        // for any command once it exits, rather than delaying its start.
        // Cancellable: a Ctrl-C while a slow write commits ends the run here.
        // Biased: a cancel that landed while a dep build was stuck elsewhere
        // wins over a drain that has just become ready.
        tokio::select! {
            biased;
            () = hcore::hasync::Cancellable::cancelled(rs.ctoken()) => {
                return Err(anyhow::Error::new(hplugin::error::CancelledError))
                    .with_context(|| format!("waiting for {addr}'s deps to be cached"));
            }
            () = self.sqlite_writes.drain_queued_writes() => {}
        }

        let deferred_refs = self.deferred_refs(&spec.spec, &spec.driver)?;
        let deferred = self
            .resolve_deferred(rs, &target, &deferred_refs)
            .await
            .with_context(|| format!("resolve deferred values for {addr}"))?;
        let resolved_scratch = self
            .resolve_scratch(rs, addr, &target.inputs)
            .await
            .with_context(|| format!("resolve scratch for {addr}"))?;
        let (scratch, _scratch_guards) = self
            .acquire_scratch(rs, addr, &resolved_scratch, false)
            .await
            .with_context(|| format!("acquire scratch for {addr}"))?;
        let credentials = self
            .resolve_credentials(rs, addr, &target.inputs)
            .await
            .with_context(|| format!("resolve credentials for {addr}"))?;

        // The invocation's own sandbox, created exclusively; then any sibling a
        // crashed heph left, swept off the request's path.
        let parent = invoke_parent(&self.home, addr);
        let pending = rs.bg_pending();
        let sandbox = hcore::blocking::run({
            let (parent, pending) = (parent.clone(), Arc::clone(&pending));
            move || InvokeSandbox::create(&parent, Some(pending))
        })
        .await?;
        crate::engine::sandbox_cleaner::enqueue(
            crate::engine::sandbox_cleaner::Lane::Reclaim,
            format!("sweep invocation sandboxes under {}", parent.display()),
            Box::new(move || {
                crate::engine::invoke_sandbox::sweep(&parent);
                Ok(())
            }),
            pending,
        );

        let cwd = match entrypoint.workdir {
            Workdir::Caller => Some(req.cwd.clone()),
            Workdir::Root => Some(self.cfg.root.clone()),
            Workdir::Sandbox => None,
        };
        let args = entrypoint.accepts_args.then_some(req.args);
        tracing::debug!(
            addr = %addr,
            args = ?args,
            cwd = ?cwd,
            sandbox = %sandbox.dir().display(),
            accepts_args = entrypoint.accepts_args,
            shell = req.shell,
            "invoking",
        );
        // The caller's env, then the real root under a name of its own:
        // `$WORKSPACE_ROOT` is the sandbox's, consistent with `$SRC_x` (R17).
        let mut base_env = req.env;
        base_env.push((
            OsString::from("HEPH_ROOT"),
            self.cfg.root.clone().into_os_string(),
        ));
        let mut foreground = Foreground::new(req.signals, req.dispositions)
            .with_nofile(req.nofile)
            .with_blocked(req.blocked);
        if let Some(handover) = req.handover {
            foreground = foreground.with_handover(handover);
        }
        let mut options = RunOptions::default()
            .with_base_env(base_env)
            .with_stdio(RunStdio::Inherit(foreground));
        if let Some(cwd) = cwd {
            options = options.with_cwd(cwd);
        }
        if let Some(args) = args {
            options = options.with_args(args);
        }

        let ran = self
            .run_in_sandbox(
                rs,
                addr,
                SandboxLease::invocation(&sandbox),
                SandboxRun {
                    driver,
                    target,
                    inputs,
                    hashin: String::new(),
                    scratch,
                    credentials,
                    deferred,
                    wrapper: req.wrapper,
                    shell: req.shell,
                    options,
                },
            )
            .await;
        // The run's guards (stage holds, slots) go before the tree does; the
        // response carries nothing else an invocation keeps.
        let ran = ran.map(|r| r.map(drop));

        // Every exit removes the sandbox, before this returns (E20).
        if let Err(e) = hcore::blocking::run(move || sandbox.teardown()).await {
            tracing::warn!(
                addr = %addr,
                error = %e,
                "could not remove the invocation sandbox; the next invocation or `heph gc` sweeps it",
            );
        }
        ran
    }
}
