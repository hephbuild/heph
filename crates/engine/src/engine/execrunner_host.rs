//! The engine half of the exec-runner seam: turn a runner *address* into a
//! rewritten spec.
//!
//! `hexecrunner` cannot do this itself. Reading a runner target's output is
//! `Engine::result_addr`, and the engine already depends on that crate through
//! every driver — naming it there would close a dependency cycle. So the
//! resolver is installed into the crate's process-global slot at engine
//! construction, the same way `hproc`'s supervisor sink is.

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use hcore::hasync::Cancellable;
use hexecrunner::config::{RUNNER_JSON, RunnerConfig};
use hexecrunner::registry::RunnerRegistry;
use hexecrunner::{RunnerHost, SpecRewrite};
use hmodel::htaddr::Addr;
use hplugin::eresult::EResult;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

/// Resolves runner addresses against the live engine and dispatches through the
/// registry.
pub struct EngineRunnerHost {
    /// Weak so the host, which lives in a process-global, cannot keep the
    /// engine alive past its own teardown.
    engine: Weak<Engine>,
    registry: Arc<RunnerRegistry>,
    /// Parsed configs, keyed on the runner target's **hashout**.
    ///
    /// The hashout is content-addressed, so a given hashout always parses to
    /// the same config — the cache is sound process-wide and bounded by the
    /// number of distinct runner configurations, which is tiny.
    ///
    /// Note what is *not* cached: the `result_addr` call itself. Memoizing that
    /// on the address would let a waiter skip the `DepDag` edge insert, which
    /// is the engine's only synchronous cycle check — a real dependency cycle
    /// would then hang in the memoizer instead of surfacing as a typed
    /// `CycleError`. `ProviderExecutor::result` documents this; the rule is
    /// "cache the derived value, never the resolution".
    parsed: Mutex<HashMap<String, RunnerConfig>>,
}

impl EngineRunnerHost {
    pub fn new(engine: Weak<Engine>, registry: Arc<RunnerRegistry>) -> Self {
        Self {
            engine,
            registry,
            parsed: Mutex::new(HashMap::new()),
        }
    }

    /// The live request this exec belongs to.
    ///
    /// `Engine::requests` holds only *root* request states — `with_parent`
    /// children share the same data and are never registered — so what comes
    /// back is root-scoped. That is fine here and worth being explicit about:
    /// the dep edge this resolution registers is `root → runner`, not
    /// `consumer → runner`. The consumer's edge already exists, because the
    /// runner is one of its hashed inputs; that Input is also what makes a
    /// self-referential runner a `CycleError` rather than a deadlock. This
    /// lookup is a read of an already-resolved result, not the thing that makes
    /// cycles safe.
    fn request(&self, request_id: &str) -> anyhow::Result<(Arc<Engine>, Arc<RequestState>)> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("exec runner: engine is gone"))?;
        let rs = {
            let requests = engine.requests.lock().map_err(|_poisoned| {
                anyhow::anyhow!("exec runner: request registry lock poisoned")
            })?;
            requests.get(request_id).and_then(Weak::upgrade)
        };
        let rs = rs.ok_or_else(|| {
            anyhow::anyhow!(
                "exec runner: request '{request_id}' is no longer live. The runner is resolved \
                 while the target that named it is executing, so this means the request was \
                 dropped mid-execution."
            )
        })?;
        Ok((engine, rs))
    }

    /// Read `runner.json` out of a resolved runner target's artifacts.
    ///
    /// The contract is exactly one file, named [`RUNNER_JSON`]. Enforced rather
    /// than "take the first": a driver that emits two files, or none, is a bug
    /// the runner's author needs told about by address, not something to guess
    /// past.
    fn read_config(res: &EResult, addr: &str) -> anyhow::Result<Vec<u8>> {
        use std::io::Read as _;

        let mut found: Option<Vec<u8>> = None;
        let mut seen: Vec<String> = Vec::new();
        for artifact in &res.artifacts {
            let walk = artifact
                .walk()
                .map_err(|e| anyhow::anyhow!("runner {addr}: read outputs: {e}"))?;
            for entry in walk {
                let entry =
                    entry.map_err(|e| anyhow::anyhow!("runner {addr}: read outputs: {e}"))?;
                let name = entry.path.to_string_lossy().to_string();
                let hcore::hartifactcontent::WalkEntryKind::File { mut data, .. } = entry.kind
                else {
                    continue;
                };
                seen.push(name.clone());
                if !name.ends_with(RUNNER_JSON) {
                    continue;
                }
                if found.is_some() {
                    anyhow::bail!(
                        "runner {addr}: produced more than one {RUNNER_JSON}; a runner target's \
                         output must be exactly that one file"
                    );
                }
                let mut buf = Vec::new();
                data.read_to_end(&mut buf)
                    .map_err(|e| anyhow::anyhow!("runner {addr}: read {RUNNER_JSON}: {e}"))?;
                found = Some(buf);
            }
        }

        found.ok_or_else(|| {
            anyhow::anyhow!(
                "runner {addr}: produced no {RUNNER_JSON}. A runner target's output must be a \
                 single {RUNNER_JSON} file; this target produced: {}",
                if seen.is_empty() {
                    "nothing".to_string()
                } else {
                    seen.join(", ")
                }
            )
        })
    }

    /// Resolve the runner target and return its parsed config.
    ///
    /// The `Arc<EResult>` is dropped before this returns, and that is
    /// load-bearing rather than tidy: a resolved result carries a riding
    /// `flock` read guard on its address's result lock. Holding one for the
    /// lifetime of a spawned child would pin the runner target for every
    /// in-flight target at once, and any attempt to re-execute it would block
    /// on the write lock — a hang whose only symptom is an ENOENT on a lock
    /// file, with nothing pointing at locks.
    async fn resolve(&self, request_id: &str, addr: &Addr) -> anyhow::Result<RunnerConfig> {
        let formatted = addr.format();
        let (engine, rs) = self.request(request_id)?;

        // Always resolved, never memoized here — see `parsed`.
        let res = engine
            // `All`, not `None`: the config *is* the output, so a matcher that
            // returns no artifacts leaves nothing to read.
            .result_addr(rs, addr, OutputMatcher::All, &ResultOptions::default())
            .await
            .map_err(|e| anyhow::anyhow!("runner {formatted}: resolve: {e:#}"))?;

        let hashout = res
            .artifacts_meta
            .first()
            .map(|m| m.hashout.clone())
            .unwrap_or_default();

        if !hashout.is_empty()
            && let Ok(cache) = self.parsed.lock()
            && let Some(hit) = cache.get(&hashout)
        {
            return Ok(hit.clone());
        }

        let bytes = Self::read_config(&res, &formatted)?;
        // Drop the read guard before anything slow (a session start) runs.
        drop(res);

        let cfg = RunnerConfig::parse(&bytes, &formatted)?;
        self.registry.validate(&formatted, &cfg)?;

        if !hashout.is_empty()
            && let Ok(mut cache) = self.parsed.lock()
        {
            cache.insert(hashout, cfg.clone());
        }
        Ok(cfg)
    }
}

#[async_trait::async_trait]
impl RunnerHost for EngineRunnerHost {
    fn owns(&self, request_id: &str) -> bool {
        self.engine
            .upgrade()
            .and_then(|e| e.requests.lock().ok().map(|r| r.contains_key(request_id)))
            .unwrap_or(false)
    }

    fn alive(&self) -> bool {
        self.engine.strong_count() > 0
    }

    async fn prepare(
        &self,
        request_id: &str,
        addr: &Addr,
        rewrite: SpecRewrite,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<SpecRewrite> {
        let cfg = self.resolve(request_id, addr).await?;
        self.registry
            .prepare(&addr.format(), &cfg, rewrite, ctoken)
            .await
    }
}
