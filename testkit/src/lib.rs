use anyhow::Context as _;
use rheph::engine::driver::Driver as SDKDriver;
use rheph::engine::driver_managed::ManagedDriver as SDKManagedDriver;
use rheph::engine::provider::Provider as SDKProvider;
use rheph::engine::provider::TargetSpec;
use rheph::engine::{Config, EResult, Engine, OutputMatcher, ResultOptions};
use rheph::htaddr::{Addr, parse_addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

type SetupFn = Box<dyn FnOnce(&mut Engine) -> anyhow::Result<()>>;

pub struct WorkspaceBuilder {
    dir: TempDir,
    parallelism: Option<usize>,
    setups: Vec<SetupFn>,
}

impl WorkspaceBuilder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir().context("create workspace tempdir")?,
            parallelism: None,
            setups: vec![],
        })
    }

    pub fn from_dir(dir: TempDir) -> Self {
        Self {
            dir,
            parallelism: None,
            setups: vec![],
        }
    }

    pub fn with_parallelism(mut self, p: usize) -> Self {
        self.parallelism = Some(p);
        self
    }

    pub fn with_provider(
        mut self,
        factory: impl FnOnce(&Path) -> Box<dyn SDKProvider> + 'static,
    ) -> Self {
        self.setups
            .push(Box::new(move |e: &mut Engine| e.register_provider(factory)));
        self
    }

    pub fn with_managed_driver(mut self, driver: Box<dyn SDKManagedDriver>) -> Self {
        self.setups.push(Box::new(move |e: &mut Engine| {
            e.register_managed_driver(driver)
        }));
        self
    }

    pub fn with_driver(mut self, driver: Box<dyn SDKDriver>) -> Self {
        self.setups
            .push(Box::new(move |e: &mut Engine| e.register_driver(driver)));
        self
    }

    pub fn build(self) -> anyhow::Result<Workspace> {
        let mut e = Engine::new(Config {
            root: self.dir.path().to_path_buf(),
            parallelism: self.parallelism,
        })?;
        for setup in self.setups {
            setup(&mut e)?;
        }
        Ok(Workspace {
            dir: self.dir,
            engine: Arc::new(e),
        })
    }
}

pub struct Workspace {
    pub dir: TempDir,
    pub engine: Arc<Engine>,
}

impl Workspace {
    pub fn write_build_file(&self, pkg: &str, content: &str) {
        let pkg_dir = if pkg.is_empty() {
            self.dir.path().to_path_buf()
        } else {
            self.dir.path().join(pkg)
        };
        std::fs::create_dir_all(&pkg_dir).expect("create pkg dir");
        std::fs::write(pkg_dir.join("BUILD"), content).expect("write BUILD file");
    }

    pub fn write_file(&self, rel_path: &str, content: &str) {
        let full = self.dir.path().join(rel_path);
        std::fs::create_dir_all(full.parent().expect("parent dir")).expect("create dirs");
        std::fs::write(full, content).expect("write file");
    }

    pub async fn run(&self, addr_str: &str) -> anyhow::Result<EResult> {
        let addr = parse_addr(addr_str)?;
        self.run_addr(addr).await
    }

    pub async fn run_addr(&self, addr: Addr) -> anyhow::Result<EResult> {
        let e = self.engine.clone();
        let rs = e.new_state();
        e.result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
            .await
    }

    pub async fn get_spec(&self, addr_str: &str) -> anyhow::Result<Arc<TargetSpec>> {
        let addr = parse_addr(addr_str)?;
        let rs = self.engine.new_state();
        self.engine.clone().get_spec(rs, &addr).await
    }
}

pub fn artifact_bytes(result: &EResult) -> Vec<u8> {
    use std::io::Read as _;
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk artifacts") {
            entry
                .expect("artifact entry")
                .data
                .read_to_end(&mut out)
                .expect("read artifact");
        }
    }
    out
}

pub fn artifact_string(result: &EResult) -> String {
    String::from_utf8(artifact_bytes(result)).expect("artifact is utf8")
}

pub fn artifact_paths(result: &EResult) -> Vec<PathBuf> {
    result
        .artifacts
        .iter()
        .flat_map(|a| {
            a.walk()
                .expect("walk artifacts")
                .map(|e| e.expect("artifact entry").path)
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn root(ws: &Workspace) -> &Path {
    ws.dir.path()
}

pub fn copy_dir_to_tempdir(src: &Path) -> anyhow::Result<TempDir> {
    let tmp = tempfile::tempdir().context("create tempdir")?;
    copy_dir_all(src, tmp.path())?;
    Ok(tmp)
}

fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("create dir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read dir {}", src.display()))? {
        let entry = entry.context("read dir entry")?;
        let ty = entry.file_type().context("get file type")?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))
                .with_context(|| format!("copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}
