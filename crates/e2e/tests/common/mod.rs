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
            root: root.clone(),
            home_dir: std::path::PathBuf::new(),
            parallelism: cfg.parallelism,
            fs_skip: cfg.fs_skip.clone(),
            scratch,
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
        let engine = Arc::new(engine);
        engine.install_exec_runner_host();
        Ok(engine)
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
