#![allow(dead_code, unused_imports)]

use anyhow::Result;
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginstatictarget;
use htestkit::WorkspaceBuilder;

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
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash()));
        let builder = if let Some(p) = p {
            builder.with_parallelism(p)
        } else {
            builder
        };
        Self(builder.build().expect("build workspace"))
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
