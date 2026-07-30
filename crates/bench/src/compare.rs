//! Regression decision: baseline (N-1) vs candidate (N) results, per
//! scenario, matched against a noise floor derived from the baseline's own
//! rep spread — the `perf-measurement` agent's doctrine ("any delta inside
//! noise band is noise") applied mechanically instead of by eyeballing.

use crate::timing::RunResults;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioVerdict {
    pub scenario: String,
    pub baseline_reps: usize,
    pub candidate_reps: usize,
    pub baseline_mean_ms: f64,
    pub candidate_mean_ms: f64,
    pub delta_pct: f64,
    /// The noise floor derived from the baseline's own spread
    /// (`noise_k * stdev/mean * 100`).
    pub noise_band_pct: f64,
    /// `max(configured threshold, noise_band_pct)` — the delta actually has
    /// to clear this to count as a regression.
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
/// so a noisy scenario never trips on run-to-run variance alone.
pub fn decide(
    scenario: &str,
    baseline: &[f64],
    candidate: &[f64],
    threshold_pct: f64,
    noise_k: f64,
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
    let effective_threshold_pct = threshold_pct.max(noise_band_pct);

    ScenarioVerdict {
        scenario: scenario.to_string(),
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
/// scenarios with different natural variance get different floors. Missing
/// entries fall back to `default_threshold_pct`.
pub fn load_thresholds(path: Option<&Path>) -> Result<HashMap<String, f64>> {
    let Some(path) = path else {
        return Ok(HashMap::new());
    };
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn verdicts(
    baseline: &RunResults,
    candidate: &RunResults,
    thresholds: &HashMap<String, f64>,
    default_threshold_pct: f64,
    noise_k: f64,
) -> Vec<ScenarioVerdict> {
    let mut out = Vec::new();
    for b in &baseline.scenarios {
        let Some(c) = candidate
            .scenarios
            .iter()
            .find(|c| c.scenario == b.scenario)
        else {
            continue;
        };
        let threshold_pct = thresholds
            .get(&b.scenario)
            .copied()
            .unwrap_or(default_threshold_pct);
        out.push(decide(
            &b.scenario,
            &b.wall_ms,
            &c.wall_ms,
            threshold_pct,
            noise_k,
        ));
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
            "| {} | {:.1} (n={}) | {:.1} (n={}) | {:+.1}% | {:.1}% | {} |\n",
            v.scenario,
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
        );
        assert!(v.regression, "{v:?}");
        assert!(v.delta_pct > 25.0);
    }

    #[test]
    fn noisy_baseline_widens_the_effective_threshold() {
        // Same +9% delta: tight baseline flags it, noisy baseline (whose own
        // spread already covers 9%) does not.
        let tight = decide("x", &[100.0, 100.0, 100.0, 100.0], &[109.0], 8.0, 2.0);
        let noisy = decide("x", &[80.0, 100.0, 120.0, 100.0], &[109.0], 8.0, 2.0);
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
}
