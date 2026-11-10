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
use heph::engine::{Config, ConfigYamlExt as _, Engine, HomeDir, OutputMatcher, ResultOptions};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use std::path::Path;
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

pub(crate) fn build_engine(root: &Path) -> Result<Arc<Engine>> {
    let mut e = Engine::new(Config::new(root.to_path_buf(), cache_dir(root)?))
        .context("construct engine")?;
    e.register_provider(|init| {
        Box::new(heph::pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            std::sync::Arc::clone(&init.functions),
        ))
    })
    .context("register pluginbuildfile provider")?;
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_exec().with_host_path()))
        .context("register exec driver")?;
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_bash().with_host_path()))
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

/// The corpus's heph home, resolved from its config the way the binary does.
pub(crate) fn cache_dir(corpus: &Path) -> Result<HomeDir> {
    Ok(heph::engine::config_yaml::load_from_root(corpus)
        .context("load corpus config")?
        .resolve(corpus)
        .context("resolve corpus config")?
        .home_dir)
}

/// Delete the corpus's heph home. A Tier B build leaves read-only directories
/// in it (the Go module cache is `0555`), which a plain `remove_dir_all`
/// cannot empty, so the removal makes them writable and retries.
pub(crate) fn wipe_cache(corpus: &Path) -> Result<()> {
    let dir = cache_dir(corpus)?;
    if dir.exists() {
        hcore::fsutil::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly generated corpus must take a whole Tier A rep: `corpus`, then
    /// `prepare`, then `measure_once` — the sequence CI runs. Each stage has
    /// broken on its own while the Perf job stayed green (most recently: no
    /// `.hephconfig` at the corpus root, so every `prepare` failed).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_generated_corpus_runs_a_full_hit_rep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let params = bench_corpus::CorpusParams {
            seed: 1,
            target_count: 8,
            packages: 2,
            layers: 2,
            fan_out: 2,
            ..Default::default()
        };
        let manifest = bench_corpus::generate(&params, dir.path()).expect("generate corpus");

        prepare(dir.path(), Scenario::FullHit)
            .await
            .expect("prepare a generated corpus");
        let ms = measure_once(dir.path(), &manifest, Scenario::FullHit, 0)
            .await
            .expect("measure a generated corpus");
        assert!(ms > 0.0, "a rep that resolved targets took no time: {ms}");
    }

    /// A Tier B build leaves read-only directories in the corpus home (the Go
    /// module cache is `0555`); the next wipe must still clear it. A plain
    /// `remove_dir_all` failed here with `Permission denied` on every push.
    #[test]
    fn wipe_cache_clears_a_home_holding_read_only_dirs() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(bench_corpus::CONFIG_FILE), "").expect("write config");
        let home = cache_dir(dir.path()).expect("home dir");
        let locked = home.join("gomodcache/example.com/m@v1.0.0");
        std::fs::create_dir_all(&locked).expect("create module dir");
        std::fs::write(locked.join("go.mod"), "module example.com/m\n").expect("write module file");
        for d in [&locked, &home.join("gomodcache/example.com")] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555))
                .expect("make dir read-only");
        }

        wipe_cache(dir.path()).expect("wipe a home holding read-only dirs");
        assert!(!home.exists(), "home survived the wipe: {}", home.display());
    }
}
