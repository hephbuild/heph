mod compare;
mod dist;
mod inprocess;
mod timing;

use anyhow::{Context, Result};
use bench_corpus::CorpusParams;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Command;
use timing::{RunResults, ScenarioResult};

/// Perf-regression harness: generate a deterministic synthetic corpus, time
/// `heph` scenarios against it (in-process or the real prebuilt binary +
/// plugin cdylib), and decide baseline-vs-candidate regression.
#[derive(Parser)]
#[command(name = "heph-bench")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a synthetic corpus.
    Corpus(GenerateArgs),
    /// Orchestrate a scenario across both candidate and baseline, in-process
    /// (Tier A) or against real binaries (Tier B).
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Tier A's minimal, stable "subject" primitive — see its own doc
    /// comment. Not meant to be run by hand; `run inprocess` spawns it.
    MeasureInprocess(MeasureInprocessArgs),
    /// Decide regression between two `run` result files.
    Compare(CompareArgs),
}

#[derive(Args)]
struct GenerateArgs {
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value_t = 1000)]
    targets: usize,
    #[arg(long, default_value_t = 100)]
    packages: usize,
    #[arg(long, default_value_t = 6)]
    layers: usize,
    #[arg(long, default_value_t = 3)]
    fan_out: usize,
    /// 0 = no go/ subtree (Tier B scenarios need this > 0).
    #[arg(long, default_value_t = 0)]
    go_packages: usize,
    #[arg(long, default_value_t = 4)]
    go_max_depth: usize,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum MeasureMode {
    /// Get the corpus into the state a measured rep expects (wipe + one
    /// throwaway build). Prints nothing.
    Prepare,
    /// One measured rep. Prints ONLY the elapsed milliseconds, as a single
    /// line — the entire contract a caller can rely on.
    Once,
}

#[derive(Args)]
struct MeasureInprocessArgs {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, value_enum)]
    scenario: inprocess::Scenario,
    #[arg(long, value_enum)]
    mode: MeasureMode,
    /// Only consulted for `incremental` in `--mode once`. Ignored otherwise.
    #[arg(long, default_value_t = 0)]
    mutate_seed: u64,
}

#[derive(Subcommand)]
enum RunCmd {
    Inprocess(RunInprocessArgs),
    Dist(RunDistArgs),
}

/// Why this orchestrates both sides in one call instead of being invoked
/// once per side: the engine under test is compiled into the `heph-bench`
/// binary itself for Tier A, so candidate and baseline are two different
/// compiled artifacts — there is no single binary that can link both
/// engines to compare them in-process. Baseline's binary is always fetched
/// from a past release, so it is compiled from OLDER source than whatever
/// orchestration logic (warmup counts, rep counts, interleaving order,
/// output format) the current PR wants — asking baseline's binary to
/// understand new orchestration flags means every such change needs a
/// bootstrap PR before it can be exercised (this crate hit that twice).
///
/// The fix: shrink what a per-commit binary needs to expose to the smallest
/// possible stable contract — `measure-inprocess --mode prepare|once`,
/// which prints only a single elapsed-ms number and needs no format changes
/// to support new orchestration ideas — and keep ALL orchestration in the
/// current checkout's own `heph-bench`, spawned fresh for every run. This
/// command spawns that primitive on `--candidate-bin` AND `--baseline-bin`
/// symmetrically (never calling in-process on itself, even though it could
/// for the candidate side) so both sides pay identical process-spawn
/// overhead — that overhead is then symmetric noise on both sides of every
/// rep, not a fixed advantage for whichever side skips it.
#[derive(Args)]
struct RunInprocessArgs {
    #[arg(long)]
    candidate_bin: PathBuf,
    #[arg(long)]
    baseline_bin: PathBuf,
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, value_enum)]
    scenario: inprocess::Scenario,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 5)]
    reps: usize,
    #[arg(long)]
    out_candidate: PathBuf,
    #[arg(long)]
    out_baseline: PathBuf,
}

/// Unlike Tier A, `heph`/the go plugin are already prebuilt artifacts on
/// both sides and the code driving them (`dist::prepare`/`measure_once`) is
/// always the current checkout's own — there is no per-commit "subject"
/// binary to keep compatible here, so this calls into that module directly
/// rather than spawning anything. Still orchestrates both sides in one
/// command, interleaved, for the same symmetric-noise reasoning as
/// `RunInprocessArgs` — and for a uniform shape between the two tiers.
#[derive(Args)]
struct RunDistArgs {
    /// Directory containing the prebuilt `heph` binary and plugin cdylibs
    /// (same layout `crates/bin-e2e`'s `Dist` expects). Never rebuilt here.
    #[arg(long)]
    candidate_dist: PathBuf,
    #[arg(long)]
    baseline_dist: PathBuf,
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, value_enum)]
    scenario: dist::Scenario,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 3)]
    reps: usize,
    #[arg(long)]
    out_candidate: PathBuf,
    #[arg(long)]
    out_baseline: PathBuf,
}

#[derive(Args)]
struct CompareArgs {
    #[arg(long)]
    baseline: PathBuf,
    #[arg(long)]
    candidate: PathBuf,
    /// JSON map of scenario -> threshold pct, e.g. `{"scale": 12.0}`.
    /// Scenarios not listed use `--default-threshold-pct`.
    #[arg(long)]
    thresholds: Option<PathBuf>,
    #[arg(long, default_value_t = 8.0)]
    default_threshold_pct: f64,
    /// Regression must clear `noise_k` baseline standard deviations, not
    /// just the pct threshold.
    #[arg(long, default_value_t = 2.0)]
    noise_k: f64,
    #[arg(long)]
    out: Option<PathBuf>,
    /// Write the per-scenario verdicts as JSON — machine-readable numbers
    /// (mean_ms, delta_pct, threshold_pct, regression) for a caller that
    /// wants to push them somewhere (a metrics backend, a dashboard) rather
    /// than parse the markdown table.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print the verdict but always exit 0 — the local escape hatch. In CI
    /// the equivalent is the repo admin's existing bypass on required
    /// checks, not a flag baked into the job.
    #[arg(long)]
    allow_regression: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Corpus(args) => run_corpus(args),
        Cmd::Run { cmd } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(run_run(cmd))
        }
        Cmd::MeasureInprocess(args) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(run_measure_inprocess(args))
        }
        Cmd::Compare(args) => run_compare(args),
    }
}

fn run_corpus(args: GenerateArgs) -> Result<()> {
    std::fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;
    let params = CorpusParams {
        seed: args.seed,
        target_count: args.targets,
        packages: args.packages,
        layers: args.layers,
        fan_out: args.fan_out,
        go_packages: args.go_packages,
        go_max_depth: args.go_max_depth,
        ..Default::default()
    };
    let manifest = bench_corpus::generate(&params, &args.out).context("generate corpus")?;
    println!(
        "generated {} bash targets across {} packages, {} go packages, at {}",
        manifest.bash_addrs.len(),
        manifest.bash_packages.len(),
        manifest.go_package_count,
        args.out.display()
    );
    Ok(())
}

async fn run_measure_inprocess(args: MeasureInprocessArgs) -> Result<()> {
    match args.mode {
        MeasureMode::Prepare => {
            inprocess::prepare(&args.corpus, args.scenario)
                .await
                .context("prepare inprocess scenario")?;
        }
        MeasureMode::Once => {
            let manifest = bench_corpus::load_manifest(&args.corpus)
                .context("load corpus manifest (run `corpus` first)")?;
            let ms =
                inprocess::measure_once(&args.corpus, &manifest, args.scenario, args.mutate_seed)
                    .await
                    .context("measure inprocess scenario")?;
            println!("{ms}");
        }
    }
    Ok(())
}

/// Spawns `bin measure-inprocess ...` and, for `Once`, parses its one line
/// of stdout as the elapsed milliseconds. `corpus` must already be
/// absolute — this never changes the child's cwd, but staying consistent
/// with the rest of this crate's (hard-won) path handling avoids relying on
/// that.
fn spawn_measure_inprocess(
    bin: &Path,
    corpus: &Path,
    scenario: inprocess::Scenario,
    mode: MeasureMode,
    mutate_seed: u64,
) -> Result<Option<f64>> {
    let mode_str = match mode {
        MeasureMode::Prepare => "prepare",
        MeasureMode::Once => "once",
    };
    let out = Command::new(bin)
        .arg("measure-inprocess")
        .arg("--corpus")
        .arg(corpus)
        .args(["--scenario", scenario.name(), "--mode", mode_str])
        .args(["--mutate-seed", &mutate_seed.to_string()])
        .output()
        .with_context(|| format!("spawn {} measure-inprocess", bin.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} measure-inprocess failed: status {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            bin.display(),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    match mode {
        MeasureMode::Prepare => Ok(None),
        MeasureMode::Once => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let ms: f64 = stdout.trim().parse().with_context(|| {
                format!(
                    "parse elapsed ms from {} measure-inprocess: {stdout:?}",
                    bin.display()
                )
            })?;
            Ok(Some(ms))
        }
    }
}

async fn run_run(cmd: RunCmd) -> Result<()> {
    match cmd {
        RunCmd::Inprocess(args) => run_inprocess_both(args).await,
        RunCmd::Dist(args) => run_dist_both(args),
    }
}

/// Prepares both sides once, then interleaves `reps` measured rounds
/// (candidate, baseline, candidate, baseline, ...) so a systematic drift on
/// the runner — thermal state ramping up, a noisy neighbor arriving
/// mid-job — lands on both sides symmetrically instead of biasing whichever
/// side happens to run its whole batch second. `blocking spawn` per rep is
/// deliberate: there is nothing to overlap (the whole point is serialized,
/// alternating timing), so async buys nothing here.
async fn run_inprocess_both(args: RunInprocessArgs) -> Result<()> {
    let corpus = args
        .corpus
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.corpus.display()))?;

    for _ in 0..args.warmup {
        spawn_measure_inprocess(
            &args.candidate_bin,
            &corpus,
            args.scenario,
            MeasureMode::Prepare,
            0,
        )
        .context("prepare candidate")?;
        spawn_measure_inprocess(
            &args.baseline_bin,
            &corpus,
            args.scenario,
            MeasureMode::Prepare,
            0,
        )
        .context("prepare baseline")?;
    }

    let mut candidate_ms = Vec::with_capacity(args.reps);
    let mut baseline_ms = Vec::with_capacity(args.reps);
    for i in 0..args.reps {
        let seed = i as u64;
        let cand = spawn_measure_inprocess(
            &args.candidate_bin,
            &corpus,
            args.scenario,
            MeasureMode::Once,
            seed,
        )
        .context("measure candidate")?
        .context("candidate measure-inprocess printed no timing")?;
        candidate_ms.push(cand);
        let base = spawn_measure_inprocess(
            &args.baseline_bin,
            &corpus,
            args.scenario,
            MeasureMode::Once,
            seed,
        )
        .context("measure baseline")?
        .context("baseline measure-inprocess printed no timing")?;
        baseline_ms.push(base);
    }

    write_results(
        &args.out_candidate,
        "inprocess",
        args.scenario.name(),
        candidate_ms,
    )?;
    write_results(
        &args.out_baseline,
        "inprocess",
        args.scenario.name(),
        baseline_ms,
    )?;
    Ok(())
}

/// Same interleaving shape as `run_inprocess_both`, calling straight into
/// `dist::prepare`/`measure_once` (no subprocess spawn of `heph-bench`
/// itself needed — see `RunDistArgs`'s doc comment for why).
fn run_dist_both(args: RunDistArgs) -> Result<()> {
    let corpus = args
        .corpus
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.corpus.display()))?;
    let manifest = bench_corpus::load_manifest(&corpus)
        .context("load corpus manifest (run `corpus` first)")?;

    for _ in 0..args.warmup {
        dist::prepare(&args.candidate_dist, &corpus, args.scenario).context("prepare candidate")?;
        dist::prepare(&args.baseline_dist, &corpus, args.scenario).context("prepare baseline")?;
    }

    let mut candidate_ms = Vec::with_capacity(args.reps);
    let mut baseline_ms = Vec::with_capacity(args.reps);
    for i in 0..args.reps {
        let seed = i as u64;
        candidate_ms.push(
            dist::measure_once(
                &args.candidate_dist,
                &corpus,
                &manifest,
                args.scenario,
                seed,
            )
            .context("measure candidate")?,
        );
        baseline_ms.push(
            dist::measure_once(&args.baseline_dist, &corpus, &manifest, args.scenario, seed)
                .context("measure baseline")?,
        );
    }

    write_results(
        &args.out_candidate,
        "dist",
        args.scenario.name(),
        candidate_ms,
    )?;
    write_results(
        &args.out_baseline,
        "dist",
        args.scenario.name(),
        baseline_ms,
    )?;
    Ok(())
}

fn write_results(out: &Path, tier: &str, scenario: &str, wall_ms: Vec<f64>) -> Result<()> {
    let mean = wall_ms.iter().sum::<f64>() / wall_ms.len().max(1) as f64;
    println!("{scenario}: {} reps, {mean:.1}ms mean", wall_ms.len());
    let results = RunResults {
        tier: tier.to_string(),
        scenarios: vec![ScenarioResult {
            scenario: scenario.to_string(),
            wall_ms,
        }],
    };
    let bytes = serde_json::to_vec_pretty(&results).context("encode results")?;
    std::fs::write(out, bytes).with_context(|| format!("write {}", out.display()))
}

fn run_compare(args: CompareArgs) -> Result<()> {
    let baseline = compare::load(&args.baseline).context("load baseline results")?;
    let candidate = compare::load(&args.candidate).context("load candidate results")?;
    let thresholds =
        compare::load_thresholds(args.thresholds.as_deref()).context("load thresholds")?;

    let verdicts = compare::verdicts(
        &baseline,
        &candidate,
        &thresholds,
        args.default_threshold_pct,
        args.noise_k,
    );
    let report = compare::render_markdown(&verdicts);
    print!("{report}");
    if let Some(out) = &args.out {
        std::fs::write(out, &report).with_context(|| format!("write {}", out.display()))?;
    }
    if let Some(json) = &args.json {
        let bytes = serde_json::to_vec_pretty(&verdicts).context("encode verdicts")?;
        std::fs::write(json, bytes).with_context(|| format!("write {}", json.display()))?;
    }

    let any_regression = verdicts.iter().any(|v| v.regression);
    if any_regression && !args.allow_regression {
        anyhow::bail!("regression detected — see table above");
    }
    Ok(())
}
