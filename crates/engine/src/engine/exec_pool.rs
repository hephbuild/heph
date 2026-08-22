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
use tokio::sync::Mutex;

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
        let cell = {
            let mut entries = self.entries.lock().await;
            Arc::clone(entries.entry(key.clone()).or_default())
        };

        let res = cell
            .get_or_try_init(|| async { runner.open(req, ctoken).await })
            .await;

        match res {
            Ok(s) => Ok(Arc::clone(s)),
            Err(e) => {
                self.entries.lock().await.remove(&key);
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
