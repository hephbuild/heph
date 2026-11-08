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

/// A scenario's entry in the thresholds file: a bare number is the pct floor
/// (`{"cold": 20.0}`); an object can change how noise applies and take the
/// baseline from another scenario (`{"pct": 10.0, "against":
/// "codegen-glob-cold"}`: the candidate's import against the baseline's
/// glob).
#[derive(Debug, Clone, PartialEq)]
pub enum Threshold {
    Pct(f64),
    Bar(Bar),
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
}

impl Threshold {
    fn bar(&self) -> (f64, NoiseRule, Option<&str>) {
        match self {
            Threshold::Pct(pct) => (*pct, NoiseRule::Max, None),
            Threshold::Bar(b) => (b.pct, b.noise, b.against.as_deref()),
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
    pub delta_pct: f64,
    /// The noise floor derived from the baseline's own spread
    /// (`noise_k * stdev/mean * 100`).
    pub noise_band_pct: f64,
    /// `max(configured threshold, noise_band_pct)`, or their sum under
    /// [`NoiseRule::Add`] — the delta actually has to clear this to count as
    /// a regression.
    pub effective_threshold_pct: f64,
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

/// Pure decision function — the thing under test. `threshold_pct` is the
/// configured floor for this scenario; the actual bar a regression must
/// clear is `max(threshold_pct, noise_k * baseline stdev-as-pct-of-mean)`,
/// so a noisy scenario never trips on run-to-run variance alone. Under
/// [`NoiseRule::Add`] the bar is their sum instead.
pub fn decide(
    scenario: &str,
    baseline: &[f64],
    candidate: &[f64],
    threshold_pct: f64,
    noise_k: f64,
    noise: NoiseRule,
) -> ScenarioVerdict {
    let baseline_mean_ms = mean(baseline);
    let candidate_mean_ms = mean(candidate);
    let baseline_stdev = stdev(baseline, baseline_mean_ms);

    let delta_pct = if baseline_mean_ms > 0.0 {
        (candidate_mean_ms - baseline_mean_ms) / baseline_mean_ms * 100.0
    } else {
        0.0
    };
    let noise_band_pct = if baseline_mean_ms > 0.0 {
        noise_k * (baseline_stdev / baseline_mean_ms * 100.0)
    } else {
        0.0
    };
    let effective_threshold_pct = match noise {
        NoiseRule::Max => threshold_pct.max(noise_band_pct),
        NoiseRule::Add => threshold_pct + noise_band_pct,
    };

    ScenarioVerdict {
        scenario: scenario.to_string(),
        against: None,
        baseline_reps: baseline.len(),
        candidate_reps: candidate.len(),
        baseline_mean_ms,
        candidate_mean_ms,
        delta_pct,
        noise_band_pct,
        effective_threshold_pct,
        regression: delta_pct > effective_threshold_pct,
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

/// One verdict per candidate scenario whose baseline (its own, or the
/// `against` one) is present; the rest are skipped.
pub fn verdicts(
    baseline: &RunResults,
    candidate: &RunResults,
    thresholds: &HashMap<String, Threshold>,
    default_threshold_pct: f64,
    noise_k: f64,
) -> Vec<ScenarioVerdict> {
    let mut out = Vec::new();
    for c in &candidate.scenarios {
        let (threshold_pct, noise, against) = thresholds.get(&c.scenario).map_or(
            (default_threshold_pct, NoiseRule::Max, None),
            Threshold::bar,
        );
        let base_name = against.unwrap_or(&c.scenario);
        let Some(b) = baseline.scenarios.iter().find(|b| b.scenario == base_name) else {
            continue;
        };
        let mut v = decide(
            &c.scenario,
            &b.wall_ms,
            &c.wall_ms,
            threshold_pct,
            noise_k,
            noise,
        );
        v.against = against.map(str::to_string);
        out.push(v);
    }
    out
}

pub fn render_markdown(verdicts: &[ScenarioVerdict]) -> String {
    let mut out = String::from(
        "| Scenario | Baseline (ms) | Candidate (ms) | Delta | Threshold | Verdict |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---|\n");
    for v in verdicts {
        out.push_str(&format!(
            "| {}{} | {:.1} (n={}) | {:.1} (n={}) | {:+.1}% | {:.1}% | {} |\n",
            v.scenario,
            v.against
                .as_deref()
                .map(|a| format!(" (vs baseline {a})"))
                .unwrap_or_default(),
            v.baseline_mean_ms,
            v.baseline_reps,
            v.candidate_mean_ms,
            v.candidate_reps,
            v.delta_pct,
            v.effective_threshold_pct,
            if v.regression { "REGRESSION" } else { "ok" },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
            parsed.get("codegen-warm-build").map(Threshold::bar),
            Some((2.0, NoiseRule::Add, None))
        );
        assert_eq!(
            parsed.get("codegen-import").map(Threshold::bar),
            Some((10.0, NoiseRule::Max, Some("codegen-glob-cold")))
        );
        serde_json::from_str::<HashMap<String, Threshold>>(r#"{"x": {"pct": 1.0, "agains": "y"}}"#)
            .expect_err("a misspelt key is refused");
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
                pct: 10.0,
                noise: NoiseRule::Max,
                against: Some("codegen-glob-cold".into()),
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
