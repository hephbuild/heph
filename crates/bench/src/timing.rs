//! Shared result schema for both tiers.
//!
//! Methodology mirrors the `perf-measurement` agent's doctrine: warm up
//! first (discarded), then N measured reps, report the full spread rather
//! than a single number — the `compare` step is what decides noise vs
//! signal, not this one. Each tier drives its own timing loop (Tier A is
//! async and in-process, Tier B spawns a child process per rep), so there is
//! no shared loop here — just the result shape they both produce.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub scenario: String,
    /// Wall time per measured rep, milliseconds. Warmup reps are not
    /// included.
    pub wall_ms: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResults {
    /// "inprocess" or "dist" — which tier produced this file.
    pub tier: String,
    pub scenarios: Vec<ScenarioResult>,
}
