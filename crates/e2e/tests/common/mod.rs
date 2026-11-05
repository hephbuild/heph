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

/// A fresh engine rooted at `root` — buildfile provider, bash driver — wired
/// to a single read+write remote cache at `remote_uri`. Each call gets its own
/// empty in-memory cache tier, so two calls over one root are two `heph`
/// invocations sharing the on-disk cache.
pub fn build_engine(root: &std::path::Path, remote_uri: &str) -> Arc<Engine> {
    build_engine_with_remote(root, Some(remote_uri))
}

/// [`build_engine`] with the remote optional: `None` configures no remote.
pub fn build_engine_with_remote(root: &std::path::Path, remote_uri: Option<&str>) -> Arc<Engine> {
    let mut e = Engine::new(Config {
        remote_caches: remote_uri
            .map(|uri| heph::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: uri.to_string(),
                read: true,
                write: true,
                concurrency: 10,
                endpoint: None,
                region: None,
            })
            .into_iter()
            .collect(),
        ..Config::for_tests(root)
    })
    .expect("engine");
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            std::sync::Arc::clone(&init.functions),
        ))
    })
    .expect("register buildfile provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .expect("register bash driver");
    Arc::new(e)
}

/// Release `rs` and wait for its background work (remote uploads) to drain,
/// as `heph run` does before exiting.
///
/// Takes the request by value and drops it before waiting: some background
/// work is only *submitted* when the request state drops (the post-write cache
/// trim) and holds a slot until then, so a waiter that keeps `rs` alive waits
/// on a counter that cannot reach zero. `heph run` unwinds in this order too.
pub async fn drain(rs: Arc<heph::engine::request_state::RequestState>) {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let bg = rs.bg_pending();
    drop(rs);
    let deadline = Instant::now() + Duration::from_secs(10);
    while bg.load(Ordering::Acquire) > 0 {
        assert!(Instant::now() < deadline, "background upload never drained");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Run a target with all its outputs and return its artifact string, after
/// draining any background remote upload. Without the drain the upload could
/// still be in flight when the caller inspects the remote.
pub async fn run_drain(engine: &Arc<Engine>, addr: &str) -> String {
    let addr = heph::htaddr::parse_addr(addr).expect("parse addr");
    let rs = engine.new_state();
    let result = engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await
        .expect("run target");
    drain(rs).await;
    artifact_string(&result)
}

/// `heph r <matcher>` on one event-collecting request: the batch result and
/// every event the request emitted, after the background work drained.
pub async fn run_drain_events(
    engine: &Arc<Engine>,
    m: &heph::htmatcher::Matcher,
) -> (
    anyhow::Result<heph::engine::BatchResult>,
    Vec<heph::engine::event::BuildEvent>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result(
            rs.clone(),
            m,
            heph::engine::OutputMatcher::All,
            &heph::engine::ResultOptions::default(),
        )
        .await;
    drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (res, events)
}

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

impl Workspace {
    pub fn new() -> Self {
        Self::with_parallelism(None)
    }

    pub fn with_parallelism(parallelism: impl Into<Option<usize>>) -> Self {
        let p = parallelism.into();
        let builder = WorkspaceBuilder::new()
            .expect("workspace tempdir")
            .with_provider(|init| {
                Box::new(pluginbuildfile::Provider::new(
                    init.root.to_path_buf(),
                    init.runtime.clone(),
                    std::sync::Arc::clone(&init.functions),
                ))
            })
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec().with_host_path()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
            .with_managed_driver(Box::new(pluginhttp::Driver))
            .with_driver(Box::new(heph::plugintemplate::Driver));
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
        self.reopen_scoped(Default::default())
    }

    /// [`reopen`](Self::reopen) with an explicit scratch lineage policy — what a
    /// second `heph` invocation sees after the branch changed under it.
    pub fn reopen_scoped(&self, scratch: heph::engine::ScratchOptions) -> Result<Arc<Engine>> {
        let cfg = self.reopen_with.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "this workspace's provider set cannot be rebuilt; reopen is unsupported"
            )
        })?;
        let root = self.inner.dir.path().to_path_buf();
        let mut engine = Engine::new(Config {
            parallelism: cfg.parallelism,
            fs_skip: cfg.fs_skip.clone(),
            scratch,
            ..Config::for_tests(root.clone())
        })
        .context("reopen: build engine over the existing workspace root")?;
        engine
            .register_provider(move |init| {
                Box::new(pluginbuildfile::Provider::new(
                    root,
                    init.runtime.clone(),
                    std::sync::Arc::clone(&init.functions),
                ))
            })
            .context("reopen: register buildfile provider")?;
        engine
            .register_managed_driver(|_| Box::new(pluginexec::Driver::new_exec().with_host_path()))
            .context("reopen: register exec driver")?;
        engine
            .register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))
            .context("reopen: register bash driver")?;
        engine
            .register_managed_driver(|_| Box::new(pluginhttp::Driver))
            .context("reopen: register http driver")?;
        engine
            .register_driver(|_| Box::new(heph::plugintemplate::Driver))
            .context("reopen: register template driver")?;
        let engine = Arc::new(engine);
        engine.install_exec_runner_host();
        Ok(engine)
    }

    pub fn with_static(targets: Vec<pluginstatictarget::Target>) -> Result<Self> {
        let provider = pluginstatictarget::Provider::new(targets)?;
        let ws = WorkspaceBuilder::new()?
            .with_provider(move |_| Box::new(provider))
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec().with_host_path()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
            .build()?;
        Ok(Self {
            inner: ws,
            reopen_with: None,
        })
    }
}
