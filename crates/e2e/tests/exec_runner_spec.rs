#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The exec-runner **ABI lane**, through the real engine.
//!
//! `crates/plugin-sdk` covers the lane at the seam itself: a driver's
//! `open_session` / `prepare_spec` / `close_session` crossing a real stabby
//! vtable. What that cannot show is the half above it — that the engine
//! resolves a runner target to its driver, opens one session for the
//! environment, and routes every target's actual process through it.
//!
//! So these run a real BUILD graph. The runner is a `DriverExecRunner` over a
//! `ManagedDriver`, which is exactly what a loaded cdylib plugin becomes on the
//! host side.
//!
//! See `docs/EXEC_RUNNERS.md`.

mod common;

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::Workspace;
use heph::htaddr::parse_addr;

/// A runner that holds "one shell" open and routes every spawn through it —
/// the shape of a devenv session runner, without needing devenv.
///
/// Note what is absent: no `parse`, no `run`, no schema. A runner is its own
/// component kind, so it implements three methods and nothing else.
///
/// It rewrites each spawn to `env FROM_RUNNER=… SEQ=<n> <program> <args>`, so a
/// target's own `run` can observe both that the session reached it and that the
/// runner was consulted for *this* spawn rather than once for the environment.
struct MuxRunner {
    opens: Arc<AtomicUsize>,
    prepares: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    /// Per-session environment, held **inside the runner**.
    ///
    /// Under this lane the host does not merge `base_env` for anyone: the
    /// runner owns the whole transformation, because the runner is what starts
    /// the process. `ExecSession::base_env` is what the host *reports*, not
    /// what it applies.
    envs: std::sync::Mutex<std::collections::HashMap<String, Vec<(OsString, OsString)>>>,
}

#[async_trait::async_trait]
impl heph::engine::exec_runner::ExecRunnerPlugin for MuxRunner {
    async fn open_session(
        &self,
        req: heph::engine::exec_runner::OpenRequest,
        _ct: &(dyn heph::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::exec_runner::OpenedSession> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        // Derived from the artifact, not invented — the runner target's bytes
        // are what the consumer's key already covers.
        let from_artifact = req
            .artifacts
            .first()
            .map(|a| String::from_utf8_lossy(&a.bytes).trim().to_string())
            .unwrap_or_default();
        let base_env = vec![(
            OsString::from("FROM_RUNNER"),
            OsString::from(from_artifact.clone()),
        )];
        let session_id = format!("mux-{}", req.key);
        self.envs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session_id.clone(), base_env.clone());
        Ok(heph::engine::exec_runner::OpenedSession {
            session_id,
            caps: heph::engine::exec_runner::SessionCaps {
                pty: true,
                max_concurrent: Some(4),
                identity: heph::engine::exec_runner::Identity::Pinned {
                    by: format!("{} ({})", req.runner_addr, req.key),
                },
            },
            description: heph::engine::exec_runner::SessionDescription {
                runner: req.runner_addr.clone(),
                shell_functions: vec![],
                key: req.key.clone(),
                summary: format!("mux over {from_artifact}"),
            },
            base_env: Some(base_env),
        })
    }

    async fn prepare_spec(
        &self,
        session_id: &str,
        mut spec: heph::proc_exec::Spec,
    ) -> anyhow::Result<heph::proc_exec::Spec> {
        let n = self.prepares.fetch_add(1, Ordering::SeqCst);
        // The session's environment goes UNDER the caller's own — a runner
        // supplies the floor, or it could silently change what a target builds.
        let base = self
            .envs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        for (k, v) in base {
            if !spec.env.iter().any(|(sk, _)| *sk == k) {
                spec.env.push((k, v));
            }
        }
        // Per-SPAWN state. An environment described once at open could not
        // produce this — it is the capability the ABI lane exists for.
        spec.env
            .push((OsString::from("SEQ"), OsString::from(n.to_string())));
        Ok(spec)
    }

    async fn close_session(&self, _session_id: &str) -> anyhow::Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Counts {
    opens: Arc<AtomicUsize>,
    prepares: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
}

/// A workspace whose `bash` runner targets are served by a driver over the
/// exec-runner lane — the same adapter a loaded cdylib gets on the host side.
fn mux_workspace() -> (Workspace, Counts) {
    let c = Counts {
        opens: Arc::new(AtomicUsize::new(0)),
        prepares: Arc::new(AtomicUsize::new(0)),
        closes: Arc::new(AtomicUsize::new(0)),
    };
    let plugin: Arc<dyn heph::engine::exec_runner::ExecRunnerPlugin> = Arc::new(MuxRunner {
        opens: Arc::clone(&c.opens),
        prepares: Arc::clone(&c.prepares),
        closes: Arc::clone(&c.closes),
        envs: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    // Registered under `bash`: a runner target's driver name selects the runner
    // that serves it, and the runner targets below are bash targets.
    let ws = Workspace::with_exec_runner_named(
        "bash",
        Arc::new(heph::engine::exec_runner::PluginExecRunner::new(plugin)),
    );
    (ws, c)
}

/// The engine resolves a runner target to the runner registered for its driver
/// name, the runner opens a session, and the environment reaches the target's
/// real process.
#[tokio::test]
async fn a_plugin_served_session_reaches_the_target() -> anyhow::Result<()> {
    let (ws, c) = mux_workspace();
    ws.write_build_file(
        "m",
        r#"
target(name = "env", driver = "bash", run = "echo SHELL_A > $OUT", out = "env.txt")
target(
    name = "consumer",
    driver = "bash",
    run = "echo \"$FROM_RUNNER|$SEQ\" > $OUT",
    out = "o",
    runner = "//m:env",
)
"#,
    );

    let res = ws.run("//m:consumer").await?;
    let out = common::artifact_string(&res);
    assert!(
        out.contains("SHELL_A"),
        "the session's environment, derived from the runner artifact, must reach \
         the target: {out:?}"
    );
    assert_eq!(c.opens.load(Ordering::SeqCst), 1);
    assert!(
        c.prepares.load(Ordering::SeqCst) >= 1,
        "the runner must be consulted for the spawn"
    );
    Ok(())
}

/// The reason this lives on the ABI rather than in the artifact: the runner is
/// asked about **every** spawn, while the environment is opened once.
#[tokio::test]
async fn the_runner_is_consulted_per_target_not_per_environment() -> anyhow::Result<()> {
    let (ws, c) = mux_workspace();
    ws.write_build_file(
        "p",
        r#"
target(name = "env", driver = "bash", run = "echo E > $OUT", out = "env.txt")
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "o", runner = "//p:env")
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "o", runner = "//p:env")
target(name = "c", driver = "bash", run = "echo c > $OUT", out = "o", runner = "//p:env")
"#,
    );

    for t in ["//p:a", "//p:b", "//p:c"] {
        ws.run(t).await?;
    }

    assert_eq!(
        c.opens.load(Ordering::SeqCst),
        1,
        "three targets sharing one environment must open it once",
    );
    assert!(
        c.prepares.load(Ordering::SeqCst) >= 3,
        "but every target's process must go through the runner, got {}",
        c.prepares.load(Ordering::SeqCst),
    );
    Ok(())
}

/// A runner target whose driver has no runner registered must refuse, not
/// degrade. Running the target in the host environment under a key asserting
/// the runner's is the silently-wrong build this check exists to prevent.
///
/// With runners as a component, absence is structural — a plugin exporting no
/// runners simply registers none — rather than a probe that could answer wrong.
#[tokio::test]
async fn a_runner_target_with_no_registered_runner_is_refused() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "r",
        r#"
target(name = "env", driver = "bash", run = "echo E > $OUT", out = "env.txt")
target(name = "c", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//r:env")
"#,
    );

    let msg = match ws.run("//r:c").await {
        Ok(_) => panic!("a runner with no session-serving driver must not resolve"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        msg.contains("exec runner"),
        "the error must name the actual problem, got: {msg}"
    );
    Ok(())
}

/// The runner's identity still reaches the cache key: it is a dependency, so
/// swapping the environment re-keys every artifact built in it.
#[tokio::test]
async fn the_runner_still_reaches_the_cache_key() -> anyhow::Result<()> {
    let (ws, _c) = mux_workspace();
    ws.write_build_file(
        "k",
        r#"
target(name = "a", driver = "bash", run = "echo ONE > $OUT", out = "env.txt")
target(name = "b", driver = "bash", run = "echo TWO > $OUT", out = "env.txt")
target(name = "ca", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//k:a")
target(name = "cb", driver = "bash", run = "echo x > $OUT", out = "o", runner = "//k:b")
"#,
    );

    let hashin = async |addr: &str| -> anyhow::Result<String> {
        let rs = ws.engine.new_state();
        let meta = ws.engine.clone().meta(rs, &parse_addr(addr)?).await?;
        Ok(meta.hashin)
    };
    assert_ne!(
        hashin("//k:ca").await?,
        hashin("//k:cb").await?,
        "two different environments must not share a cache key"
    );
    Ok(())
}
