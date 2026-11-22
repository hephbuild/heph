//! The explorer rows (`spans`, `span_events`, `metrics`) derived from a
//! recording's records (I4).
//!
//! One [`Deriver`] serves the live recorder and a re-derivation from
//! `records` alone, so the two write the same rows. Span and metric names,
//! the trace-id rule and the span-id encoding are frozen once released (I9);
//! enumerations (`class`, `status`, `outcome`, `metrics.kind`) are open.
//!
//! Spans stay flat: every `heph.result` is parented by the session span,
//! never by a dependent, so a memoized result reached from several
//! dependents is drawn once.

use std::collections::{BTreeMap, HashMap};

use hcore::events::BuildEventKind;
use serde_json::{Map, Value};

use super::format::SessionRecordKind;

/// The session span's id. Span ids are never 0.
pub const SESSION_SPAN: i64 = 1;

/// The trace id: the first 16 bytes of SHA-256 over `heph.trace.v1:` and the
/// session id, as lowercase hex. Frozen (I9).
pub fn trace_id(session_id: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(b"heph.trace.v1:");
    h.update(session_id.as_bytes());
    let digest = h.finalize();
    hex::encode(digest.get(..16).unwrap_or_default())
}

/// A span id as OTLP writes it: 8 bytes big-endian, lowercase hex. Frozen (I9).
pub fn span_id_hex(span_id: i64) -> String {
    hex::encode(span_id.to_be_bytes())
}

/// What a `PoolUsage` event says about one pool.
#[derive(Debug, Clone, Copy)]
struct PoolRow {
    held: u64,
    capacity: u64,
    waiting: u64,
    removals_pending: u64,
}

/// A phase's class, by name: one table (D17). `None` for a name this heph
/// does not know, which is still recorded.
pub fn phase_class(name: &str) -> Option<&'static str> {
    Some(match name {
        "heph.process" => "target",
        "heph.driver.run" => "driver",
        "heph.resolve" | "heph.cache.read" | "heph.stage" | "heph.collect" | "heph.cache.write" => {
            "heph"
        }
        "heph.deps" | "heph.lock_wait" | "heph.admission" | "heph.scratch" => "waiting",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpanRow {
    pub session_id: String,
    pub span_id: i64,
    pub parent_span_id: Option<i64>,
    pub name: String,
    pub class: Option<&'static str>,
    pub addr: Option<String>,
    pub driver: Option<String>,
    pub outcome: Option<&'static str>,
    pub start_us: i64,
    pub end_us: i64,
    pub status: &'static str,
    pub attributes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventRow {
    pub session_id: String,
    pub span_id: i64,
    pub name: &'static str,
    pub time_us: i64,
    pub attributes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MetricRow {
    pub session_id: String,
    pub name: &'static str,
    pub kind: &'static str,
    pub unit: &'static str,
    pub time_us: i64,
    pub value: f64,
    pub attributes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Row {
    Span(SpanRow),
    Event(EventRow),
    Metric(MetricRow),
}

impl Row {
    /// Roughly what the row costs on disk, toward the recording's ceiling.
    pub(crate) fn approx_bytes(&self) -> u64 {
        let n = match self {
            Self::Span(s) => {
                64 + s.name.len()
                    + s.addr.as_ref().map_or(0, String::len)
                    + s.driver.as_ref().map_or(0, String::len)
                    + s.attributes.as_ref().map_or(0, String::len)
            }
            Self::Event(e) => 40 + e.name.len() + e.attributes.as_ref().map_or(0, String::len),
            Self::Metric(m) => 48 + m.name.len() + m.attributes.as_ref().map_or(0, String::len),
        };
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

/// A unit a pool is written in, and the divisor from the engine's amounts.
/// A pool the `RequestConfig` does not name (a host that predates `units`)
/// has its default unit, as every other renderer reads it: a builtin's own
/// (`cpu` in micro-CPUs, `memory` in bytes), else a count.
fn pool_unit(units: &BTreeMap<String, String>, pool: &str) -> (&'static str, f64) {
    use hcore::capacity::Unit;
    match hcore::events::unit_of(units, pool) {
        Unit::Cpu => ("{cpu}", hcore::capacity::MICROS_PER_CPU as f64),
        Unit::Bytes => ("By", 1.0),
        Unit::Count => ("1", 1.0),
    }
}

#[derive(Debug)]
struct OpenResult {
    span_id: i64,
    start_us: i64,
    driver: Option<String>,
    executed: bool,
    attrs: Map<String, Value>,
}

#[derive(Debug)]
struct PoolAcc {
    unit: &'static str,
    div: f64,
    first_us: i64,
    last_us: i64,
    last_held: u64,
    last_waiting: u64,
    held_area: f64,
    peak: u64,
    waiting_us: i64,
}

impl PoolAcc {
    fn advance(&mut self, t: i64) {
        let dt = (t - self.last_us).max(0);
        // Amounts up to 2^53 are exact in an f64; a pool is far below.
        self.held_area += self.last_held as f64 * dt as f64;
        if self.last_waiting > 0 {
            self.waiting_us += dt;
        }
        self.last_us = self.last_us.max(t);
    }
}

/// A phase span as `ResultEnd.spans` carries it (session store PR 1). Read
/// from the record's body, so a host that does not send it yields none.
#[derive(Debug, serde::Deserialize)]
struct PhaseSpan {
    name: String,
    start_unix_us: i64,
    end_unix_us: i64,
}

#[derive(Debug, serde::Deserialize)]
struct WithSpans {
    #[serde(default)]
    kind: Option<KindSpans>,
}

#[derive(Debug, serde::Deserialize)]
struct KindSpans {
    #[serde(default)]
    spans: Vec<PhaseSpan>,
}

/// Turns records into explorer rows, keeping only open results in memory.
#[derive(Debug)]
pub(crate) struct Deriver {
    session_id: String,
    started_us: i64,
    next_span: i64,
    open: HashMap<String, OpenResult>,
    first_result_us: Option<i64>,
    last_result_end_us: Option<i64>,
    units: BTreeMap<String, String>,
    pools: BTreeMap<String, PoolAcc>,
    outcomes: BTreeMap<&'static str, u64>,
}

fn us(at_ms: u64) -> i64 {
    i64::try_from(at_ms)
        .unwrap_or(i64::MAX / 1000)
        .saturating_mul(1000)
}

fn attrs_json(map: &Map<String, Value>) -> Option<String> {
    (!map.is_empty()).then(|| Value::Object(map.clone()).to_string())
}

impl Deriver {
    pub(crate) fn new(session_id: &str, started_ms: u64) -> Self {
        Self {
            session_id: session_id.to_owned(),
            started_us: us(started_ms),
            next_span: SESSION_SPAN + 1,
            open: HashMap::new(),
            first_result_us: None,
            last_result_end_us: None,
            units: BTreeMap::new(),
            pools: BTreeMap::new(),
            outcomes: BTreeMap::new(),
        }
    }

    fn alloc(&mut self) -> i64 {
        let id = self.next_span;
        self.next_span += 1;
        id
    }

    fn open_result(&mut self, addr: &str, t: i64) -> &mut OpenResult {
        let next = &mut self.next_span;
        self.open.entry(addr.to_owned()).or_insert_with(|| {
            let span_id = *next;
            *next += 1;
            OpenResult {
                span_id,
                start_us: t,
                driver: None,
                executed: false,
                attrs: Map::new(),
            }
        })
    }

    fn event(&self, span_id: i64, name: &'static str, t: i64, attributes: Option<String>) -> Row {
        Row::Event(EventRow {
            session_id: self.session_id.clone(),
            span_id,
            name,
            time_us: t,
            attributes,
        })
    }

    fn span_of(&self, addr: &str) -> i64 {
        self.open.get(addr).map_or(SESSION_SPAN, |o| o.span_id)
    }

    fn metric(
        &self,
        name: &'static str,
        kind: &'static str,
        unit: &'static str,
        t: i64,
        value: f64,
        attributes: Option<String>,
    ) -> Row {
        Row::Metric(MetricRow {
            session_id: self.session_id.clone(),
            name,
            kind,
            unit,
            time_us: t,
            value,
            attributes,
        })
    }

    /// Derive what one record completes. `body` is the record's JSON, read
    /// only for what the typed record drops (`ResultEnd.spans`).
    pub(crate) fn apply(
        &mut self,
        at_ms: u64,
        kind: Option<&BuildEventKind>,
        session: Option<&SessionRecordKind>,
        body: &str,
        rows: &mut Vec<Row>,
    ) {
        let t = us(at_ms);
        if let Some(kind) = kind {
            self.apply_event(t, kind, body, rows);
        } else if let Some(session) = session {
            self.apply_session(t, session, rows);
        }
    }

    fn apply_session(&mut self, t: i64, kind: &SessionRecordKind, rows: &mut Vec<Row>) {
        match kind {
            SessionRecordKind::LockHolder { pid, session } => {
                let mut a = Map::new();
                a.insert("heph.lock.holder_pid".into(), Value::from(*pid));
                if let Some(s) = session {
                    a.insert("heph.lock.holder_session".into(), Value::from(s.as_str()));
                }
                rows.push(self.event(SESSION_SPAN, "heph.lock_holder", t, attrs_json(&a)));
            }
            SessionRecordKind::Approval { prompt, .. } => {
                let name = if prompt.is_some() {
                    "heph.approval.asked"
                } else {
                    "heph.approval.answered"
                };
                rows.push(self.event(SESSION_SPAN, name, t, None));
            }
            SessionRecordKind::Truncated => {
                rows.push(self.event(SESSION_SPAN, "heph.truncated", t, None));
            }
            SessionRecordKind::Log { .. }
            | SessionRecordKind::End { .. }
            | SessionRecordKind::InflightWritten
            | SessionRecordKind::Label { .. }
            | SessionRecordKind::Finished { .. }
            | SessionRecordKind::Output { .. }
            | SessionRecordKind::RunLog(_)
            | SessionRecordKind::Unknown => {}
        }
    }

    fn apply_event(&mut self, t: i64, kind: &BuildEventKind, body: &str, rows: &mut Vec<Row>) {
        use BuildEventKind as K;
        match kind {
            K::RequestConfig { units, .. } => self.units.clone_from(units),
            K::ResultStart { addr } => {
                self.first_result_us.get_or_insert(t);
                self.open_result(addr, t);
            }
            K::ExecuteStart {
                addr,
                driver,
                declared,
                reserved,
                queued_ms,
                ..
            } => {
                let units = self.units.clone();
                let open = self.open_result(addr, t);
                open.executed = true;
                open.driver = Some(driver.clone());
                for (prefix, amounts) in [
                    ("heph.resources.declared.", declared),
                    ("heph.resources.reserved.", reserved),
                ] {
                    for (pool, amount) in amounts.iter().flatten() {
                        let (_, div) = pool_unit(&units, pool);
                        let v = *amount as f64 / div;
                        open.attrs.insert(format!("{prefix}{pool}"), Value::from(v));
                    }
                }
                if let Some(q) = queued_ms {
                    open.attrs
                        .insert("heph.admission.queued_ms".into(), Value::from(*q));
                }
            }
            K::LocalCacheHit { addr } => {
                rows.push(self.event(self.span_of(addr), "heph.cache.local_hit", t, None));
            }
            K::LocalCacheMiss { addr } => {
                rows.push(self.event(self.span_of(addr), "heph.cache.local_miss", t, None));
            }
            K::RemoteCacheHit { addr, .. } => {
                rows.push(self.event(self.span_of(addr), "heph.cache.remote_hit", t, None));
            }
            K::RemoteCacheMiss { addr, .. } => {
                rows.push(self.event(self.span_of(addr), "heph.cache.remote_miss", t, None));
            }
            K::CacheOutcome { addr, outcome } => {
                let v = serde_json::to_value(outcome).unwrap_or(Value::Null);
                if let Some(open) = self.open.get_mut(addr.as_str()) {
                    open.attrs.insert("heph.cache".into(), v);
                }
            }
            K::CacheHitRebuilt { addr, was } => {
                let mut a = Map::new();
                a.insert(
                    "heph.cache.was".into(),
                    serde_json::to_value(was).unwrap_or(Value::Null),
                );
                rows.push(self.event(
                    self.span_of(addr),
                    "heph.cache.hit_rebuilt",
                    t,
                    attrs_json(&a),
                ));
            }
            K::CodegenExcluded {
                path,
                status,
                reason,
                addr,
            } => {
                let mut a = Map::new();
                a.insert("heph.codegen.path".into(), Value::from(path.as_str()));
                a.insert("heph.codegen.status".into(), Value::from(status.as_str()));
                if let Some(r) = reason {
                    a.insert("heph.codegen.reason".into(), Value::from(r.as_str()));
                }
                let span = addr.as_deref().map_or(SESSION_SPAN, |a| self.span_of(a));
                rows.push(self.event(span, "heph.codegen.excluded", t, attrs_json(&a)));
            }
            K::PoolUsage {
                pool,
                held,
                capacity,
                waiting,
                removals_pending,
            } => self.pool_usage(
                t,
                pool,
                PoolRow {
                    held: *held,
                    capacity: *capacity,
                    waiting: *waiting,
                    removals_pending: *removals_pending,
                },
                rows,
            ),
            K::ResultEnd {
                addr,
                error,
                cancelled,
                timed_out,
                upstream_of,
                ..
            } => {
                self.last_result_end_us = Some(t);
                let open = self.open.remove(addr.as_str());
                let (span_id, start_us, driver, executed, mut attrs) = match open {
                    Some(o) => (o.span_id, o.start_us, o.driver, o.executed, o.attrs),
                    None => (self.alloc(), t, None, false, Map::new()),
                };
                let outcome = if *cancelled {
                    "cancelled"
                } else if error.is_some() {
                    "failed"
                } else if executed {
                    "ran"
                } else {
                    "hit"
                };
                *self.outcomes.entry(outcome).or_default() += 1;
                if let Some(u) = upstream_of {
                    attrs.insert("heph.upstream_of".into(), Value::from(u.as_str()));
                }
                if timed_out.is_some() {
                    attrs.insert("heph.timed_out".into(), Value::from(true));
                }
                rows.push(Row::Span(SpanRow {
                    session_id: self.session_id.clone(),
                    span_id,
                    parent_span_id: Some(SESSION_SPAN),
                    name: "heph.result".to_owned(),
                    class: None,
                    addr: Some(addr.clone()),
                    driver: driver.clone(),
                    outcome: Some(outcome),
                    start_us,
                    end_us: t.max(start_us),
                    status: if error.is_some() { "error" } else { "ok" },
                    attributes: attrs_json(&attrs),
                }));
                self.phase_spans(span_id, addr, driver.as_deref(), body, rows);
            }
            K::Matched { .. }
            | K::SelectionIncomplete { .. }
            | K::ResultOutputs { .. }
            | K::ExecuteEnd { .. }
            | K::LocalCacheWriteStart { .. }
            | K::LocalCacheWriteEnd { .. }
            | K::ResultLockWaitStart { .. }
            | K::ResultLockWaitEnd { .. }
            | K::AdmissionWaitStart { .. }
            | K::AdmissionWaitEnd { .. }
            | K::ScratchLockWaitStart { .. }
            | K::ScratchLockWaitEnd { .. }
            | K::ScratchPrepareStart { .. }
            | K::ScratchPrepareEnd { .. }
            | K::RemoteCacheReadStart { .. }
            | K::RemoteCacheReadEnd { .. }
            | K::RemoteCacheWriteStart { .. }
            | K::RemoteCacheWriteEnd { .. }
            | K::GcTargetSwept { .. }
            | K::GcFailureLogsSwept { .. }
            | K::GcCacheDb { .. }
            | K::CodegenImported { .. }
            | K::Unknown => {}
        }
    }

    /// The phase spans a `ResultEnd` carries, children of its result span.
    fn phase_spans(
        &mut self,
        parent: i64,
        addr: &str,
        driver: Option<&str>,
        body: &str,
        rows: &mut Vec<Row>,
    ) {
        if !body.contains("\"spans\"") {
            return;
        }
        let Ok(WithSpans { kind: Some(k) }) = serde_json::from_str::<WithSpans>(body) else {
            return;
        };
        for p in k.spans {
            let span_id = self.alloc();
            rows.push(Row::Span(SpanRow {
                session_id: self.session_id.clone(),
                span_id,
                parent_span_id: Some(parent),
                class: phase_class(&p.name),
                name: p.name,
                addr: Some(addr.to_owned()),
                driver: driver.map(str::to_owned),
                outcome: None,
                start_us: p.start_unix_us,
                end_us: p.end_unix_us.max(p.start_unix_us),
                status: "unset",
                attributes: None,
            }));
        }
    }

    fn pool_usage(&mut self, t: i64, pool: &str, usage: PoolRow, rows: &mut Vec<Row>) {
        let PoolRow {
            held,
            capacity,
            waiting,
            removals_pending,
        } = usage;
        let (unit, div) = pool_unit(&self.units, pool);
        let mut a = Map::new();
        a.insert("heph.pool".into(), Value::from(pool));
        let attrs = attrs_json(&a);
        let (held_v, cap_v) = (held as f64 / div, capacity as f64 / div);
        rows.push(self.metric("heph.pool.capacity", "gauge", unit, t, cap_v, attrs.clone()));
        rows.push(self.metric("heph.pool.held", "gauge", unit, t, held_v, attrs.clone()));
        rows.push(self.metric(
            "heph.pool.waiting",
            "gauge",
            "1",
            t,
            waiting as f64,
            attrs.clone(),
        ));
        if pool == "sandbox" {
            rows.push(self.metric(
                "heph.pool.removals_pending",
                "gauge",
                "1",
                t,
                removals_pending as f64,
                attrs,
            ));
        }
        let acc = self.pools.entry(pool.to_owned()).or_insert(PoolAcc {
            unit,
            div,
            first_us: t,
            last_us: t,
            last_held: 0,
            last_waiting: 0,
            held_area: 0.0,
            peak: 0,
            waiting_us: 0,
        });
        acc.advance(t);
        acc.last_held = held;
        acc.last_waiting = waiting;
        acc.peak = acc.peak.max(held);
    }

    /// The session-wide rows, at the end: the session span, startup and
    /// teardown, results left open, per-pool summaries and the result
    /// counters.
    pub(crate) fn finish(&mut self, ended_ms: u64, rows: &mut Vec<Row>) {
        let end = us(ended_ms).max(self.started_us);
        let mut open: Vec<(String, OpenResult)> = self.open.drain().collect();
        open.sort_by_key(|(_, o)| o.span_id);
        for (addr, o) in open {
            rows.push(Row::Span(SpanRow {
                session_id: self.session_id.clone(),
                span_id: o.span_id,
                parent_span_id: Some(SESSION_SPAN),
                name: "heph.result".to_owned(),
                class: None,
                addr: Some(addr),
                driver: o.driver,
                outcome: None,
                start_us: o.start_us,
                end_us: end.max(o.start_us),
                status: "unset",
                attributes: attrs_json(&o.attrs),
            }));
        }
        let span = |id: i64, parent: Option<i64>, name: &str, start: i64, stop: i64| {
            Row::Span(SpanRow {
                session_id: self.session_id.clone(),
                span_id: id,
                parent_span_id: parent,
                name: name.to_owned(),
                class: None,
                addr: None,
                driver: None,
                outcome: None,
                start_us: start,
                end_us: stop.max(start),
                status: "unset",
                attributes: None,
            })
        };
        rows.push(span(
            SESSION_SPAN,
            None,
            "heph.session",
            self.started_us,
            end,
        ));
        let mut next = self.next_span;
        if let Some(first) = self.first_result_us {
            rows.push(span(
                next,
                Some(SESSION_SPAN),
                "heph.startup",
                self.started_us,
                first,
            ));
            next += 1;
        }
        if let Some(last) = self.last_result_end_us {
            rows.push(span(next, Some(SESSION_SPAN), "heph.teardown", last, end));
            next += 1;
        }
        self.next_span = next;
        for (pool, acc) in &mut self.pools {
            acc.advance(end);
            let mut a = Map::new();
            a.insert("heph.pool".into(), Value::from(pool.as_str()));
            let attrs = attrs_json(&a);
            let window = acc.last_us - acc.first_us;
            let mean = if window > 0 {
                acc.held_area / window as f64 / acc.div
            } else {
                acc.last_held as f64 / acc.div
            };
            let peak = acc.peak as f64 / acc.div;
            let (unit, waiting) = (acc.unit, acc.waiting_us);
            let session_id = self.session_id.clone();
            let m = |name, kind, unit, value| {
                Row::Metric(MetricRow {
                    session_id: session_id.clone(),
                    name,
                    kind,
                    unit,
                    time_us: end,
                    value,
                    attributes: attrs.clone(),
                })
            };
            rows.push(m("heph.session.pool.held.mean", "gauge", unit, mean));
            rows.push(m("heph.session.pool.held.peak", "gauge", unit, peak));
            rows.push(m(
                "heph.session.pool.waiting_us",
                "counter",
                "us",
                waiting as f64,
            ));
        }
        for (outcome, n) in &self.outcomes {
            let mut a = Map::new();
            a.insert("heph.outcome".into(), Value::from(*outcome));
            rows.push(self.metric(
                "heph.session.results",
                "counter",
                "1",
                end,
                *n as f64,
                attrs_json(&a),
            ));
        }
    }
}
