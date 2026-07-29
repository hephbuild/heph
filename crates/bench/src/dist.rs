//! Tier B: the real, prebuilt `heph` binary spawned as a child process,
//! dlopening the real `go` plugin cdylib — the seam `crates/bin-e2e` exists
//! to cover and an in-process test structurally cannot reach.
//!
//! Only prebuilt artifacts are used, located the same way
//! `crates/bin-e2e/tests/common/mod.rs`'s `Dist` does: a normalized
//! directory (`heph`, `heph-<name>-plugin.<ext>`), never rebuilt here.

use crate::timing::{RunResults, ScenarioResult};
use anyhow::{Context, Result, bail};
use bench_corpus::CorpusManifest;
use clap::ValueEnum;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const DYLIB_EXT: &str = if cfg!(target_os = "macos") {
    "dylib"
} else {
    "so"
};

pub struct Dist {
    root: PathBuf,
}

impl Dist {
    pub fn locate(dir: &Path) -> Result<Self> {
        let root = dir.to_path_buf();
        let heph = root.join("heph");
        if !heph.is_file() {
            bail!("{} does not contain a `heph` binary", root.display());
        }
        Ok(Self { root })
    }

    fn heph(&self) -> PathBuf {
        self.root.join("heph")
    }

    fn plugin(&self, name: &str) -> PathBuf {
        self.root.join(format!("heph-{name}-plugin.{DYLIB_EXT}"))
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(&bytes))))
}

fn host_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// Write the go-plugin manifest + a `.hephconfig` pointing at it, into
/// `corpus` — a real config a real `heph` invocation loads, forcing the
/// dlopen + ABI-negotiation + checksum-verify path.
fn write_go_config(corpus: &Path, dist: &Dist) -> Result<()> {
    let dylib = dist.plugin("go");
    if !dylib.is_file() {
        bail!(
            "missing {} — the dist dir must contain the go plugin cdylib \
             (heph-go-plugin.{DYLIB_EXT}), not just the `heph` binary",
            dylib.display()
        );
    }
    let manifest_path = corpus.join("heph-go-plugin.json");
    let sum = sha256_file(&dylib)?;
    let doc = serde_json::json!({
        "name": "go",
        "version": "bench",
        "artifacts": [{
            "os": host_os(),
            "arch": host_arch(),
            "path": dylib,
            "checksum": sum,
        }],
    });
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&doc)?)
        .with_context(|| format!("write {}", manifest_path.display()))?;

    let config = format!(
        "plugins:\n  \
         - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
         - builtin: exec\n  \
         - builtin: bash\n  \
         - path: {}\n",
        manifest_path.display()
    );
    std::fs::write(corpus.join(".hephconfig"), config).context("write .hephconfig")
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scenario {
    Cold,
    FullHit,
    Incremental,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Cold => "cold",
            Scenario::FullHit => "full-hit",
            Scenario::Incremental => "incremental",
        }
    }
}

fn cache_dir(corpus: &Path) -> PathBuf {
    corpus.join(".heph3")
}

fn wipe_cache(corpus: &Path) -> Result<()> {
    let dir = cache_dir(corpus);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(())
}

fn build_go_tree(dist: &Dist, corpus: &Path, home: &Path) -> Result<()> {
    let out = Command::new(dist.heph())
        .args(["r", "build", "//go/..."])
        .current_dir(corpus)
        .env("HOME", home)
        .env("HEPH_CWD", corpus)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .context("spawn heph r build //go/...")?;
    if !out.status.success() {
        bail!(
            "heph r build //go/... failed: status {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    Ok(())
}

pub fn run(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    warmup: usize,
    reps: usize,
) -> Result<RunResults> {
    if manifest.go_package_count == 0 {
        bail!(
            "corpus has no go/ subtree (generate with --go-packages > 0) — nothing for Tier B to build"
        );
    }
    let dist = Dist::locate(dist_dir)?;
    write_go_config(corpus, &dist)?;
    let home = tempfile::tempdir().context("create HOME tempdir")?;

    let mut wall_ms = Vec::with_capacity(reps);
    let timed_build = |home: &Path| -> Result<f64> {
        let start = Instant::now();
        build_go_tree(&dist, corpus, home)?;
        Ok(start.elapsed().as_secs_f64() * 1000.0)
    };

    match scenario {
        Scenario::Cold => {
            for _ in 0..warmup {
                wipe_cache(corpus)?;
                timed_build(home.path())?;
            }
            for _ in 0..reps {
                wipe_cache(corpus)?;
                wall_ms.push(timed_build(home.path())?);
            }
        }
        Scenario::FullHit => {
            wipe_cache(corpus)?;
            for _ in 0..warmup {
                timed_build(home.path())?;
            }
            for _ in 0..reps {
                wall_ms.push(timed_build(home.path())?);
            }
        }
        Scenario::Incremental => {
            wipe_cache(corpus)?;
            for _ in 0..warmup {
                timed_build(home.path())?;
            }
            for i in 0..reps {
                // Mutates the go/ subtree being built here, not the bash
                // pkgN packages `bench_corpus::incrementalize` touches — Tier
                // B only builds `//go/...`.
                bench_corpus::incrementalize_go(&corpus.join("go"), 0.01, 0xDEC1 ^ i as u64)
                    .context("mutate go corpus for incremental scenario")?;
                wall_ms.push(timed_build(home.path())?);
            }
        }
    }

    Ok(RunResults {
        tier: "dist".to_string(),
        scenarios: vec![ScenarioResult {
            scenario: scenario.name().to_string(),
            wall_ms,
        }],
    })
}
