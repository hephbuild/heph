use anyhow::Context as _;
use heph::engine::driver::Driver as SDKDriver;
use heph::engine::driver_managed::ManagedDriver as SDKManagedDriver;
use heph::engine::provider::Provider as SDKProvider;
use heph::engine::{
    Config, EResult, Engine, EngineTargetSpec, OutputMatcher, PluginInit, ResultOptions,
};
use heph::htaddr::{Addr, parse_addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

type SetupFn = Box<dyn FnOnce(&mut Engine) -> anyhow::Result<()>>;

pub struct WorkspaceBuilder {
    dir: TempDir,
    parallelism: Option<usize>,
    fs_skip: Vec<String>,
    setups: Vec<SetupFn>,
}

impl WorkspaceBuilder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir().context("create workspace tempdir")?,
            parallelism: None,
            fs_skip: vec![],
            setups: vec![],
        })
    }

    pub fn from_dir(dir: TempDir) -> Self {
        Self {
            dir,
            parallelism: None,
            fs_skip: vec![],
            setups: vec![],
        }
    }

    pub fn with_parallelism(mut self, p: usize) -> Self {
        self.parallelism = Some(p);
        self
    }

    /// Mirrors the config file's `fs.skip`: directories/globs every tree-walking
    /// plugin prunes. The engine splits these into literal dirs and globs.
    pub fn with_fs_skip(mut self, skip: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.fs_skip = skip.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_provider(
        mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKProvider> + 'static,
    ) -> Self {
        self.setups
            .push(Box::new(move |e: &mut Engine| e.register_provider(factory)));
        self
    }

    pub fn with_managed_driver(mut self, driver: Box<dyn SDKManagedDriver>) -> Self {
        self.setups.push(Box::new(move |e: &mut Engine| {
            e.register_managed_driver(|_| driver)
        }));
        self
    }

    pub fn with_driver(mut self, driver: Box<dyn SDKDriver>) -> Self {
        self.setups.push(Box::new(move |e: &mut Engine| {
            e.register_driver(|_| driver)
        }));
        self
    }

    pub fn build(self) -> anyhow::Result<Workspace> {
        let mut e = Engine::new(Config {
            root: self.dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: self.parallelism,
            fs_skip: self.fs_skip,
            ..Default::default()
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

    pub async fn run(&self, addr_str: &str) -> anyhow::Result<Arc<EResult>> {
        let addr = parse_addr(addr_str)?;
        self.run_addr(addr).await
    }

    pub async fn run_addr(&self, addr: Addr) -> anyhow::Result<Arc<EResult>> {
        let e = self.engine.clone();
        let rs = e.new_state();
        match e
            .result_addr(
                rs.clone(),
                &addr,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await
        {
            Ok(r) => Ok(r),
            // result_addr returns a lightweight `UpstreamFailed` marker; the rich
            // root-cause diagnostics live in the request's failure registry (the
            // CLI drains and renders them at end-of-run). Mirror that here so
            // direct engine consumers get the actual cause, not "dependency failed".
            Err(err) => {
                let failures = rs.take_failures();
                if failures.is_empty() {
                    return Err(err);
                }
                let mut msg = String::new();
                for f in &failures {
                    msg.push_str(&format!("{}: {:#}", f.addr.format(), f.source));
                    if let Some(tail) = &f.log_tail {
                        msg.push_str(&format!("\nlast log lines:\n{tail}"));
                    }
                    msg.push('\n');
                }
                Err(anyhow::anyhow!("{}", msg.trim_end()))
            }
        }
    }

    /// Build every target matching `matcher` as one batch — the exact path the
    /// `run` CLI takes for `heph r build ./pkg` (a Label/Package query, not a
    /// single addr). Concurrent whole-graph resolution; surfaces per-addr
    /// failures as an aggregated error so cycle bugs that only appear under
    /// fan-out are visible. Returns the successful results on full success.
    pub async fn run_matcher(
        &self,
        matcher: &heph::htmatcher::Matcher,
    ) -> anyhow::Result<Vec<Arc<EResult>>> {
        let e = self.engine.clone();
        let rs = e.new_state();
        let batch = e.result(rs, matcher, &ResultOptions::default()).await?;
        if !batch.errors.is_empty() {
            let mut msg = String::new();
            for (addr, err) in &batch.errors {
                msg.push_str(&format!("{}: {:#}\n", addr.format(), err));
            }
            return Err(anyhow::anyhow!("{}", msg.trim_end()));
        }
        Ok(batch.ok)
    }

    pub async fn get_spec(&self, addr_str: &str) -> anyhow::Result<Arc<EngineTargetSpec>> {
        let addr = parse_addr(addr_str)?;
        let rs = self.engine.new_state();
        self.engine.clone().get_spec(rs, &addr).await
    }
}

pub fn artifact_bytes(result: &EResult) -> Vec<u8> {
    use heph::hartifactcontent::WalkEntryKind;
    use std::io::Read as _;
    let mut out = Vec::new();
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk artifacts") {
            let entry = entry.expect("artifact entry");
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                data.read_to_end(&mut out).expect("read artifact");
            }
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
