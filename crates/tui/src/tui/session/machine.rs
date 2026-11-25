//! What a recording says about the machine it ran on (session store PR 5):
//! `heph session show`'s machine line and its `--json` `machine`.
//!
//! Read from the explorer rows only (`metrics`, `span_events`, `spans`), the
//! same rows a query reads (I9), and the waiting split by running the
//! [`WAITING_BY_LOAD`](super::query::WAITING_BY_LOAD) recipe itself. A file
//! may be foreign: every count is capped, every number checked, and a value
//! that is not one heph could write is left out rather than trusted.

use std::collections::BTreeMap;

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde::Serialize;

use super::derive::{SESSION_SPAN, machine as names};

/// CPU busy at or above this makes a sample interval busy (D29). The
/// recipe's SQL carries the same literal.
pub const THRESHOLD_CPU: f64 = 0.9;
/// Memory used at or above this share of the limit makes a sample interval
/// busy (D29). The recipe's SQL carries the same literal.
pub const THRESHOLD_MEMORY: f64 = 0.95;

/// The most stopped sources kept: heph names each of its sources at most
/// once, so a file with more is not one heph wrote.
const FAILURES_CAP: usize = 64;
/// The most waiting phases kept, for the same reason.
const PHASES_CAP: usize = 64;
/// The longest source name, error text or phase name kept, in bytes.
const TEXT_CAP: usize = 1024;

/// What the sampler left in the recording. An open enum: a consumer reads a
/// value it does not know as `partial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    /// Samples, and no source stopped.
    Sampled,
    /// Samples, and at least one source stopped (`sources_failed`).
    Partial,
    /// No sample, and at least one source stopped.
    Failed,
    /// Nothing sampled and nothing failed; `reason` says why.
    None,
}

/// Why a recording has no machine figures. An open enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoneReason {
    /// No machine record at all: recorded by a heph from before machine
    /// stats, whose recorder writes its start-time figures (or the failure
    /// to start its sampler) first. Re-deriving it changes nothing.
    OlderDerivation,
    /// The start-time figures, but no sample: the run was shorter than one
    /// tick.
    NoSamples,
}

/// Where a limit came from. An open enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// A cgroup v2 `cpu.max` quota tighter than the CPU count.
    Quota,
    /// The host: the CPU count, or physical RAM.
    Host,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::Host => "host",
        }
    }
}

/// One sampler source that stopped: a `heph.sampler.stopped` span event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceFailed {
    /// The metric the source fed, e.g. `heph.machine.memory.used`.
    pub source: String,
    pub error: String,
    pub at_unix_us: u64,
}

/// One waiting phase's time, split by load (D29). Span-microseconds summed
/// across targets, so concurrent waits add up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Split {
    pub busy_us: u64,
    pub spare_us: u64,
    pub unknown_us: u64,
}

impl Split {
    pub fn total_us(&self) -> u64 {
        self.busy_us
            .saturating_add(self.spare_us)
            .saturating_add(self.unknown_us)
    }
}

/// The waiting split: `--json`'s `machine.waiting`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Waiting {
    pub threshold_cpu: f64,
    pub threshold_memory: f64,
    /// By phase, the span name without its `heph.` prefix (`admission`,
    /// `deps`, `lock_wait`, `scratch`). A phase nothing waited in is absent.
    pub by_phase: BTreeMap<String, Split>,
}

impl Default for Waiting {
    fn default() -> Self {
        Self {
            threshold_cpu: THRESHOLD_CPU,
            threshold_memory: THRESHOLD_MEMORY,
            by_phase: BTreeMap::new(),
        }
    }
}

/// `heph session show --json`'s `machine`. A figure with no sample behind
/// it is absent, never 0 or null.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MachineSummary {
    pub status: MachineStatus,
    /// Only with `status: none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoneReason>,
    /// Logical CPUs online.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// The CPUs heph could use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<f64>,
    /// `quota` when the limit is below the CPU count, else `host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit_source: Option<LimitSource>,
    /// Physical RAM.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    /// Always `host` (O11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_source: Option<LimitSource>,
    /// The session summaries: the rows the seal writes, or while it runs
    /// the same aggregates over the samples so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_busy_mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_busy_peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_peak_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heph_resident_peak_bytes: Option<u64>,
    /// In the order they stopped.
    pub sources_failed: Vec<SourceFailed>,
    pub waiting: Waiting,
}

impl MachineSummary {
    /// What a source that keeps no recording reports.
    pub fn none(reason: NoneReason) -> Self {
        Self {
            status: MachineStatus::None,
            reason: Some(reason),
            cpus: None,
            cpu_limit: None,
            cpu_limit_source: None,
            memory_limit_bytes: None,
            memory_limit_source: None,
            cpu_busy_mean: None,
            cpu_busy_peak: None,
            memory_used_peak_bytes: None,
            heph_resident_peak_bytes: None,
            sources_failed: Vec::new(),
            waiting: Waiting::default(),
        }
    }
}

/// A fraction heph could have written: 0–1.
fn fraction(v: f64) -> Option<f64> {
    (0.0..=1.0).contains(&v).then_some(v)
}

/// A count heph could have written: finite, non-negative, whole bytes.
fn whole(v: f64) -> Option<u64> {
    // `as i64` saturates; a negative value is refused by `try_from`.
    v.is_finite()
        .then(|| u64::try_from(v as i64).ok())
        .flatten()
}

/// The machine summary of the recording `conn` reads.
pub fn read_machine(conn: &Connection) -> rusqlite::Result<MachineSummary> {
    // `status` and `reason` are decided at the end, from what was read.
    let mut m = MachineSummary::none(NoneReason::NoSamples);

    // The once-written rows; ordered by time, so the last one wins.
    let mut stmt = conn.prepare(
        "SELECT name, value, time_us FROM metrics WHERE name IN (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         AND value IS NOT NULL ORDER BY time_us",
    )?;
    // When the sampler read the start-time figures: what the first sample's
    // interval runs from.
    let mut started_us: Option<i64> = None;
    let mut rows = stmt.query([
        names::CPU_COUNT,
        names::CPU_LIMIT,
        names::MEMORY_LIMIT,
        names::SESSION_BUSY_MEAN,
        names::SESSION_BUSY_PEAK,
        names::SESSION_MEMORY_USED_PEAK,
        names::SESSION_RESIDENT_PEAK,
    ])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(0)?;
        let value: f64 = r.get(1)?;
        match name.as_str() {
            names::CPU_COUNT => {
                m.cpus = whole(value).and_then(|n| u32::try_from(n).ok());
                if let ValueRef::Integer(t) = r.get_ref(2)? {
                    started_us = Some(t);
                }
            }
            names::CPU_LIMIT => m.cpu_limit = (value.is_finite() && value > 0.0).then_some(value),
            names::MEMORY_LIMIT => m.memory_limit_bytes = whole(value),
            names::SESSION_BUSY_MEAN => m.cpu_busy_mean = fraction(value),
            names::SESSION_BUSY_PEAK => m.cpu_busy_peak = fraction(value),
            names::SESSION_MEMORY_USED_PEAK => m.memory_used_peak_bytes = whole(value),
            _ => m.heph_resident_peak_bytes = whole(value),
        }
    }
    drop(rows);
    // From the rows, not a stored attribute (the derivation writes none):
    // a quota at or above the CPU count reads as the host, as the sampler
    // itself decides.
    m.cpu_limit_source = m.cpu_limit.map(|limit| match m.cpus {
        Some(n) if limit < f64::from(n) => LimitSource::Quota,
        _ => LimitSource::Host,
    });
    m.memory_limit_source = m.memory_limit_bytes.map(|_| LimitSource::Host);

    let sampled: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM metrics WHERE name IN (?1, ?2, ?3, ?4))",
        names::PER_SAMPLE,
        |r| r.get(0),
    )?;
    if sampled {
        from_samples(conn, &mut m, started_us)?;
    }

    let mut stmt = conn.prepare(
        "SELECT time_us, substr(json_extract(attributes, '$.\"heph.sampler.source\"'), 1, ?3), \
           substr(json_extract(attributes, '$.\"heph.sampler.error\"'), 1, ?3) \
         FROM span_events \
         WHERE span_id = ?1 AND name = ?2 AND json_valid(attributes) \
         ORDER BY time_us LIMIT ?4",
    )?;
    let failed = stmt.query_map(
        rusqlite::params![
            SESSION_SPAN,
            names::SAMPLER_STOPPED,
            i64::try_from(TEXT_CAP).unwrap_or(i64::MAX),
            i64::try_from(FAILURES_CAP).unwrap_or(i64::MAX)
        ],
        |r| {
            Ok(SourceFailed {
                at_unix_us: micros(r.get_ref(0)?),
                source: text(r.get_ref(1)?),
                error: text(r.get_ref(2)?),
            })
        },
    )?;
    m.sources_failed = failed.collect::<rusqlite::Result<_>>()?;

    m.status = match (sampled, m.sources_failed.is_empty()) {
        (true, true) => MachineStatus::Sampled,
        (true, false) => MachineStatus::Partial,
        (false, false) => MachineStatus::Failed,
        (false, true) => MachineStatus::None,
    };
    // A heph with machine stats always records something first: the
    // start-time figures, or why they (or the whole sampler) could not be
    // read, which makes the status `failed`. So nothing at all is a
    // recording from before machine stats, at any derivation.
    let started = m.cpus.is_some() || m.cpu_limit.is_some() || m.memory_limit_bytes.is_some();
    m.reason = (m.status == MachineStatus::None).then_some(if started {
        NoneReason::NoSamples
    } else {
        NoneReason::OlderDerivation
    });
    m.waiting.by_phase = read_waiting(conn)?;
    Ok(m)
}

/// The session summaries the seal has not written yet (a live or lost
/// recording), as the seal computes them, from the per-sample rows: busy
/// mean weighted by each sample's interval (the time since the sample
/// before it, or since the start-time figures for the first), and the
/// peaks. A summary row already read is kept. Each read is one name, so
/// the `(name, time_us)` index serves it when the recording has one.
fn from_samples(
    conn: &Connection,
    m: &mut MachineSummary,
    started_us: Option<i64>,
) -> rusqlite::Result<()> {
    if m.cpu_busy_mean.is_none() || m.cpu_busy_peak.is_none() {
        let (mean, peak): (Option<f64>, Option<f64>) = conn.query_row(
            "SELECT sum(value * w) / sum(w), max(value) FROM ( \
               SELECT value, max(time_us - coalesce(lag(time_us) OVER (ORDER BY time_us), \
                                                    ?2, time_us - 1), 1) AS w \
               FROM metrics WHERE name = ?1 AND value BETWEEN 0 AND 1)",
            rusqlite::params![names::CPU_BUSY, started_us],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        m.cpu_busy_mean = m.cpu_busy_mean.or(mean.and_then(fraction));
        m.cpu_busy_peak = m.cpu_busy_peak.or(peak.and_then(fraction));
    }
    let peak = |name: &str| -> rusqlite::Result<Option<u64>> {
        let v: Option<f64> = conn.query_row(
            "SELECT max(value) FROM metrics WHERE name = ?1 AND value >= 0",
            [name],
            |r| r.get(0),
        )?;
        Ok(v.and_then(whole))
    };
    if m.memory_used_peak_bytes.is_none() {
        m.memory_used_peak_bytes = peak(names::MEMORY_USED)?;
    }
    if m.heph_resident_peak_bytes.is_none() {
        m.heph_resident_peak_bytes = peak(names::PROCESS_RESIDENT)?;
    }
    Ok(())
}

/// A text column, or `""` for anything else.
fn text(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        _ => String::new(),
    }
}

/// The `waiting_by_load` recipe over `conn`, by phase, summed over the
/// sessions it holds (a recording holds one).
pub fn read_waiting(conn: &Connection) -> rusqlite::Result<BTreeMap<String, Split>> {
    let mut stmt = conn.prepare(super::query::WAITING_BY_LOAD.sql)?;
    let mut rows = stmt.query([])?;
    let mut by_phase: BTreeMap<String, Split> = BTreeMap::new();
    while let Some(r) = rows.next()? {
        let ValueRef::Text(phase) = r.get_ref(1)? else {
            continue;
        };
        if phase.len() > TEXT_CAP {
            continue;
        }
        let phase = String::from_utf8_lossy(phase).into_owned();
        if by_phase.len() >= PHASES_CAP && !by_phase.contains_key(&phase) {
            continue;
        }
        let split = by_phase.entry(phase).or_default();
        split.busy_us = split.busy_us.saturating_add(micros(r.get_ref(2)?));
        split.spare_us = split.spare_us.saturating_add(micros(r.get_ref(3)?));
        split.unknown_us = split.unknown_us.saturating_add(micros(r.get_ref(4)?));
    }
    Ok(by_phase)
}

/// A non-negative whole number of microseconds, or 0 for anything else.
fn micros(v: ValueRef<'_>) -> u64 {
    match v {
        ValueRef::Integer(i) => u64::try_from(i).unwrap_or(0),
        ValueRef::Real(f) => whole(f).unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smoke check of the recipe's arithmetic; `waiting_split_by_load`
    /// (stage B) holds it to every row of the spec's table.
    #[test]
    fn waiting_split_smoke() {
        let db = Connection::open_in_memory().expect("db");
        db.execute_batch(
            "CREATE TABLE spans (session_id, span_id, name, class, start_us, end_us);
             CREATE TABLE metrics (session_id, name, time_us, value);
             INSERT INTO metrics VALUES
               ('s', 'heph.machine.memory.limit', 0, 100),
               ('s', 'heph.machine.cpu.busy', 1000000, 0.1),
               ('s', 'heph.machine.cpu.busy', 2000000, 0.95),
               ('s', 'heph.machine.memory.used', 2000000, 10),
               ('s', 'heph.machine.cpu.busy', 3000000, 0.5),
               ('s', 'heph.machine.memory.used', 3000000, 10);
             -- 200 ms inside the busy interval; 1 s half busy, half spare;
             -- 0.5 s before the first sample.
             INSERT INTO spans VALUES
               ('s', 2, 'heph.admission', 'waiting', 1200000, 1400000),
               ('s', 3, 'heph.deps', 'waiting', 1500000, 2500000),
               ('s', 4, 'heph.deps', 'waiting', 500000, 1000000),
               ('s', 5, 'heph.process', 'target', 0, 3000000);",
        )
        .expect("setup");
        let got = read_waiting(&db).expect("runs");
        assert_eq!(
            got.get("admission"),
            Some(&Split {
                busy_us: 200_000,
                spare_us: 0,
                unknown_us: 0
            })
        );
        assert_eq!(
            got.get("deps"),
            Some(&Split {
                busy_us: 500_000,
                spare_us: 500_000,
                unknown_us: 500_000
            })
        );
        assert_eq!(got.len(), 2, "{got:?}");
    }
}
