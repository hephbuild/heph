//! `heph session export --format otlp` (spec "OTLP out", D11, D13): a
//! recording's spans, logs and metrics as OTLP/JSON, the file format the
//! collector's `otlpjsonfile` receiver reads. One line per signal, each a
//! `TracesData` / `LogsData` / `MetricsData` (the shape of the matching
//! export request); a signal with nothing to say writes no line.
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
//!   this heph does not know) a gauge.

use std::collections::{BTreeMap, HashMap};
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

/// What an export wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub spans: usize,
    pub span_events: usize,
    pub logs: usize,
    /// Metric data points: one per `metrics` row.
    pub points: usize,
}

/// Write `explorer`'s sessions to `out` as OTLP/JSON lines.
pub fn write(explorer: &Explorer, out: &mut dyn Write) -> anyhow::Result<Written> {
    let conn = explorer.conn();
    let sessions = read_sessions(conn)?;
    let mut written = Written::default();
    let mut traces = trace::TracesData::default();
    let mut log_data = logs::LogsData::default();
    let mut metric_data = metrics::MetricsData::default();
    for s in &sessions {
        let scope = || common::InstrumentationScope {
            name: SCOPE.into(),
            version: s.version.clone(),
            ..Default::default()
        };
        let spans = read_spans(conn, s, &mut written)?;
        if !spans.is_empty() {
            traces.resource_spans.push(trace::ResourceSpans {
                resource: s.resource.clone().into(),
                scope_spans: vec![trace::ScopeSpans {
                    scope: scope().into(),
                    spans,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        let records = read_logs(conn, s, &mut written)?;
        if !records.is_empty() {
            log_data.resource_logs.push(logs::ResourceLogs {
                resource: s.resource.clone().into(),
                scope_logs: vec![logs::ScopeLogs {
                    scope: scope().into(),
                    log_records: records,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        let ms = read_metrics(conn, s, &mut written)?;
        if !ms.is_empty() {
            metric_data.resource_metrics.push(metrics::ResourceMetrics {
                resource: s.resource.clone().into(),
                scope_metrics: vec![metrics::ScopeMetrics {
                    scope: scope().into(),
                    metrics: ms,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
    }
    if !traces.resource_spans.is_empty() {
        line(&traces, out).context("writing the spans")?;
    }
    if !log_data.resource_logs.is_empty() {
        line(&log_data, out).context("writing the logs")?;
    }
    if !metric_data.resource_metrics.is_empty() {
        line(&metric_data, out).context("writing the metrics")?;
    }
    out.flush().context("flushing the export")?;
    Ok(written)
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

fn read_spans(
    conn: &rusqlite::Connection,
    s: &Session,
    written: &mut Written,
) -> anyhow::Result<Vec<trace::Span>> {
    let mut events: HashMap<i64, Vec<trace::span::Event>> = HashMap::new();
    let mut stmt = conn
        .prepare(
            "SELECT span_id, name, time_us, attributes FROM span_events \
             WHERE session_id = ?1 ORDER BY span_id, time_us",
        )
        .context("reading the span events")?;
    let mut rows = stmt.query([&s.id]).context("reading the span events")?;
    let mut n_events = 0;
    while let Some(r) = rows.next().context("reading the span events")? {
        let span_id: i64 = r.get(0).context("reading a span event")?;
        let attributes: Option<String> = r.get(3).context("reading a span event")?;
        events.entry(span_id).or_default().push(trace::span::Event {
            name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            time_unix_nano: nanos(r.get::<_, Option<i64>>(2)?.unwrap_or_default()),
            attributes: attrs(attributes.as_deref())
                .context("reading a span event's attributes")?,
            ..Default::default()
        });
        n_events += 1;
    }
    drop(rows);

    let mut stmt = conn
        .prepare(
            "SELECT span_id, parent_span_id, name, class, addr, driver, outcome, start_us, \
             end_us, status, attributes FROM spans WHERE session_id = ?1 ORDER BY span_id",
        )
        .context("reading the spans")?;
    let mut rows = stmt.query([&s.id]).context("reading the spans")?;
    let mut out = Vec::new();
    let mut attached = 0;
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
        let span_events = events.remove(&span_id).unwrap_or_default();
        attached += span_events.len();
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
    written.spans += out.len();
    written.span_events += attached;
    if attached < n_events {
        tracing::debug!(
            session = %s.id,
            orphans = n_events - attached,
            "span events whose span is not recorded are not exported"
        );
    }
    Ok(out)
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

fn read_logs(
    conn: &rusqlite::Connection,
    s: &Session,
    written: &mut Written,
) -> anyhow::Result<Vec<logs::LogRecord>> {
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
    }
    written.logs += out.len();
    Ok(out)
}

fn read_metrics(
    conn: &rusqlite::Connection,
    s: &Session,
    written: &mut Written,
) -> anyhow::Result<Vec<metrics::Metric>> {
    let mut stmt = conn
        .prepare(
            "SELECT name, kind, unit, time_us, value, attributes FROM metrics \
             WHERE session_id = ?1 ORDER BY name, kind, unit, time_us",
        )
        .context("reading the metrics")?;
    let mut rows = stmt.query([&s.id]).context("reading the metrics")?;
    // (name, kind, unit) -> points, in that order.
    let mut grouped: BTreeMap<(String, String, String), Vec<metrics::NumberDataPoint>> =
        BTreeMap::new();
    while let Some(r) = rows.next().context("reading the metrics")? {
        let get = |i| -> anyhow::Result<String> {
            Ok(r.get::<_, Option<String>>(i)
                .context("reading a metric")?
                .unwrap_or_default())
        };
        let (name, kind, unit) = (get(0)?, get(1)?, get(2)?);
        let attributes: Option<String> = r.get(5).context("reading a metric")?;
        let counter = kind == "counter";
        grouped
            .entry((name, kind, unit))
            .or_default()
            .push(metrics::NumberDataPoint {
                attributes: attrs(attributes.as_deref())
                    .context("reading a metric's attributes")?,
                start_time_unix_nano: if counter { s.started_ns } else { 0 },
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
            });
    }
    let mut out = Vec::with_capacity(grouped.len());
    for ((name, kind, unit), data_points) in grouped {
        written.points += data_points.len();
        let data = match kind.as_str() {
            "counter" => metrics::metric::Data::Sum(Box::new(metrics::Sum {
                data_points,
                aggregation_temporality:
                    metrics::AggregationTemporality::AGGREGATION_TEMPORALITY_CUMULATIVE.into(),
                is_monotonic: true,
                ..Default::default()
            })),
            _ => metrics::metric::Data::Gauge(Box::new(metrics::Gauge {
                data_points,
                ..Default::default()
            })),
        };
        out.push(metrics::Metric {
            name,
            unit,
            data: Some(data),
            ..Default::default()
        });
    }
    Ok(out)
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
