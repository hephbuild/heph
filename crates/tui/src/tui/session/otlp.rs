//! `heph session export --format otlp` (spec "OTLP out", D11, D13): a
//! recording's spans, logs and metrics as OTLP/JSON, the file format the
//! collector's `otlp_json_file` receiver reads (`otlpjsonfile`, its older
//! name, is a deprecated alias). Each line is a `TracesData` /
//! `LogsData` / `MetricsData` (the shape of the matching export request) of
//! one session and at most [`LINE_ITEMS`] spans and their events, log
//! records or data points (a span with more events is a line alone);
//! the spans' lines come first, then the logs', then the metrics'. A signal
//! with nothing to say writes no line. Rows are streamed: an export holds
//! one line in memory, not the session.
//!
//! Written from the explorer names ([`Explorer`]), after the fact: nothing
//! runs in the build to produce it. The messages are the OTLP proto types
//! generated from the vendored `.proto` files (`gen/otlp`), serialised with
//! their proto3 JSON mapping, then given the two things OTLP/JSON does
//! differently: trace and span ids as hex rather than base64, and enums as
//! integers rather than names ([`otlp_shape`]).
//!
//! Mapping:
//! - a session is a resource: `service.name` `heph`, `service.version`,
//!   `host.name`, `user.name`, `process.pid`, `process.command_args`, and
//!   every other `sessions` column as `heph.session.<column>`;
//! - a span keeps its name, ids (`derive::trace_id`, `derive::span_id_hex`)
//!   and times; `class`, `addr`, `driver` and `outcome` become `heph.class`,
//!   `heph.addr`, `heph.driver` and `heph.outcome`, beside its own
//!   attributes; `status` `ok` / `error` map to the status codes, anything
//!   else is unset; `span_events` are its events;
//! - a log record carries the session span's ids, its `severity` as text and
//!   number, its `seq` as `heph.seq`;
//! - metric rows of one name, kind and unit are one metric: `counter` a
//!   monotonic cumulative sum from the session's start, `gauge` (and a kind
//!   this heph does not know) a gauge;
//! - a machine metric whose meaning matches an OpenTelemetry semantic
//!   convention is written under that name and instrument
//!   ([`MACHINE_METRICS`], D27), and every resource and scope carries the
//!   pinned semconv version as [`SCHEMA_URL`].

use std::io::Write;

use anyhow::Context as _;
use base64::Engine as _;
use buffa::Enumeration;
use otlp_gen::opentelemetry::proto::common::v1 as common;
use otlp_gen::opentelemetry::proto::logs::v1 as logs;
use otlp_gen::opentelemetry::proto::metrics::v1 as metrics;
use otlp_gen::opentelemetry::proto::resource::v1 as resource;
use otlp_gen::opentelemetry::proto::trace::v1 as trace;
use rusqlite::types::Value as Sql;
use serde_json::Value as Json;

use super::derive;
use super::query::Explorer;

/// The instrumentation scope every signal is under.
pub const SCOPE: &str = "heph";

/// The OpenTelemetry semantic-convention version the export's names follow
/// (D27): the resource attributes and [`MACHINE_METRICS`]' mapping.
pub const SEMCONV_VERSION: &str = "1.27.0";
/// [`SEMCONV_VERSION`]'s schema, on every resource and scope written.
pub const SCHEMA_URL: &str = "https://opentelemetry.io/schemas/1.27.0";

/// An OpenTelemetry instrument, as the export writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instrument {
    Gauge,
    /// A monotonic cumulative sum from the session's start.
    Counter,
    /// A non-monotonic cumulative sum from the session's start.
    UpDownCounter,
}

impl Instrument {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Counter => "counter",
            Self::UpDownCounter => "updowncounter",
        }
    }
}

/// A machine metric (Design, "Machine stats"): its heph name, kind and unit,
/// frozen (I9), and the semantic convention it is exported as, if one means
/// the same.
#[derive(Debug, Clone, Copy)]
pub struct MachineMetric {
    pub name: &'static str,
    /// The `metrics.kind` heph writes.
    pub kind: &'static str,
    pub unit: &'static str,
    /// The semconv name and instrument the OTLP export writes it as; `None`:
    /// no convention means the same, so it keeps its heph name.
    pub otel: Option<(&'static str, Instrument)>,
}

/// Every machine metric heph writes, and its OTLP mapping (D27), checked
/// against semantic conventions [`SEMCONV_VERSION`]. `--schema` prints it.
///
/// Not mapped, as the meaning differs: `heph.machine.cpu.busy` is the
/// non-idle share of `cpu.limit` (a quota), where `system.cpu.utilization`
/// is per `cpu.mode` and of every CPU; `heph.machine.memory.used` is what
/// cannot be reclaimed, where `system.memory.usage` is by state and its
/// states sum to the limit; `heph.machine.cpu.limit` has no convention; the
/// `heph.session.*` summaries are heph's own.
pub const MACHINE_METRICS: [MachineMetric; 11] = {
    use super::derive::machine as m;
    const fn heph(name: &'static str, kind: &'static str, unit: &'static str) -> MachineMetric {
        MachineMetric {
            name,
            kind,
            unit,
            otel: None,
        }
    }
    const fn otel(
        name: &'static str,
        kind: &'static str,
        unit: &'static str,
        to: &'static str,
        instrument: Instrument,
    ) -> MachineMetric {
        MachineMetric {
            name,
            kind,
            unit,
            otel: Some((to, instrument)),
        }
    }
    [
        otel(
            m::CPU_COUNT,
            "gauge",
            "{cpu}",
            "system.cpu.logical.count",
            Instrument::UpDownCounter,
        ),
        heph(m::CPU_LIMIT, "gauge", "{cpu}"),
        heph(m::CPU_BUSY, "gauge", "1"),
        otel(
            m::MEMORY_LIMIT,
            "gauge",
            "By",
            "system.memory.limit",
            Instrument::UpDownCounter,
        ),
        heph(m::MEMORY_USED, "gauge", "By"),
        otel(
            m::PROCESS_CPU_TIME,
            "counter",
            "s",
            "process.cpu.time",
            Instrument::Counter,
        ),
        otel(
            m::PROCESS_RESIDENT,
            "gauge",
            "By",
            "process.memory.usage",
            Instrument::UpDownCounter,
        ),
        heph(m::SESSION_BUSY_MEAN, "gauge", "1"),
        heph(m::SESSION_BUSY_PEAK, "gauge", "1"),
        heph(m::SESSION_MEMORY_USED_PEAK, "gauge", "By"),
        heph(m::SESSION_RESIDENT_PEAK, "gauge", "By"),
    ]
};

/// The name and instrument rows of `key` are exported as: a machine
/// metric's semconv mapping when its kind and unit are the ones heph writes
/// (`Some` name), else its own name (`None`), a `counter` as a counter and
/// anything else a gauge.
fn exported((name, kind, unit): &MetricKey) -> (Option<&'static str>, Instrument) {
    if let Some((to, instrument)) = MACHINE_METRICS
        .iter()
        .find(|m| m.name == name && m.kind == kind && m.unit == unit)
        .and_then(|m| m.otel)
    {
        return (Some(to), instrument);
    }
    let instrument = if kind == "counter" {
        Instrument::Counter
    } else {
        Instrument::Gauge
    };
    (None, instrument)
}

/// What an export wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub spans: usize,
    pub span_events: usize,
    pub logs: usize,
    /// Metric data points: one per `metrics` row.
    pub points: usize,
}

/// Spans and span events, log records or metric data points one line holds
/// at most, so an export holds one line's worth in memory, whatever the
/// session's size. A span is never split from its events: one with more
/// events than this is a line of its own.
pub const LINE_ITEMS: usize = 1000;

/// Write `explorer`'s sessions to `out` as OTLP/JSON lines.
pub fn write(explorer: &Explorer, out: &mut dyn Write) -> anyhow::Result<Written> {
    write_lines(explorer, out, LINE_ITEMS)
}

/// [`write`], with at most `per_line` items a line.
pub(crate) fn write_lines(
    explorer: &Explorer,
    out: &mut dyn Write,
    per_line: usize,
) -> anyhow::Result<Written> {
    let per_line = per_line.max(1);
    let conn = explorer.conn();
    let sessions = read_sessions(conn)?;
    let mut written = Written::default();
    let mut lines = Lines { out, per_line };
    for s in &sessions {
        write_spans(conn, s, &mut lines, &mut written)?;
    }
    for s in &sessions {
        write_logs(conn, s, &mut lines, &mut written)?;
    }
    for s in &sessions {
        write_metrics(conn, s, &mut lines, &mut written)?;
    }
    lines.out.flush().context("flushing the export")?;
    Ok(written)
}

/// Where a signal's lines go, each one resource's `per_line` items at most.
struct Lines<'a> {
    out: &'a mut dyn Write,
    per_line: usize,
}

impl Lines<'_> {
    fn spans(&mut self, s: &Session, spans: Vec<trace::Span>) -> anyhow::Result<()> {
        let data = trace::TracesData {
            resource_spans: vec![trace::ResourceSpans {
                resource: s.resource.clone().into(),
                scope_spans: vec![trace::ScopeSpans {
                    scope: s.scope().into(),
                    spans,
                    schema_url: SCHEMA_URL.into(),
                    ..Default::default()
                }],
                schema_url: SCHEMA_URL.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        line(&data, self.out).context("writing the spans")
    }

    fn logs(&mut self, s: &Session, log_records: Vec<logs::LogRecord>) -> anyhow::Result<()> {
        let data = logs::LogsData {
            resource_logs: vec![logs::ResourceLogs {
                resource: s.resource.clone().into(),
                scope_logs: vec![logs::ScopeLogs {
                    scope: s.scope().into(),
                    log_records,
                    schema_url: SCHEMA_URL.into(),
                    ..Default::default()
                }],
                schema_url: SCHEMA_URL.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        line(&data, self.out).context("writing the logs")
    }

    fn metrics(&mut self, s: &Session, ms: Vec<metrics::Metric>) -> anyhow::Result<()> {
        let data = metrics::MetricsData {
            resource_metrics: vec![metrics::ResourceMetrics {
                resource: s.resource.clone().into(),
                scope_metrics: vec![metrics::ScopeMetrics {
                    scope: s.scope().into(),
                    metrics: ms,
                    schema_url: SCHEMA_URL.into(),
                    ..Default::default()
                }],
                schema_url: SCHEMA_URL.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        line(&data, self.out).context("writing the metrics")
    }
}

/// One message as one OTLP/JSON line.
fn line(msg: &impl serde::Serialize, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut v = serde_json::to_value(msg).context("encoding OTLP/JSON")?;
    otlp_shape(&mut v)?;
    serde_json::to_writer(&mut *out, &v).context("writing OTLP/JSON")?;
    out.write_all(b"\n").context("writing OTLP/JSON")
}

/// Proto3 JSON made OTLP/JSON: ids (`traceId`, `spanId`, `parentSpanId`)
/// from base64 to lowercase hex, and the enums OTLP carries (`kind`, `code`,
/// `severityNumber`, `aggregationTemporality`) from names to integers. Keys
/// are proto field names only (attribute keys are values, under `key`), so a
/// key names one field wherever it appears.
pub(crate) fn otlp_shape(v: &mut Json) -> anyhow::Result<()> {
    match v {
        Json::Object(map) => {
            for (k, child) in map.iter_mut() {
                let Json::String(s) = child else {
                    otlp_shape(child)?;
                    continue;
                };
                let to = match k.as_str() {
                    "traceId" | "spanId" | "parentSpanId" => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(s.as_bytes())
                            .with_context(|| format!("decoding the {k} the encoder wrote"))?;
                        Json::String(hex::encode(bytes))
                    }
                    "kind" => enum_int::<trace::span::SpanKind>(k, s)?,
                    "code" => enum_int::<trace::status::StatusCode>(k, s)?,
                    "severityNumber" => enum_int::<logs::SeverityNumber>(k, s)?,
                    "aggregationTemporality" => enum_int::<metrics::AggregationTemporality>(k, s)?,
                    _ => continue,
                };
                *child = to;
            }
        }
        Json::Array(items) => {
            for item in items {
                otlp_shape(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn enum_int<E: Enumeration>(key: &str, name: &str) -> anyhow::Result<Json> {
    let e = E::from_proto_name(name)
        .with_context(|| format!("the encoder wrote {key} `{name}`, which is not a value"))?;
    Ok(Json::from(e.to_i32()))
}

/// A session: its resource, and what its signals need of it.
struct Session {
    id: String,
    version: String,
    trace_id: Vec<u8>,
    started_ns: u64,
    resource: resource::Resource,
}

impl Session {
    fn scope(&self) -> common::InstrumentationScope {
        common::InstrumentationScope {
            name: SCOPE.into(),
            version: self.version.clone(),
            ..Default::default()
        }
    }
}

/// `sessions` columns that map to an OpenTelemetry semantic-convention
/// resource attribute; every other column is `heph.session.<column>`.
fn resource_key(column: &str) -> Option<&'static str> {
    Some(match column {
        "session_id" => "heph.session.id",
        "heph_version" => "service.version",
        "host" => "host.name",
        "user" => "user.name",
        "owner_pid" => "process.pid",
        "argv" => "process.command_args",
        _ => return None,
    })
}

fn read_sessions(conn: &rusqlite::Connection) -> anyhow::Result<Vec<Session>> {
    let mut stmt = conn
        .prepare("SELECT * FROM sessions ORDER BY session_id")
        .context("reading the sessions")?;
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let mut rows = stmt.query([]).context("reading the sessions")?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().context("reading the sessions")? {
        let mut attrs = vec![kv("service.name", text("heph"))];
        let (mut id, mut version, mut started_ms) = (String::new(), String::new(), 0_i64);
        for (i, name) in names.iter().enumerate() {
            let v: Sql = row.get(i).context("reading the sessions")?;
            match (name.as_str(), &v) {
                ("session_id", Sql::Text(s)) => id.clone_from(s),
                ("heph_version", Sql::Text(s)) => version.clone_from(s),
                ("started_ms", Sql::Integer(n)) => started_ms = *n,
                _ => {}
            }
            let key =
                resource_key(name).map_or_else(|| format!("heph.session.{name}"), str::to_owned);
            let value = match (name.as_str(), v) {
                // A JSON array of strings, as the recorder writes it.
                ("argv", Sql::Text(s)) => serde_json::from_str::<Json>(&s)
                    .ok()
                    .and_then(|j| any(&j))
                    .unwrap_or_else(|| text(&s)),
                (_, v) => match sql_any(v) {
                    Some(a) => a,
                    None => continue,
                },
            };
            attrs.push(kv(key, value));
        }
        out.push(Session {
            trace_id: hex::decode(derive::trace_id(&id)).context("the session's trace id")?,
            started_ns: nanos(started_ms.saturating_mul(1000)),
            resource: resource::Resource {
                attributes: attrs,
                ..Default::default()
            },
            id,
            version,
        });
    }
    Ok(out)
}

/// A session's span events, read alongside its spans: both are in `span_id`
/// order, so a span's events are the next ones of its id.
struct SpanEvents<'s> {
    rows: rusqlite::Rows<'s>,
    /// The next event, read ahead.
    next: Option<(i64, trace::span::Event)>,
    /// Events whose span is not recorded, skipped.
    orphans: usize,
}

impl<'s> SpanEvents<'s> {
    fn new(rows: rusqlite::Rows<'s>) -> anyhow::Result<Self> {
        let mut events = Self {
            rows,
            next: None,
            orphans: 0,
        };
        events.advance()?;
        Ok(events)
    }

    fn advance(&mut self) -> anyhow::Result<()> {
        self.next = match self.rows.next().context("reading the span events")? {
            None => None,
            Some(r) => {
                let span_id: i64 = r.get(0).context("reading a span event")?;
                let attributes: Option<String> = r.get(3).context("reading a span event")?;
                Some((
                    span_id,
                    trace::span::Event {
                        name: r
                            .get::<_, Option<String>>(1)
                            .context("reading a span event")?
                            .unwrap_or_default(),
                        time_unix_nano: nanos(
                            r.get::<_, Option<i64>>(2)
                                .context("reading a span event")?
                                .unwrap_or_default(),
                        ),
                        attributes: attrs(attributes.as_deref())
                            .context("reading a span event's attributes")?,
                        ..Default::default()
                    },
                ))
            }
        };
        Ok(())
    }

    /// The events of `span_id`; those of a lower id, which no span had, are
    /// skipped as orphans.
    fn of(&mut self, span_id: i64) -> anyhow::Result<Vec<trace::span::Event>> {
        let mut out = Vec::new();
        while let Some((id, _)) = &self.next {
            match (*id).cmp(&span_id) {
                std::cmp::Ordering::Greater => break,
                std::cmp::Ordering::Less => self.orphans += 1,
                std::cmp::Ordering::Equal => {
                    if let Some((_, e)) = self.next.take() {
                        out.push(e);
                    }
                }
            }
            self.advance()?;
        }
        Ok(out)
    }

    /// Skip what is left: every one an orphan.
    fn finish(mut self) -> anyhow::Result<usize> {
        while self.next.is_some() {
            self.orphans += 1;
            self.advance()?;
        }
        Ok(self.orphans)
    }
}

fn write_spans(
    conn: &rusqlite::Connection,
    s: &Session,
    lines: &mut Lines<'_>,
    written: &mut Written,
) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare(
            "SELECT span_id, name, time_us, attributes FROM span_events \
             WHERE session_id = ?1 ORDER BY span_id, time_us",
        )
        .context("reading the span events")?;
    let mut events = SpanEvents::new(stmt.query([&s.id]).context("reading the span events")?)?;

    let mut stmt = conn
        .prepare(
            "SELECT span_id, parent_span_id, name, class, addr, driver, outcome, start_us, \
             end_us, status, attributes FROM spans WHERE session_id = ?1 ORDER BY span_id",
        )
        .context("reading the spans")?;
    let mut rows = stmt.query([&s.id]).context("reading the spans")?;
    let mut out = Vec::new();
    let mut items = 0;
    while let Some(r) = rows.next().context("reading the spans")? {
        let span_id: i64 = r.get(0).context("reading a span")?;
        let parent: Option<i64> = r.get(1).context("reading a span")?;
        let mut attributes = Vec::new();
        for (i, key) in [
            (3, "heph.class"),
            (4, "heph.addr"),
            (5, "heph.driver"),
            (6, "heph.outcome"),
        ] {
            if let Some(v) = r.get::<_, Option<String>>(i).context("reading a span")? {
                attributes.push(kv(key, text(&v)));
            }
        }
        let own: Option<String> = r.get(10).context("reading a span")?;
        attributes.extend(attrs(own.as_deref()).context("reading a span's attributes")?);
        let start = r
            .get::<_, Option<i64>>(7)
            .context("reading a span")?
            .unwrap_or_default();
        let end = r
            .get::<_, Option<i64>>(8)
            .context("reading a span")?
            .unwrap_or(start);
        let code = match r
            .get::<_, Option<String>>(9)
            .context("reading a span")?
            .as_deref()
        {
            Some("ok") => trace::status::StatusCode::STATUS_CODE_OK,
            Some("error") => trace::status::StatusCode::STATUS_CODE_ERROR,
            _ => trace::status::StatusCode::STATUS_CODE_UNSET,
        };
        let span_events = events.of(span_id)?;
        written.span_events += span_events.len();
        // A span counts with its events: a line of spans that each carry
        // many would otherwise be many times the bound. A span is never
        // split, so one with more events than the bound is a line alone.
        let span_items = 1 + span_events.len();
        if !out.is_empty() && items + span_items > lines.per_line {
            written.spans += out.len();
            lines.spans(s, std::mem::take(&mut out))?;
            items = 0;
        }
        items += span_items;
        out.push(trace::Span {
            trace_id: s.trace_id.clone(),
            span_id: span_id.to_be_bytes().to_vec(),
            parent_span_id: parent.map(|p| p.to_be_bytes().to_vec()).unwrap_or_default(),
            name: r
                .get::<_, Option<String>>(2)
                .context("reading a span")?
                .unwrap_or_default(),
            kind: trace::span::SpanKind::SPAN_KIND_INTERNAL.into(),
            start_time_unix_nano: nanos(start),
            end_time_unix_nano: nanos(end),
            attributes,
            events: span_events,
            status: trace::Status {
                code: code.into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        });
    }
    if !out.is_empty() {
        written.spans += out.len();
        lines.spans(s, out)?;
    }
    let orphans = events.finish()?;
    if orphans > 0 {
        tracing::debug!(
            session = %s.id,
            orphans,
            "span events whose span is not recorded are not exported"
        );
    }
    Ok(())
}

/// A recorded severity as OTLP's number; `UNSPECIFIED` for one it does not
/// name (the text is kept either way).
fn severity(text: &str) -> logs::SeverityNumber {
    match text {
        "trace" => logs::SeverityNumber::SEVERITY_NUMBER_TRACE,
        "debug" => logs::SeverityNumber::SEVERITY_NUMBER_DEBUG,
        "info" => logs::SeverityNumber::SEVERITY_NUMBER_INFO,
        "warn" | "warning" => logs::SeverityNumber::SEVERITY_NUMBER_WARN,
        "error" => logs::SeverityNumber::SEVERITY_NUMBER_ERROR,
        "fatal" => logs::SeverityNumber::SEVERITY_NUMBER_FATAL,
        _ => logs::SeverityNumber::SEVERITY_NUMBER_UNSPECIFIED,
    }
}

fn write_logs(
    conn: &rusqlite::Connection,
    s: &Session,
    lines: &mut Lines<'_>,
    written: &mut Written,
) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare("SELECT seq, time_us, severity, body FROM logs WHERE session_id = ?1 ORDER BY seq")
        .context("reading the logs")?;
    let mut rows = stmt.query([&s.id]).context("reading the logs")?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().context("reading the logs")? {
        let sev = r
            .get::<_, Option<String>>(2)
            .context("reading a log")?
            .unwrap_or_default();
        let mut attributes = Vec::new();
        if let Some(seq) = r.get::<_, Option<i64>>(0).context("reading a log")? {
            attributes.push(kv("heph.seq", common::any_value::Value::IntValue(seq)));
        }
        out.push(logs::LogRecord {
            time_unix_nano: nanos(
                r.get::<_, Option<i64>>(1)
                    .context("reading a log")?
                    .unwrap_or_default(),
            ),
            severity_number: severity(&sev).into(),
            severity_text: sev,
            body: r
                .get::<_, Option<String>>(3)
                .context("reading a log")?
                .map(|b| common::AnyValue {
                    value: Some(text(&b)),
                    ..Default::default()
                })
                .into(),
            attributes,
            trace_id: s.trace_id.clone(),
            span_id: derive::SESSION_SPAN.to_be_bytes().to_vec(),
            ..Default::default()
        });
        if out.len() == lines.per_line {
            written.logs += out.len();
            lines.logs(s, std::mem::take(&mut out))?;
        }
    }
    if !out.is_empty() {
        written.logs += out.len();
        lines.logs(s, out)?;
    }
    Ok(())
}

/// A metric's name, kind and unit: rows of one key are one metric.
type MetricKey = (String, String, String);

/// Rows of one key as one metric, under the name and instrument it is
/// [`exported`] as.
fn metric(key: MetricKey, data_points: Vec<metrics::NumberDataPoint>) -> metrics::Metric {
    let (mapped, instrument) = exported(&key);
    let data = match instrument {
        Instrument::Gauge => metrics::metric::Data::Gauge(Box::new(metrics::Gauge {
            data_points,
            ..Default::default()
        })),
        Instrument::Counter | Instrument::UpDownCounter => {
            metrics::metric::Data::Sum(Box::new(metrics::Sum {
                data_points,
                aggregation_temporality:
                    metrics::AggregationTemporality::AGGREGATION_TEMPORALITY_CUMULATIVE.into(),
                is_monotonic: instrument == Instrument::Counter,
                ..Default::default()
            }))
        }
    };
    let (name, _, unit) = key;
    let name = mapped.map_or(name, str::to_owned);
    metrics::Metric {
        name,
        unit,
        data: Some(data),
        ..Default::default()
    }
}

/// A metric whose points run past a line goes on in the next line, as a
/// metric of the same name, kind and unit.
fn write_metrics(
    conn: &rusqlite::Connection,
    s: &Session,
    lines: &mut Lines<'_>,
    written: &mut Written,
) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare(
            "SELECT name, kind, unit, time_us, value, attributes FROM metrics \
             WHERE session_id = ?1 ORDER BY name, kind, unit, time_us",
        )
        .context("reading the metrics")?;
    let mut rows = stmt.query([&s.id]).context("reading the metrics")?;
    // The line being filled, its points, and the metric being read.
    let mut line_metrics = Vec::new();
    let mut in_line = 0;
    let mut current: Option<(MetricKey, Vec<metrics::NumberDataPoint>)> = None;
    let mut is_sum = false;
    while let Some(r) = rows.next().context("reading the metrics")? {
        let get = |i| -> anyhow::Result<String> {
            Ok(r.get::<_, Option<String>>(i)
                .context("reading a metric")?
                .unwrap_or_default())
        };
        let key = (get(0)?, get(1)?, get(2)?);
        let attributes: Option<String> = r.get(5).context("reading a metric")?;
        // A sum's points start at the session; looked up once a metric.
        if current.as_ref().is_none_or(|(k, _)| *k != key) {
            is_sum = exported(&key).1 != Instrument::Gauge;
        }
        let point = metrics::NumberDataPoint {
            attributes: attrs(attributes.as_deref()).context("reading a metric's attributes")?,
            start_time_unix_nano: if is_sum { s.started_ns } else { 0 },
            time_unix_nano: nanos(
                r.get::<_, Option<i64>>(3)
                    .context("reading a metric")?
                    .unwrap_or_default(),
            ),
            value: r
                .get::<_, Option<f64>>(4)
                .context("reading a metric")?
                .map(metrics::number_data_point::Value::AsDouble),
            ..Default::default()
        };
        match &mut current {
            Some((k, points)) if *k == key => points.push(point),
            _ => {
                if let Some((k, points)) = current.take() {
                    line_metrics.push(metric(k, points));
                }
                current = Some((key, vec![point]));
            }
        }
        in_line += 1;
        if in_line == lines.per_line {
            if let Some((k, points)) = current.take() {
                line_metrics.push(metric(k, points));
            }
            written.points += in_line;
            in_line = 0;
            lines.metrics(s, std::mem::take(&mut line_metrics))?;
        }
    }
    if let Some((k, points)) = current {
        line_metrics.push(metric(k, points));
    }
    if !line_metrics.is_empty() {
        written.points += in_line;
        lines.metrics(s, line_metrics)?;
    }
    Ok(())
}

/// Microseconds as nanoseconds; a negative time (never recorded) is 0.
fn nanos(us: i64) -> u64 {
    u64::try_from(us).unwrap_or_default().saturating_mul(1000)
}

fn text(s: &str) -> common::any_value::Value {
    common::any_value::Value::StringValue(s.to_owned())
}

fn kv(key: impl Into<String>, value: common::any_value::Value) -> common::KeyValue {
    common::KeyValue {
        key: key.into(),
        value: common::AnyValue {
            value: Some(value),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

/// A SQL value as an attribute value; `None` for NULL.
fn sql_any(v: Sql) -> Option<common::any_value::Value> {
    use common::any_value::Value as V;
    Some(match v {
        Sql::Null => return None,
        Sql::Integer(n) => V::IntValue(n),
        Sql::Real(f) => V::DoubleValue(f),
        Sql::Text(s) => V::StringValue(s),
        Sql::Blob(b) => V::BytesValue(b),
    })
}

/// A JSON value as an attribute value; `None` for `null`.
fn any(j: &Json) -> Option<common::any_value::Value> {
    use common::any_value::Value as V;
    let wrap = |v| common::AnyValue {
        value: Some(v),
        ..Default::default()
    };
    Some(match j {
        Json::Null => return None,
        Json::Bool(b) => V::BoolValue(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => V::IntValue(i),
            None => V::DoubleValue(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => V::StringValue(s.clone()),
        Json::Array(items) => V::from(common::ArrayValue {
            values: items.iter().filter_map(any).map(wrap).collect(),
            ..Default::default()
        }),
        Json::Object(map) => V::from(common::KeyValueList {
            values: map
                .iter()
                .filter_map(|(k, v)| any(v).map(|v| kv(k.as_str(), v)))
                .collect(),
            ..Default::default()
        }),
    })
}

/// An `attributes` column (a JSON object, or NULL) as key-values.
fn attrs(json: Option<&str>) -> anyhow::Result<Vec<common::KeyValue>> {
    let Some(json) = json else {
        return Ok(Vec::new());
    };
    let map: serde_json::Map<String, Json> =
        serde_json::from_str(json).context("attributes are not a JSON object")?;
    Ok(map
        .iter()
        .filter_map(|(k, v)| any(v).map(|v| kv(k.as_str(), v)))
        .collect())
}
