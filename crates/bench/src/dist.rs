//! Tier B: the real, prebuilt `heph` binary spawned as a child process,
//! dlopening the real `go` plugin cdylib — the seam `crates/bin-e2e` exists
//! to cover and an in-process test structurally cannot reach.
//!
//! Only prebuilt artifacts are used, located the same way
//! `crates/bin-e2e/tests/common/mod.rs`'s `Dist` does: a normalized
//! directory (`heph`, `heph-<name>-plugin.<ext>`), never rebuilt here.
//!
//! Unlike Tier A, the thing under test here (the real `heph` binary) is
//! already a prebuilt artifact on both sides, and the code driving it
//! (this module) is always the current checkout's own — there is no
//! per-commit "subject" binary to keep a stable contract with. `prepare`/
//! `measure_once` still split the same way as `inprocess`'s for a uniform
//! orchestrator shape, not because a compatibility seam requires it here.

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
        let heph = dir.join("heph");
        if !heph.is_file() {
            bail!("{} does not contain a `heph` binary", dir.display());
        }
        // Absolute: `build_go_tree` spawns `heph` with `current_dir(corpus)`,
        // so a relative `--dist` path (the common case — CI passes plain
        // `candidate-dist`) would silently resolve against the corpus dir
        // instead of the caller's cwd once that happens.
        let root = dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", dir.display()))?;
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
/// dlopen + ABI-negotiation + checksum-verify path. `corpus` must already be
/// absolute (see `Dist::locate`'s comment for why).
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

    // `gotool: host` — the go provider requires an explicit choice (host /
    // pinned version / a toolchain-producing target) and has no default.
    // `host` uses the Go `actions/setup-go` already installed for
    // `tools/gorepogen`, so this stays offline and pays no extra hermetic-
    // SDK download — same choice `bin-e2e`'s own go-plugin fixture makes,
    // and for the same reason.
    let config = format!(
        "plugins:\n  \
         - builtin: buildfile\n    options:\n      patterns:\n        - BUILD\n  \
         - builtin: exec\n  \
         - builtin: bash\n  \
         - path: {}\n    options:\n      gotool: \"host\"\n",
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
    pub fn name(self) -> &'static str {
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

fn build_go_tree(dist: &Dist, corpus: &Path) -> Result<f64> {
    let home = tempfile::tempdir().context("create HOME tempdir")?;
    let start = Instant::now();
    let out = Command::new(dist.heph())
        .args(["r", "build", "//go/..."])
        .current_dir(corpus)
        .env("HOME", home.path())
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
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// `corpus` must already be absolute (canonicalize before calling — see
/// `write_go_config`'s comment on why a relative path breaks once `heph`
/// runs with `current_dir(corpus)`).
pub fn prepare(dist_dir: &Path, corpus: &Path, scenario: Scenario) -> Result<()> {
    let dist = Dist::locate(dist_dir)?;
    write_go_config(corpus, &dist)?;
    match scenario {
        Scenario::Cold | Scenario::FullHit | Scenario::Incremental => {
            wipe_cache(corpus)?;
            build_go_tree(&dist, corpus)?;
        }
    }
    Ok(())
}

/// One measured rep. `mutate_seed` only matters for `Incremental` (ignored
/// otherwise) — the caller must vary it across repeated calls against the
/// same corpus, or every call mutates the same files again. `corpus` must
/// already be absolute, same as `prepare`.
pub fn measure_once(
    dist_dir: &Path,
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    mutate_seed: u64,
) -> Result<f64> {
    if manifest.go_package_count == 0 {
        bail!(
            "corpus has no go/ subtree (generate with --go-packages > 0) — nothing for Tier B to build"
        );
    }
    let dist = Dist::locate(dist_dir)?;
    write_go_config(corpus, &dist)?;

    if let Scenario::Cold = scenario {
        wipe_cache(corpus)?;
    }
    if let Scenario::Incremental = scenario {
        // Mutates the go/ subtree being built here, not the bash pkgN
        // packages `bench_corpus::incrementalize` touches — Tier B only
        // builds `//go/...`.
        bench_corpus::incrementalize_go(&corpus.join("go"), 0.01, 0xDEC1 ^ mutate_seed)
            .context("mutate go corpus for incremental scenario")?;
    }
    build_go_tree(&dist, corpus)
}
