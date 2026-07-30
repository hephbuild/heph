//! Tier A: engine linked in-process, no process spawn, no plugin cdylib —
//! the `crates/e2e` pattern applied to timing instead of assertions.
//!
//! Isolates engine-internals cost (cache lookup, hashing, DAG resolution,
//! BUILD-file/Starlark eval) from process-startup and dlopen cost, which
//! Tier B (`run dist`) measures instead. Only `bash`/`exec` builtins are
//! registered — real users' most common path, and the one that never
//! crosses the plugin ABI seam.
//!
//! Exposes two primitives, [`prepare`] and [`measure_once`], rather than a
//! self-contained "do N reps" loop. The engine under test is compiled into
//! *this* binary, so candidate and baseline are necessarily two different
//! compiled artifacts — orchestrating warmup counts, rep counts, and
//! interleaving order belongs in one stable place (the `run inprocess`
//! orchestrator in `main.rs`, always built from the current checkout), not
//! duplicated inside every historical binary that gets fetched as a
//! baseline. See that orchestrator's doc comment for the full rationale.

use anyhow::{Context, Result};
use bench_corpus::CorpusManifest;
use clap::ValueEnum;
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Scenario {
    /// Every rep starts from an empty on-disk cache.
    Cold,
    /// `prepare` populates the cache; every `measure_once` hits it.
    FullHit,
    /// Like `full-hit`, but each `measure_once` first rewrites a small
    /// fraction of BUILD files, so it does a partial rebuild.
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

fn build_engine(root: &Path) -> Result<Arc<Engine>> {
    let mut e = Engine::new(Config {
        root: root.to_path_buf(),
        home_dir: PathBuf::new(),
        parallelism: None,
        fs_skip: Vec::new(),
        ..Default::default()
    })
    .context("construct engine")?;
    e.register_provider(|init| {
        Box::new(heph::pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
        ))
    })
    .context("register pluginbuildfile provider")?;
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_exec()))
        .context("register exec driver")?;
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_bash()))
        .context("register bash driver")?;
    Ok(Arc::new(e))
}

async fn resolve_all(root: &Path) -> Result<()> {
    let e = build_engine(root)?;
    let rs = e.new_state();
    let matcher = Matcher::PackagePrefix(PkgBuf::from(""));
    let batch = e
        .result(rs, &matcher, OutputMatcher::All, &ResultOptions::default())
        .await
        .context("resolve //...")?;
    if !batch.errors.is_empty() {
        let mut msg = String::new();
        for (addr, err) in &batch.errors {
            msg.push_str(&format!("{}: {:#}\n", addr.format(), err));
        }
        anyhow::bail!("{}", msg.trim_end());
    }
    Ok(())
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

/// Get the corpus into the state `measure_once` expects, once. Cold has
/// nothing to share across calls (every `measure_once` is independently
/// cold) but still benefits from a throwaway build to warm the OS/page
/// cache. `full-hit`/`incremental` wipe and do one real build here — that
/// build's cache is what every subsequent `measure_once` on this corpus
/// hits.
pub async fn prepare(corpus: &Path, scenario: Scenario) -> Result<()> {
    match scenario {
        Scenario::Cold | Scenario::FullHit | Scenario::Incremental => {
            wipe_cache(corpus)?;
            resolve_all(corpus).await?;
        }
    }
    Ok(())
}

/// One measured rep. `mutate_seed` only matters for `Incremental` (ignored
/// otherwise) — the caller must vary it across repeated calls against the
/// same corpus, or every call mutates the same files again.
pub async fn measure_once(
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    mutate_seed: u64,
) -> Result<f64> {
    if let Scenario::Cold = scenario {
        wipe_cache(corpus)?;
    }
    if let Scenario::Incremental = scenario {
        bench_corpus::incrementalize(manifest, corpus, 0.01, 0xDEC1 ^ mutate_seed)
            .context("mutate corpus for incremental scenario")?;
    }
    let start = Instant::now();
    resolve_all(corpus).await?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}
