#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use anyhow::Result;
use tempfile::TempDir;
use rheph::engine::{Config, Engine, EResult, OutputMatcher, ResultOptions};
use rheph::engine::provider::TargetSpec;
use rheph::htaddr::{parse_addr, Addr};
use rheph::pluginbuildfile;
use rheph::pluginexec;
use rheph::pluginstatictarget;

pub struct Workspace {
    pub dir: TempDir,
    pub engine: Arc<Engine>,
}

impl Workspace {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();

        let mut e = Engine::new(Config {
            root: dir.path().to_path_buf(),
        })
        .unwrap();

        e.register_provider(|root| {
            Box::new(pluginbuildfile::Provider {
                root: root.to_path_buf(),
                ..Default::default()
            })
        })
        .unwrap();
        e.register_managed_driver(Box::new(pluginexec::Driver::new_exec()))
            .unwrap();
        e.register_managed_driver(Box::new(pluginexec::Driver::new_bash()))
            .unwrap();

        Self {
            dir,
            engine: Arc::new(e),
        }
    }

    pub fn with_static(targets: Vec<pluginstatictarget::Target>) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let provider = pluginstatictarget::Provider::new(targets)?;

        let mut e = Engine::new(Config {
            root: dir.path().to_path_buf(),
        })?;

        e.register_provider(move |_root| Box::new(provider))?;
        e.register_managed_driver(Box::new(pluginexec::Driver::new_exec()))?;
        e.register_managed_driver(Box::new(pluginexec::Driver::new_bash()))?;

        Ok(Self {
            dir,
            engine: Arc::new(e),
        })
    }

    pub fn write_build_file(&self, pkg: &str, content: &str) {
        let pkg_dir = if pkg.is_empty() {
            self.dir.path().to_path_buf()
        } else {
            self.dir.path().join(pkg)
        };
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("BUILD"), content).unwrap();
    }

    pub fn write_file(&self, rel_path: &str, content: &str) {
        let full = self.dir.path().join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    pub async fn run(&self, addr_str: &str) -> Result<EResult> {
        let addr = parse_addr(addr_str)?;
        self.run_addr(addr).await
    }

    pub async fn run_addr(&self, addr: Addr) -> Result<EResult> {
        let e = self.engine.clone();
        let rs = e.new_state();
        e.result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default()).await
    }

    pub async fn get_spec(&self, addr_str: &str) -> Result<TargetSpec> {
        let addr = parse_addr(addr_str)?;
        let rs = self.engine.new_state();
        self.engine.get_spec(rs, &addr).await
    }
}

pub fn artifact_bytes(result: &EResult) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().unwrap() {
            entry.unwrap().data.read_to_end(&mut out).unwrap();
        }
    }
    out
}

pub fn artifact_string(result: &EResult) -> String {
    String::from_utf8(artifact_bytes(result)).unwrap()
}

pub fn artifact_paths(result: &EResult) -> Vec<std::path::PathBuf> {
    result
        .artifacts
        .iter()
        .flat_map(|a| {
            a.walk()
                .unwrap()
                .map(|e| e.unwrap().path)
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn root(ws: &Workspace) -> &Path {
    ws.dir.path()
}
