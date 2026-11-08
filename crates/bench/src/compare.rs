//! Regression decision: baseline (N-1) vs candidate (N) results, per
//! scenario, matched against a noise floor derived from the baseline's own
//! rep spread — the `perf-measurement` agent's doctrine ("any delta inside
//! noise band is noise") applied mechanically instead of by eyeballing.

use crate::timing::RunResults;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// How a scenario's noise band combines with its configured pct.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoiseRule {
    /// `max(pct, band)`: the band only widens a bar it already exceeds.
    #[default]
    Max,
    /// `pct + band`: the bar is "pct over the baseline, plus the baseline's
    /// own run-to-run spread" (the codegen warm build's).
    Add,
}

/// Which statistic of each side's reps a bar compares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stat {
    /// The mean: every scenario that does not say otherwise.
    #[default]
    Mean,
    /// The median, which one slow rep cannot move: the codegen bars' (spec:
    /// "the median of 5 interleaved runs").
    Median,
}

/// A scenario's entry in the thresholds file: a bare number is the pct floor
/// (`{"cold": 20.0}`); an object can change how noise applies, which
/// statistic is compared, and take the baseline from another scenario
/// (`{"pct": 10.0, "against": "codegen-glob-cold"}`: the candidate's import
/// against the baseline's glob); a list holds several bars for one
/// scenario, one row each, e.g. a gating bar and a reported one.
#[derive(Debug, Clone, PartialEq)]
pub enum Threshold {
    Pct(f64),
    Bar(Bar),
    Bars(Vec<Bar>),
}

impl<'de> Deserialize<'de> for Threshold {
    /// Through a `Value`, not `#[serde(untagged)]`: with serde_json's
    /// `arbitrary_precision` on in the workspace, an untagged `f64` variant
    /// never matches a JSON number.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if let Some(pct) = v.as_f64() {
            return Ok(Threshold::Pct(pct));
        }
        if v.is_array() {
            return serde_json::from_value(v)
                .map(Threshold::Bars)
                .map_err(serde::de::Error::custom);
        }
        serde_json::from_value(v)
            .map(Threshold::Bar)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bar {
    pub pct: f64,
    #[serde(default)]
    pub noise: NoiseRule,
    /// The baseline scenario to compare against, when not the same-named
    /// one.
    #[serde(default)]
    pub against: Option<String>,
    #[serde(default)]
    pub stat: Stat,
    /// `false`: the row is reported, never a regression.
    #[serde(default = "gate_default")]
    pub gate: bool,
}

fn gate_default() -> bool {
    true
}

impl Bar {
    /// A gating bar on the mean against the same-named baseline: what a
    /// bare number in the thresholds file means.
    pub fn pct(pct: f64) -> Self {
        Bar {
            pct,
            noise: NoiseRule::Max,
            against: None,
            stat: Stat::Mean,
            gate: true,
        }
    }
}

impl Threshold {
    fn bars(&self) -> Vec<Bar> {
        match self {
            Threshold::Pct(pct) => vec![Bar::pct(*pct)],
            Threshold::Bar(b) => vec![b.clone()],
            Threshold::Bars(bars) => bars.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioVerdict {
    pub scenario: String,
    /// The baseline scenario the candidate was compared with, when it is not
    /// `scenario` itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<String>,
    pub baseline_reps: usize,
    pub candidate_reps: usize,
    pub baseline_mean_ms: f64,
    pub candidate_mean_ms: f64,
    /// The statistic `delta_pct` compares.
    pub stat: Stat,
    /// Each side's `stat`: the mean, or the median.
    pub baseline_ms: f64,
    pub candidate_ms: f64,
    /// `candidate_ms` over `baseline_ms`, as a pct change.
    pub delta_pct: f64,
    /// The noise floor derived from the baseline's own spread
    /// (`noise_k * stdev/mean * 100`).
    pub noise_band_pct: f64,
    /// `max(configured threshold, noise_band_pct)`, or their sum under
    /// [`NoiseRule::Add`] — the delta actually has to clear this to count as
    /// a regression.
    pub effective_threshold_pct: f64,
    /// `false` for a reported row: `regression` is then always `false`, and
    /// `over_bar` says whether it would have been one.
    pub gate: bool,
    pub over_bar: bool,
    pub regression: bool,
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn stdev(v: &[f64], m: f64) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let var = v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() as f64 - 1.0);
    var.sqrt()
}

fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match (v.get(n / 2), n % 2) {
        (None, _) => 0.0,
        (Some(mid), 1) => *mid,
        (Some(hi), _) => (v.get(n / 2 - 1).copied().unwrap_or(*hi) + hi) / 2.0,
    }
}

impl Stat {
    fn of(self, v: &[f64]) -> f64 {
        match self {
            Stat::Mean => mean(v),
            Stat::Median => median(v),
        }
    }
}

/// Pure decision function — the thing under test. `bar.pct` is the
/// configured floor for this scenario; the actual bar a regression must
/// clear is `max(pct, noise_k * baseline stdev-as-pct-of-mean)`, so a noisy
/// scenario never trips on run-to-run variance alone. Under
/// [`NoiseRule::Add`] the bar is their sum instead. The delta is between
/// each side's `bar.stat`; the noise band is the baseline's spread whatever
/// the stat. A bar with `gate: false` is never a regression.
pub fn decide_bar(
    scenario: &str,
    baseline: &[f64],
    candidate: &[f64],
    bar: &Bar,
    noise_k: f64,
) -> ScenarioVerdict {
    let baseline_mean_ms = mean(baseline);
    let candidate_mean_ms = mean(candidate);
    let baseline_stdev = stdev(baseline, baseline_mean_ms);
    let baseline_ms = bar.stat.of(baseline);
    let candidate_ms = bar.stat.of(candidate);

    let delta_pct = if baseline_ms > 0.0 {
        (candidate_ms - baseline_ms) / baseline_ms * 100.0
    } else {
        0.0
    };
    let noise_band_pct = if baseline_mean_ms > 0.0 {
        noise_k * (baseline_stdev / baseline_mean_ms * 100.0)
    } else {
        0.0
    };
    let effective_threshold_pct = match bar.noise {
        NoiseRule::Max => bar.pct.max(noise_band_pct),
        NoiseRule::Add => bar.pct + noise_band_pct,
    };
    let over_bar = delta_pct > effective_threshold_pct;

    ScenarioVerdict {
        scenario: scenario.to_string(),
        against: bar.against.clone(),
        baseline_reps: baseline.len(),
        candidate_reps: candidate.len(),
        baseline_mean_ms,
        candidate_mean_ms,
        stat: bar.stat,
        baseline_ms,
        candidate_ms,
        delta_pct,
        noise_band_pct,
        effective_threshold_pct,
        gate: bar.gate,
        over_bar,
        regression: bar.gate && over_bar,
    }
}

pub fn load(path: &Path) -> Result<RunResults> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Per-scenario threshold overrides, e.g. `{"scale": 12.0, "cold": 15.0}` —
/// scenarios with different natural variance get different floors (see
/// [`Threshold`] for the object form). Missing entries fall back to
/// `default_threshold_pct`.
pub fn load_thresholds(path: Option<&Path>) -> Result<HashMap<String, Threshold>> {
    let Some(path) = path else {
        return Ok(HashMap::new());
    };
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// One verdict per bar of each candidate scenario whose baseline (its own,
/// or the `against` one) is present; the rest are skipped.
pub fn verdicts(
    baseline: &RunResults,
    candidate: &RunResults,
    thresholds: &HashMap<String, Threshold>,
    default_threshold_pct: f64,
    noise_k: f64,
) -> Vec<ScenarioVerdict> {
    let mut out = Vec::new();
    for c in &candidate.scenarios {
        let bars = thresholds
            .get(&c.scenario)
            .map_or_else(|| vec![Bar::pct(default_threshold_pct)], Threshold::bars);
        for bar in &bars {
            let base_name = bar.against.as_deref().unwrap_or(&c.scenario);
            let Some(b) = baseline.scenarios.iter().find(|b| b.scenario == base_name) else {
                continue;
            };
            out.push(decide_bar(&c.scenario, &b.wall_ms, &c.wall_ms, bar, noise_k));
        }
    }
    out
}

pub fn render_markdown(verdicts: &[ScenarioVerdict]) -> String {
    let mut out = String::from(
        "| Scenario | Baseline (ms) | Candidate (ms) | Delta | Threshold | Verdict |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---|\n");
    for v in verdicts {
        let stat = match v.stat {
            Stat::Mean => "",
            Stat::Median => "median, ",
        };
        let verdict = match (v.gate, v.over_bar) {
            (true, true) => "REGRESSION",
            (true, false) => "ok",
            (false, true) => "over bar (reported, not gating)",
            (false, false) => "ok (reported, not gating)",
        };
        out.push_str(&format!(
            "| {}{} | {:.1} ({stat}n={}) | {:.1} ({stat}n={}) | {:+.1}% | {:.1}% | {verdict} |\n",
            v.scenario,
            v.against
                .as_deref()
                .map(|a| format!(" (vs baseline {a})"))
                .unwrap_or_default(),
            v.baseline_ms,
            v.baseline_reps,
            v.candidate_ms,
            v.candidate_reps,
            v.delta_pct,
            v.effective_threshold_pct,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`decide_bar`] with a gating bar on the mean.
    fn decide(
        scenario: &str,
        baseline: &[f64],
        candidate: &[f64],
        threshold_pct: f64,
        noise_k: f64,
        noise: NoiseRule,
    ) -> ScenarioVerdict {
        let bar = Bar {
            noise,
            ..Bar::pct(threshold_pct)
        };
        decide_bar(scenario, baseline, candidate, &bar, noise_k)
    }

    #[test]
    fn stable_baseline_small_delta_is_not_a_regression() {
        let v = decide(
            "cold",
            &[100.0, 101.0, 99.0, 100.0],
            &[103.0, 104.0, 102.0],
            8.0,
            2.0,
            NoiseRule::Max,
        );
        assert!(!v.regression, "{v:?}");
    }

    #[test]
    fn large_delta_past_threshold_is_a_regression() {
        let v = decide(
            "cold",
            &[100.0, 101.0, 99.0, 100.0],
            &[130.0, 129.0, 131.0],
            8.0,
            2.0,
            NoiseRule::Max,
        );
        assert!(v.regression, "{v:?}");
        assert!(v.delta_pct > 25.0);
    }

    #[test]
    fn noisy_baseline_widens_the_effective_threshold() {
        // Same +9% delta: tight baseline flags it, noisy baseline (whose own
        // spread already covers 9%) does not.
        let tight = decide(
            "x",
            &[100.0, 100.0, 100.0, 100.0],
            &[109.0],
            8.0,
            2.0,
            NoiseRule::Max,
        );
        let noisy = decide(
            "x",
            &[80.0, 100.0, 120.0, 100.0],
            &[109.0],
            8.0,
            2.0,
            NoiseRule::Max,
        );
        assert!(tight.regression, "{tight:?}");
        assert!(!noisy.regression, "{noisy:?}");
    }

    #[test]
    fn faster_candidate_is_never_a_regression() {
        let v = decide(
            "cold",
            &[100.0, 100.0, 100.0],
            &[70.0, 71.0, 69.0],
            8.0,
            2.0,
            NoiseRule::Max,
        );
        assert!(!v.regression, "{v:?}");
        assert!(v.delta_pct < 0.0);
    }

    #[test]
    fn missing_scenario_in_candidate_is_skipped_not_a_crash() {
        let baseline = RunResults {
            tier: "inprocess".into(),
            scenarios: vec![crate::timing::ScenarioResult {
                scenario: "cold".into(),
                wall_ms: vec![100.0],
            }],
        };
        let candidate = RunResults {
            tier: "inprocess".into(),
            scenarios: vec![],
        };
        let out = verdicts(&baseline, &candidate, &HashMap::new(), 8.0, 2.0);
        assert!(out.is_empty());
    }

    fn results(scenarios: &[(&str, &[f64])]) -> RunResults {
        RunResults {
            tier: "dist".into(),
            scenarios: scenarios
                .iter()
                .map(|(s, ms)| crate::timing::ScenarioResult {
                    scenario: (*s).into(),
                    wall_ms: ms.to_vec(),
                })
                .collect(),
        }
    }

    /// Both forms parse; an unknown key in the object form is an error, not
    /// a bar silently dropped to the default.
    #[test]
    fn thresholds_file_takes_numbers_and_bars() {
        let parsed: HashMap<String, Threshold> = serde_json::from_str(
            r#"{"cold": 20.0,
                "codegen-warm-build": {"pct": 2.0, "noise": "add"},
                "codegen-import": {"pct": 10.0, "against": "codegen-glob-cold"}}"#,
        )
        .expect("parse");
        assert_eq!(parsed.get("cold"), Some(&Threshold::Pct(20.0)));
        assert_eq!(
            parsed.get("codegen-warm-build").map(Threshold::bars),
            Some(vec![Bar {
                noise: NoiseRule::Add,
                ..Bar::pct(2.0)
            }])
        );
        assert_eq!(
            parsed.get("codegen-import").map(Threshold::bars),
            Some(vec![Bar {
                against: Some("codegen-glob-cold".into()),
                ..Bar::pct(10.0)
            }])
        );
        serde_json::from_str::<HashMap<String, Threshold>>(r#"{"x": {"pct": 1.0, "agains": "y"}}"#)
            .expect_err("a misspelt key is refused");
        serde_json::from_str::<HashMap<String, Threshold>>(r#"{"x": {"pct": 1.0, "stat": "p90"}}"#)
            .expect_err("an unknown stat is refused");
    }

    /// `stat` and `gate` parse, a list is several bars, and the file in the
    /// repo parses with the codegen bars on the median.
    #[test]
    fn thresholds_file_takes_stat_gate_and_lists() {
        let parsed: HashMap<String, Threshold> = serde_json::from_str(
            r#"{"g": [{"pct": 25.0, "stat": "median", "against": "g-free"},
                      {"pct": 25.0, "stat": "median", "gate": false}]}"#,
        )
        .expect("parse");
        let bars = parsed.get("g").map(Threshold::bars).expect("g");
        assert_eq!(
            bars,
            vec![
                Bar {
                    stat: Stat::Median,
                    against: Some("g-free".into()),
                    ..Bar::pct(25.0)
                },
                Bar {
                    stat: Stat::Median,
                    gate: false,
                    ..Bar::pct(25.0)
                },
            ]
        );

        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/perfbench-thresholds.json");
        let repo = load_thresholds(Some(&repo)).expect("the repo's thresholds file parses");
        for (scenario, t) in &repo {
            for bar in t.bars() {
                let codegen = scenario.starts_with("codegen-");
                assert_eq!(
                    bar.stat == Stat::Median,
                    codegen,
                    "{scenario}: the codegen bars, and only they, are on the median"
                );
            }
        }
        let gating: Vec<Option<String>> = repo
            .get("codegen-glob-cold")
            .map(Threshold::bars)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| b.gate)
            .map(|b| b.against)
            .collect();
        assert_eq!(
            gating,
            [Some("codegen-glob-cold-probe-free".to_string())],
            "the cold glob gates on the probe-free baseline alone"
        );
    }

    /// The median ignores one slow rep the mean does not: the same samples
    /// regress on the mean and pass on the median.
    #[test]
    fn median_bar_is_not_moved_by_one_slow_rep() {
        let base = [100.0, 100.0, 101.0, 99.0, 100.0];
        let cand = [102.0, 101.0, 400.0, 103.0, 102.0];
        let on_mean = decide("x", &base, &cand, 10.0, 2.0, NoiseRule::Max);
        assert!(on_mean.regression, "{on_mean:?}");
        let median_bar = Bar {
            stat: Stat::Median,
            ..Bar::pct(10.0)
        };
        let on_median = decide_bar("x", &base, &cand, &median_bar, 2.0);
        assert!(!on_median.regression, "{on_median:?}");
        assert!((on_median.candidate_ms - 102.0).abs() < 1e-9);
        assert!((on_median.baseline_ms - 100.0).abs() < 1e-9);
        assert!((on_median.candidate_mean_ms - 161.6).abs() < 1e-9);

        // And a real shift moves it.
        let shifted = [130.0, 131.0, 129.0, 130.0, 400.0];
        assert!(decide_bar("x", &base, &shifted, &median_bar, 2.0).regression);
    }

    #[test]
    fn median_of_even_and_odd_and_empty() {
        assert!((median(&[3.0, 1.0, 2.0]) - 2.0).abs() < f64::EPSILON);
        assert!((median(&[4.0, 1.0, 3.0, 2.0]) - 2.5).abs() < f64::EPSILON);
        assert!(median(&[]).abs() < f64::EPSILON);
    }

    /// A scenario with a gating bar and a reported one gets a row for each;
    /// the reported row over its bar says so and is never a regression, so
    /// the workflow's grep for REGRESSION never fails on it.
    #[test]
    fn a_reported_bar_is_a_row_but_never_a_regression() {
        let baseline = results(&[
            ("codegen-glob-cold", &[100.0, 100.0, 100.0, 100.0, 100.0]),
            ("codegen-glob-cold-probe-free", &[90.0, 90.0, 90.0, 90.0, 90.0]),
        ]);
        let candidate = results(&[("codegen-glob-cold", &[140.0, 140.0, 140.0, 140.0, 140.0])]);
        let thresholds = HashMap::from([(
            "codegen-glob-cold".to_string(),
            Threshold::Bars(vec![
                Bar {
                    stat: Stat::Median,
                    against: Some("codegen-glob-cold-probe-free".into()),
                    ..Bar::pct(25.0)
                },
                Bar {
                    stat: Stat::Median,
                    gate: false,
                    ..Bar::pct(25.0)
                },
            ]),
        )]);
        let out = verdicts(&baseline, &candidate, &thresholds, 8.0, 2.0);
        let [gating, reported] = out.as_slice() else {
            panic!("two rows: {out:?}")
        };
        assert!(gating.regression, "+55% over the probe-free baseline: {gating:?}");
        assert!(reported.over_bar && !reported.regression, "{reported:?}");
        let md = render_markdown(&out);
        assert_eq!(md.matches("REGRESSION").count(), 1, "{md}");
        assert!(md.contains("over bar (reported, not gating)"), "{md}");
        assert!(md.contains("(median, n=5)"), "{md}");

        let quiet = results(&[("codegen-glob-cold", &[100.0; 5])]);
        let out = verdicts(&baseline, &quiet, &thresholds, 8.0, 2.0);
        assert!(out.iter().all(|v| !v.regression), "{out:?}");
        assert!(render_markdown(&out).contains("ok (reported, not gating)"));
    }

    /// `pct + band`: a delta inside the noise band but past `pct` alone is
    /// fine under `add`, and one past both is a regression either way.
    #[test]
    fn additive_noise_adds_the_spread_to_the_bar() {
        let base = [100.0, 98.0, 102.0, 100.0];
        // Spread 1.63% at k = 1: the bar is 3.63%. +3% clears 2% alone.
        let within = decide("w", &base, &[103.0], 2.0, 1.0, NoiseRule::Add);
        assert!(!within.regression, "{within:?}");
        let max_rule = decide("w", &base, &[103.0], 2.0, 1.0, NoiseRule::Max);
        assert!(max_rule.regression, "{max_rule:?}");
        assert!((within.effective_threshold_pct - (2.0 + within.noise_band_pct)).abs() < 1e-9);
        let past = decide("w", &base, &[106.0], 2.0, 1.0, NoiseRule::Add);
        assert!(past.regression, "{past:?}");
    }

    /// A candidate-only scenario is decided against another scenario's
    /// baseline, and says so; without that baseline it is skipped.
    #[test]
    fn against_takes_the_baseline_from_the_named_scenario() {
        let baseline = results(&[("codegen-glob-cold", &[100.0, 100.0, 100.0])]);
        let candidate = results(&[("codegen-import", &[112.0, 112.0])]);
        let thresholds = HashMap::from([(
            "codegen-import".to_string(),
            Threshold::Bar(Bar {
                against: Some("codegen-glob-cold".into()),
                ..Bar::pct(10.0)
            }),
        )]);
        let out = verdicts(&baseline, &candidate, &thresholds, 8.0, 2.0);
        let [v] = out.as_slice() else {
            panic!("one verdict: {out:?}")
        };
        assert_eq!(v.scenario, "codegen-import");
        assert_eq!(v.against.as_deref(), Some("codegen-glob-cold"));
        assert!(v.regression, "+12% past a 10% bar: {v:?}");
        assert!(render_markdown(&out).contains("codegen-import (vs baseline codegen-glob-cold)"));

        let none = verdicts(&results(&[]), &candidate, &thresholds, 8.0, 2.0);
        assert!(none.is_empty());
    }
}
