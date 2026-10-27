//! `Engine::invoke`: the one entry point an `Entrypoint`-capable target runs
//! through (the tool-shims spec, D1, D30).
//!
//! An entrypoint is *invoked*, not built: it runs in the caller's cwd with the
//! caller's environment and stdio, is never cached, never sandboxed, and never
//! anyone's input. `invoke` resolves it, refuses undeclared args before any dep
//! builds, builds its closure, drains the cache writes those builds queued,
//! stages and holds every entry the closure points at, and hands the driver
//! the private [`Invocation`] token to shape the command. It spawns nothing:
//! the binary owns spawn, wait, terminal and exit, so nothing here touches a
//! terminal or a signal disposition in a library tests link in-process.
//!
//! What is deliberately absent: the per-address execute lock (I2 — no sandbox
//! to protect, so any number of invocations run at once), and every cache
//! write of the entrypoint's own (I6).

use crate::engine::Engine;
use crate::engine::driver::entrypoint::{
    Caller, EntrypointDriver, Invocation, PrepareRequest, PreparedCommand, StagedInput,
};
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use anyhow::Context;
use hdriver_support::stage::{StageHolds, staged_files};
use hmodel::htaddr::Addr;
use hplugin::eresult::EResult;
use std::sync::Arc;

/// What the CLI asks for: an address, and everything about the caller.
#[derive(Debug, Clone)]
pub struct InvokeRequest {
    pub addr: Addr,
    pub caller: Caller,
    /// `heph run --shell`: an interactive shell in the tool's world.
    pub shell: bool,
}

/// A command ready to spawn, and everything that must stay alive while it
/// runs: the holds on its staged closure and the read locks riding on its
/// deps' results. Dropping it releases them all; the binary drops it once the
/// process has exited.
#[must_use = "dropping the invocation releases the holds its process relies on"]
pub struct PreparedInvocation {
    pub command: PreparedCommand,
    holds: StageHolds,
    /// Each dep's result carries a read lock on its cache revision, so a
    /// `--force` rebuild of the same revision waits for the tool.
    _results: Vec<Arc<EResult>>,
}

impl std::fmt::Debug for PreparedInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedInvocation")
            .field("command", &self.command)
            .field("holds", &self.holds.len())
            .finish_non_exhaustive()
    }
}

impl PreparedInvocation {
    /// How many distinct stage entries are held.
    pub fn held_entries(&self) -> usize {
        self.holds.len()
    }
}

/// An entrypoint named as some target's input (I8).
#[derive(Debug, thiserror::Error)]
#[error(
    "{entrypoint} is an exec.entrypoint target and cannot be an input{}: an entrypoint runs in \
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
/// selection, or anything else that would execute it (I8).
#[derive(Debug, thiserror::Error)]
#[error(
    "{addr} is an exec.entrypoint target: it is invoked on its own with `heph run {addr}`, never \
     built, and never as part of a multi-target selection"
)]
pub struct EntrypointNotBuildable {
    pub addr: Addr,
}

/// Trailing args for an entrypoint that did not declare `args = True` (D18).
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
    /// Whether the driver registered as `name` has the `Entrypoint` capability.
    pub fn is_entrypoint_driver(&self, name: &str) -> bool {
        self.drivers_by_name
            .get(name)
            .is_some_and(|d| d.is_entrypoint())
    }

    /// Whether `addr` is an entrypoint. Resolves its spec only.
    pub async fn is_entrypoint(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<bool> {
        let spec = Arc::clone(self).get_spec(rs.clone(), addr).await?;
        Ok(self.is_entrypoint_driver(&spec.driver))
    }

    /// Refuse an entrypoint reached by a multi-target selection, before
    /// anything of it builds.
    pub(crate) async fn refuse_entrypoint_in_selection(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<()> {
        if self.is_entrypoint(rs, addr).await? {
            return Err(EntrypointNotBuildable { addr: addr.clone() }.into());
        }
        Ok(())
    }

    fn entrypoint_driver(
        &self,
        driver: &str,
        addr: &Addr,
    ) -> anyhow::Result<Arc<dyn EntrypointDriver>> {
        self.drivers_by_name
            .get(driver)
            .with_context(|| format!("driver not found: {driver}"))?
            .entrypoint
            .clone()
            .with_context(|| {
                format!("{addr} uses the `{driver}` driver, which is not an entrypoint driver")
            })
    }

    /// Prepare `req.addr` to run: see the module docs.
    pub async fn invoke(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        req: InvokeRequest,
    ) -> anyhow::Result<PreparedInvocation> {
        let addr = &req.addr;
        rs.track_dep(addr).map_err(anyhow::Error::new)?;
        let rs = rs.with_parent(addr.clone());

        let spec = Arc::clone(&self)
            .get_spec_no_track(rs.clone(), addr)
            .await?;
        let driver = self.entrypoint_driver(&spec.driver, addr)?;
        let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;
        let target = Arc::clone(&def.target_def);

        // After the spec resolves, before any dep builds: a flagless release
        // script must not swallow a mistyped `--dry-run` and run for real.
        if !req.caller.args.is_empty() && !driver.accepts_args(&target) {
            return Err(UndeclaredArgs {
                addr: addr.clone(),
                count: req.caller.args.len(),
            }
            .into());
        }
        if req.shell {
            anyhow::bail!("`--shell` is not supported yet on the exec.entrypoint target {addr}");
        }

        let deferred_refs = self.deferred_refs(&spec.spec, &spec.driver)?;
        if let Some(r) = deferred_refs.iter().find(|r| r.needs_sandbox()) {
            anyhow::bail!(
                "{addr}: {:?} names a `${{src://…}}`, which needs a sandbox path; an \
                 exec.entrypoint has no sandbox. Use `$SRC_<group>` with a dep instead",
                r.raw
            );
        }

        // The closure. Expanding the inputs is what refuses an entrypoint
        // among them (I8); linking selects the runtime ones and their outputs.
        let expanded = Arc::clone(&self)
            .expanded_inputs_for(rs.clone(), addr)
            .await?;
        let linked = Arc::clone(&self)
            .link(rs.clone(), Arc::clone(&target))
            .await?;

        // Build everything: the runtime inputs for their artifacts, and every
        // other input (hash-only deps, `${read://…}` producers) because the
        // invocation depends on them having been built — they are just not
        // wired in.
        let fail_fast = rs.fail_fast();
        let runtime = linked.inputs.iter().map(|input| {
            let engine = Arc::clone(&self);
            let rs = rs.clone();
            async move {
                let res = engine
                    .result_addr(
                        rs,
                        &input.target.addr,
                        OutputMatcher::Exact(input.output_names.clone()),
                        &ResultOptions::default(),
                    )
                    .await?;
                anyhow::Ok((input, res))
            }
        });
        let hash_only = expanded.iter().filter(|i| !i.runtime).map(|input| {
            let engine = Arc::clone(&self);
            let rs = rs.clone();
            async move {
                engine
                    .result_addr(
                        rs,
                        &input.r#ref.r#ref,
                        OutputMatcher::All,
                        &ResultOptions::default(),
                    )
                    .await?;
                anyhow::Ok(())
            }
        });
        let (runtime, hash_only) = futures::future::join(
            crate::engine::fanout::join_all_failable(runtime, fail_fast),
            crate::engine::fanout::join_all_failable(hash_only, fail_fast),
        )
        .await;
        let built = runtime?;
        hash_only?;

        // The process may exit as soon as the tool does, without the teardown
        // that would otherwise flush what these builds queued: drain it now, so
        // a cold call leaves its deps cached (D43).
        self.sqlite_writes.drain_queued_writes().await;
        rs.bg_pending().wait_drained().await;

        // `${read://…}` in the entrypoint's own options, through the same
        // resolver `execute` uses (D46).
        let deferred = self
            .resolve_deferred(&rs, &target, &deferred_refs)
            .await
            .with_context(|| format!("resolve deferred values for {addr}"))?;

        // Stage and hold every entry the closure points at (I4, D27).
        let stage_root = self.home.join("stage");
        let mut holds = StageHolds::new();
        let mut staged: Vec<StagedInput> = Vec::with_capacity(built.len());
        let mut results = Vec::with_capacity(built.len());
        for (input, res) in built {
            let source = input.target.addr.format();
            let mut files = Vec::new();
            for artifact in &res.artifacts {
                let entry = holds
                    .hold_content(artifact, &stage_root, &source, rs.ctoken())
                    .await
                    .with_context(|| format!("stage {source} for {addr}"))?;
                files.extend(
                    staged_files(&entry, &input.filters)
                        .with_context(|| format!("list staged {source}"))?,
                );
            }
            files.sort();
            staged.push(StagedInput {
                origin_id: input.origin_id.clone(),
                files,
            });
            results.push(res);
        }

        let token = Invocation::for_engine_invoke();
        let links = driver.toolbin(&token, &target, &staged)?;
        let toolbin = if links.is_empty() {
            None
        } else {
            Some(
                holds
                    .hold_toolbin(&stage_root, &links, rs.ctoken())
                    .await
                    .with_context(|| format!("toolbin for {addr}"))?,
            )
        };

        let command = driver
            .prepare(
                &token,
                PrepareRequest {
                    target: &target,
                    inputs: &staged,
                    toolbin: toolbin.as_deref(),
                    deferred: &deferred,
                    caller: &req.caller,
                    root: &self.cfg.root,
                    shell_init: None,
                },
            )
            .with_context(|| format!("prepare {addr}"))?;

        Ok(PreparedInvocation {
            command,
            holds,
            _results: results,
        })
    }
}
