use crate::engine::driver::inputartifact::{InputArtifact, Type};
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::{RunInput, RunRequest, RunResponse};
use crate::engine::link::{LinkedTargetDef, LinkedTargetDefInput};
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use crate::engine::{Engine, InteractiveInner, InteractiveWrapper};
use anyhow::Context;
use async_recursion::async_recursion;
use enclose::enclose;
use hmodel::htaddr::Addr;
use std::sync::Arc;

impl Engine {
    #[async_recursion]
    #[expect(
        clippy::too_many_arguments,
        reason = "execute orchestrates per-request state plus driver/exec wrapping"
    )]
    pub(crate) async fn execute(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        spec: &TargetSpec,
        def: &LinkedTargetDef,
        hashin: &str,
        exec_wrapper: Option<InteractiveWrapper>,
        shell: bool,
        no_scratch: bool,
        // `outputs_are_material`: this target's outputs are credential material.
        // The one thing it changes is the failure path — see the `Err` arm below.
        outputs_are_material: bool,
    ) -> anyhow::Result<(
        Vec<OutputArtifact>,
        crate::engine::sandbox_cleaner::SandboxTeardown,
        Vec<hplugin::driver::SandboxGuard>,
        crate::engine::result_lock::ExecuteGuard,
    )> {
        let driver = self
            .drivers_by_name
            .get(&spec.driver)
            .ok_or_else(|| anyhow::anyhow!("driver not found: {}", spec.driver))
            .cloned()?;
        // The backstop for a direct `result_addr` on an entrypoint: it runs
        // only through `Engine::invoke`, never in a sandbox (D30, I8).
        if def.target.entrypoint.is_some() {
            return Err(crate::engine::invoke::EntrypointNotBuildable {
                addr: addr.clone(),
                selected: None,
            }
            .into());
        }

        hcore::hmemoizer::set_phase("execute:inputs_result_exec");
        let deps_result = self
            .clone()
            .inputs_result_exec(rs.clone(), &def.inputs)
            .await?;

        // Scratch slots, between dep resolution and the worker permit — see
        // `acquire_scratch` for why both sides of that sandwich are load-bearing.
        // In short: after deps, or a dep needing the same slot could never get it;
        // before the permit, so a target queued on a contended slot holds no
        // worker, which also makes the wait provably bounded.
        //
        // The guards ride to the end of the run and drop with `_scratch_guards`.
        hcore::hmemoizer::set_phase("execute:scratch_acquire");
        let resolved_scratch = self
            .resolve_scratch(&rs, addr, &def.target.inputs)
            .await
            .with_context(|| format!("resolve scratch for {addr}"))?;
        let (scratch_mounts, _scratch_guards) = self
            .acquire_scratch(&rs, addr, &resolved_scratch, no_scratch)
            .await
            .with_context(|| format!("acquire scratch for {addr}"))?;

        // Credentials, after scratch and after every dep has resolved.
        //
        // The ordering is load-bearing in two directions. **After the cache
        // decision**, because a cache hit never reaches `execute` at all — which
        // is what makes "zero cost on a hit" structural rather than aspirational:
        // a fully cached build probes nothing, spawns nothing and prompts nobody.
        // And **strictly downstream of runner preparation**, because the devenv
        // `wrap` runner captures its entire resolved environment into a
        // `runner.json` that is a cached, remotely-shippable artifact; a runner
        // capture must never see material.
        //
        // Resolution (which declaration does this reference name, and is the set
        // coherent?) already happened at `get_def`. What happens here is the part
        // that costs something: walking the chain, acquiring, and writing the
        // presented files into this run's sandbox.
        // Deferred option values. The producers are already built — they are
        // `hashed` inputs, so `hashin` waited on them — and what is left here is
        // reading their bytes. Inside `execute`, so a cache hit resolves nothing.
        hcore::hmemoizer::set_phase("execute:deferred_resolve");
        let deferred_refs = self.deferred_refs(spec, &spec.driver)?;
        let deferred = self
            .resolve_deferred(&rs, &def.target, &deferred_refs)
            .await
            .with_context(|| format!("resolve deferred values for {addr}"))?;

        hcore::hmemoizer::set_phase("execute:credential_acquire");
        let resolved_credentials = self
            .resolve_credentials(&rs, addr, &def.target.inputs)
            .await
            .with_context(|| format!("resolve credentials for {addr}"))?;

        // One run of this target at a time, in any process: its sandbox is per
        // addr. After deps (holding it across a dep's build could deadlock with
        // a forced rebuild of that dep), before the worker permit (a target
        // parked here holds no worker). Returned to the caller, which holds it
        // until the outputs are cached. See `ResultLock::lock_execute`.
        hcore::hmemoizer::set_phase("execute:execute_lock");
        let execute_guard = self
            .acquire_with_notice(
                &rs,
                addr,
                None,
                self.result_lock().lock_execute(addr, rs.ctoken()),
            )
            .await?;

        // Acquire semaphore AFTER dep resolution so no permit is held while waiting for
        // deps — prevents the classic diamond deadlock where mid-nodes hold permits while
        // waiting for a leaf that also needs a permit.
        hcore::hmemoizer::set_phase("execute:semaphore_acquire");
        let pool = Arc::clone(&self.result_permits);
        // Observed *before* the acquire, so the stamp times the wait rather than
        // its end. This queue is invisible everywhere else: the permit is taken
        // after dep resolution but before `ExecuteStart`, so a target parked here
        // has no open `execute` span and shows only as an open `result`.
        let d = crate::engine::diag::global();
        d.limiter("workers").observe(pool.available(), d.now_ms());
        // Fair queue — see `worker_pool`'s module docs for the contract: a
        // waiter dropped mid-wait leaves the queue, and a permit assigned to
        // a since-aborted waiter returns on its `Acquire` drop.
        let _permit = pool.acquire().await.context("worker pool closed")?;
        // Counted once the permit is actually in hand.
        let _running = crate::engine::diag::RunningPermit::new();

        let addr_str = addr.format();
        // Telemetry: per-target execute wall time. This path runs only on a real
        // execution (cache misses), so it captures exactly the executed targets.
        let exec_started = std::time::Instant::now();
        let res = crate::engine::event::emit_scope(
            &rs,
            crate::engine::event::BuildEventKind::ExecuteStart {
                addr: addr_str.clone(),
                driver: driver.name.clone(),
                cache: def.target.cache.enabled,
            },
            move |error| crate::engine::event::BuildEventKind::ExecuteEnd {
                addr: addr_str,
                error: error.map(crate::engine::event::ErrorDetail::into_message),
            },
            async {
                let sandbox_dir = {
                    let mut dir = self.home.join("sandbox");
                    for c in addr.package.components() {
                        dir = dir.join(c);
                    }
                    if addr.args.is_empty() {
                        dir.join(format!("__target_{}", addr.name))
                    } else {
                        dir.join(format!("__target_{}_{}", addr.name, addr.hash_str()))
                    }
                };
                // Claim the sandbox path for this execute — inline, before any
                // removal job is queued. The claim is what orders this run
                // against every straggler: it happens synchronously here, so a
                // predecessor's queued job (in any queue, under any lock-grant
                // order) that runs after this line sees a stale claim and
                // declines. See `sandbox_cleaner::generation`; `claim` takes
                // only the fast generation lock, never the walk lock, so it is
                // safe on the async path.
                let sandbox_generation =
                    crate::engine::sandbox_cleaner::generation::claim(&sandbox_dir);
                // From the moment the path is claimed, its teardown is owned,
                // and every exit resolves it exactly once, three ways:
                // completion hands the bridge's cleanup job to `complete()` in
                // `execute_and_cache_inner`; a *failing* run resolves it as
                // `leave_for_diagnostics` in the match below (the failure
                // renderer reads the log tail lazily from the sandbox, so a
                // failed target's tree must survive until its next run — its
                // documented pre-teardown behaviour); and a bare drop —
                // cancellation or an unwind, and only those — queues a
                // generation-checked reclaim of the directory. Without that
                // last leg, mass fail-fast leaves one abandoned sandbox tree
                // per *cancelled* execute and nothing ever collects them
                // (`gc` has no sandbox sweep).
                let mut sandbox_teardown = crate::engine::sandbox_cleaner::SandboxTeardown::arm(
                    sandbox_dir.clone(),
                    sandbox_generation,
                    rs.bg_pending(),
                );

                // Everything fallible between the claim and the run response
                // lives in the shared block, so an `Err` — a failing target
                // above all — can be told apart from a drop: the error leaves
                // the sandbox for diagnostics, the drop reclaims it.
                let run = self
                    .run_in_sandbox(
                        &rs,
                        addr,
                        SandboxLease::per_addr(&sandbox_teardown),
                        SandboxRun {
                            driver,
                            target: Arc::clone(&def.target),
                            inputs: deps_result,
                            hashin: hashin.to_owned(),
                            scratch: scratch_mounts,
                            credentials: resolved_credentials,
                            deferred,
                            wrapper: exec_wrapper,
                            shell,
                            options: crate::engine::driver::RunOptions::default(),
                        },
                    )
                    .await
                    .and_then(|ran| ran.context("run"));

                let res = match run {
                    Ok(res) => res,
                    Err(err) => {
                        // The target failed. Its sandbox *is* the diagnostic —
                        // the failure paragraph reads the process's last log
                        // lines from it lazily, at render time — so nothing may
                        // be queued against it, or the diagnostic races its own
                        // cleanup and a failing target reports an exit status
                        // with no output. The tree survives until this target's
                        // next run, whose `remove_stale` reclaims it.
                        //
                        // Unless the outputs are material, in which case the
                        // diagnostic is not worth the secret it would leave on
                        // disk.
                        if outputs_are_material {
                            sandbox_teardown.complete(addr.format());
                        } else {
                            sandbox_teardown.leave_for_diagnostics();
                        }
                        return Err(err);
                    }
                };

                // Bridge owns the cleanup closure (knows whether the sandbox
                // lives in the plain `<home>/sandbox/...` tree or under the
                // FUSE upper-side dir). It rides inside the teardown guard from
                // here: `execute_and_cache_inner` completes the teardown after
                // `cache_locally` (which reads from the sandbox), and any drop
                // on the way there queues the generation-checked reclaim
                // instead. Slot guards travel with the response so result.rs
                // can drop them before completing the teardown.
                sandbox_teardown.set_job(res.sandbox_cleanup);
                Ok((res.artifacts, sandbox_teardown, res.sandbox_guards))
            },
        )
        .await;
        htelemetry::telemetry::record_execute_ms(exec_started.elapsed().as_millis() as u64);
        res.map(|(artifacts, teardown, guards)| (artifacts, teardown, guards, execute_guard))
    }

    /// The one sandbox block every run goes through, a build's
    /// ([`execute`](Self::execute)) and an invocation's
    /// ([`invoke`](Self::invoke)) alike (R12, E7): prepare the leased
    /// directory, present the credentials into it, assemble the
    /// [`RunRequest`] and call the driver's `run` or `run_shell` through the
    /// interactive wrapper.
    ///
    /// Two results, apart: the outer `Err` is heph's own failure around the
    /// run (preparing the sandbox, credentials, the wrapper); the inner one is
    /// the driver's `run` itself. An invocation reads a process exit off the
    /// inner one only, so a dep's or a credential source's `ProcessFailed`
    /// further out can never pass for the tool's own exit.
    ///
    /// The lease decides what "prepare" means; resolving it after the run —
    /// keep for diagnostics, reclaim, complete — stays with the caller, which
    /// owns it ([`SandboxTeardown`](crate::engine::sandbox_cleaner::SandboxTeardown)
    /// or [`InvokeSandbox`](crate::engine::invoke_sandbox::InvokeSandbox)).
    pub(crate) async fn run_in_sandbox(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        lease: SandboxLease<'_>,
        run: SandboxRun,
    ) -> anyhow::Result<anyhow::Result<RunResponse>> {
        let SandboxRun {
            driver,
            target,
            inputs,
            hashin,
            scratch,
            credentials,
            deferred,
            wrapper,
            shell,
            options,
        } = run;
        // N6/N9: only an entrypoint's run is ever sent anything but the
        // default options, whichever path assembled them.
        if !options.is_default() && target.entrypoint.is_none() {
            anyhow::bail!(
                "{addr}: run options are only sent to an entrypoint (`entrypoint = True`); this \
                 is a bug in heph"
            );
        }
        let sandbox_dir = lease.dir().to_path_buf();

        hcore::hmemoizer::set_phase("execute:sandbox_remove");
        lease.prepare().await?;

        // Presented *inside* the sandbox lease, so the files land under a
        // directory this run owns and its teardown deletes them whatever the
        // outcome — including a build's failure path, which deliberately keeps
        // the rest of the sandbox for diagnostics. The log tail is what makes
        // that useful, not the token.
        let (credential_mounts, _credential_teardown) = self
            .acquire_and_present(rs, addr, &sandbox_dir, &credentials)
            .await
            .with_context(|| format!("credentials for {addr}"))?;
        let exec_wrapper: InteractiveWrapper = wrapper.unwrap_or_else(|| {
            Arc::new(|inner: InteractiveInner| {
                Box::pin(async move { inner(None, None, None).await })
            })
        });

        let (tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<RunResponse>>();
        // Under `Inherit` the process has heph's own fds: there is nothing to
        // copy, so whatever the wrapper hands in is not passed on — except to
        // a shell, which runs on a PTY of its own and ignores `stdio`: the
        // streams are how the terminal reaches that PTY.
        let inherit =
            !shell && matches!(options.stdio, crate::engine::driver::RunStdio::Inherit(_));

        let inner: InteractiveInner = Box::new(enclose!(
            (rs, self => engine, sandbox_dir)
            move |stdin, stdout, stderr| {
                Box::pin(async move {
                    let mut req = RunRequest::new(rs.request_id(), &target, sandbox_dir);
                    req.tree_root_path = engine.cfg.root.clone();
                    req.inputs = inputs;
                    req.hashin = &hashin;
                    if !inherit {
                        req.stdin = stdin;
                        req.stdout = stdout;
                        req.stderr = stderr;
                    }
                    req.scratch = scratch;
                    req.credentials = credential_mounts;
                    req.deferred = deferred;
                    req.options = options;
                    // A plain driver spawns straight from `run`; a managed
                    // one materializes first and its layer fires it.
                    if !driver.managed {
                        req.options.fire_handover();
                    }
                    let res = if shell {
                        driver.driver.run_shell(req, rs.ctoken()).await
                    } else {
                        driver.driver.run(req, rs.ctoken()).await
                    };
                    drop(tx.send(res));
                    Ok(())
                })
            }
        ));

        hcore::hmemoizer::set_phase("execute:driver_run");
        exec_wrapper(inner).await.with_context(|| "run")?;

        hcore::hmemoizer::set_phase("execute:oneshot_rx");
        rx.await
            .map_err(|_recv_err| anyhow::anyhow!("wrapper never invoked inner"))
    }

    pub(crate) async fn inputs_result_exec(
        self: Arc<Self>,
        rc: Arc<RequestState>,
        inputs: &[LinkedTargetDefInput],
    ) -> anyhow::Result<Vec<RunInput>> {
        let fail_fast = rc.fail_fast();
        let futs = inputs.iter().map(|input| {
            enclose!((self => engine, rc, input) async move {
                let res = engine.result_addr(rc, &input.target.addr, OutputMatcher::Exact(input.output_names), &ResultOptions::default()).await?;
                let dep_inputs = res.artifacts.iter().map(|art| RunInput {
                    artifact: InputArtifact {
                        r#type: Type::Dep,
                        origin_id: input.origin_id.clone(),
                        content: Arc::clone(art),
                    },
                    origin_id: input.origin_id.clone(),
                    source_addr: input.target.addr.clone(),
                    filters: input.filters.clone(),
                    annotations: input.annotations.clone(),
                });
                // Support artifacts share the dep's origin_id/source_addr/
                // annotations but are routed as Type::Support so the managed
                // bridge materializes them into the sandbox without a list
                // file. Filters don't apply — support files are an all-or-
                // nothing per-dep set.
                let support_inputs = res.support_artifacts.iter().map(|art| RunInput {
                    artifact: InputArtifact {
                        r#type: Type::Support,
                        origin_id: input.origin_id.clone(),
                        content: Arc::clone(art),
                    },
                    origin_id: input.origin_id.clone(),
                    source_addr: input.target.addr.clone(),
                    filters: vec![],
                    annotations: input.annotations.clone(),
                });
                let run_inputs: Vec<RunInput> = dep_inputs.chain(support_inputs).collect();
                anyhow::Ok(run_inputs)
            })
        });

        let results = crate::engine::fanout::join_all_failable(futs, fail_fast).await?;
        Ok(results.into_iter().flatten().collect())
    }
}

/// Whose sandbox directory a run is in, which decides how it is prepared
/// (R12). Borrowed: the caller owns the lease itself — a build's
/// [`SandboxTeardown`](crate::engine::sandbox_cleaner::SandboxTeardown), an
/// invocation's [`InvokeSandbox`](crate::engine::invoke_sandbox::InvokeSandbox)
/// — and resolves it after the run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SandboxLease<'l> {
    /// A build's per-address directory, reused run after run and ordered
    /// against a predecessor's stragglers by its generation claim. Prepared
    /// by a generation-checked stale removal; on failure the owner keeps the
    /// tree for the failure diagnostic, on a drop it reclaims it.
    PerAddr {
        dir: &'l std::path::Path,
        generation: u64,
    },
    /// An invocation's own directory, created empty and exclusively, held
    /// through its `.owner` lock. Nothing to prepare; the owner removes it,
    /// synchronously, on every exit.
    Invocation { dir: &'l std::path::Path },
}

impl<'l> SandboxLease<'l> {
    pub(crate) fn per_addr(teardown: &'l crate::engine::sandbox_cleaner::SandboxTeardown) -> Self {
        Self::PerAddr {
            dir: teardown.dir(),
            generation: teardown.generation(),
        }
    }

    pub(crate) fn invocation(sandbox: &'l crate::engine::invoke_sandbox::InvokeSandbox) -> Self {
        Self::Invocation { dir: sandbox.dir() }
    }

    fn dir(&self) -> &'l std::path::Path {
        match *self {
            Self::PerAddr { dir, .. } | Self::Invocation { dir } => dir,
        }
    }

    async fn prepare(&self) -> anyhow::Result<()> {
        match *self {
            Self::PerAddr { dir, generation } => {
                // Stale cleanup only — the driver bridge owns the create step
                // because it may redirect this path into a FUSE mount (v2
                // single-mount mode). Creating here would waste an inode and
                // leave an orphan empty dir when the bridge picks the FUSE side.
                //
                // Queued with this run's claim: if this future is cancelled at
                // the await below (the production wedge's phase), the job still
                // runs eventually — and by then a successor may own the path,
                // in which case the job declines rather than deleting the
                // successor's live sandbox.
                let owned = dir.to_path_buf();
                sync_fs_op_on_thread(move || {
                    crate::engine::sandbox_cleaner::generation::remove_stale(&owned, generation)
                })
                .await
                .with_context(|| format!("remove stale sandbox dir {}", dir.display()))
            }
            Self::Invocation { .. } => Ok(()),
        }
    }
}

/// Everything [`Engine::run_in_sandbox`] needs besides the lease.
pub(crate) struct SandboxRun {
    pub driver: Arc<crate::engine::engine::Driver>,
    pub target: Arc<crate::engine::driver::targetdef::TargetDef>,
    pub inputs: Vec<RunInput>,
    pub hashin: String,
    pub scratch: Vec<crate::engine::driver::ScratchMount>,
    pub credentials: Vec<crate::engine::ResolvedCredential>,
    pub deferred: std::collections::BTreeMap<String, crate::engine::driver::DeferredValue>,
    pub wrapper: Option<InteractiveWrapper>,
    pub shell: bool,
    pub options: crate::engine::driver::RunOptions,
}

/// Run a synchronous `std::fs` operation through `hcore::blocking::run`.
///
/// Not `tokio::fs::*` — that routes through `spawn_blocking` too, but outside
/// `hcore::blocking`'s concurrency bound — and not inline on the worker (which
/// parks it without telling the runtime). See `hcore::blocking`.
async fn sync_fs_op_on_thread<F, T>(f: F) -> std::io::Result<T>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    hcore::blocking::run(f).await
}
