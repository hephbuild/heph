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
    /// 0 = no go/ subtree (Tier B `--lang go`/`both` scenarios need this > 0).
    #[arg(long, default_value_t = 0)]
    go_packages: usize,
    #[arg(long, default_value_t = 4)]
    go_max_depth: usize,
    /// 0 = no js/ subtree (Tier B `--lang js`/`both` scenarios need this > 0).
    #[arg(long, default_value_t = 0)]
    js_packages: usize,
    #[arg(long, default_value_t = 4)]
    js_max_depth: usize,
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

/// Unlike Tier A, `heph`/the plugin cdylibs are already prebuilt artifacts on
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
    /// Which language(s)' plugin cdylib to build+time. Defaults to `go` —
    /// the only language Tier B measured before this flag existed, so an
    /// existing caller that never passes `--lang` (CI's `perf.yml` today)
    /// keeps building and reporting exactly what it always has.
    #[arg(long, value_enum, default_value_t = LangArg::Go)]
    lang: LangArg,
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    #[arg(long, default_value_t = 3)]
    reps: usize,
    #[arg(long)]
    out_candidate: PathBuf,
    #[arg(long)]
    out_baseline: PathBuf,
}

/// `run dist`'s language selector. `Both` measures `go` and `js` in the same
/// invocation — one `.hephconfig` with both plugins loaded, two separately
/// timed `heph r -e '//<lang>/...'` calls per rep — and reports each as its
/// own [`ScenarioResult`] rather than summing/averaging them together (see
/// `write_dist_results`).
///
/// Caveat: because `Both` shares one `.hephconfig` declaring both plugins,
/// each language's timed build also pays the *other* language's plugin
/// dlopen + ABI-negotiation + checksum-verify cost as fixed per-process
/// overhead — a `--lang both` js number is not apples-to-apples with a
/// solo `--lang js` run (what CI's `distjs` tier actually uses today). Not
/// exercised by CI (`perf.yml` never passes `--lang both`), so this is
/// latent, not live; fixing it for real would mean writing a separate
/// single-plugin `.hephconfig` per language even under `Both`, which isn't
/// done here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum LangArg {
    Go,
    Js,
    Both,
}

impl LangArg {
    fn langs(self) -> Vec<&'static dist::Lang> {
        match self {
            LangArg::Go => vec![&dist::GO],
            LangArg::Js => vec![&dist::JS],
            LangArg::Both => vec![&dist::GO, &dist::JS],
        }
    }
}

impl std::fmt::Display for LangArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LangArg::Go => "go",
            LangArg::Js => "js",
            LangArg::Both => "both",
        })
    }
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
        js_packages: args.js_packages,
        js_max_depth: args.js_max_depth,
        ..Default::default()
    };
    let manifest = bench_corpus::generate(&params, &args.out).context("generate corpus")?;
    println!(
        "generated {} bash targets across {} packages, {} go packages, {} js packages, at {}",
        manifest.bash_addrs.len(),
        manifest.bash_packages.len(),
        manifest.go_package_count,
        manifest.js_package_count,
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
        vec![(args.scenario.name().to_string(), candidate_ms)],
    )?;
    write_results(
        &args.out_baseline,
        "inprocess",
        vec![(args.scenario.name().to_string(), baseline_ms)],
    )?;
    Ok(())
}

/// Same interleaving shape as `run_inprocess_both`, calling straight into
/// `dist::prepare`/`measure_once` (no subprocess spawn of `heph-bench`
/// itself needed — see `RunDistArgs`'s doc comment for why).
///
/// `--lang both` measures both languages inside the same interleaved
/// candidate/baseline loop rather than running two separate passes: each
/// `measure_once` call already builds every requested language's tree back
/// to back for one side, so interleaving stays per-*rep*, not per-language —
/// a systematic drift across the rep still lands on both sides symmetrically
/// for both languages, not just the first one measured.
fn run_dist_both(args: RunDistArgs) -> Result<()> {
    let corpus = args
        .corpus
        .canonicalize()
        .with_context(|| format!("canonicalize {}", args.corpus.display()))?;
    let manifest = bench_corpus::load_manifest(&corpus)
        .context("load corpus manifest (run `corpus` first)")?;
    let langs = args.lang.langs();

    for _ in 0..args.warmup {
        dist::prepare(
            &args.candidate_dist,
            &corpus,
            &manifest,
            args.scenario,
            &langs,
        )
        .context("prepare candidate")?;
        dist::prepare(
            &args.baseline_dist,
            &corpus,
            &manifest,
            args.scenario,
            &langs,
        )
        .context("prepare baseline")?;
    }

    // Indexed the same as `langs` — `dist::measure_once` returns its
    // per-language results in that exact order (see its doc comment), so
    // these accumulate without a name lookup.
    let mut candidate_ms: Vec<Vec<f64>> = langs
        .iter()
        .map(|_| Vec::with_capacity(args.reps))
        .collect();
    let mut baseline_ms: Vec<Vec<f64>> = langs
        .iter()
        .map(|_| Vec::with_capacity(args.reps))
        .collect();
    for i in 0..args.reps {
        let seed = i as u64;
        let cand = dist::measure_once(
            &args.candidate_dist,
            &corpus,
            &manifest,
            args.scenario,
            seed,
            &langs,
        )
        .context("measure candidate")?;
        for (slot, (_, ms)) in candidate_ms.iter_mut().zip(cand) {
            slot.push(ms);
        }
        let base = dist::measure_once(
            &args.baseline_dist,
            &corpus,
            &manifest,
            args.scenario,
            seed,
            &langs,
        )
        .context("measure baseline")?;
        for (slot, (_, ms)) in baseline_ms.iter_mut().zip(base) {
            slot.push(ms);
        }
    }

    write_dist_results(&args.out_candidate, args.scenario, &langs, candidate_ms)?;
    write_dist_results(&args.out_baseline, args.scenario, &langs, baseline_ms)?;
    Ok(())
}

/// `--lang go` (the default) or `--lang js` alone reports the bare scenario
/// name (`"cold"`, unchanged from before this flag existed — see
/// `RunDistArgs::lang`'s doc comment on why that matters for an existing
/// caller). `--lang both` reports two scenario rows, `"<scenario>-go"` and
/// `"<scenario>-js"`, so a regression in one language is never averaged away
/// by the other — see `crates/bench/src/timing.rs`'s `RunResults` doc: it
/// already carries a `Vec<ScenarioResult>` for exactly this.
fn write_dist_results(
    out: &Path,
    scenario: dist::Scenario,
    langs: &[&dist::Lang],
    wall_ms_by_lang: Vec<Vec<f64>>,
) -> Result<()> {
    let scenarios = langs
        .iter()
        .zip(wall_ms_by_lang)
        .map(|(lang, wall_ms)| {
            let name = if langs.len() == 1 {
                scenario.name().to_string()
            } else {
                format!("{}-{}", scenario.name(), lang.name)
            };
            (name, wall_ms)
        })
        .collect();
    write_results(out, "dist", scenarios)
}

fn write_results(out: &Path, tier: &str, scenarios: Vec<(String, Vec<f64>)>) -> Result<()> {
    for (scenario, wall_ms) in &scenarios {
        let mean = wall_ms.iter().sum::<f64>() / wall_ms.len().max(1) as f64;
        println!("{scenario}: {} reps, {mean:.1}ms mean", wall_ms.len());
    }
    let results = RunResults {
        tier: tier.to_string(),
        scenarios: scenarios
            .into_iter()
            .map(|(scenario, wall_ms)| ScenarioResult { scenario, wall_ms })
            .collect(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `--lang go` (the default, and the only case `run dist` measured
    /// before `--lang` existed) must keep reporting the bare scenario name —
    /// a caller (CI's `perf.yml`) reading `"cold"`/`"full-hit"`/etc out of
    /// the result JSON, or matching thresholds keyed by those bare names,
    /// must see byte-identical scenario names whether or not `--lang` is
    /// ever passed.
    #[test]
    fn write_dist_results_single_lang_keeps_bare_scenario_name() {
        let out = tempfile::NamedTempFile::new().expect("tempfile");
        write_dist_results(
            out.path(),
            dist::Scenario::Cold,
            &[&dist::GO],
            vec![vec![12.0, 14.0]],
        )
        .expect("write_dist_results");

        let results: RunResults =
            serde_json::from_slice(&std::fs::read(out.path()).expect("read results"))
                .expect("parse results");
        assert_eq!(results.scenarios.len(), 1);
        assert_eq!(results.scenarios[0].scenario, "cold");
        assert_eq!(results.scenarios[0].wall_ms, vec![12.0, 14.0]);
    }

    /// `--lang both` must report go and js as two DISTINCT scenario rows,
    /// each carrying only its own language's wall-clock samples — never
    /// summed or averaged into one number (see `RunDistArgs::lang`'s doc
    /// comment and this crate's `timing.rs`). A regression that only exists
    /// on one side would be invisible if the two got merged.
    #[test]
    fn write_dist_results_both_langs_reports_two_distinct_scenarios_never_merged() {
        let out = tempfile::NamedTempFile::new().expect("tempfile");
        write_dist_results(
            out.path(),
            dist::Scenario::FullHit,
            &[&dist::GO, &dist::JS],
            vec![vec![100.0, 110.0], vec![5.0, 6.0]],
        )
        .expect("write_dist_results");

        let results: RunResults =
            serde_json::from_slice(&std::fs::read(out.path()).expect("read results"))
                .expect("parse results");
        assert_eq!(results.scenarios.len(), 2);
        assert_eq!(results.scenarios[0].scenario, "full-hit-go");
        assert_eq!(results.scenarios[0].wall_ms, vec![100.0, 110.0]);
        assert_eq!(results.scenarios[1].scenario, "full-hit-js");
        assert_eq!(results.scenarios[1].wall_ms, vec![5.0, 6.0]);
    }

    #[test]
    fn lang_arg_selects_expected_lang_set() {
        assert_eq!(LangArg::Go.langs().len(), 1);
        assert_eq!(LangArg::Go.langs()[0].name, "go");
        assert_eq!(LangArg::Js.langs().len(), 1);
        assert_eq!(LangArg::Js.langs()[0].name, "js");
        let both: Vec<&str> = LangArg::Both.langs().iter().map(|l| l.name).collect();
        assert_eq!(both, vec!["go", "js"]);
    }

    /// `heph-bench corpus --js-packages N` must actually reach
    /// `bench_corpus::generate`'s `js_packages` param — this is the CLI
    /// wiring gap this task found (the corpus subcommand's `GenerateArgs`
    /// had no `--js-packages`/`--js-max-depth` flags at all before this
    /// change, even though `bench_corpus::CorpusParams` already supported
    /// them). Goes through `Cli::try_parse_from` → `run_corpus`, the real
    /// CLI→`CorpusParams` mapping, not a hand-built `CorpusParams` — a
    /// regression that drops `args.js_packages`/`args.js_max_depth` from
    /// `run_corpus`'s field mapping (main.rs) would still leave a
    /// hand-built `CorpusParams` test green, which is exactly what this
    /// test replaces. `--go-packages` is left at 0 so this test needs no
    /// network (go corpus generation shells out to `go mod tidy`).
    #[test]
    fn generate_args_wires_js_packages_into_corpus_params() {
        let out = tempfile::tempdir().expect("tempdir");
        let cli = Cli::try_parse_from([
            "heph-bench",
            "corpus",
            "--targets",
            "20",
            "--packages",
            "4",
            "--layers",
            "2",
            "--fan-out",
            "2",
            "--js-packages",
            "6",
            "--js-max-depth",
            "2",
            "--out",
            out.path().to_str().expect("tempdir path is utf8"),
        ])
        .expect("parse `heph-bench corpus --js-packages 6 ...`");
        let Cmd::Corpus(args) = cli.cmd else {
            panic!("expected Cmd::Corpus, got a different subcommand");
        };
        run_corpus(args).expect("run_corpus");

        let manifest = bench_corpus::load_manifest(out.path()).expect("load generated manifest");
        assert_eq!(manifest.js_package_count, 6);
        assert_eq!(manifest.go_package_count, 0, "go_packages defaulted to 0");
    }
}
