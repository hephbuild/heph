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
use std::io;
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
    ) -> anyhow::Result<(
        Vec<OutputArtifact>,
        Option<crate::engine::sandbox_cleaner::SandboxCleanupJob>,
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

        // Acquire semaphore AFTER dep resolution so no permit is held while waiting for
        // deps — prevents the classic diamond deadlock where mid-nodes hold permits while
        // waiting for a leaf that also needs a permit.
        hcore::hmemoizer::set_phase("execute:semaphore_acquire");
        let semaphore = Arc::clone(&self.result_semaphore);
        // Observed *before* the acquire, so the stamp times the wait rather than
        // its end. This queue is invisible everywhere else: the permit is taken
        // after dep resolution but before `ExecuteStart`, so a target parked here
        // has no open `execute` span and shows only as an open `result`. A build
        // wedged with every worker held reported "N result" and no limits line at
        // all, because the `workers` limiter was declared and never fed.
        let d = crate::engine::diag::global();
        d.limiter("workers")
            .observe(semaphore.available_permits(), d.now_ms());
        let _permit = semaphore
            .acquire()
            .await
            .context("result semaphore closed")?;
        // Counted only once `acquire` has *returned*. A permit handed to a
        // waiter that is never polled again never reaches this line, which is
        // precisely what makes the pool's free count and this counter disagree
        // — see `WorkerPermits::unaccounted`.
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
                error,
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
                // Stale cleanup only — the driver bridge owns the create step
                // because it may redirect this path into a FUSE mount (v2 single-
                // mount mode). Creating here would waste an inode + leave an
                // orphan empty dir when the bridge picks the FUSE side.
                hcore::hmemoizer::set_phase("execute:sandbox_remove");
                sync_fs_op_on_thread(enclose!((sandbox_dir) move || {
                    match crate::engine::sandbox_cleaner::remove_dir_all(&sandbox_dir) {
                        Ok(_) => Ok(()),
                        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                        Err(err) => Err(err),
                    }
                }))
                .await
                .with_context(|| format!("remove stale sandbox dir {}", sandbox_dir.display()))?;

                let exec_wrapper: InteractiveWrapper = exec_wrapper.unwrap_or_else(|| {
                    Arc::new(|inner: InteractiveInner| {
                        Box::pin(async move { inner(None, None, None).await })
                    })
                });

                let (tx, rx) = tokio::sync::oneshot::channel::<RunResponse>();

                let hashin = hashin.to_owned();

                let inner: InteractiveInner = Box::new(enclose!(
                    (driver, def, rs, self => engine, sandbox_dir)
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
                let res = rx
                    .await
                    .map_err(|_recv_err| anyhow::anyhow!("wrapper never invoked inner"))?;

                // Bridge owns the cleanup closure (knows whether the sandbox
                // lives in the plain `<home>/sandbox/...` tree or under the
                // FUSE upper-side dir). Slot guards travel with the response
                // so result.rs can drop them in the same defer that fires
                // sandbox cleanup.
                Ok((res.artifacts, res.sandbox_cleanup, res.sandbox_guards))
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

/// Run a synchronous `std::fs` operation on the dedicated blocking pool.
///
/// Not `tokio::fs::*` (which routes through `spawn_blocking`, observed to lose
/// wake-ups on macOS under heavy load) and not inline on the worker (which parks
/// it without telling the runtime). See `hcore::blocking`.
async fn sync_fs_op_on_thread<F>(f: F) -> std::io::Result<()>
where
    F: FnOnce() -> std::io::Result<()> + Send + 'static,
{
    hcore::blocking::run(f).await
}
