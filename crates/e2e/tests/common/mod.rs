#![allow(dead_code, unused_imports)]

use anyhow::Result;
use heph::engine::{Config, Engine};
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginhttp;
use heph::pluginstatictarget;
use htestkit::WorkspaceBuilder;
use std::sync::Arc;

pub use htestkit::{artifact_bytes, artifact_paths, artifact_string, root};

pub struct Workspace(htestkit::Workspace);

impl std::ops::Deref for Workspace {
    type Target = htestkit::Workspace;
    fn deref(&self) -> &Self::Target {
        &self.0
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
            .with_provider(|init| Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf())))
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
            .with_managed_driver(Box::new(pluginhttp::Driver));
        let builder = if let Some(p) = p {
            builder.with_parallelism(p)
        } else {
            builder
        };
        Self(builder.build().expect("build workspace"))
    }

    /// A second `Engine` over this workspace's root — what the *next* `heph`
    /// invocation sees: the same on-disk cache and lock directory, but empty
    /// in-memory provider/driver caches.
    ///
    /// Needed by any test that must observe a *changed* BUILD file. The spec a
    /// target resolves to is memoized per engine, so rewriting the BUILD file
    /// and re-running through the same engine replays the original definition
    /// and produces no new cache revision.
    pub fn reopen(&self) -> Result<Arc<Engine>> {
        let root = self.dir.path().to_path_buf();
        let mut engine = Engine::new(Config {
            root: root.clone(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| Box::new(pluginbuildfile::Provider::new(root)))?;
        engine.register_managed_driver(|_| Box::new(pluginexec::Driver::new_exec()))?;
        engine.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))?;
        engine.register_managed_driver(|_| Box::new(pluginhttp::Driver))?;
        Ok(Arc::new(engine))
    }

    pub fn with_static(targets: Vec<pluginstatictarget::Target>) -> Result<Self> {
        let provider = pluginstatictarget::Provider::new(targets)?;
        let ws = WorkspaceBuilder::new()?
            .with_provider(move |_| Box::new(provider))
            .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
            .build()?;
        Ok(Self(ws))
    }
}
