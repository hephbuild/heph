mod compare;
mod dist;
mod inprocess;
mod timing;

use anyhow::{Context, Result};
use bench_corpus::CorpusParams;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

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
    /// Time a scenario, in-process (Tier A) or against a real binary (Tier B).
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
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

#[derive(Subcommand)]
enum RunCmd {
    Inprocess(InprocessArgs),
    Dist(DistArgs),
}

#[derive(Args)]
struct InprocessArgs {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, value_enum)]
    scenario: inprocess::Scenario,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 5)]
    reps: usize,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Args)]
struct DistArgs {
    /// Directory containing the prebuilt `heph` binary and plugin cdylibs
    /// (same layout `crates/bin-e2e`'s `Dist` expects). Never rebuilt here.
    #[arg(long)]
    dist: PathBuf,
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, value_enum)]
    scenario: dist::Scenario,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 3)]
    reps: usize,
    #[arg(long)]
    out: PathBuf,
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

async fn run_run(cmd: RunCmd) -> Result<()> {
    match cmd {
        RunCmd::Inprocess(args) => {
            let manifest = bench_corpus::load_manifest(&args.corpus)
                .context("load corpus manifest (run `corpus` first)")?;
            let results = inprocess::run(
                &args.corpus,
                &manifest,
                args.scenario,
                args.warmup,
                args.reps,
            )
            .await
            .context("run inprocess scenario")?;
            write_results(&args.out, &results)
        }
        RunCmd::Dist(args) => {
            let manifest = bench_corpus::load_manifest(&args.corpus)
                .context("load corpus manifest (run `corpus` first)")?;
            let results = dist::run(
                &args.dist,
                &args.corpus,
                &manifest,
                args.scenario,
                args.warmup,
                args.reps,
            )
            .context("run dist scenario")?;
            write_results(&args.out, &results)
        }
    }
}

fn write_results(out: &std::path::Path, results: &timing::RunResults) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(results).context("encode results")?;
    std::fs::write(out, bytes).with_context(|| format!("write {}", out.display()))?;
    for s in &results.scenarios {
        println!(
            "{}: {} reps, {:.1}ms mean",
            s.scenario,
            s.wall_ms.len(),
            s.wall_ms.iter().sum::<f64>() / s.wall_ms.len().max(1) as f64
        );
    }
    Ok(())
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
