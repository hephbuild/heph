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
    ) -> anyhow::Result<(
        Vec<OutputArtifact>,
        crate::engine::sandbox_cleaner::SandboxTeardown,
        Vec<hplugin::driver::SandboxGuard>,
    )> {
        let driver = self
            .drivers_by_name
            .get(&spec.driver)
            .ok_or_else(|| anyhow::anyhow!("driver not found: {}", spec.driver))
            .cloned()?;

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

        // Declarations only: read the referenced `secret()` targets and reject a
        // set that would fight over one file or variable. Nothing is minted
        // here — that waits until there is a sandbox to render into, which is
        // also what keeps a cache hit from touching an IdP at all.
        hcore::hmemoizer::set_phase("execute:secrets_resolve");
        let resolved_secrets = self
            .resolve_secrets(&rs, addr, &def.target.inputs)
            .await
            .with_context(|| format!("resolve secrets for {addr}"))?;

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
                // lives in this block, so an `Err` — a failing target above
                // all — can be told apart from a drop: the error leaves the
                // sandbox for diagnostics, the drop reclaims it.
                let run = async {
                    // Stale cleanup only — the driver bridge owns the create
                    // step because it may redirect this path into a FUSE mount
                    // (v2 single-mount mode). Creating here would waste an
                    // inode + leave an orphan empty dir when the bridge picks
                    // the FUSE side.
                    //
                    // Queued with this run's claim: if this future is cancelled
                    // at the await below (the production wedge's phase), the
                    // job still runs eventually — and by then a successor may
                    // own the path, in which case the job declines rather than
                    // deleting the successor's live sandbox.
                    hcore::hmemoizer::set_phase("execute:sandbox_remove");
                    sync_fs_op_on_thread(enclose!(
                        (sandbox_dir) move ||
                            crate::engine::sandbox_cleaner::generation::remove_stale(
                                &sandbox_dir,
                                sandbox_generation,
                            )
                    ))
                    .await
                    .with_context(|| {
                        format!("remove stale sandbox dir {}", sandbox_dir.display())
                    })?;

                    let exec_wrapper: InteractiveWrapper = exec_wrapper.unwrap_or_else(|| {
                        Arc::new(|inner: InteractiveInner| {
                            Box::pin(async move { inner(None, None, None).await })
                        })
                    });

                    let (tx, rx) = tokio::sync::oneshot::channel::<RunResponse>();

                    // Mint and render now that there is a sandbox to write into.
                    // Deliberately *after* the cache decision: a hit never gets
                    // here, so a fully warm build touches no IdP at all.
                    hcore::hmemoizer::set_phase("execute:secrets_deliver");
                    let secret_delivery = self
                        .deliver_secrets(&rs, addr, &resolved_secrets, &sandbox_dir)
                        .await
                        .with_context(|| format!("deliver secrets for {addr}"))?;
                    let secret_env: Vec<(String, String)> = secret_delivery
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let secret_values = secret_delivery.values.clone();

                    let hashin = hashin.to_owned();

                    let inner: InteractiveInner = Box::new(enclose!(
                        (driver, def, rs, self => engine, sandbox_dir, scratch_mounts, secret_env, secret_values)
                        move |stdin, stdout, stderr| {
                            Box::pin(async move {
                                let req = RunRequest {
                                    request_id: rs.request_id(),
                                    target: &def.target,
                                    tree_root_path: engine.cfg.root.clone(),
                                    inputs: deps_result,
                                    hashin: &hashin,
                                    stdin,
                                    stdout,
                                    stderr,
                                    sandbox_dir,
                                    scratch: scratch_mounts,
                                    secret_env,
                                    secret_values,
                                };
                                let res = if shell {
                                    driver.driver.run_shell(req, rs.ctoken()).await?
                                } else {
                                    driver.driver.run(req, rs.ctoken()).await?
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
                .await;

                // Credentials come off the sandbox the moment the process is
                // done with them, on **both** paths.
                //
                // Doing it only on failure was a real leak, caught by a test:
                // on success the rendered file simply stayed in the sandbox,
                // where it survives until that target's next run — and the
                // sandbox teardown is no answer, because it is queued rather
                // than immediate and the whole claim is that nothing durable is
                // written. Nothing needs them any more either: credentials are
                // rendered outside `ws/`, so no output glob can collect one and
                // `cache_locally` never reads them.
                if !resolved_secrets.is_empty() {
                    crate::engine::secrets::SecretDelivery::scrub(&sandbox_dir);
                }

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
                        // Already scrubbed above, which matters most here: this
                        // tree is deliberately kept as the diagnostic and
                        // survives until the target's next run.
                        sandbox_teardown.leave_for_diagnostics();
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
        res
    }

    async fn inputs_result_exec(
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
