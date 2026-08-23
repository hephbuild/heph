// `allow`, not `expect`: this module is included by every integration-test
// binary in the crate, and each one uses a different subset of it — so whether
// an item is dead or an import unused varies per binary, and an `expect` would
// be unfulfilled in some of them.
#![allow(
    dead_code,
    unused_imports,
    reason = "shared test harness; each test binary uses a different subset"
)]

use anyhow::{Context as _, Result};
use heph::engine::{Config, Engine};
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginhttp;
use heph::pluginstatictarget;
use htestkit::WorkspaceBuilder;
use std::sync::Arc;

pub use htestkit::{artifact_bytes, artifact_paths, artifact_string, root};

pub struct Workspace {
    inner: htestkit::Workspace,
    /// The configuration [`reopen`](Workspace::reopen) must reproduce. `None`
    /// for a workspace whose engine it cannot rebuild.
    reopen_with: Option<ReopenConfig>,
}

/// Everything `reopen` needs to build an engine that resolves the same targets
/// from the same inputs as the one the workspace was built with.
///
/// Held explicitly rather than re-derived: `Config` fields like `fs_skip` decide
/// which files a tree-walking plugin sees, i.e. the declared inputs, i.e. the
/// `hashin`. An engine that silently dropped them would write into the same
/// on-disk cache under different keys, and a test asserting a cache hit across
/// the two would be comparing two different targets and proving nothing.
struct ReopenConfig {
    parallelism: Option<usize>,
    fs_skip: Vec<String>,
}

impl std::ops::Deref for Workspace {
    type Target = htestkit::Workspace;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Wraps a session so its teardown can be observed.
struct TeardownSession {
    inner: heph::engine::exec_runner::EnvSession,
    /// Taken on the first hand-over, so a second caller gets `None` — teardown
    /// must not be runnable twice.
    torn: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicUsize>>>,
}

#[async_trait::async_trait]
impl heph::engine::exec_runner::ExecSession for TeardownSession {
    async fn prepare(
        &self,
        spec: heph::proc_exec::Spec,
    ) -> Result<heph::proc_exec::Spec, heph::engine::exec_runner::SpawnError> {
        self.inner.prepare(spec).await
    }
    fn base_env(&self) -> Option<&[(std::ffi::OsString, std::ffi::OsString)]> {
        self.inner.base_env()
    }
    fn caps(&self) -> &heph::engine::exec_runner::SessionCaps {
        self.inner.caps()
    }
    fn describe(&self) -> &heph::engine::exec_runner::SessionDescription {
        self.inner.describe()
    }
    fn teardown(&self) -> Option<heph::engine::exec_runner::TeardownJob> {
        let torn = self.torn.lock().ok()?.take()?;
        Some(Box::new(move || {
            torn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }))
    }
}

/// A test [`ExecRunner`] that counts its opens and hands back an environment.
///
/// The open counter is the point: the whole premise of a session is "acquire
/// once, serve many", and nothing else can prove it.
pub struct RecordingExecRunner {
    pub opens: Arc<std::sync::atomic::AtomicUsize>,
    /// Env var name/value the returned session applies to every process.
    pub var: (String, String),
    /// When set, the session also supplies `PATH`, which must then REPLACE the
    /// driver's own rather than sit under it.
    pub path: Option<String>,
    /// Incremented by the session's teardown job, so a test can prove it ran.
    pub torn: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[async_trait::async_trait]
impl heph::engine::exec_runner::ExecRunner for RecordingExecRunner {
    async fn open(
        &self,
        req: heph::engine::exec_runner::OpenRequest,
        _ctoken: &(dyn heph::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn heph::engine::exec_runner::ExecSession>> {
        use heph::engine::exec_runner as er;
        self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // The environment is derived from the artifact, not invented: that is
        // the rule that keeps `open` a pure parse of content the cache key
        // already covers (docs/EXEC_RUNNERS.md §4.7).
        let from_artifact = req
            .artifacts
            .first()
            .map(|a| String::from_utf8_lossy(&a.bytes).trim().to_string())
            .unwrap_or_default();

        let mut base = vec![];
        if let Some(p) = &self.path {
            base.push((
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from(p.clone()),
            ));
        }
        base.extend(vec![
            (
                std::ffi::OsString::from(self.var.0.clone()),
                std::ffi::OsString::from(self.var.1.clone()),
            ),
            (
                std::ffi::OsString::from("HEPH_TEST_RUNNER_ARTIFACT"),
                std::ffi::OsString::from(from_artifact),
            ),
        ]);
        if let Some(torn) = self.torn.clone() {
            return Ok(Arc::new(TeardownSession {
                inner: er::EnvSession::new(
                    base,
                    er::SessionCaps {
                        pty: true,
                        max_concurrent: None,
                        identity: er::Identity::Pinned {
                            by: req.key.clone(),
                        },
                    },
                    er::SessionDescription {
                        runner: req.runner_addr.clone(),
                        shell_functions: Vec::new(),
                        key: req.key.clone(),
                        summary: "teardown test runner".to_string(),
                    },
                ),
                torn: std::sync::Mutex::new(Some(torn)),
            }));
        }

        Ok(Arc::new(er::EnvSession::new(
            base,
            er::SessionCaps {
                pty: true,
                max_concurrent: None,
                identity: er::Identity::Pinned {
                    by: req.key.clone(),
                },
            },
            er::SessionDescription {
                runner: req.runner_addr.clone(),
                shell_functions: Vec::new(),
                key: req.key,
                summary: "recording test runner".to_string(),
            },
        )))
    }
}

impl Workspace {
    /// A workspace whose `bash`-driven runner targets are served by
    /// [`RecordingExecRunner`], with `defaultRunner` unset.
    pub fn with_recording_runner(
        opens: Arc<std::sync::atomic::AtomicUsize>,
        var: (&str, &str),
    ) -> Self {
        Self::with_recording_runner_path(opens, var, None)
    }

    /// [`Self::with_recording_runner`] whose session also supplies `PATH`.
    pub fn with_recording_runner_path(
        opens: Arc<std::sync::atomic::AtomicUsize>,
        var: (&str, &str),
        path: Option<&str>,
    ) -> Self {
        let path = path.map(str::to_owned);
        Self {
            inner: WorkspaceBuilder::new()
                .expect("workspace tempdir")
                .with_provider(|init| {
                    Box::new(pluginbuildfile::Provider::new(
                        init.root.to_path_buf(),
                        init.runtime.clone(),
                    ))
                })
                .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
                .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
                // Keyed by the runner target's DRIVER name — a runner target
                // built by `bash` is read back by the runner registered here.
                .with_exec_runner(
                    "bash",
                    Arc::new(RecordingExecRunner {
                        opens,
                        var: (var.0.to_string(), var.1.to_string()),
                        path,
                        torn: None,
                    }),
                )
                .build()
                .expect("build workspace"),
            reopen_with: None,
        }
    }

    pub fn new() -> Self {
        Self::with_parallelism(None)
    }

    /// A workspace whose session records that its teardown ran.
    pub fn with_teardown_runner(
        opens: Arc<std::sync::atomic::AtomicUsize>,
        torn: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            inner: WorkspaceBuilder::new()
                .expect("workspace tempdir")
                .with_provider(|init| {
                    Box::new(pluginbuildfile::Provider::new(
                        init.root.to_path_buf(),
                        init.runtime.clone(),
                    ))
                })
                .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
                .with_exec_runner(
                    "bash",
                    Arc::new(RecordingExecRunner {
                        opens,
                        var: ("V".to_string(), "1".to_string()),
                        path: None,
                        torn: Some(torn),
                    }),
                )
                .build()
                .expect("build workspace"),
            reopen_with: None,
        }
    }

    /// A workspace with `defaultRunner:` set — the exec environment every
    /// target inherits unless it authored `runner =` or opted out.
    ///
    /// Not reopenable: `ReopenConfig` would have to carry the default too, and
    /// a reopen that silently dropped it would resolve the same targets under
    /// different keys into the same on-disk cache — the exact confusion that
    /// struct's doc comment exists to prevent.
    pub fn with_default_runner(addr: &str) -> Self {
        let addr = heph::htaddr::parse_addr(addr).expect("parse defaultRunner addr");
        Self {
            inner: WorkspaceBuilder::new()
                .expect("workspace tempdir")
                .with_provider(|init| {
                    Box::new(pluginbuildfile::Provider::new(
                        init.root.to_path_buf(),
                        init.runtime.clone(),
                    ))
                })
                .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
                .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
                .with_default_runner(addr)
                // A default runner is only useful if something can serve it.
                .with_exec_runner(
                    "bash",
                    Arc::new(RecordingExecRunner {
                        opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        var: ("FROM_DEFAULT_RUNNER".to_string(), "1".to_string()),
                        path: None,
                        torn: None,
                    }),
                )
                .build()
                .expect("build workspace"),
            reopen_with: None,
        }
    }

    pub fn with_parallelism(parallelism: impl Into<Option<usize>>) -> Self {
        let p = parallelism.into();
        let builder = WorkspaceBuilder::new()
            .expect("workspace tempdir")
            .with_provider(|init| {
                Box::new(pluginbuildfile::Provider::new(
                    init.root.to_path_buf(),
                    init.runtime.clone(),
                ))
            })
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
            .with_managed_driver(Box::new(pluginhttp::Driver));
        let builder = if let Some(p) = p {
            builder.with_parallelism(p)
        } else {
            builder
        };
        Self {
            inner: builder.build().expect("build workspace"),
            reopen_with: Some(ReopenConfig {
                parallelism: p,
                fs_skip: Vec::new(),
            }),
        }
    }

    /// A second `Engine` over this workspace's root — what the *next* `heph`
    /// invocation sees: the same on-disk cache and lock directory, the same
    /// providers, drivers and input-visibility config, but empty in-memory
    /// provider/driver caches.
    ///
    /// Needed by any test that must observe a *changed* BUILD file. The spec a
    /// target resolves to is memoized per engine, so rewriting the BUILD file
    /// and re-running through the same engine replays the original definition
    /// and produces no new cache revision.
    ///
    /// Only workspaces built by [`new`](Workspace::new) /
    /// [`with_parallelism`](Workspace::with_parallelism) can be reopened;
    /// [`with_static`](Workspace::with_static) owns a provider instance this
    /// cannot reconstruct, and silently substituting a different provider set
    /// would resolve different targets into the same cache.
    pub fn reopen(&self) -> Result<Arc<Engine>> {
        let cfg = self.reopen_with.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "this workspace's provider set cannot be rebuilt; reopen is unsupported"
            )
        })?;
        let root = self.inner.dir.path().to_path_buf();
        let mut engine = Engine::new(Config {
            root: root.clone(),
            home_dir: std::path::PathBuf::new(),
            parallelism: cfg.parallelism,
            fs_skip: cfg.fs_skip.clone(),
            ..Default::default()
        })
        .context("reopen: build engine over the existing workspace root")?;
        engine
            .register_provider(move |init| {
                Box::new(pluginbuildfile::Provider::new(root, init.runtime.clone()))
            })
            .context("reopen: register buildfile provider")?;
        engine
            .register_managed_driver(|_| Box::new(pluginexec::Driver::new_exec()))
            .context("reopen: register exec driver")?;
        engine
            .register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))
            .context("reopen: register bash driver")?;
        engine
            .register_managed_driver(|_| Box::new(pluginhttp::Driver))
            .context("reopen: register http driver")?;
        Ok(Arc::new(engine))
    }

    pub fn with_static(targets: Vec<pluginstatictarget::Target>) -> Result<Self> {
        let provider = pluginstatictarget::Provider::new(targets)?;
        let ws = WorkspaceBuilder::new()?
            .with_provider(move |_| Box::new(provider))
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
            .build()?;
        Ok(Self {
            inner: ws,
            reopen_with: None,
        })
    }
}
