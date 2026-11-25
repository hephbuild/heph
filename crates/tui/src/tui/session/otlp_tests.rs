//! `heph session export --format otlp` (spec "OTLP out"): what it writes
//! decodes with the OTLP proto types and holds the recording, in OTLP/JSON's
//! own encoding.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use buffa::{Enumeration as _, Message as _};
use otlp_gen::opentelemetry::proto::common::v1 as common;
use otlp_gen::opentelemetry::proto::logs::v1 as logs;
use otlp_gen::opentelemetry::proto::metrics::v1 as metrics;
use otlp_gen::opentelemetry::proto::trace::v1 as trace;
use serde_json::Value as Json;

use super::derive;
use super::otlp::{self, Written};
use super::query::{Explorer, RecordingFile};
use super::recording::Recording;
use super::store;

/// The committed fixture of the current format (`opens_format_1_fixture`):
/// spans with phases, span events, gauges and counters, a log.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/session/testdata")
        .join(format!("format-{}", store::FORMAT))
        .join(store::FILE)
}

fn explorer(path: &Path, tmp: &Path) -> Explorer {
    let rec = Recording::open_foreign(path, tmp).expect("open the fixture");
    Explorer::open(RecordingFile::foreign(rec).expect("foreign")).expect("explorer")
}

fn export(path: &Path) -> (String, Written) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ex = explorer(path, tmp.path());
    let mut out = Vec::new();
    let written = otlp::write(&ex, &mut out).expect("export");
    (String::from_utf8(out).expect("utf-8"), written)
}

/// OTLP/JSON's hex ids back to proto3 JSON's base64, so the generated
/// types' own JSON mapping reads the line.
fn unshape(v: &mut Json) {
    match v {
        Json::Object(map) => {
            for (k, child) in map.iter_mut() {
                match (k.as_str(), &*child) {
                    ("traceId" | "spanId" | "parentSpanId", Json::String(s)) => {
                        let bytes = hex::decode(s).expect("hex id");
                        *child =
                            Json::String(base64::engine::general_purpose::STANDARD.encode(bytes));
                    }
                    _ => unshape(child),
                }
            }
        }
        Json::Array(items) => items.iter_mut().for_each(unshape),
        _ => {}
    }
}

#[derive(Default)]
struct Decoded {
    traces: Vec<trace::TracesData>,
    logs: Vec<logs::LogsData>,
    metrics: Vec<metrics::MetricsData>,
}

/// Each line through the OTLP proto types' JSON mapping (unknown fields
/// refused), then through the binary encoding and back.
fn decode(text: &str) -> Decoded {
    let mut d = Decoded::default();
    for line in text.lines() {
        let mut v: Json = serde_json::from_str(line).expect("a JSON line");
        unshape(&mut v);
        let key = v
            .as_object()
            .and_then(|o| o.keys().next().cloned())
            .expect("an object");
        match key.as_str() {
            "resourceSpans" => {
                let m: trace::TracesData = serde_json::from_value(v).expect("TracesData");
                let bin = trace::TracesData::decode_from_slice(&m.encode_to_vec()).expect("binary");
                assert!(bin == m, "binary round trip");
                d.traces.push(m);
            }
            "resourceLogs" => {
                let m: logs::LogsData = serde_json::from_value(v).expect("LogsData");
                let bin = logs::LogsData::decode_from_slice(&m.encode_to_vec()).expect("binary");
                assert!(bin == m, "binary round trip");
                d.logs.push(m);
            }
            "resourceMetrics" => {
                let m: metrics::MetricsData = serde_json::from_value(v).expect("MetricsData");
                let bin =
                    metrics::MetricsData::decode_from_slice(&m.encode_to_vec()).expect("binary");
                assert!(bin == m, "binary round trip");
                d.metrics.push(m);
            }
            other => panic!("a line of an unknown signal: {other}"),
        }
    }
    d
}

fn str_of(a: &common::AnyValue) -> Option<&str> {
    match a.value.as_ref()? {
        common::any_value::Value::StringValue(s) => Some(s),
        _ => None,
    }
}

fn attr<'a>(kvs: &'a [common::KeyValue], key: &str) -> Option<&'a common::AnyValue> {
    kvs.iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_option())
}

fn strings(conn: &rusqlite::Connection, sql: &str) -> BTreeSet<String> {
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows")
}

#[test]
fn otlp_export_round_trips() {
    let path = fixture();
    let (text, written) = export(&path);
    let d = decode(&text);
    assert_eq!(
        (d.traces.len(), d.logs.len(), d.metrics.len()),
        (1, 1, 1),
        "{text}"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let ex = explorer(&path, tmp.path());
    let conn = ex.conn();
    let count = |t: &str| -> usize {
        conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| usize::try_from(n).expect("count"))
        .expect("count")
    };
    let session_id: String = conn
        .query_row("SELECT session_id FROM sessions", [], |r| r.get(0))
        .expect("session");

    // The resource: the session, with user, host and argv.
    let rs = d
        .traces
        .first()
        .and_then(|t| t.resource_spans.first())
        .expect("resource spans");
    let res = rs.resource.as_option().expect("resource");
    assert_eq!(
        attr(&res.attributes, "service.name").and_then(str_of),
        Some("heph")
    );
    assert_eq!(
        attr(&res.attributes, "heph.session.id").and_then(str_of),
        Some(session_id.as_str())
    );
    for key in [
        "host.name",
        "user.name",
        "process.command_args",
        "service.version",
    ] {
        assert!(attr(&res.attributes, key).is_some(), "resource lacks {key}");
    }
    assert!(
        matches!(
            attr(&res.attributes, "process.command_args").and_then(|a| a.value.as_ref()),
            Some(common::any_value::Value::ArrayValue(_))
        ),
        "argv is an array"
    );

    // Every span, by id and name, with its events.
    let spans: Vec<&trace::Span> = rs.scope_spans.iter().flat_map(|s| &s.spans).collect();
    assert_eq!(spans.len(), count("spans"));
    assert_eq!(written.spans, spans.len());
    let got: BTreeSet<String> = spans
        .iter()
        .map(|s| {
            let id = i64::from_be_bytes(s.span_id.as_slice().try_into().expect("8 bytes"));
            let parent = (!s.parent_span_id.is_empty()).then(|| {
                i64::from_be_bytes(s.parent_span_id.as_slice().try_into().expect("8 bytes"))
            });
            format!(
                "{id} {parent:?} {} {} {}",
                s.name,
                s.start_time_unix_nano / 1000,
                s.end_time_unix_nano / 1000
            )
        })
        .collect();
    let want: BTreeSet<String> = strings(
        conn,
        "SELECT span_id || ' ' || CASE WHEN parent_span_id IS NULL THEN 'None' \
           ELSE 'Some(' || parent_span_id || ')' END || ' ' || name || ' ' || start_us || ' ' \
           || coalesce(end_us, start_us) FROM spans",
    );
    assert_eq!(got, want);
    let trace_id = hex::decode(derive::trace_id(&session_id)).expect("hex");
    assert!(spans.iter().all(|s| s.trace_id == trace_id));
    let events: BTreeSet<String> = spans
        .iter()
        .flat_map(|s| {
            let id = i64::from_be_bytes(s.span_id.as_slice().try_into().expect("8 bytes"));
            s.events
                .iter()
                .map(move |e| format!("{id} {} {}", e.name, e.time_unix_nano / 1000))
        })
        .collect();
    assert_eq!(
        events,
        strings(
            conn,
            "SELECT span_id || ' ' || name || ' ' || time_us FROM span_events"
        )
    );
    assert_eq!(written.span_events, count("span_events"));
    let results = spans.iter().filter(|s| s.name == "heph.result");
    for s in results {
        assert!(
            attr(&s.attributes, "heph.addr").is_some(),
            "{} has no addr",
            s.name
        );
        let code = s.status.as_option().map(|st| st.code.to_i32());
        assert!(code.is_some(), "a result has a status");
    }

    // Every log, in order.
    let records: Vec<&logs::LogRecord> = d
        .logs
        .iter()
        .flat_map(|l| &l.resource_logs)
        .flat_map(|r| &r.scope_logs)
        .flat_map(|s| &s.log_records)
        .collect();
    assert_eq!(records.len(), count("logs"));
    assert_eq!(written.logs, records.len());
    let got: Vec<String> = records
        .iter()
        .map(|r| {
            let body = r.body.as_option().and_then(str_of).unwrap_or_default();
            format!("{} {} {body}", r.time_unix_nano / 1000, r.severity_text)
        })
        .collect();
    let want: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT time_us || ' ' || severity || ' ' || body FROM logs ORDER BY seq")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows")
    };
    assert_eq!(got, want);
    assert!(records.iter().all(|r| r.trace_id == trace_id));

    // Every metric row is a data point, of the type its kind maps to.
    let ms: Vec<&metrics::Metric> = d
        .metrics
        .iter()
        .flat_map(|m| &m.resource_metrics)
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
        .collect();
    let mut points = BTreeSet::new();
    let mut n = 0;
    for m in &ms {
        let (kind, dps) = match m.data.as_ref().expect("data") {
            metrics::metric::Data::Gauge(g) => ("gauge", &g.data_points),
            metrics::metric::Data::Sum(s) => {
                assert!(s.is_monotonic, "{} is monotonic", m.name);
                assert_eq!(
                    s.aggregation_temporality.to_i32(),
                    metrics::AggregationTemporality::AGGREGATION_TEMPORALITY_CUMULATIVE.to_i32()
                );
                assert!(
                    dps_start(&s.data_points),
                    "{}: a sum starts at the session",
                    m.name
                );
                ("counter", &s.data_points)
            }
            other => panic!("unexpected metric data {other:?}"),
        };
        for p in dps {
            n += 1;
            let v = match p.value {
                Some(metrics::number_data_point::Value::AsDouble(f)) => f,
                _ => panic!("a double"),
            };
            points.insert(format!(
                "{} {kind} {} {} {v}",
                m.name,
                m.unit,
                p.time_unix_nano / 1000
            ));
        }
    }
    assert_eq!(n, count("metrics"));
    assert_eq!(written.points, n);
    let want = {
        let mut stmt = conn
            .prepare("SELECT name, kind, unit, time_us, value FROM metrics")
            .expect("prepare");
        stmt.query_map([], |r| {
            Ok(format!(
                "{} {} {} {} {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, f64>(4)?
            ))
        })
        .expect("query")
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .expect("rows")
    };
    assert_eq!(points, want);
    assert!(
        ms.iter()
            .any(|m| matches!(m.data, Some(metrics::metric::Data::Sum(_))))
    );
    assert!(
        ms.iter()
            .any(|m| matches!(m.data, Some(metrics::metric::Data::Gauge(_))))
    );
}

fn dps_start(points: &[metrics::NumberDataPoint]) -> bool {
    points
        .iter()
        .all(|p| p.start_time_unix_nano > 0 && p.start_time_unix_nano <= p.time_unix_nano)
}

/// Every id in `v`: (key, value).
fn ids(v: &Json, out: &mut Vec<(String, String)>) {
    match v {
        Json::Object(map) => {
            for (k, child) in map {
                match (k.as_str(), child) {
                    ("traceId" | "spanId" | "parentSpanId", Json::String(s)) => {
                        out.push((k.clone(), s.clone()));
                    }
                    _ => ids(child, out),
                }
            }
        }
        Json::Array(items) => items.iter().for_each(|i| ids(i, out)),
        _ => {}
    }
}

/// Every `(key, value)` whose key is one of `keys`.
fn values_of<'a>(v: &'a Json, keys: &[&str], out: &mut Vec<(&'a str, &'a Json)>) {
    match v {
        Json::Object(map) => {
            for (k, child) in map {
                if keys.contains(&k.as_str()) || k.ends_with("UnixNano") {
                    out.push((k, child));
                }
                values_of(child, keys, out);
            }
        }
        Json::Array(items) => items.iter().for_each(|i| values_of(i, keys, out)),
        _ => {}
    }
}

#[test]
fn otlp_is_conformant() {
    let path = fixture();
    let (first, _) = export(&path);
    let (second, _) = export(&path);
    // The trace id derives from the session id by a fixed rule: a second
    // export is the same, byte for byte.
    assert_eq!(first, second);

    let mut signals = Vec::new();
    for line in first.lines() {
        let v: Json = serde_json::from_str(line).expect("one JSON value per line");
        let obj = v.as_object().expect("an object");
        assert_eq!(obj.len(), 1, "one signal per line: {line}");
        signals.extend(obj.keys().cloned());

        let mut found = Vec::new();
        ids(&v, &mut found);
        // Spans and log records carry ids; metrics none.
        assert_eq!(
            !found.is_empty(),
            !obj.contains_key("resourceMetrics"),
            "{line}"
        );
        for (k, id) in &found {
            let len = if k == "traceId" { 32 } else { 16 };
            assert_eq!(id.len(), len, "{k} {id}");
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{k} is lowercase hex: {id}"
            );
            assert!(id.bytes().any(|b| b != b'0'), "{k} is non-zero");
        }

        let mut vals = Vec::new();
        values_of(
            &v,
            &[
                "kind",
                "code",
                "severityNumber",
                "aggregationTemporality",
                "intValue",
            ],
            &mut vals,
        );
        for (k, val) in vals {
            match k {
                "kind" | "code" | "severityNumber" | "aggregationTemporality" => {
                    assert!(val.is_u64(), "{k} is an integer: {val}");
                }
                // int64 and fixed64 values are strings of digits.
                _ => {
                    let s = val
                        .as_str()
                        .unwrap_or_else(|| panic!("{k} is a string: {val}"));
                    assert!(
                        !s.is_empty()
                            && s.trim_start_matches('-')
                                .bytes()
                                .all(|b| b.is_ascii_digit()),
                        "{k} is a decimal string: {s}"
                    );
                }
            }
        }
    }
    signals.sort();
    assert_eq!(
        signals,
        ["resourceLogs", "resourceMetrics", "resourceSpans"]
    );
    // And each line decodes with the OTLP proto types.
    decode(&first);
}

/// The shaping keys on proto field names only: an attribute whose key reads
/// like an id or an enum field keeps its value, wherever it is nested.
#[test]
fn otlp_shape_leaves_attribute_values_alone() {
    let lookalike = |key: &str| common::KeyValue {
        key: key.into(),
        value: common::AnyValue {
            value: Some(common::any_value::Value::StringValue("AAAA".into())),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let span = trace::Span {
        trace_id: vec![1; 16],
        span_id: vec![2; 8],
        kind: trace::span::SpanKind::SPAN_KIND_INTERNAL.into(),
        attributes: vec![
            lookalike("traceId"),
            lookalike("kind"),
            common::KeyValue {
                key: "nested".into(),
                value: common::AnyValue {
                    value: Some(common::any_value::Value::from(common::KeyValueList {
                        values: vec![lookalike("spanId"), lookalike("code")],
                        ..Default::default()
                    })),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mut v = serde_json::to_value(&span).expect("encode");
    otlp::otlp_shape(&mut v).expect("shape");
    assert_eq!(v["traceId"], "01010101010101010101010101010101");
    assert_eq!(v["spanId"], "0202020202020202");
    assert_eq!(v["kind"], 1);
    let text = v.to_string();
    assert_eq!(text.matches("\"AAAA\"").count(), 4, "{text}");
}
