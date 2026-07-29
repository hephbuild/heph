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

/// How to run a scenario — shared by `inprocess::run` and `dist::run`.
///
/// `skip_prepare` lets a caller split "wipe + warm the shared cache state"
/// from "measure one more rep" across *separate invocations* — needed to
/// interleave candidate/baseline measurement rep-by-rep (alternating whole
/// scenario runs confounds a regression with whatever drifted on the runner
/// between the two, e.g. thermal state or a noisy neighbor). Cold has no
/// shared state to prep (every rep is independently cold), so both tiers'
/// Cold arm ignores this flag. `rep_offset` seeds `Incremental`'s per-rep
/// mutation — callers doing exactly one rep per invocation must pass a
/// distinct offset each time, or every call mutates the same files again.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub warmup: usize,
    pub reps: usize,
    pub skip_prepare: bool,
    pub rep_offset: usize,
}
