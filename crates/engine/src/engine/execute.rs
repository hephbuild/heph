use crate::engine::driver::inputartifact::{InputArtifact, Type};
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::{RunInput, RunRequest, RunResponse};
use crate::engine::error::SandboxFate;
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

/// How [`Engine::execute`] runs a target, beyond the target itself.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecuteFlags {
    /// An interactive `--shell` in the target's sandbox, not its command.
    pub shell: bool,
    /// Run without the target's scratch slots (`--no-scratch`).
    pub no_scratch: bool,
    /// This target's outputs are credential material. The one thing it
    /// changes is the failure path — see the `Err` arm in `execute`.
    pub outputs_are_material: bool,
}

/// What one run of [`Engine::execute`] produced, and the guards its caller
/// holds until the outputs are cached.
pub(crate) struct Executed {
    pub artifacts: Vec<OutputArtifact>,
    /// The host paths the driver declared, as it returned them: not yet
    /// deduplicated, capped or checked to exist. The caller checks them
    /// before it writes the entry (I6, I8).
    pub host_paths: Vec<hplugin::driver::HostPath>,
    pub teardown: crate::engine::sandbox_cleaner::SandboxTeardown,
    pub sandbox_guards: Vec<hplugin::driver::SandboxGuard>,
    pub execute_guard: crate::engine::result_lock::ExecuteGuard,
    /// The live log's guard: held until the entry that replaces it is
    /// written, then dropped to unlink it.
    pub live_log: Option<crate::engine::run_log::LiveGuard>,
}

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
        flags: ExecuteFlags,
    ) -> anyhow::Result<Executed> {
        let ExecuteFlags {
            shell,
            no_scratch,
            outputs_are_material,
        } = flags;
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

        // Resolution (which declaration does each reference name, and is the
        // set coherent?) already happened at `get_def`; this repeats it on the
        // finalized def. Acquisition happens in `admit_with_credentials`.
        let resolved_credentials = self
            .resolve_credentials(&rs, addr, &def.target.inputs)
            .await
            .with_context(|| format!("resolve credentials for {addr}"))?;

        let Admitted {
            credentials,
            execute_guard,
            reservation,
            sandbox_slot,
            queued_ms,
        } = self
            .admit_with_credentials(&rs, addr, &driver, def, resolved_credentials)
            .await?;

        // This target resolves again: its last failure no longer describes it.
        // Under the execute lock, before the sandbox exists, so a failure of
        // this run fills a slot nothing clears behind it.
        self.failure_slots.clear(addr).await;

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
        let mut start = execute_start(
            addr_str.clone(),
            driver.name.clone(),
            def.target.cache.enabled,
            &def.target.demand,
            &reservation,
            queued_ms,
            // Only an event reads them: a request nobody listens to pays no
            // allocation.
            if rs.has_listeners() {
                def.target.output_names()
            } else {
                Vec::new()
            },
        );
        // Held until the run ends; a failed run hands it back before keeping
        // what it failed with (D17).
        if let crate::engine::event::BuildEventKind::ExecuteStart {
            log_not_streamed, ..
        } = &mut start
        {
            // Diagnostics only: a reader's wording while no byte has arrived.
            *log_not_streamed = !driver.driver.streams_log();
        }
        let mut reservation = Some(reservation);
        let res = crate::engine::event::emit_scope(
            &rs,
            start,
            move |error| crate::engine::event::BuildEventKind::ExecuteEnd {
                addr: addr_str,
                error: error.map(crate::engine::event::ErrorDetail::into_message),
            },
            async {
                // The run's live log (never on a cache hit). Its file is
                // created by the run's first byte, so a run that writes
                // nothing touches no file. Not for a `--shell`, which is the
                // user's own session (R22), nor for a credential source,
                // whose output is material and whose log stays in its
                // sandbox, as before. A file that cannot be created fails
                // the run (`end_run_log`): its output would go nowhere.
                let live = if shell || outputs_are_material {
                    None
                } else {
                    Some(self.run_logs.create_for_driver(
                        addr,
                        hashin,
                        driver.driver.streams_log(),
                    ))
                };
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
                // and every exit resolves it exactly once:
                // - completion hands the bridge's cleanup job to `complete()`
                //   in `execute_and_cache_inner`;
                // - a *failing* run resolves it in the match below: as
                //   `defer_for_failure` then `reclaim` once the failure slot
                //   is filled (the slot keeps the log and `failure_out`, and
                //   the failure box reads the tail from the slot, never from
                //   the sandbox); as `leave_for_diagnostics` under
                //   `--keep-failed-sandboxes` (credential material is
                //   reclaimed regardless);
                // - a deferred teardown dropped without its `reclaim` — a
                //   cancellation during the fill, or a run abandoned after
                //   its timeout — queues nothing and leaves the tree to this
                //   addr's next run;
                // - a bare drop of the armed teardown — cancellation or an
                //   unwind, and only those — queues a generation-checked
                //   reclaim of the directory. Without that leg, mass
                //   fail-fast leaves one abandoned sandbox tree per
                //   *cancelled* execute and nothing ever collects them (`gc`
                //   has no sandbox sweep).
                // The run's `sandbox` slot rides in the teardown from here:
                // released when the removal it queues returns, or at once by
                // a path that queues none.
                let mut sandbox_teardown = self.sandbox_teardown(
                    sandbox_dir.clone(),
                    sandbox_generation,
                    rs.bg_pending(),
                    sandbox_slot,
                    rs.sandbox_removals(),
                );

                // Everything fallible between the claim and the run response
                // lives in the shared block, so an `Err` — a failing target
                // above all — can be told apart from a drop: the error keeps
                // the sandbox until the failure slot is filled, the drop
                // reclaims it at once.
                // Only a managed driver makes a sandbox tree (its layer
                // creates the directory); a plain one (`fs`, `template`, …)
                // runs with none, and its failure box has no sandbox to name.
                let managed = driver.managed;
                let ran = self
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
                            credentials,
                            deferred,
                            wrapper: exec_wrapper,
                            shell,
                            options: crate::engine::driver::RunOptions::default(),
                            allotment: Some(allotment),
                            log: live.as_ref().map(|(log, _)| Arc::clone(log)),
                        },
                    )
                    .await;
                // The outer `Err` is heph's own failure around the run: the
                // stale removal, the credentials' presentation, the wrapper.
                // Past it the driver was called, and a managed one has made
                // its tree.
                let had_sandbox = managed && ran.is_ok();
                let run = ran.and_then(|ran| ran.context("run"));
                // End the live log while the sandbox still exists: a log the
                // driver returned as an artifact is read from it.
                let (run, live) = match live {
                    Some((log, guard)) => {
                        let (run, file) = self
                            .end_run_log(addr, &log, run, self.sandbox_roots(&sandbox_dir))
                            .await;
                        (run, Some((log, guard, file)))
                    }
                    None => (run, None),
                };

                let res = match run {
                    Ok(res) => res,
                    Err(err) => {
                        // The run is over: its capacity goes back before
                        // the engine keeps anything (D17). The managed layer
                        // already packed `failure_out` into a tar in the
                        // sandbox inside the run, under the reservation; what
                        // is left — the write-back and unpacking that tar into
                        // the slot — holds no pool.
                        drop(reservation.take());
                        // What becomes of the sandbox is settled before the
                        // fill is awaited (D17), and it is removed only once
                        // the write-back and the fill are done reading it:
                        // - credential material: always removed, the teardown
                        //   staying armed meanwhile, since no diagnostic is
                        //   worth the secret a kept tree would leave on disk —
                        //   an abandoned run's included, whose removal may
                        //   race its writer: a partial removal still beats
                        //   leaving the credentials;
                        // - `--keep-failed-sandboxes`: left on disk, nothing
                        //   queued; this addr's next run reclaims it;
                        // - a run abandoned after its timeout: left on disk,
                        //   nothing queued, the slot back at once. Its
                        //   process may still be writing to the tree, and a
                        //   removal racing that writer can neither finish
                        //   nor be counted as done; the next run's
                        //   `remove_stale` reclaims it;
                        // - otherwise: deferred, then removed. Deferring
                        //   disarms the drop reclaim, so a cancellation
                        //   during the fill queues no removal under the
                        //   blocking job still unpacking the `failure_out`
                        //   tar out of it; that tree is left to the next run
                        //   instead.
                        // A removal holds the `sandbox` slot until the tree
                        // is gone, as a passing run's does.
                        //
                        // The failure box's `sandbox:` line comes out of the
                        // same branch, and only for a run that had a tree.
                        // Material says nothing unless the flag asked to keep
                        // it: its removal is not a choice to explain.
                        let keep = self.cfg.keep_failed_sandboxes;
                        let (removal, fate) = if outputs_are_material {
                            (
                                Some(sandbox_teardown),
                                keep.then_some(SandboxFate::RemovedMaterial),
                            )
                        } else if keep {
                            let kept = self.sandbox_on_disk(&sandbox_dir);
                            sandbox_teardown.leave_for_diagnostics();
                            (None, Some(SandboxFate::Kept(kept)))
                        } else if crate::engine::run_timeout::is_abandoned(&err) {
                            drop(sandbox_teardown.defer_for_failure());
                            (None, Some(SandboxFate::LeftAbandoned))
                        } else {
                            (
                                Some(sandbox_teardown.defer_for_failure()),
                                Some(SandboxFate::Removed),
                            )
                        };
                        let fate = fate.filter(|_| had_sandbox);
                        // The target failed. Keep its log and its `failure_out`
                        // in the failure slot (for material, the log's tail in
                        // memory and `out/` only) before anything is done to
                        // the sandbox they live in. Here, under the execute
                        // lock, once per run — not per caller.
                        // A cancelled run did not fail: nothing to keep. A
                        // driver may report a cancellation as an error of its
                        // own (exec's bare "cancelled", log attached so that a
                        // timeout keeps it), so the stop decides — the
                        // request's token, or this execute abandoned by
                        // fail-fast — except for a timeout, which is a failure
                        // even when the request was cancelled during its grace.
                        let cancelled = hplugin::error::is_cancelled(&err)
                            || (crate::engine::run_timeout::stop_requested(rs.ctoken())
                                && !crate::engine::run_timeout::is_timed_out(&err));
                        let kept = if cancelled {
                            None
                        } else {
                            // An `in_place` run's fixes go over the tree
                            // first, while the sandbox still holds their tars
                            // (nothing removes it before the fill is done), so
                            // the slot can list what was written (D21).
                            let write_back = self
                                .materialize_codegen_on_failure(&rs, def, shell, &err)
                                .await;
                            let fill = crate::engine::failure_slot::Fill {
                                addr,
                                hashin,
                                roots: self.sandbox_roots(&sandbox_dir),
                                err: &err,
                                lines: rs.log_tail_lines(),
                                material: outputs_are_material,
                                fixes: write_back.fixes,
                                keep_log: true,
                            };
                            // A run with a live log keeps it, through the
                            // descriptor; its guard unlinks it once the slot
                            // is in place, at the end of this arm.
                            let slot = match live {
                                Some((_, _, file)) => {
                                    self.failure_slots.fill_live(fill, file).await
                                }
                                None => self.failure_slots.fill(fill).await,
                            }
                            .with_sandbox(fate);
                            Some((slot, write_back.error))
                        };
                        // Not `complete`: a failed run handed over no bridge
                        // job, so it would enqueue nothing and leave the tree
                        // (presented credentials included) on disk — in the
                        // FUSE upper side, under FUSE, which goes too.
                        if let Some(teardown) = removal {
                            teardown.reclaim(self.fuse_upper(&sandbox_dir));
                        }
                        let Some((slot, write_back_error)) = kept else {
                            return Err(err);
                        };
                        // Past the reclaim: it reads no sandbox, and a lookup
                        // of the generator must not hold the `sandbox` slot.
                        let err = Arc::clone(&self).reword_removed_generator(&rs, err).await;
                        let out_error = slot.out_error.clone();
                        let err = err.context(slot);
                        // I7: the keep failed, the failure did not.
                        let err = match out_error {
                            Some(e) => err.context(format!("failure_out not kept: {e}")),
                            None => err,
                        };
                        // I5: a refused write-back is reported beside the
                        // tool's own error, never in its place.
                        let err = match write_back_error {
                            Some(e) => err.context(e),
                            None => err,
                        };
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
                let mut artifacts = res.artifacts;
                // The live log becomes the entry's `Log` artifact, read by
                // path while `cache_locally` packs it; the guard rides with
                // the artifacts and unlinks it after. A `cache = false`
                // success has no entry: its log stays readable until this
                // heph exits.
                let live = match live {
                    Some((log, guard, file)) => {
                        let written = file
                            .as_ref()
                            .and_then(|f| f.metadata().ok())
                            .is_some_and(|m| m.len() > 0);
                        if !written {
                            Some(guard)
                        } else if def.target.cache.enabled {
                            // Diagnostics only: a log that cannot be named
                            // never fails a run that passed.
                            match live_log_artifact(log.path()) {
                                Ok(a) => artifacts.push(a),
                                Err(e) => tracing::warn!(addr = %addr, error = %e, "keeping the run's log"),
                            }
                            Some(guard)
                        } else {
                            guard.retain();
                            None
                        }
                    }
                    None => None,
                };
                Ok((
                    artifacts,
                    res.host_paths,
                    sandbox_teardown,
                    res.sandbox_guards,
                    live,
                ))
            },
        )
        .await;
        htelemetry::telemetry::record_execute_ms(exec_started.elapsed().as_millis() as u64);
        res.map(
            |(artifacts, host_paths, teardown, sandbox_guards, live_log)| Executed {
                artifacts,
                host_paths,
                teardown,
                sandbox_guards,
                execute_guard,
                live_log,
            },
        )
    }

    /// End a run's live log, once its driver returned and while its sandbox
    /// still exists: finalize it, fail an `Ok` run with its sticky write
    /// error, and import the log the run returned itself, if any.
    ///
    /// A driver may return its log as an artifact at the end of the run
    /// instead of streaming it, or do both: the returned one is complete, so
    /// it wins, imported once and scrubbed, only from a
    /// regular file inside the sandbox or inline bytes. Every other `Log` a
    /// run returns is dropped, so the live file is the only log that
    /// reaches the entry or the slot. Returns the finalized file, `None` when
    /// the run wrote no byte and imported none.
    async fn end_run_log(
        &self,
        addr: &Addr,
        log: &Arc<crate::engine::run_log::RunLog>,
        mut run: anyhow::Result<RunResponse>,
        roots: Vec<std::path::PathBuf>,
    ) -> (anyhow::Result<RunResponse>, Option<std::fs::File>) {
        use crate::engine::driver::outputartifact::Type as ArtifactType;
        use crate::engine::run_log::Returned;
        // Closed for writes now; `ended` only once the import is in place,
        // so a follower never finishes on the streamed file while the
        // returned one is renamed over it.
        let file = log.take_file();
        let returned_of = |a: &OutputArtifact| match &a.content {
            crate::engine::driver::outputartifact::Content::File(f) => {
                Some(Returned::File(std::path::PathBuf::from(&f.source_path)))
            }
            crate::engine::driver::outputartifact::Content::Raw(r) => {
                Some(Returned::Bytes(r.data.clone()))
            }
            _ => None,
        };
        let returned = match &mut run {
            Ok(res) => {
                let mut first = None;
                res.artifacts.retain(|a| {
                    if a.r#type != ArtifactType::Log {
                        return true;
                    }
                    if first.is_none() {
                        first = Some(returned_of(a));
                    }
                    false
                });
                first.flatten()
            }
            Err(err) => hplugin::error::failure_log(err).and_then(returned_of),
        };
        let file = match returned {
            None => file,
            Some(source) => import_run_log(addr, log, file, source, roots).await,
        };
        log.mark_ended();
        let run = crate::engine::run_log::fail_on_write_error(log, run);
        (run, file)
    }
}

/// Import a log the run returned into its finalized live file, off the async
/// workers. Creates the file when the run streamed nothing.
async fn import_run_log(
    addr: &Addr,
    log: &Arc<crate::engine::run_log::RunLog>,
    mut file: Option<std::fs::File>,
    source: crate::engine::run_log::Returned,
    roots: Vec<std::path::PathBuf>,
) -> Option<std::fs::File> {
    use crate::engine::run_log::Imported;
    let log = Arc::clone(log);
    let addr = addr.format();
    hcore::blocking::run(move || {
        match log.import(&mut file, source, &roots) {
            Imported::Replaced | Imported::Abandoned => {}
            Imported::Unavailable => tracing::warn!(
                addr,
                "log unavailable: the run returned a log heph does not read (outside its \
                     sandbox, a symlink, or unreadable); keeping what it streamed"
            ),
            Imported::Failed(e) => tracing::warn!(
                addr,
                error = %e,
                "importing the log the run returned; the log may be incomplete"
            ),
        }
        file
    })
    .await
}

impl Engine {
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
    pub(crate) fn sandbox_roots(&self, sandbox_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut roots = vec![sandbox_dir.to_path_buf()];
        if let Some(rel) = self.fuse_rel(sandbox_dir) {
            roots.push(self.fuse.lower.join(rel));
            roots.push(self.fuse.upper.join(rel));
        }
        roots
    }

    /// The backing-side directory a run in `sandbox_dir` wrote to, when FUSE
    /// is mounted: what the FUSE bridge's cleanup job removes after a passing
    /// run, and what a failed run's reclaim removes beside the plain one.
    pub(crate) fn fuse_upper(&self, sandbox_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        self.fuse_rel(sandbox_dir)
            .map(|rel| self.fuse.upper.join(rel))
    }

    /// Where a kept sandbox's files are on disk, for the failure box: the
    /// FUSE upper side when the run went through the mount (its writes live
    /// there), else `sandbox_dir`.
    pub(crate) fn sandbox_on_disk(&self, sandbox_dir: &std::path::Path) -> std::path::PathBuf {
        self.fuse_upper(sandbox_dir)
            .unwrap_or_else(|| sandbox_dir.to_path_buf())
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
    /// reclaim, keep on disk, complete — stays with the caller, which
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
            log,
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
        // outcome — including a build's failure path, which keeps the rest of
        // the sandbox while the failure slot is filled, and on disk under
        // `--keep-failed-sandboxes`. The log is what makes that useful, not
        // the token. Only presented: the material was
        // acquired before the run took any pool (`acquire_credentials`).
        let (credential_mounts, _credential_teardown) = self
            .present_credentials(rs, addr, &sandbox_dir, &credentials)
            .await
            .with_context(|| format!("credentials for {addr}"))?;
        // The secrets the log is scrubbed of are known only once presented.
        // Set before the run, so its first byte is scrubbed.
        if let Some(log) = &log {
            log.set_needles(hdriver_support::redact::Needles::new(
                credential_mounts.iter().flat_map(|m| m.redact.iter()),
            ));
        }
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

        // Taken here, on the execute's own task: the wrapper may run `inner`
        // anywhere, and only this task can see the memo cell it would be
        // abandoned through.
        let stop = crate::engine::run_timeout::RunStop::new(rs.ctoken());
        let inner: InteractiveInner = Box::new(enclose!(
            (rs, self => engine, sandbox_dir)
            move |stdin, stdout, stderr, winsize| {
                Box::pin(async move {
                    let mut req = RunRequest::new(rs.request_id(), &target, sandbox_dir);
                    req.tree_root_path = engine.cfg.root.clone();
                    req.inputs = inputs;
                    req.hashin = &hashin;
                    // A captured build run's writers are its live log, teed
                    // into whatever sinks the wrapper handed in. Never on a
                    // run in the caller's terminal: nothing is copied there.
                    let (mut log_out, mut log_err) = match log.as_ref().filter(|_| !inherit) {
                        Some(log) => (
                            Some(crate::engine::run_log::RunLogWriter::new(
                                Arc::clone(log),
                                crate::engine::run_log::Stream::Stdout,
                                stdout,
                            )),
                            Some(crate::engine::run_log::RunLogWriter::new(
                                Arc::clone(log),
                                crate::engine::run_log::Stream::Stderr,
                                stderr,
                            )),
                        ),
                        None => {
                            if !inherit {
                                req.stdout = stdout.map(shorten);
                                req.stderr = stderr.map(shorten);
                            }
                            (None, None)
                        }
                    };
                    if let (Some(out), Some(err)) = (log_out.as_mut(), log_err.as_mut()) {
                        req.stdout = Some(out);
                        req.stderr = Some(err);
                        req.log_capture = true;
                    }
                    if !inherit {
                        req.stdin = stdin.map(|s| {
                            let s: &mut (dyn tokio::io::AsyncRead + Send + Sync + Unpin) = s;
                            s
                        });
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
                    // run without a timeout gets its stop as is.
                    let res = match target.timeout {
                        _ if shell => driver.driver.run_shell(req, &stop).await,
                        None => driver.driver.run(req, &stop).await,
                        Some(timeout) => {
                            let run_token = hcore::hasync::StdCancellationToken::new();
                            // A driver that arms the clock is timed from its
                            // process's spawn to its exit; any other, over
                            // its whole `run`.
                            let clock = driver.driver.arms_run_clock().then(|| {
                                let (clock, reader) = hplugin::driver::RunClock::new();
                                req.clock = clock;
                                reader
                            });
                            let run = driver.driver.run(req, &run_token);
                            crate::engine::run_timeout::enforce(
                                run,
                                &run_token,
                                &stop,
                                &target.addr,
                                timeout,
                                engine.timeout_grace,
                                clock,
                            )
                            .await
                        }
                    };
                    // The run returned: each stream's held-back tail goes to
                    // the sinks as well as the file. Not after an abandoned
                    // run, whose process may still be writing.
                    if !matches!(&res, Err(e) if crate::engine::run_timeout::is_abandoned(e)) {
                        for w in [log_out.as_mut(), log_err.as_mut()].into_iter().flatten() {
                            w.finish().await;
                        }
                    }
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
            enclose!((self => engine, rc) async move {
                let res = engine.clone().result_addr(rc.clone(), &input.target.addr, OutputMatcher::Exact(input.output_names.clone()), &ResultOptions::default()).await?;
                for output in &input.tool_outputs_counted_at_run {
                    // The one selected output's artifacts are `res`'s; with
                    // several, ask for that output's alone (memoized).
                    let own;
                    let artifacts = if input.output_names.len() == 1 {
                        &res.artifacts
                    } else {
                        own = engine.clone().result_addr(rc.clone(), &input.target.addr, OutputMatcher::Exact(vec![output.clone()]), &ResultOptions::default()).await?;
                        &own.artifacts
                    };
                    crate::engine::link::check_tool_output_files(&input.target.addr, output, artifacts)?;
                }
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

    /// Everything a run takes before its sandbox exists, in the one order
    /// that cannot deadlock: credential material, the execute lock, then
    /// admission — whose request, for a managed driver, carries the run's
    /// `sandbox` slot beside its cpu (see `resource_pool`'s module docs).
    ///
    /// **Material before both.** Acquiring can build — a producer source is
    /// an `execute` of its own, needing its deps, the producer's gateway and
    /// execute lock, and its own admission (cpu and a sandbox slot) — so a run
    /// holding any of its own while it acquires can wait on itself:
    ///
    /// - the execute lock is a leaf (`ResultLock::lock_execute`): nothing is
    ///   acquired under it, and a producer's gateway would be;
    /// - the producer, or a dep of it, waits for the sandbox slot or the cpu
    ///   its consumer holds — at once with `--resource sandbox=1`, or at
    ///   `-j 1`;
    /// - a plain run of the producer holds the producer's gateway while it
    ///   waits for a slot the consumer holds, and the consumer waits for
    ///   that gateway.
    ///
    /// Acquiring first, the consumer holds only what its caller does (its
    /// result lock, its scratch slots), none of which a producer's build
    /// takes.
    ///
    /// **One admission for cpu and sandbox**, atomic: a run waiting
    /// for cleanup holds no cpu, and a run waiting for cpu holds no slot for
    /// a sandbox not yet made. Slots come back from the reclaim lane, which
    /// waits on nothing, so the wait ends.
    ///
    /// **Material that lapsed while the run waited** (expired, or inside its
    /// skew margin) is acquired again with everything handed back first:
    /// re-acquiring while holding it is the deadlock above. Bounded —
    /// material whose lifetime is shorter than the wait fails the run naming
    /// the credential instead of looping.
    async fn admit_with_credentials(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
        driver: &crate::engine::engine::Driver,
        def: &LinkedTargetDef,
        resolved: Vec<crate::engine::ResolvedCredential>,
    ) -> anyhow::Result<Admitted> {
        /// Acquisitions before a run whose material keeps lapsing fails.
        const MAX_CREDENTIAL_ROUNDS: usize = 3;
        let elapsed_ms = |since: std::time::Instant| {
            u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
        };
        let mut resolved = resolved;
        let mut round = 0;
        loop {
            round += 1;
            hcore::hmemoizer::set_phase("execute:credential_acquire");
            let credentials = self
                .acquire_credentials(rs, addr, resolved)
                .await
                .with_context(|| format!("credentials for {addr}"))?;

            // One run of this target at a time, in any process: its sandbox
            // is per addr. After deps and credentials (holding it across a
            // build could deadlock with a forced rebuild of that build).
            // Returned to the caller, which holds it until the outputs are
            // cached. See `ResultLock::lock_execute`.
            hcore::hmemoizer::set_phase("execute:execute_lock");
            let execute_guard = self
                .acquire_with_notice(
                    rs,
                    addr,
                    None,
                    self.result_lock().lock_execute(addr, rs.ctoken()),
                )
                .await?;

            // Admission AFTER dep resolution so nothing is reserved while
            // waiting for deps — prevents the classic diamond deadlock where
            // mid-nodes hold capacity while waiting for a leaf that also needs
            // some.
            hcore::hmemoizer::set_phase("execute:semaphore_acquire");
            let admit_started = std::time::Instant::now();
            let mut reservation = self
                .admit(rs, addr, &def.target.demand, driver.managed)
                .await?;
            // On `ExecuteStart`, so a wait under the notice threshold (which
            // announces nothing) is still answerable from the stream.
            let queued_ms = elapsed_ms(admit_started);
            // The sandbox slot outlives the run: it rides in the sandbox's
            // teardown into its removal. Split off before anything reads the
            // grant, so it is never part of the allotment or `reserved`.
            let sandbox_slot = reservation.split_off(hplugin::provider::PoolName::SANDBOX);

            let Some(lapsed) = credentials.lapsed(std::time::SystemTime::now()) else {
                return Ok(Admitted {
                    credentials,
                    execute_guard,
                    reservation,
                    sandbox_slot,
                    queued_ms,
                });
            };
            if round >= MAX_CREDENTIAL_ROUNDS {
                anyhow::bail!(
                    "credential {lapsed} expired while {addr} waited to start, {round} times in a \
                     row: its material lives shorter than the wait for a sandbox and cpu. Give it \
                     a longer lifetime (`ttl`, or what its source reports), or build fewer \
                     targets at once"
                );
            }
            tracing::debug!(
                addr = %addr,
                credential = %lapsed,
                round,
                "credential material lapsed while the run waited to start; acquiring it again"
            );
            drop(reservation);
            drop(sandbox_slot);
            drop(execute_guard);
            resolved = credentials.into_resolved();
        }
    }

    /// Own the teardown of the sandbox `dir` just claimed as `generation`,
    /// carrying the run's `sandbox` pool slot into its removal.
    fn sandbox_teardown(
        &self,
        dir: std::path::PathBuf,
        generation: u64,
        pending: crate::engine::sandbox_cleaner::PendingCounter,
        slot: Option<crate::engine::resource_pool::Reservation>,
        removals: crate::engine::sandbox_cleaner::PendingCounter,
    ) -> crate::engine::sandbox_cleaner::SandboxTeardown {
        let teardown =
            crate::engine::sandbox_cleaner::SandboxTeardown::arm(dir, generation, pending)
                .with_sandbox_slot(slot, removals);
        #[cfg(any(test, feature = "test-support"))]
        let teardown = teardown.with_removal_hold(self.sandbox_removal_hold.clone());
        teardown
    }

    /// Reserve what `addr` declares (`resources`; an undeclared `cpu` is one
    /// CPU) from the engine's pools, parking until it is admitted
    /// (`resource_pool`'s module docs say who is, and in what order); a
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
    ///
    /// `managed`: the run makes a sandbox, so the request carries `sandbox:
    /// 1` on top of what the target declares, granted atomically with it.
    /// The caller splits it off the grant (`Reservation::split_off`).
    pub(crate) async fn admit(
        &self,
        rs: &Arc<RequestState>,
        addr: &Addr,
        resources: &hplugin::driver::Demand,
        managed: bool,
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
        let implicit: &[_] = if managed {
            std::slice::from_ref(&*SANDBOX_IMPLICIT)
        } else {
            &[]
        };
        // Before the acquire, so the sampler counts it as the first change.
        rs.sample_pool(pool);
        let acquire = pool
            .acquire_with(resources, implicit)
            .map_err(|err| anyhow::anyhow!("{addr} {err}"))?;
        tokio::pin!(acquire);
        // Raced against the request's cancellation: a `sandbox` slot comes
        // back only once a removal finishes, which a cancellation does not
        // hurry, so a cancelled run must not wait for one. Dropping the
        // acquire leaves the queue.
        let wait = async {
            match tokio::time::timeout(self.admission_notice, &mut acquire).await {
                Ok(granted) => granted.map_err(Into::into),
                Err(_elapsed) => match acquire.wait_info() {
                    // Granted between the timeout and here: nothing to announce.
                    None => (&mut acquire).await.map_err(Into::into),
                    Some(info) => {
                        let addr_str = addr.format();
                        crate::engine::event::emit_scope(
                            rs,
                            admission_wait_start(addr_str.clone(), &info, pool.removals_pending()),
                            move |_| crate::engine::event::BuildEventKind::AdmissionWaitEnd {
                                addr: addr_str,
                            },
                            async { (&mut acquire).await.map_err(Into::into) },
                        )
                        .await
                    }
                },
            }
        };
        let granted: anyhow::Result<_> = {
            use hcore::hasync::Cancellable as _;
            tokio::select! {
                biased;
                () = rs.ctoken().cancelled() => {
                    return Err(anyhow::Error::new(hplugin::error::CancelledError));
                }
                granted = wait => granted,
            }
        };
        let reservation = granted.with_context(|| format!("admitting {addr}"))?;
        // What the run is allotted: the `sandbox` slot the caller splits off
        // is the engine's, never part of it, so the logs leave it out.
        let allotment = || {
            reservation
                .iter()
                .filter(|(p, _)| p.as_str() != hplugin::provider::PoolName::SANDBOX)
                .collect::<Vec<_>>()
        };
        tracing::debug!(
            addr = %addr,
            granted = ?allotment(),
            how = admitted_kind(reservation.admitted()).as_str(),
            "admitted"
        );
        // Once per addr per request: `execute` is memoised per addr.
        if reservation.clamped() {
            tracing::info!(
                addr = %addr,
                declared = ?resources,
                granted = ?allotment(),
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

/// What [`Engine::admit_with_credentials`] took for one run, in the order it
/// took them.
struct Admitted {
    credentials: crate::engine::credential::HeldCredentials,
    execute_guard: crate::engine::result_lock::ExecuteGuard,
    /// Everything granted but the sandbox slot: released when the run ends.
    reservation: crate::engine::resource_pool::Reservation,
    /// The run's `sandbox` slot, split off the grant: released once its
    /// sandbox has been removed. `None` for a non-managed driver, which
    /// makes no sandbox.
    sandbox_slot: Option<crate::engine::resource_pool::Reservation>,
    /// How long admission took, the wait for a sandbox slot included.
    queued_ms: u64,
}

/// Everything [`Engine::run_in_sandbox`] needs besides the lease.
pub(crate) struct SandboxRun {
    pub driver: Arc<crate::engine::engine::Driver>,
    pub target: Arc<crate::engine::driver::targetdef::TargetDef>,
    pub inputs: Vec<RunInput>,
    pub hashin: String,
    pub scratch: Vec<crate::engine::driver::ScratchMount>,
    /// Acquired before the run took any pool; presented by `run_in_sandbox`.
    pub credentials: crate::engine::credential::HeldCredentials,
    pub deferred: std::collections::BTreeMap<String, crate::engine::driver::DeferredValue>,
    pub wrapper: Option<InteractiveWrapper>,
    pub shell: bool,
    pub options: crate::engine::driver::RunOptions,
    /// What the run is told it may use (`RunRequest.allotment`): a build's
    /// reservation, an invocation's full cpu capacity.
    pub allotment: Option<hplugin::driver::Allotment>,
    /// The run's live log, for a captured build run: its writers go into
    /// `RunRequest.stdout` / `stderr`, teed with the wrapper's sinks, and the
    /// driver is told `log_capture`. `None` for an invocation, a `--shell`
    /// and a credential source (see [`Engine::execute`]).
    pub log: Option<Arc<crate::engine::run_log::RunLog>>,
}

/// What a managed run is admitted with on top of what it declares: one
/// `sandbox` slot. Built once, so admission allocates no pool name per run.
static SANDBOX_IMPLICIT: std::sync::LazyLock<(hplugin::provider::PoolName, u64)> =
    std::sync::LazyLock::new(|| (hplugin::provider::PoolName::sandbox(), 1));

/// The `AdmissionWaitStart` event for a queued request: the first pool it does
/// not fit in, or — fitting everywhere but queued behind others — the first it
/// needs. On a `sandbox` wait it carries `removals_pending`, the pool's count
/// of finished runs whose sandbox has not been removed yet: the wait's cause.
/// A request that fits and is held back for the oldest waiter carries
/// `yielding_to`, that waiter's need.
fn admission_wait_start(
    addr: String,
    info: &crate::engine::resource_pool::WaitInfo,
    removals_pending: u64,
) -> crate::engine::event::BuildEventKind {
    // An unbounded pool (`free: None`) never holds a request back, so it is
    // named only when every pool the request needs is unbounded.
    let pool = info
        .pools
        .iter()
        .find(|p| p.free.is_some_and(|f| p.need > f))
        .or_else(|| info.pools.iter().find(|p| p.free.is_some()))
        .or_else(|| info.pools.first());
    let removals_pending = pool
        .is_some_and(|p| p.pool.as_str() == hplugin::provider::PoolName::SANDBOX)
        .then_some(removals_pending);
    crate::engine::event::BuildEventKind::AdmissionWaitStart {
        addr,
        removals_pending,
        pool: pool.map_or_else(String::new, |p| p.pool.as_str().to_owned()),
        need: pool.map_or(0, |p| p.need),
        free: pool.map_or(0, |p| p.free.unwrap_or(u64::MAX)),
        ahead: u64::try_from(info.ahead).unwrap_or(u64::MAX),
        yielding_to: info
            .yielding_to
            .as_ref()
            .map(|to| hcore::events::YieldingToData {
                pool: to.pool.as_str().to_owned(),
                need: to.need,
            }),
    }
}

/// How the pool admitted a run, as `ExecuteStart` and the logs spell it.
fn admitted_kind(admitted: crate::engine::resource_pool::Admitted) -> hcore::events::AdmittedKind {
    // Not imported as `Admitted`: that is this module's own admission bundle.
    use crate::engine::resource_pool::Admitted as Pool;
    use hcore::events::AdmittedKind as Kind;
    match admitted {
        Pool::Arrival => Kind::Arrival,
        Pool::Head => Kind::Head,
        Pool::Handoff => Kind::Handoff,
        Pool::Pass => Kind::Pass,
    }
}

/// What a run is told of its reservation: `cpu` is what it was granted, and
/// every other pool what it was granted ([`render_allotment`]).
pub(crate) fn allotment_of(
    reservation: &crate::engine::resource_pool::Reservation,
    unit_of: impl Fn(&str) -> Option<hcore::capacity::Unit>,
) -> hplugin::driver::Allotment {
    use hplugin::provider::PoolName;
    render_allotment(
        reservation.amount(PoolName::CPU),
        reservation.iter(),
        unit_of,
    )
}

/// What an entrypoint run is told, having reserved nothing: `cpu` is the
/// whole `cpu_capacity`, and every other pool its declared amount
/// ([`render_allotment`]).
pub(crate) fn entrypoint_allotment(
    cpu_capacity: u64,
    demand: &hplugin::driver::Demand,
    unit_of: impl Fn(&str) -> Option<hcore::capacity::Unit>,
) -> hplugin::driver::Allotment {
    render_allotment(cpu_capacity, demand.iter(), unit_of)
}

/// The `${resource:<pool>}` values: `cpu` always, `cpu_micros` in whole CPUs
/// (rounded up, at least 1, so a zero-cost grant is told 1), and every other
/// pool of `amounts` above 0, each rendered by its pool's unit (`unit_of`,
/// the engine's pools; else the name's default). A `cpu` in `amounts` is
/// ignored: `cpu_micros` is the cpu.
fn render_allotment<'a>(
    cpu_micros: u64,
    amounts: impl Iterator<Item = (&'a hplugin::provider::PoolName, u64)>,
    unit_of: impl Fn(&str) -> Option<hcore::capacity::Unit>,
) -> hplugin::driver::Allotment {
    use hcore::capacity::Unit;
    use hplugin::provider::PoolName;
    let cpu = (
        PoolName::CPU.to_owned(),
        Unit::Cpu.render(cpu_micros.max(1)),
    );
    let rest = amounts
        .filter(|(p, n)| p.as_str() != PoolName::CPU && *n > 0)
        .map(|(p, n)| {
            let unit = unit_of(p.as_str()).unwrap_or_else(|| Unit::default_for(p.as_str()));
            (p.as_str().to_owned(), unit.render(n))
        });
    hplugin::driver::Allotment::new(std::iter::once(cpu).chain(rest))
}

/// The `ExecuteStart` event: what the target declared (only its declared
/// keys, in each pool's unit's base) and what it was granted, after
/// clamping, how long admission took and how the grant came about.
/// `reserved` always carries `cpu`, 0 on a zero-cost grant. `output_groups`
/// is the target's declared group names, for a consumer to show before the
/// run has written any.
pub(crate) fn execute_start(
    addr: String,
    driver: String,
    cache: bool,
    declared: &hplugin::driver::Demand,
    reservation: &crate::engine::resource_pool::Reservation,
    queued_ms: u64,
    output_groups: Vec<String>,
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
        admitted: Some(admitted_kind(reservation.admitted())),
        output_groups,
        log_not_streamed: false,
    }
}

/// A sink borrowed for a shorter while: the request may also hold the run
/// log's writers, which live only as long as the run.
fn shorten<'s>(
    w: &'s mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + '_),
) -> &'s mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin + 's) {
    w
}

/// The `Log` artifact a successful run's live log becomes: packed into the
/// cache entry, never passed through, and like every `Log` kept out of the
/// output hashes.
fn live_log_artifact(path: &std::path::Path) -> anyhow::Result<OutputArtifact> {
    use crate::engine::driver::outputartifact::{Content, ContentFile, Type};
    Ok(OutputArtifact {
        group: String::new(),
        name: "log.txt".to_owned(),
        r#type: Type::Log,
        content: Content::File(ContentFile {
            source_path: path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("the run log's path is not valid UTF-8"))?
                .to_owned(),
            out_path: "log.txt".to_owned(),
            x: false,
            passthrough: false,
        }),
        hashout: String::new(),
    })
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
    use crate::engine::resource_pool::Admitted as PoolAdmitted;
    use crate::engine::resource_pool::Limit;
    use crate::engine::resource_pool::ResourcePool;
    use futures::FutureExt as _;
    use hcore::capacity::Unit;
    use hcore::events::AdmittedKind;
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
        let ev = execute_start(
            "//p:t".into(),
            "exec".into(),
            true,
            &declared,
            &r,
            7,
            Vec::new(),
        );
        let BuildEventKind::ExecuteStart {
            declared,
            reserved,
            queued_ms,
            admitted,
            ..
        } = ev
        else {
            panic!("wrong variant: {ev:?}");
        };
        assert_eq!(queued_ms, Some(7));
        assert_eq!(admitted, Some(AdmittedKind::Arrival), "nobody was waiting");
        assert_eq!(
            declared,
            Some(std::collections::BTreeMap::from([("cpu".into(), 16 * CPU)]))
        );
        assert_eq!(
            reserved,
            Some(std::collections::BTreeMap::from([("cpu".into(), 4 * CPU)]))
        );
    }

    /// `ExecuteStart.admitted` is what the pool says of that grant: a run
    /// that queued and started as the oldest waiter says `head`, not the
    /// `arrival` of the run it waited for.
    #[test]
    fn execute_start_says_how_the_run_was_admitted() {
        let one = req(&[("cpu", CPU)]);
        let pool = ResourcePool::new([(
            PoolName::try_from("cpu").expect("pool"),
            Limit::Bounded(CPU),
        )]);
        let admitted = |r: &crate::engine::resource_pool::Reservation| match execute_start(
            "//p:t".into(),
            "exec".into(),
            true,
            &one,
            r,
            0,
            Vec::new(),
        ) {
            BuildEventKind::ExecuteStart { admitted, .. } => admitted,
            other => panic!("wrong variant: {other:?}"),
        };
        let first = pool
            .acquire(&one)
            .expect("acquire")
            .now_or_never()
            .expect("granted at once")
            .expect("granted");
        assert_eq!(admitted(&first), Some(AdmittedKind::Arrival));

        let mut waiter = pool.acquire(&one).expect("acquire");
        assert!((&mut waiter).now_or_never().is_none(), "the pool is full");
        drop(first);
        let second = waiter
            .now_or_never()
            .expect("granted on the release")
            .expect("granted");
        assert_eq!(admitted(&second), Some(AdmittedKind::Head));

        for (from, to) in [
            (PoolAdmitted::Arrival, AdmittedKind::Arrival),
            (PoolAdmitted::Head, AdmittedKind::Head),
            (PoolAdmitted::Handoff, AdmittedKind::Handoff),
            (PoolAdmitted::Pass, AdmittedKind::Pass),
        ] {
            assert_eq!(admitted_kind(from), to);
        }
    }

    /// `AdmissionWaitStart` carries the pool's `yielding_to` — the oldest
    /// waiter's pool and need, which need not be the pool the event names —
    /// and nothing when the waiter is not held back for it.
    #[test]
    fn admission_wait_start_carries_what_the_waiter_yields_to() {
        use crate::engine::resource_pool::{PoolNeed, WaitInfo, YieldingTo};
        let name = |n: &str| PoolName::try_from(n).expect("pool");
        let info = |yielding_to| WaitInfo {
            ahead: 2,
            pools: vec![PoolNeed {
                pool: name("cpu"),
                need: CPU,
                free: Some(3 * CPU),
            }],
            since: std::time::Instant::now(),
            yielding_to,
        };
        let held_back = info(Some(YieldingTo {
            pool: name("memory"),
            need: 8 << 30,
        }));
        let BuildEventKind::AdmissionWaitStart {
            pool,
            need,
            free,
            ahead,
            removals_pending,
            yielding_to,
            ..
        } = admission_wait_start("//p:t".into(), &held_back, 0)
        else {
            panic!("wrong variant");
        };
        assert_eq!((pool.as_str(), need, free, ahead), ("cpu", CPU, 3 * CPU, 2));
        assert_eq!(removals_pending, None, "not a sandbox wait");
        assert_eq!(
            yielding_to,
            Some(hcore::events::YieldingToData {
                pool: "memory".into(),
                need: 8 << 30,
            })
        );

        let BuildEventKind::AdmissionWaitStart { yielding_to, .. } =
            admission_wait_start("//p:t".into(), &info(None), 0)
        else {
            panic!("wrong variant");
        };
        assert_eq!(yielding_to, None);
    }

    /// A zero-cost target reserves nothing: `reserved` still carries `cpu`
    /// (0), and the run is told one CPU.
    #[test]
    fn a_zero_cost_grant_reserves_zero_cpu_and_is_told_one() {
        let declared = req(&[("cpu", 0)]);
        let r = grant(&[("cpu", Limit::Bounded(4 * CPU))], &declared);
        let BuildEventKind::ExecuteStart { reserved, .. } = execute_start(
            "//p:t".into(),
            "exec".into(),
            true,
            &declared,
            &r,
            0,
            Vec::new(),
        ) else {
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

    /// An entrypoint reserves nothing: it is told the whole cpu capacity in
    /// whole CPUs (at least 1), whatever cpu it declared, and every other pool
    /// it declared above 0, unclamped, each rendered by its unit.
    #[test]
    fn entrypoint_allotment_is_the_cpu_capacity_and_every_declared_pool() {
        let declared = req(&[
            ("cpu", CPU),
            ("gpu", 1),
            ("memory", 8 << 30),
            ("scratch", 0),
        ]);
        let a = entrypoint_allotment(4 * CPU, &declared, units);
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            [("cpu", "4"), ("gpu", "1"), ("memory", "8589934592")],
            "the capacity, not the declared cpu; a pool declared at 0 is left out"
        );

        let a = entrypoint_allotment(2_500_000, &req(&[]), units);
        assert_eq!(a.iter().collect::<Vec<_>>(), [("cpu", "3")], "rounded up");
        let a = entrypoint_allotment(0, &req(&[]), units);
        assert_eq!(a.iter().collect::<Vec<_>>(), [("cpu", "1")], "at least 1");
    }
}
