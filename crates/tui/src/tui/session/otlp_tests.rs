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
    round_trips(&path, &d, written);
}

/// What `text` decoded to (`d`) holds every explorer row of the recording
/// at `path`, whatever lines it was cut into.
fn round_trips(path: &Path, d: &Decoded, written: Written) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ex = explorer(path, tmp.path());
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
    let spans: Vec<&trace::Span> = d
        .traces
        .iter()
        .flat_map(|t| &t.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .collect();
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

/// A session of thousands of spans, logs and metric points is cut into
/// lines of at most `LINE_ITEMS` each (a span counted with its events; a
/// metric runs on into the next line), each line a valid message of its
/// signal, spans first, then logs, then metrics; and together the lines
/// still hold every row exactly.
#[test]
fn otlp_cuts_a_large_session_into_bounded_lines() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(store::FILE);
    std::fs::copy(fixture(), &path).expect("copy the fixture");
    let many = otlp::LINE_ITEMS * 2 + 500;
    // Explorer rows written straight into the copy: it is stamped with the
    // current derivation, or opening it re-derives them away.
    rusqlite::Connection::open(&path)
        .expect("open the copy")
        .execute_batch(&format!(
            "CREATE TEMP TABLE k AS WITH RECURSIVE k(i) AS \
               (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < {many}) SELECT i FROM k;
             INSERT INTO spans (session_id, span_id, parent_span_id, name, class, addr, \
               start_us, end_us, duration_us, status, attributes) \
               SELECT s.id, 1000000 + i, NULL, 'heph.result', 'result', '//many:t' || i, \
                 s.started_ms * 1000 + i, s.started_ms * 1000 + i + 10, 10, 'ok', \
                 json_object('n', i) FROM temp.k, session s;
             INSERT INTO span_events (session_id, span_id, name, time_us, attributes) \
               SELECT s.id, 1000000 + i, 'cache.miss', s.started_ms * 1000 + i + 5, NULL \
               FROM temp.k, session s WHERE i % 2 = 0;
             INSERT INTO records (at_ms, kind, addr, body) \
               SELECT s.started_ms + i, 'Log', NULL, json_object('session', \
                 json_object('type', 'Log', 'level', 'info', 'msg', 'line ' || i)) \
               FROM temp.k, session s;
             INSERT INTO metrics (session_id, name, kind, unit, time_us, value, attributes) \
               SELECT s.id, 'heph.many', 'gauge', '1', s.started_ms * 1000 + i, i, NULL \
               FROM temp.k, session s;
             UPDATE session SET derivation = {derivation};",
            derivation = store::DERIVATION,
        ))
        .expect("grow the recording");

    let (text, written) = export(&path);
    assert!(written.spans > many && written.logs > many && written.points > many);
    let d = decode(&text);
    let lines = |items: usize| items.div_ceil(otlp::LINE_ITEMS);
    assert_eq!(
        (d.logs.len(), d.metrics.len()),
        (lines(written.logs), lines(written.points)),
        "{written:?}"
    );
    // A span counts with its events, and is never split from them: a line
    // of spans is cut when the next span would not fit.
    let span_items = |s: &trace::Span| 1 + s.events.len();
    let spans: Vec<usize> = d
        .traces
        .iter()
        .map(|t| {
            t.resource_spans
                .iter()
                .flat_map(|r| &r.scope_spans)
                .flat_map(|s| &s.spans)
                .map(span_items)
                .sum()
        })
        .collect();
    assert_eq!(
        spans.iter().sum::<usize>(),
        written.spans + written.span_events,
        "every span and event is on a line once"
    );
    let largest = d
        .traces
        .iter()
        .flat_map(|t| &t.resource_spans)
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .map(span_items)
        .max()
        .expect("a span");
    let (last, full) = spans.split_last().expect("a line");
    assert!(
        full.iter()
            .all(|&n| n <= otlp::LINE_ITEMS && n + largest > otlp::LINE_ITEMS)
            && (1..=otlp::LINE_ITEMS).contains(last),
        "spans and events a line: {spans:?}"
    );
    let logs: Vec<usize> = d
        .logs
        .iter()
        .map(|l| {
            l.resource_logs
                .iter()
                .flat_map(|r| &r.scope_logs)
                .map(|s| s.log_records.len())
                .sum()
        })
        .collect();
    let points: Vec<usize> = d
        .metrics
        .iter()
        .map(|m| {
            m.resource_metrics
                .iter()
                .flat_map(|r| &r.scope_metrics)
                .flat_map(|s| &s.metrics)
                .map(|m| match m.data.as_ref().expect("data") {
                    metrics::metric::Data::Gauge(g) => g.data_points.len(),
                    metrics::metric::Data::Sum(s) => s.data_points.len(),
                    other => panic!("unexpected metric data {other:?}"),
                })
                .sum()
        })
        .collect();
    for (signal, per_line) in [("logs", &logs), ("points", &points)] {
        let (last, full) = per_line.split_last().expect("a line");
        assert!(
            full.iter().all(|&n| n == otlp::LINE_ITEMS) && (1..=otlp::LINE_ITEMS).contains(last),
            "{signal} a line: {per_line:?}"
        );
    }
    let many_in = d
        .metrics
        .iter()
        .filter(|m| {
            m.resource_metrics
                .iter()
                .flat_map(|r| &r.scope_metrics)
                .flat_map(|s| &s.metrics)
                .any(|m| m.name == "heph.many")
        })
        .count();
    assert_eq!(many_in, 3, "one metric runs on across lines");
    // Each line is bounded: a line of spans is far from the session's size.
    let longest = text.lines().map(str::len).max().unwrap_or_default();
    assert!(
        longest < text.len() / 2,
        "longest line {longest} of {}",
        text.len()
    );
    // The signals' lines in order: every span line, then logs, then metrics.
    let order: Vec<&str> = text
        .lines()
        .map(|l| {
            let v: Json = serde_json::from_str(l).expect("a JSON line");
            match v
                .as_object()
                .and_then(|o| o.keys().next())
                .map(String::as_str)
            {
                Some("resourceSpans") => "spans",
                Some("resourceLogs") => "logs",
                _ => "metrics",
            }
        })
        .collect();
    let mut sorted = order.clone();
    sorted.sort_by_key(|s| ["spans", "logs", "metrics"].iter().position(|x| x == s));
    assert_eq!(order, sorted);

    round_trips(&path, &d, written);
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

/// A span's events count toward its line's bound, and a span is never split
/// from them: one with more events than the bound is a line alone, and the
/// lines around it stay within it.
#[test]
fn otlp_counts_span_events_toward_a_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(store::FILE);
    std::fs::copy(fixture(), &path).expect("copy the fixture");
    // Stamped current, as in `otlp_cuts_a_large_session_into_bounded_lines`.
    rusqlite::Connection::open(&path)
        .expect("open the copy")
        .execute_batch(&format!(
            "INSERT INTO spans (session_id, span_id, parent_span_id, name, class, start_us, \
               end_us, duration_us, status) \
               SELECT s.id, 2000000, NULL, 'heph.busy', 'phase', s.started_ms * 1000, \
                 s.started_ms * 1000 + 10, 10, 'ok' FROM session s;
             WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < 5) \
             INSERT INTO span_events (session_id, span_id, name, time_us, attributes) \
               SELECT s.id, 2000000, 'tick', s.started_ms * 1000 + i, NULL FROM k, session s;
             UPDATE session SET derivation = {derivation};",
            derivation = store::DERIVATION,
        ))
        .expect("grow the recording");
    let per_line = 3;
    let work = tempfile::tempdir().expect("tempdir");
    let ex = explorer(&path, work.path());
    let mut out = Vec::new();
    let written = otlp::write_lines(&ex, &mut out, per_line).expect("export");
    let d = decode(&String::from_utf8(out).expect("utf-8"));
    let lines: Vec<Vec<&trace::Span>> = d
        .traces
        .iter()
        .map(|t| {
            t.resource_spans
                .iter()
                .flat_map(|r| &r.scope_spans)
                .flat_map(|s| &s.spans)
                .collect()
        })
        .collect();
    assert_eq!(
        lines.iter().map(Vec::len).sum::<usize>(),
        written.spans,
        "every span is on a line"
    );
    let busy = lines
        .iter()
        .find(|l| l.iter().any(|s| s.name == "heph.busy"))
        .expect("the busy span's line");
    assert_eq!(
        (busy.len(), busy.first().map(|s| s.events.len())),
        (1, Some(5)),
        "a span over the bound is a line alone, with all its events"
    );
    for line in lines
        .iter()
        .filter(|l| l.iter().all(|s| s.name != "heph.busy"))
    {
        let items: usize = line.iter().map(|s| 1 + s.events.len()).sum();
        assert!(
            (1..=per_line).contains(&items),
            "a line of {items} spans and events"
        );
    }
}

/// A line is cut only when the next span would take it past the bound: a
/// line that reaches the bound exactly is kept whole, and no two lines in a
/// row would have fit in one.
#[test]
fn otlp_fills_a_line_up_to_its_bound() {
    let work = tempfile::tempdir().expect("tempdir");
    let ex = explorer(&fixture(), work.path());
    for per_line in [2, 3] {
        let mut out = Vec::new();
        otlp::write_lines(&ex, &mut out, per_line).expect("export");
        let d = decode(&String::from_utf8(out).expect("utf-8"));
        let lines: Vec<Vec<usize>> = d
            .traces
            .iter()
            .map(|t| {
                t.resource_spans
                    .iter()
                    .flat_map(|r| &r.scope_spans)
                    .flat_map(|s| &s.spans)
                    .map(|s| 1 + s.events.len())
                    .collect()
            })
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l.len() > 1 && l.iter().sum::<usize>() == per_line),
            "no line of several spans reaches the bound of {per_line} exactly: {lines:?}"
        );
        for pair in lines.windows(2) {
            let (line, next) = (&pair[0], &pair[1]);
            let first = next.first().copied().unwrap_or_default();
            assert!(
                line.iter().sum::<usize>() + first > per_line,
                "a line was cut before its bound of {per_line}: {line:?} then {next:?}"
            );
        }
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

/// Spec row `otlp_maps_machine_metrics` (D27): a recording with every
/// machine name exports each mapped one under its semconv name and
/// instrument (`process.cpu.time` a monotonic cumulative sum, the
/// up-down counters non-monotonic, both from the session's start), keeps
/// the rest under heph's name as gauges, and stamps the semconv version as
/// `schema_url` on every resource and scope. A row with a machine name but
/// a kind or unit heph does not write is not mapped.
#[test]
fn otlp_maps_machine_metrics() {
    let id = super::id::SessionId::new(hcore::events::now_unix_ms(), 41);
    let s = id.started_unix_ms();
    let (root, conn) =
        super::store_tests::derived(id, &super::tests::machine_records(s), s + 6_000);
    // The table is what derivation writes: names, kinds and units.
    let written = strings(
        &conn,
        "SELECT DISTINCT name || ' ' || kind || ' ' || unit FROM metrics",
    );
    let table: BTreeSet<String> = otlp::MACHINE_METRICS
        .iter()
        .map(|m| format!("{} {} {}", m.name, m.kind, m.unit))
        .collect();
    assert_eq!(written, table);
    conn.execute(
        "INSERT INTO metrics (session_id, name, kind, unit, time_us, value, attributes) \
         SELECT id, 'heph.machine.cpu.count', 'gauge', 'By', ?1, 3, NULL FROM session",
        [i64::try_from((s + 2_000) * 1000).expect("time")],
    )
    .expect("a row heph does not write");
    drop(conn);
    let path = root.path().join(id.encode()).join(store::FILE);
    let (text, _) = export(&path);
    let d = decode(&text);

    // Every resource and scope, of every signal, names the semconv version.
    let mut stamped = 0;
    for t in &d.traces {
        for r in &t.resource_spans {
            assert_eq!(r.schema_url, otlp::SCHEMA_URL);
            stamped += 1;
            for sc in &r.scope_spans {
                assert_eq!(sc.schema_url, otlp::SCHEMA_URL);
            }
        }
    }
    for l in &d.logs {
        for r in &l.resource_logs {
            assert_eq!(r.schema_url, otlp::SCHEMA_URL);
            stamped += 1;
            for sc in &r.scope_logs {
                assert_eq!(sc.schema_url, otlp::SCHEMA_URL);
            }
        }
    }
    for m in &d.metrics {
        for r in &m.resource_metrics {
            assert_eq!(r.schema_url, otlp::SCHEMA_URL);
            stamped += 1;
            for sc in &r.scope_metrics {
                assert_eq!(sc.schema_url, otlp::SCHEMA_URL);
            }
        }
    }
    assert!(stamped >= 2, "spans and metrics at least: {stamped}");
    assert_eq!(
        otlp::SCHEMA_URL,
        format!("https://opentelemetry.io/schemas/{}", otlp::SEMCONV_VERSION)
    );

    // Each exported metric: (name, unit) → (instrument, points as
    // (start, time, value)).
    type Points = Vec<(u64, u64, f64)>;
    let mut got: std::collections::BTreeMap<(String, String), (&str, Points)> = Default::default();
    for m in d
        .metrics
        .iter()
        .flat_map(|m| &m.resource_metrics)
        .flat_map(|r| &r.scope_metrics)
        .flat_map(|s| &s.metrics)
    {
        let (instrument, dps) = match m.data.as_ref().expect("data") {
            metrics::metric::Data::Gauge(g) => ("gauge", &g.data_points),
            metrics::metric::Data::Sum(sum) => {
                assert_eq!(
                    sum.aggregation_temporality.to_i32(),
                    metrics::AggregationTemporality::AGGREGATION_TEMPORALITY_CUMULATIVE.to_i32(),
                    "{}",
                    m.name
                );
                assert!(dps_start(&sum.data_points), "{}", m.name);
                (
                    if sum.is_monotonic {
                        "counter"
                    } else {
                        "updowncounter"
                    },
                    &sum.data_points,
                )
            }
            other => panic!("unexpected metric data {other:?}"),
        };
        let entry = got
            .entry((m.name.clone(), m.unit.clone()))
            .or_insert((instrument, Vec::new()));
        assert_eq!(entry.0, instrument, "{}", m.name);
        entry.1.extend(dps.iter().map(|p| {
            let v = match p.value {
                Some(metrics::number_data_point::Value::AsDouble(f)) => f,
                _ => panic!("a double"),
            };
            (p.start_time_unix_nano, p.time_unix_nano / 1000, v)
        }));
    }

    let start_ns = s * 1_000_000;
    let tmp = tempfile::tempdir().expect("tempdir");
    let ex = explorer(&path, tmp.path());
    for m in otlp::MACHINE_METRICS {
        let (name, instrument) = m
            .otel
            .map_or((m.name, otlp::Instrument::Gauge), |(to, i)| (to, i));
        let want: Vec<(u64, u64, f64)> = {
            let mut stmt = ex
                .conn()
                .prepare(
                    "SELECT time_us, value FROM metrics WHERE name = ?1 AND unit = ?2 \
                     ORDER BY time_us",
                )
                .expect("prepare");
            stmt.query_map([m.name, m.unit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .expect("query")
            .map(|r| {
                let (t, v) = r.expect("row");
                let start = if instrument == otlp::Instrument::Gauge {
                    0
                } else {
                    start_ns
                };
                (start, u64::try_from(t).expect("time"), v)
            })
            .collect()
        };
        assert!(!want.is_empty(), "{} is in the recording", m.name);
        assert_eq!(
            got.remove(&(name.to_owned(), m.unit.to_owned())),
            Some((instrument.as_str(), want)),
            "{} exported as {name}",
            m.name
        );
    }
    // What is left: the row with a unit heph does not write, under its own
    // name as a gauge, and no other machine name.
    assert_eq!(
        got.into_iter()
            .filter(|((name, _), _)| name.starts_with("heph.machine.")
                || name.starts_with("heph.process.")
                || name.starts_with("heph.session.")
                || name.starts_with("system.")
                || name.starts_with("process."))
            .map(|((name, unit), (instrument, points))| (name, unit, instrument, points.len()))
            .collect::<Vec<_>>(),
        [(
            "heph.machine.cpu.count".to_owned(),
            "By".to_owned(),
            "gauge",
            1
        )]
    );

    // The table: the four semconv names and instruments, the rest unmapped.
    let mapped: Vec<(&str, &str, &str)> = otlp::MACHINE_METRICS
        .iter()
        .filter_map(|m| m.otel.map(|(to, i)| (m.name, to, i.as_str())))
        .collect();
    assert_eq!(
        mapped,
        [
            (
                "heph.machine.cpu.count",
                "system.cpu.logical.count",
                "updowncounter"
            ),
            (
                "heph.machine.memory.limit",
                "system.memory.limit",
                "updowncounter"
            ),
            ("heph.process.cpu.time", "process.cpu.time", "counter"),
            (
                "heph.process.memory.resident",
                "process.memory.usage",
                "updowncounter"
            ),
        ]
    );
}
