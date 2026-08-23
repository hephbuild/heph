//! Open exec sessions, keyed by content.
//!
//! Not `hmemoizer`. That gives single-flight and panic-guarding and nothing else
//! this needs: it has no TTL, no refcount, it memoizes *errors* for the process
//! lifetime, and every instance in the tree is request-scoped. A pool that
//! cached "devenv.nix doesn't evaluate" forever would keep failing after the
//! user fixed it. So the single-flight is borrowed and the rest is here.

use hcore::hasync::Cancellable;
use hexec_runner::{ExecRunner, ExecSession, OpenRequest};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// One key's session, or the in-flight open of it. Named because the nesting
/// is load-bearing and unreadable inline: the outer `Mutex` guards the map, the
/// `OnceCell` collapses concurrent opens of the *same* key into one call, and
/// the `Arc` lets a waiter hold the cell without holding the map lock across a
/// 30-second `open`.
type SessionCell = Arc<tokio::sync::OnceCell<Arc<dyn ExecSession>>>;

/// Sessions by key, with the open of a cold key single-flighted.
#[derive(Default)]
pub struct ExecSessionPool {
    entries: Mutex<HashMap<String, SessionCell>>,
}

impl ExecSessionPool {
    /// Tear every open session down, and forget them.
    ///
    /// **Idempotent**, and must be called explicitly rather than left to
    /// `Drop`. Two reasons, both learned the hard way:
    ///
    /// 1. `Drop` runs only if the last `Arc<Engine>` is actually released. Any
    ///    retained handle — a spawned task, a diagnostic, a test holding one —
    ///    silently disables teardown, and silence is the failure mode here.
    /// 2. heph's second-Ctrl-C path calls `std::process::exit`, which **runs no
    ///    destructors at all**. That is precisely the moment a leaked `docker
    ///    run -d` container or devenv shell matters most.
    ///
    /// `Drop` still calls this, as a backstop for the ordinary path.
    /// Orderly teardown: ask each session to close, then run its synchronous
    /// job. Use this wherever there is a runtime to await on.
    ///
    /// A session living inside a plugin can only be closed by *talking* to the
    /// plugin, which is async — so [`Self::teardown_all`] alone cannot reach
    /// it. That path stays for hard abort, where nothing async runs and a
    /// plugin-spawned process is reaped by the host supervisor that tracked it.
    pub async fn close_all(&self) {
        let sessions: Vec<(String, Arc<dyn ExecSession>)> = {
            let mut entries = match self.entries.lock() {
                Ok(e) => e,
                Err(poisoned) => poisoned.into_inner(),
            };
            entries
                .drain()
                .filter_map(|(k, cell)| cell.get().map(|s| (k, Arc::clone(s))))
                .collect()
        };
        for (key, session) in sessions {
            if let Err(e) = session.close().await {
                tracing::warn!(key = %key, error = %format!("{e:#}"), "exec session close failed");
            }
            let Some(job) = session.teardown() else {
                continue;
            };
            if let Err(e) = job() {
                tracing::warn!(key = %key, error = %format!("{e:#}"), "exec session teardown failed");
            }
        }
    }

    pub fn teardown_all(&self) {
        let mut entries = match self.entries.lock() {
            Ok(e) => e,
            // A panic while the map was locked must not stop cleanup: the data
            // is a plain `HashMap` and is still structurally sound.
            Err(poisoned) => poisoned.into_inner(),
        };
        for (key, cell) in entries.drain() {
            let Some(session) = cell.get() else { continue };
            let Some(job) = session.teardown() else {
                continue;
            };
            if let Err(e) = job() {
                // A failed teardown is a leak the user can act on — a container
                // still running, a shell still holding a lock — so it is said
                // out loud rather than swallowed.
                tracing::warn!(key = %key, error = %format!("{e:#}"), "exec session teardown failed");
            }
        }
    }
}

impl Drop for ExecSessionPool {
    fn drop(&mut self) {
        self.teardown_all();
    }
}

impl std::fmt::Debug for ExecSessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecSessionPool").finish_non_exhaustive()
    }
}

impl ExecSessionPool {
    /// The session for `req.key`, opening it once if this is the first ask.
    ///
    /// A failed open is **not** retained: the cell is removed so the next
    /// request tries again. That matters most in a long-lived process, where
    /// the alternative is a user fixing their environment and still being told
    /// it is broken until they restart.
    pub async fn get_or_open(
        &self,
        runner: &dyn ExecRunner,
        req: OpenRequest,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let key = req.key.clone();
        // The map lock is taken and released around the clone — never held
        // across the `open` below, which can be tens of seconds for a cold
        // environment and would serialize every other key behind it.
        let cell = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(entries.entry(key.clone()).or_default())
        };

        let res = cell
            .get_or_try_init(|| async { runner.open(req, ctoken).await })
            .await;

        match res {
            Ok(s) => Ok(Arc::clone(s)),
            Err(e) => {
                self.entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
                Err(e)
            }
        }
    }
}

use crate::engine::Engine;
use crate::engine::request_state::RequestState;
use anyhow::Context as _;
use hmodel::htaddr::Addr;

/// Flatten a runner target's artifacts into the files its runner will parse.
///
/// Small by construction — an environment description, not a build output — so
/// this reads them fully into memory rather than streaming.
fn read_runner_artifacts(
    res: &crate::engine::EResult,
) -> anyhow::Result<Vec<hexec_runner::RunnerArtifact>> {
    use hcore::hartifactcontent::WalkEntryKind;
    use std::io::Read as _;

    let mut out = Vec::new();
    for content in &res.artifacts {
        for entry in content.walk()? {
            let entry = entry?;
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)?;
                out.push(hexec_runner::RunnerArtifact {
                    path: entry.path.to_string_lossy().into_owned(),
                    bytes,
                });
            }
        }
    }
    // Stable order: a runner must not see its own inputs shuffled between runs.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

impl Engine {
    /// The session a target's processes are created in.
    ///
    /// `None` runner ⇒ `LocalSession`, which is the identity transform and
    /// contributes nothing to any cache key — the pre-runner behaviour, exactly.
    pub(crate) async fn exec_session_for(
        self: &Arc<Self>,
        rs: &Arc<RequestState>,
        runner: Option<&Addr>,
    ) -> anyhow::Result<Arc<dyn ExecSession>> {
        let Some(runner_addr) = runner else {
            return Ok(Arc::new(hexec_runner::LocalSession::new()));
        };

        // The runner's own result: its artifacts are the environment's
        // description, and their hashouts are its identity.
        let res = Arc::clone(self)
            .result_addr(
                Arc::clone(rs),
                runner_addr,
                crate::engine::OutputMatcher::All,
                &crate::engine::ResultOptions::default(),
            )
            .await
            .with_context(|| format!("resolving runner {}", runner_addr.format()))?;

        // Which runner implementation? The runner target's own driver name —
        // a plugin exporting a `devenv` driver exports a `devenv` runner beside
        // it, so one name covers both halves.
        let spec = Arc::clone(self)
            .get_spec(Arc::clone(rs), runner_addr)
            .await
            .with_context(|| format!("resolving runner spec {}", runner_addr.format()))?;
        let impl_name = spec.spec.driver.clone();
        let runner_impl = self.exec_runners.get(&impl_name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "no exec runner registered for `{impl_name}`, which is the driver of runner target \
                 {}. A runner target's driver names the runner implementation that reads its \
                 artifact.",
                runner_addr.format(),
            )
        })?;

        // Keyed by content, so two runner targets with byte-identical artifacts
        // are one environment and share a session.
        let mut key_parts: Vec<&str> = res
            .artifacts_meta
            .iter()
            .map(|m| m.hashout.as_str())
            .collect();
        key_parts.sort_unstable();
        let key = format!("{impl_name}:{}", key_parts.join("+"));

        let artifacts = read_runner_artifacts(&res)
            .with_context(|| format!("reading runner artifacts {}", runner_addr.format()))?;

        self.exec_sessions
            .get_or_open(
                runner_impl.as_ref(),
                OpenRequest {
                    key,
                    runner_addr: runner_addr.format(),
                    artifacts,
                },
                rs.ctoken(),
            )
            .await
            .with_context(|| format!("opening runner {}", runner_addr.format()))
    }
}
