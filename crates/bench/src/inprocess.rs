//! Tier A: engine linked in-process, no process spawn, no plugin cdylib —
//! the `crates/e2e` pattern applied to timing instead of assertions.
//!
//! Isolates engine-internals cost (cache lookup, hashing, DAG resolution,
//! BUILD-file/Starlark eval) from process-startup and dlopen cost, which
//! Tier B (`run dist`) measures instead. Only `bash`/`exec` builtins are
//! registered — real users' most common path, and the one that never
//! crosses the plugin ABI seam.

use crate::timing::{RunOptions, RunResults, ScenarioResult};
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
    /// One untimed rep populates the cache; every measured rep hits it.
    FullHit,
    /// Like `full-hit`, but a small fraction of BUILD files are rewritten
    /// after the warmup rep, so measured reps do a partial rebuild.
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

pub async fn run(
    corpus: &Path,
    manifest: &CorpusManifest,
    scenario: Scenario,
    opts: &RunOptions,
) -> Result<RunResults> {
    let mut wall_ms = Vec::with_capacity(opts.reps);

    match scenario {
        Scenario::Cold => {
            for _ in 0..opts.warmup {
                wipe_cache(corpus)?;
                resolve_all(corpus).await?;
            }
            for _ in 0..opts.reps {
                wipe_cache(corpus)?;
                let start = Instant::now();
                resolve_all(corpus).await?;
                wall_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        Scenario::FullHit => {
            if !opts.skip_prepare {
                wipe_cache(corpus)?;
                for _ in 0..opts.warmup {
                    resolve_all(corpus).await?;
                }
            }
            for _ in 0..opts.reps {
                let start = Instant::now();
                resolve_all(corpus).await?;
                wall_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
        Scenario::Incremental => {
            if !opts.skip_prepare {
                wipe_cache(corpus)?;
                for _ in 0..opts.warmup {
                    resolve_all(corpus).await?;
                }
            }
            // Fresh mutation before every measured rep — otherwise only the
            // first rep is a genuine incremental rebuild and the rest are
            // full-hit repeats wearing the wrong label.
            for i in 0..opts.reps {
                bench_corpus::incrementalize(
                    manifest,
                    corpus,
                    0.01,
                    0xDEC1 ^ (opts.rep_offset + i) as u64,
                )
                .context("mutate corpus for incremental scenario")?;
                let start = Instant::now();
                resolve_all(corpus).await?;
                wall_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }

    Ok(RunResults {
        tier: "inprocess".to_string(),
        scenarios: vec![ScenarioResult {
            scenario: scenario.name().to_string(),
            wall_ms,
        }],
    })
}
