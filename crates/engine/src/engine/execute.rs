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
        // A target run can reach the codegen index (the fs driver's lookups).
        self.open_codegen_index(&rs).await?;
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
        // Read now, substituted after admission: `${resource:…}` is this run's
        // allotment, which exists only once the reservation does.
        let deferred_reads = self
            .read_deferred(&rs, &def.target, &deferred_refs)
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

        // This target resolves again: its last failure no longer describes it.
        // Under the execute lock, before the sandbox exists, so a failure of
        // this run fills a slot nothing clears behind it.
        self.failure_slots.clear(addr).await;

        // Admission AFTER dep resolution so nothing is reserved while waiting for
        // deps — prevents the classic diamond deadlock where mid-nodes hold
        // capacity while waiting for a leaf that also needs some.
        hcore::hmemoizer::set_phase("execute:semaphore_acquire");
        let admit_started = std::time::Instant::now();
        let reservation = self.admit(&rs, addr, &def.target.demand).await?;
        // On `ExecuteStart`, so a wait under the notice threshold (which
        // announces nothing) is still answerable from the stream.
        let queued_ms = u64::try_from(admit_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Counted once the reservation is actually in hand.
        let _running = crate::engine::diag::RunningPermit::new();
        let allotment = allotment_of(&reservation, |pool| self.pools.unit(pool));
        let deferred = Self::render_deferred(
            &def.target,
            &deferred_refs,
            &deferred_reads,
            Some(&allotment),
        )
        .with_context(|| format!("resolve deferred values for {addr}"))?;

        let addr_str = addr.format();
        // Telemetry: per-target execute wall time. This path runs only on a real
        // execution (cache misses), so it captures exactly the executed targets.
        let exec_started = std::time::Instant::now();
        let start = execute_start(
            addr_str.clone(),
            driver.name.clone(),
            def.target.cache.enabled,
            &def.target.demand,
            &reservation,
            queued_ms,
        );
        // Held until the run ends; a failed run hands it back before keeping
        // what it failed with (D17).
        let mut reservation = Some(reservation);
        let res = crate::engine::event::emit_scope(
            &rs,
            start,
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
                // documented pre-teardown behaviour), or, for credential
                // material, as `reclaim` once the failure slot is filled; and
                // a bare drop —
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
                            allotment: Some(allotment),
                        },
                    )
                    .await
                    .and_then(|ran| ran.context("run"));

                let res = match run {
                    Ok(res) => res,
                    Err(err) => {
                        // The run is over: its capacity goes back before
                        // the engine keeps anything (D17). The managed layer
                        // already copied `failure_out` into its staging dir
                        // inside the run, under the reservation; what is left
                        // — the write-back and moving the staged dir into the
                        // slot, a rename unless it crosses filesystems — holds
                        // no pool.
                        drop(reservation.take());
                        // The sandbox is kept for diagnostics: nothing may be
                        // queued against it. The tree survives until this
                        // target's next run, whose `remove_stale` reclaims it.
                        // Resolved before the fill is awaited (D17): a
                        // cancellation during the fill must not drop the
                        // teardown and queue a reclaim under the blocking job
                        // still moving the staged outputs out of it.
                        //
                        // Unless the outputs are material, in which case the
                        // diagnostic is not worth the secret it would leave on
                        // disk: torn down once the fill has read it.
                        let material_teardown = if outputs_are_material {
                            Some(sandbox_teardown)
                        } else {
                            sandbox_teardown.leave_for_diagnostics();
                            None
                        };
                        // The target failed. Keep its log and its `failure_out`
                        // in the failure slot (for material, the log's tail in
                        // memory and `out/` only) before anything is done to
                        // the sandbox they live in. Here, under the execute
                        // lock, once per run — not per caller.
                        // A cancelled run did not fail: nothing to keep. A
                        // driver may report a cancellation as an error of its
                        // own (exec's bare "cancelled", log attached so that a
                        // timeout keeps it), so the request's token decides —
                        // except for a timeout, which is a failure even when
                        // the request was cancelled during its grace.
                        let cancelled = hplugin::error::is_cancelled(&err)
                            || (rs.ctoken().is_cancelled()
                                && !crate::engine::run_timeout::is_timed_out(&err));
                        let err = if cancelled {
                            err
                        } else {
                            // An `in_place` run's fixes go over the tree
                            // first, while the left sandbox still holds their
                            // tars (leaving it keeps the directory; only a
                            // later run's `remove_stale` reclaims it), so the
                            // slot can list what was written (D21).
                            let write_back = self
                                .materialize_codegen_on_failure(&rs, def, shell, &err)
                                .await;
                            let err = Arc::clone(&self).reword_removed_generator(&rs, err).await;
                            let slot = self
                                .failure_slots
                                .fill(crate::engine::failure_slot::Fill {
                                    addr,
                                    hashin,
                                    roots: self.sandbox_roots(&sandbox_dir),
                                    err: &err,
                                    lines: rs.log_tail_lines(),
                                    material: outputs_are_material,
                                    fixes: write_back.fixes,
                                })
                                .await;
                            let out_error = slot.out_error.clone();
                            let err = err.context(slot);
                            // I7: the keep failed, the failure did not.
                            let err = match out_error {
                                Some(e) => err.context(format!("failure_out not kept: {e}")),
                                None => err,
                            };
                            // I5: a refused write-back is reported beside the
                            // tool's own error, never in its place.
                            match write_back.error {
                                Some(e) => err.context(e),
                                None => err,
                            }
                        };
                        // Not `complete`: a failed run handed over no bridge
                        // job, so it would enqueue nothing and leave the
                        // presented credentials on disk — in the FUSE upper
                        // side, under FUSE, which goes too.
                        if let Some(teardown) = material_teardown {
                            teardown.reclaim(self.fuse_upper(&sandbox_dir));
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

    /// The fs driver refuses a generated path naming the row's generator, but
    /// it cannot read defs, so it cannot tell a generator that still exists
    /// from one that was removed (Decision 18). On this failure path only, the
    /// row's addr is resolved once: when it is not found, the error is rebuilt
    /// to name `codegen keep` / `discard` instead of "depend on". Any other
    /// error, or a lookup that fails otherwise, is returned unchanged.
    ///
    /// The lookup is a *tracked* edge from this fs target to the generator.
    /// A provider whose `get` of the generator builds this fs target (the go
    /// provider's `_golist` shape) closes a cycle the request's dep graph
    /// reports, instead of waiting on the in-flight spec or execute it is
    /// itself part of; the cycle is "any other error", so the original
    /// wording stands. fs targets reserve nothing, so the lookup holds no
    /// capacity another target could be waiting for.
    async fn reword_removed_generator(
        self: Arc<Self>,
        rs: &Arc<RequestState>,
        err: anyhow::Error,
    ) -> anyhow::Error {
        use hbuiltins::pluginfs::{CodegenFileError, CodegenFileErrorKind};
        let Some(cg) = hcore::hmemoizer::downcast_chain_ref::<CodegenFileError>(&err) else {
            return err;
        };
        if cg.kind != CodegenFileErrorKind::Generated {
            return err;
        }
        let Ok(generator) = hmodel::htaddr::parse_addr(&cg.addr) else {
            return err;
        };
        match self.get_spec(Arc::clone(rs), &generator).await {
            Err(e)
                if hcore::hmemoizer::downcast_chain_ref::<
                    crate::engine::error::TargetNotFoundError,
                >(&e)
                .is_some() =>
            {
                anyhow::Error::new(CodegenFileError {
                    kind: CodegenFileErrorKind::TargetRemoved,
                    ..cg.clone()
                })
                .context("run")
            }
            Ok(_) | Err(_) => err,
        }
    }

    /// Where a run in `sandbox_dir` can have written its log: the directory
    /// itself and, when FUSE is mounted, the mount-side and backing-side
    /// directories the bridge redirects it to.
    fn sandbox_roots(&self, sandbox_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut roots = vec![sandbox_dir.to_path_buf()];
        if let Some(rel) = self.fuse_rel(sandbox_dir) {
            roots.push(self.fuse.lower.join(rel));
            roots.push(self.fuse.upper.join(rel));
        }
        roots
    }

    /// The backing-side directory a run in `sandbox_dir` wrote to, when FUSE
    /// is mounted: what the FUSE bridge's cleanup job removes after a passing
    /// run.
    fn fuse_upper(&self, sandbox_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        self.fuse_rel(sandbox_dir)
            .map(|rel| self.fuse.upper.join(rel))
    }

    /// `sandbox_dir` relative to the plain sandbox root, when FUSE is mounted
    /// and the bridge redirects the run.
    fn fuse_rel<'a>(&self, sandbox_dir: &'a std::path::Path) -> Option<&'a std::path::Path> {
        self.fuse.mount.as_ref()?;
        sandbox_dir.strip_prefix(self.home.join("sandbox")).ok()
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
            allotment,
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
                Box::pin(async move { inner(None, None, None, None).await })
            })
        });

        let (tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<RunResponse>>();
        // A run in the caller's terminal has heph's own fds or the host's PTY,
        // whichever the handover picks: there is nothing to copy, so whatever
        // the wrapper hands in is not passed on, a `--shell` entrypoint's
        // included.
        let inherit = matches!(options.stdio, crate::engine::driver::RunStdio::Inherit(_));

        let inner: InteractiveInner = Box::new(enclose!(
            (rs, self => engine, sandbox_dir)
            move |stdin, stdout, stderr, winsize| {
                Box::pin(async move {
                    let mut req = RunRequest::new(rs.request_id(), &target, sandbox_dir);
                    req.tree_root_path = engine.cfg.root.clone();
                    req.inputs = inputs;
                    req.hashin = &hashin;
                    if !inherit {
                        req.stdin = stdin;
                        req.stdout = stdout;
                        req.stderr = stderr;
                        req.winsize = winsize;
                    }
                    req.scratch = scratch;
                    req.credentials = credential_mounts;
                    req.deferred = deferred;
                    req.options = options;
                    req.allotment = allotment;
                    // A plain driver spawns straight from `run`; a managed
                    // one materializes first and its layer fires it.
                    if !driver.managed {
                        req.options.fire_handover().context("handing over the terminal")?;
                    }
                    // A `--shell` is a person at a prompt: never timed. A
                    // run without a timeout gets the request's token as is.
                    let res = match target.timeout {
                        _ if shell => driver.driver.run_shell(req, rs.ctoken()).await,
                        None => driver.driver.run(req, rs.ctoken()).await,
                        Some(timeout) => {
                            let run_token = hcore::hasync::StdCancellationToken::new();
                            let run = driver.driver.run(req, &run_token);
                            crate::engine::run_timeout::enforce(
                                run,
                                &run_token,
                                rs.ctoken(),
                                &target.addr,
                                timeout,
                                engine.timeout_grace,
                            )
                            .await
                        }
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

    /// Reserve what `addr` declares (`resources`; an undeclared `cpu` is one
    /// CPU) from the engine's pools, parking in FIFO order until it fits; a
    /// request needing nothing of a bounded pool is granted at once. A pool whose capacity is 0 fails
    /// the target here, before it runs, naming the override that would set it.
    ///
    /// The reservation is held until the run ends and returns itself on drop;
    /// dropping this future mid-wait leaves the queue (`resource_pool`'s module
    /// docs give the contract).
    ///
    /// A wait that outlasts the notice threshold (`admission_notice`, the
    /// result lock's) is announced as `AdmissionWaitStart`, paired with
    /// `AdmissionWaitEnd` on grant or cancellation, the way
    /// `acquire_with_notice` announces a lock wait. A grant within the
    /// threshold announces nothing.
    pub(crate) async fn admit(
        &self,
        rs: &Arc<RequestState>,
        addr: &Addr,
        resources: &hplugin::driver::Demand,
    ) -> anyhow::Result<crate::engine::resource_pool::Reservation> {
        let pool = self.pools.get();
        // Observed *before* the acquire, so the stamp times the wait rather than
        // its end. This queue is invisible everywhere else: admission happens
        // after dep resolution but before `ExecuteStart`, so a target parked
        // here has no open `execute` span and shows only as an open `result`.
        let d = crate::engine::diag::global();
        d.limiter("workers").observe(
            crate::engine::engine::cpu_free_cpus(&self.pools),
            d.now_ms(),
        );
        let acquire = pool
            .acquire(resources)
            .map_err(|err| anyhow::anyhow!("{addr} {err}"))?;
        tokio::pin!(acquire);
        let granted: anyhow::Result<_> =
            match tokio::time::timeout(self.admission_notice, &mut acquire).await {
                Ok(granted) => granted.map_err(Into::into),
                Err(_elapsed) => match acquire.wait_info() {
                    // Granted between the timeout and here: nothing to announce.
                    None => (&mut acquire).await.map_err(Into::into),
                    Some(info) => {
                        let addr_str = addr.format();
                        crate::engine::event::emit_scope(
                            rs,
                            admission_wait_start(addr_str.clone(), &info),
                            move |_| crate::engine::event::BuildEventKind::AdmissionWaitEnd {
                                addr: addr_str,
                            },
                            async { (&mut acquire).await.map_err(Into::into) },
                        )
                        .await
                    }
                },
            };
        let reservation = granted.with_context(|| format!("admitting {addr}"))?;
        tracing::debug!(
            addr = %addr,
            granted = ?reservation.iter().collect::<Vec<_>>(),
            "admitted"
        );
        // Once per addr per request: `execute` is memoised per addr.
        if reservation.clamped() {
            tracing::info!(
                addr = %addr,
                declared = ?resources,
                granted = ?reservation.iter().collect::<Vec<_>>(),
                "reservation clamped to the pool's capacity; the target runs alone in it"
            );
        }
        Ok(reservation)
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
    /// What the run is told it may use (`RunRequest.allotment`): a build's
    /// reservation, an invocation's full cpu capacity.
    pub allotment: Option<hplugin::driver::Allotment>,
}

/// The `AdmissionWaitStart` event for a queued request: the first pool it does
/// not fit in, or — fitting everywhere but queued behind others — the first it
/// needs.
fn admission_wait_start(
    addr: String,
    info: &crate::engine::resource_pool::WaitInfo,
) -> crate::engine::event::BuildEventKind {
    // An unbounded pool (`free: None`) never holds a request back, so it is
    // named only when every pool the request needs is unbounded.
    let pool = info
        .pools
        .iter()
        .find(|p| p.free.is_some_and(|f| p.need > f))
        .or_else(|| info.pools.iter().find(|p| p.free.is_some()))
        .or_else(|| info.pools.first());
    crate::engine::event::BuildEventKind::AdmissionWaitStart {
        addr,
        pool: pool.map_or_else(String::new, |p| p.pool.as_str().to_owned()),
        need: pool.map_or(0, |p| p.need),
        free: pool.map_or(0, |p| p.free.unwrap_or(u64::MAX)),
        ahead: u64::try_from(info.ahead).unwrap_or(u64::MAX),
    }
}

/// What a run is told of its reservation: `cpu` always, in whole CPUs
/// (rounded up, at least 1, so a zero-cost grant is told 1), and every other
/// pool it was granted more than 0 of, each rendered by its pool's unit
/// (`unit_of`, the engine's pools).
pub(crate) fn allotment_of(
    reservation: &crate::engine::resource_pool::Reservation,
    unit_of: impl Fn(&str) -> Option<hcore::capacity::Unit>,
) -> hplugin::driver::Allotment {
    use hcore::capacity::Unit;
    use hplugin::provider::PoolName;
    let cpu = (
        PoolName::CPU.to_owned(),
        Unit::Cpu.render(reservation.amount(PoolName::CPU).max(1)),
    );
    let rest = reservation
        .iter()
        .filter(|(p, n)| p.as_str() != PoolName::CPU && *n > 0)
        .map(|(p, n)| {
            let unit = unit_of(p.as_str()).unwrap_or_else(|| Unit::default_for(p.as_str()));
            (p.as_str().to_owned(), unit.render(n))
        });
    hplugin::driver::Allotment::new(std::iter::once(cpu).chain(rest))
}

/// What an entrypoint run is told, having reserved nothing: `cpu` is the
/// whole `cpu_capacity` (in whole CPUs, at least 1), and every other pool its
/// declared amount when above 0, rendered by the pool's unit.
pub(crate) fn entrypoint_allotment(
    cpu_capacity: u64,
    demand: &hplugin::driver::Demand,
    unit_of: impl Fn(&str) -> Option<hcore::capacity::Unit>,
) -> hplugin::driver::Allotment {
    use hcore::capacity::Unit;
    use hplugin::provider::PoolName;
    let cpu = (
        PoolName::CPU.to_owned(),
        Unit::Cpu.render(cpu_capacity.max(1)),
    );
    let rest = demand
        .iter()
        .filter(|(p, n)| p.as_str() != PoolName::CPU && *n > 0)
        .map(|(p, n)| {
            let unit = unit_of(p.as_str()).unwrap_or_else(|| Unit::default_for(p.as_str()));
            (p.as_str().to_owned(), unit.render(n))
        });
    hplugin::driver::Allotment::new(std::iter::once(cpu).chain(rest))
}

/// The `ExecuteStart` event: what the target declared (only its declared
/// keys, in each pool's unit's base) and what it was granted, after
/// clamping, and how long admission took. `reserved` always carries `cpu`, 0
/// on a zero-cost grant.
pub(crate) fn execute_start(
    addr: String,
    driver: String,
    cache: bool,
    declared: &hplugin::driver::Demand,
    reservation: &crate::engine::resource_pool::Reservation,
    queued_ms: u64,
) -> crate::engine::event::BuildEventKind {
    use hplugin::provider::PoolName;
    let declared = declared
        .iter()
        .map(|(p, n)| (p.as_str().to_owned(), n))
        .collect();
    let mut reserved: std::collections::BTreeMap<String, u64> = reservation
        .iter()
        .map(|(p, n)| (p.as_str().to_owned(), n))
        .collect();
    reserved.entry(PoolName::CPU.to_owned()).or_insert(0);
    crate::engine::event::BuildEventKind::ExecuteStart {
        addr,
        driver,
        cache,
        declared: Some(declared),
        reserved: Some(reserved),
        queued_ms: Some(queued_ms),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::event::BuildEventKind;
    use crate::engine::resource_pool::Limit;
    use crate::engine::resource_pool::ResourcePool;
    use futures::FutureExt as _;
    use hcore::capacity::Unit;
    use hplugin::driver::{Demand, Resources};
    use hplugin::provider::PoolName;

    const CPU: u64 = Resources::MICROS_PER_CPU;

    fn req(pairs: &[(&str, u64)]) -> Demand {
        Demand::try_from_pairs(pairs.iter().map(|&(n, a)| (n.to_string(), a))).expect("demand")
    }

    fn grant(caps: &[(&str, Limit)], r: &Demand) -> crate::engine::resource_pool::Reservation {
        ResourcePool::new(
            caps.iter()
                .map(|&(n, c)| (PoolName::try_from(n).expect("pool"), c)),
        )
        .acquire(r)
        .expect("acquire")
        .now_or_never()
        .expect("granted at once")
        .expect("granted")
    }

    /// D8: 16 CPUs declared on a 4-CPU pool announces `declared: 16` and
    /// `reserved: 4`, the clamped grant.
    #[test]
    fn execute_start_reserved_is_the_granted_amount() {
        let declared = req(&[("cpu", 16 * CPU)]);
        let r = grant(
            &[
                ("cpu", Limit::Bounded(4 * CPU)),
                ("memory", Limit::Bounded(1 << 30)),
            ],
            &declared,
        );
        let ev = execute_start("//p:t".into(), "exec".into(), true, &declared, &r, 7);
        let BuildEventKind::ExecuteStart {
            declared,
            reserved,
            queued_ms,
            ..
        } = ev
        else {
            panic!("wrong variant: {ev:?}");
        };
        assert_eq!(queued_ms, Some(7));
        assert_eq!(
            declared,
            Some(std::collections::BTreeMap::from([("cpu".into(), 16 * CPU)]))
        );
        assert_eq!(
            reserved,
            Some(std::collections::BTreeMap::from([("cpu".into(), 4 * CPU)]))
        );
    }

    /// A zero-cost target reserves nothing: `reserved` still carries `cpu`
    /// (0), and the run is told one CPU.
    #[test]
    fn a_zero_cost_grant_reserves_zero_cpu_and_is_told_one() {
        let declared = req(&[("cpu", 0)]);
        let r = grant(&[("cpu", Limit::Bounded(4 * CPU))], &declared);
        let BuildEventKind::ExecuteStart { reserved, .. } =
            execute_start("//p:t".into(), "exec".into(), true, &declared, &r, 0)
        else {
            panic!("wrong variant");
        };
        assert_eq!(
            reserved,
            Some(std::collections::BTreeMap::from([("cpu".into(), 0)]))
        );
        let a = allotment_of(&r, Unit::of_builtin);
        assert_eq!(a.iter().collect::<Vec<_>>(), [("cpu", "1")]);
    }

    fn units(pool: &str) -> Option<Unit> {
        match pool {
            "gpu" => Some(Unit::Count),
            "scratch" => Some(Unit::Bytes),
            p => Unit::of_builtin(p),
        }
    }

    /// The allotment rounds the granted cpu up to whole CPUs and carries every
    /// other pool granted more than 0, each rendered by its unit — custom pools
    /// included.
    #[test]
    fn allotment_is_every_granted_pool_rendered_by_its_unit() {
        let caps = [
            ("cpu", Limit::Bounded(4 * CPU)),
            ("gpu", Limit::Bounded(2)),
            ("memory", Limit::Bounded(4 << 30)),
            ("scratch", Limit::Unbounded),
        ];
        let declared = req(&[
            ("cpu", 2_500_000),
            ("gpu", 1),
            ("memory", 8 << 30),
            ("scratch", 3 << 40),
        ]);
        let a = allotment_of(&grant(&caps, &declared), units);
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            [
                ("cpu", "3"),
                ("gpu", "1"),
                ("memory", "4294967296"),
                ("scratch", "3298534883328"),
            ],
            "memory clamped to 4 GiB; the unbounded pool carries what was declared"
        );

        // Nothing declared: cpu alone; a pool declared at 0 is not granted.
        for undeclared in [req(&[]), req(&[("gpu", 0)])] {
            let a = allotment_of(&grant(&caps, &undeclared), units);
            assert_eq!(a.iter().collect::<Vec<_>>(), [("cpu", "1")]);
        }
    }
}
