//! The recipes `docs/SESSIONS.md` carries (spec row `recipes_run`): every
//! one runs on a fixture recording, and the built-in ones are word for word
//! what `--schema` prints, give the columns the docs list, and return the
//! fixture's rows through the query engine.

use rusqlite::types::Value;

use super::*;
use crate::tui::session::query::{self as engine, Limits, RECIPES, RecordingFile, Source};
use crate::tui::session::recording::Recording;
use crate::tui::session::store;

fn doc() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/SESSIONS.md"))
        .expect("read docs/SESSIONS.md")
}

/// Every statement of every `sql` block: its leading `--` lines joined (the
/// header), and the SQL after them without its `;`.
fn statements(doc: &str) -> Vec<(String, String)> {
    doc.split("```sql\n")
        .skip(1)
        .filter_map(|rest| rest.split("```").next())
        .flat_map(|block| block.split(';'))
        .filter(|stmt| !stmt.trim().is_empty())
        .map(|stmt| {
            let stmt = stmt.trim();
            let header: Vec<&str> = stmt
                .lines()
                .take_while(|l| l.starts_with("--"))
                .map(|l| l.trim_start_matches("--").trim())
                .collect();
            let sql: Vec<&str> = stmt.lines().skip(header.len()).collect();
            (header.join(" "), sql.join("\n"))
        })
        .collect()
}

/// The columns the built-in recipes table lists for `name`: the backticked
/// words of its Columns cell, outside parentheses.
fn documented_columns(doc: &str, name: &str) -> Vec<String> {
    let row = doc
        .lines()
        .find(|l| l.starts_with(&format!("| `{name}` | ")))
        .unwrap_or_else(|| panic!("{name} has a row in the built-in recipes table"));
    let cell = row
        .trim_end_matches('|')
        .rsplit(" | ")
        .next()
        .expect("a Columns cell");
    let mut depth = 0u32;
    let outside: String = cell
        .chars()
        .filter(|c| {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ => return depth == 0,
            }
            false
        })
        .collect();
    outside
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// A value as the expected rows spell it.
fn text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("{b:?}"),
    }
}

/// The current format's committed fixture (`crates/tui`'s testdata): one
/// session, `//p:t` ran with resolve, admission, process and collect
/// phases, `//p:dep` a local hit.
fn fixture(dir: &Path) -> PathBuf {
    let committed = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("crates/tui/src/tui/session/testdata")
        .join(format!("format-{}", store::FORMAT))
        .join(store::FILE);
    let copy = dir.join(store::FILE);
    std::fs::copy(&committed, &copy).expect("copy the fixture");
    copy
}

/// Spec row `recipes_run`.
#[test]
fn recipes_run() {
    let doc = doc();
    let all = statements(&doc);

    // The built-in recipes are in the docs word for word, in order (and
    // `query_schema_lists_every_name` holds `--schema` to the same list).
    let documented: Vec<(String, String)> = all
        .iter()
        .filter(|(h, _)| {
            RECIPES
                .iter()
                .any(|r| h.starts_with(&format!("{}:", r.name)))
        })
        .cloned()
        .collect();
    let built_in: Vec<(String, String)> = RECIPES
        .iter()
        .map(|r| {
            let header: Vec<String> = r
                .comment()
                .lines()
                .map(|l| l.trim_start_matches("--").trim().to_owned())
                .collect();
            (header.join(" "), r.sql.to_owned())
        })
        .collect();
    assert_eq!(documented, built_in, "docs/SESSIONS.md's built-in recipes");

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = fixture(tmp.path());

    // Every recipe in the docs runs on the fixture as `sqlite3` would run
    // it on a sealed recording.
    let db = store::open_sealed(&path).expect("open the fixture");
    assert!(all.len() > RECIPES.len(), "{all:?}");
    for (header, sql) in &all {
        let mut stmt = db
            .prepare(sql)
            .unwrap_or_else(|e| panic!("{header:?} compiles: {e}\n{sql}"));
        let mut rows = stmt.query([]).expect("runs");
        while rows
            .next()
            .unwrap_or_else(|e| panic!("{header:?} runs: {e}\n{sql}"))
            .is_some()
        {}
    }
    drop(db);

    // The built-in ones through the engine, as `heph session query --from`
    // runs them: the columns the docs list, and the fixture's rows.
    let expected: [(&str, &[&[&str]]); 6] = [
        (
            "time_by_phase",
            &[
                &["target", "heph.process", "1", "0.003"],
                &["heph", "heph.collect", "1", "0.001"],
                &["heph", "heph.resolve", "1", "0.001"],
                &["waiting", "heph.admission", "1", "0.001"],
            ],
        ),
        ("uncovered_wall", &[&["cbszjpg001", "0.012", "0.003", "75"]]),
        (
            "phase_percentiles",
            &[
                &["target", "heph.process", "1", "3", "3", "3", "3"],
                &["heph", "heph.collect", "1", "1", "1", "1", "1"],
                &["heph", "heph.resolve", "1", "1", "1", "1", "1"],
                &["waiting", "heph.admission", "1", "1", "1", "1", "1"],
            ],
        ),
        ("hit_rate_by_package", &[&["//p", "2", "1", "50"]]),
        // Recorded before machine stats: the wait reads unknown.
        (
            "waiting_by_load",
            &[&["cbszjpg001", "admission", "0", "0", "1000"]],
        ),
        // Its cpu pool has no sample to be set against: the whole session
        // reads unknown.
        (
            "resources_vs_machine",
            &[&["cbszjpg001", "cpu", "0", "0", "0", "12000"]],
        ),
    ];
    assert_eq!(expected.len(), RECIPES.len());
    for (r, (name, rows)) in RECIPES.iter().zip(expected) {
        assert_eq!(r.name, name);
        let recording =
            Recording::open_foreign(&path, tmp.path()).expect("open the fixture as a recording");
        let source = Source::Recording(RecordingFile::foreign(recording).expect("a source"));
        let answer = engine::run(r.sql, vec![source], tmp.path(), &Limits::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(answer.columns, documented_columns(&doc, name), "{name}");
        let got: Vec<Vec<String>> = answer
            .rows
            .iter()
            .map(|row| row.iter().map(text).collect())
            .collect();
        let want: Vec<Vec<String>> = rows
            .iter()
            .map(|row| row.iter().map(|v| (*v).to_owned()).collect())
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

/// Run built-in recipe `name` over `setup`, rows as [`text`] spells them.
fn run_recipe(name: &str, setup: &str) -> Vec<Vec<String>> {
    let db = rusqlite::Connection::open_in_memory().expect("in-memory db");
    db.execute_batch(setup).expect("setup");
    recipe_rows(name, &db)
}

/// Run built-in recipe `name` over `db`, rows as [`text`] spells them.
fn recipe_rows(name: &str, db: &rusqlite::Connection) -> Vec<Vec<String>> {
    let sql = RECIPES
        .iter()
        .find(|r| r.name == name)
        .expect("a built-in recipe")
        .sql;
    let mut stmt = db.prepare(sql).expect("compiles");
    let n = stmt.column_count();
    stmt.query_map([], |row| {
        (0..n)
            .map(|i| row.get::<_, Value>(i).map(|v| text(&v)))
            .collect()
    })
    .expect("runs")
    .collect::<Result<_, _>>()
    .expect("rows")
}

/// `uncovered_wall` counts each instant once: overlapping, touching and
/// nested target spans merge; a span with no end, a non-target span and
/// another session's spans do not count; a live session has no row.
#[test]
fn uncovered_wall_merges_overlapping_runs() {
    let rows = run_recipe(
        "uncovered_wall",
        "CREATE TABLE sessions (session_id, started_ms, ended_ms);
         CREATE TABLE spans (session_id, class, start_us, end_us);
         INSERT INTO sessions VALUES ('a', 0, 100), ('b', 1000, 1100), ('live', 2000, NULL);
         INSERT INTO spans VALUES
           ('a', 'target', 10000, 20000),
           ('a', 'target', 15000, 30000),
           ('a', 'target', 30000, 40000),
           ('a', 'target', 50000, 60000),
           ('a', 'target', 52000, 55000),
           ('a', 'target', 70000, NULL),
           ('a', 'heph', 60000, 100000),
           ('b', 'heph', 1000000, 1100000),
           ('live', 'target', 2000000, 2050000);",
    );
    assert_eq!(
        rows,
        [
            ["b", "0.1", "0", "100"].map(str::to_owned),
            ["a", "0.1", "0.04", "60"].map(str::to_owned),
        ]
    );
}

/// `phase_percentiles` is nearest rank, per class and name: ten durations
/// 1..=10 give p50 5, p90 9, p99 10; ties take the tied value.
#[test]
fn phase_percentiles_are_nearest_rank() {
    let rows = run_recipe(
        "phase_percentiles",
        "CREATE TABLE spans (class, name, duration_us);
         WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < 10)
         INSERT INTO spans SELECT 'target', 'heph.process', i * 1000 FROM k;
         INSERT INTO spans VALUES
           ('heph', 'heph.resolve', 1000), ('heph', 'heph.resolve', 1000),
           ('heph', 'heph.resolve', 1000), ('heph', 'heph.resolve', 10000),
           ('heph', 'heph.process', NULL), (NULL, 'heph.result', 99000);",
    );
    assert_eq!(
        rows,
        [
            ["heph", "heph.resolve", "4", "1", "10", "10", "10"].map(str::to_owned),
            ["target", "heph.process", "10", "5", "9", "10", "10"].map(str::to_owned),
        ]
    );
}

/// One `waiting_split_by_load` case: the machine rows, the waits, and the
/// split it reads.
struct SplitCase {
    /// The spec's row, as the failure names it.
    row: &'static str,
    /// `(metric, ms, value)`; a `heph.machine.memory.limit` of 100 is added
    /// when there is any.
    metrics: Vec<(&'static str, i64, f64)>,
    /// `(span name, start ms, end ms)`: waiting spans.
    waits: Vec<(&'static str, i64, Option<i64>)>,
    /// `(phase, busy ms, spare ms, unknown ms)`, by phase.
    want: Vec<(&'static str, i64, i64, i64)>,
}

const BUSY: &str = "heph.machine.cpu.busy";
const USED: &str = "heph.machine.memory.used";

/// A sample at `ms`: CPU busy `cpu`, memory at 10% of the limit.
fn load(ms: i64, cpu: f64) -> [(&'static str, i64, f64); 2] {
    [(BUSY, ms, cpu), (USED, ms, 10.0)]
}

/// Samples at each of `points`, `(ms, cpu)`, memory at 10%.
fn loads(points: &[(i64, f64)]) -> Vec<(&'static str, i64, f64)> {
    points.iter().flat_map(|&(ms, cpu)| load(ms, cpu)).collect()
}

/// Write `case` into `db` as session `session`.
fn write_split_case(db: &rusqlite::Connection, session: &str, case: &SplitCase) {
    if !case.metrics.is_empty() {
        db.execute(
            "INSERT INTO metrics VALUES (?1, 'heph.machine.memory.limit', 0, 100.0)",
            [session],
        )
        .expect("the limit");
    }
    for &(name, ms, value) in &case.metrics {
        // A NaN binds as NULL, as SQLite stores it.
        db.execute(
            "INSERT INTO metrics VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session, name, ms * 1_000, value],
        )
        .expect("a metric");
    }
    for (i, &(name, start, end)) in case.waits.iter().enumerate() {
        db.execute(
            "INSERT INTO spans VALUES (?1, ?2, ?3, 'waiting', ?4, ?5)",
            rusqlite::params![
                session,
                i64::try_from(i).expect("few"),
                name,
                start * 1_000,
                end.map(|e| e * 1_000)
            ],
        )
        .expect("a wait");
    }
}

fn split_db() -> rusqlite::Connection {
    let db = rusqlite::Connection::open_in_memory().expect("in-memory db");
    db.execute_batch(
        "CREATE TABLE metrics (session_id, name, time_us, value);
         CREATE TABLE spans (session_id, span_id, name, class, start_us, end_us);",
    )
    .expect("schema");
    db
}

/// The expected rows of `case` as session `session`.
fn split_want(session: &str, case: &SplitCase) -> Vec<Vec<String>> {
    case.want
        .iter()
        .map(|&(phase, busy, spare, unknown)| {
            vec![
                session.to_owned(),
                phase.to_owned(),
                (busy * 1_000).to_string(),
                (spare * 1_000).to_string(),
                (unknown * 1_000).to_string(),
            ]
        })
        .collect()
}

/// Spec row `waiting_split_by_load`: every row of the spec's "Waiting
/// split" table, each as its own session, then all of them in one database
/// (sessions never bleed into one another). The table's "two samples with
/// the same time_us" row reads "the busier one wins" (a decision recorded
/// in the PR: no insertion order is readable in SQL), and its clock-step
/// row is not a recipe input: the recipe reads `time_us` only, which the
/// recorder stamps from its monotonic anchor.
#[test]
fn waiting_split_by_load() {
    let cases = [
        SplitCase {
            row: "a 200 ms admission wait inside one interval at CPU 0.95: busy 200 ms",
            metrics: loads(&[(0, 0.5), (1000, 0.95)]),
            waits: vec![("heph.admission", 300, Some(500))],
            want: vec![("admission", 200, 0, 0)],
        },
        SplitCase {
            row: "a wait half in a 0.95 interval, half in a 0.5 one: half busy, half spare",
            metrics: loads(&[(0, 0.5), (1000, 0.95), (2000, 0.5)]),
            waits: vec![("heph.admission", 500, Some(1500))],
            want: vec![("admission", 500, 500, 0)],
        },
        SplitCase {
            row: "an interval at exactly 0.9: busy (0.89 is spare)",
            metrics: loads(&[(0, 0.5), (1000, 0.9), (2000, 0.89)]),
            waits: vec![("heph.admission", 0, Some(2000))],
            want: vec![("admission", 1000, 1000, 0)],
        },
        SplitCase {
            row: "CPU 0.3 with memory at 96%, and at exactly 95%: busy (94% is spare)",
            metrics: vec![
                (BUSY, 0, 0.3),
                (USED, 0, 96.0),
                (BUSY, 1000, 0.3),
                (USED, 1000, 96.0),
                (BUSY, 2000, 0.3),
                (USED, 2000, 95.0),
                (BUSY, 3000, 0.3),
                (USED, 3000, 94.0),
            ],
            waits: vec![("heph.admission", 0, Some(3000))],
            want: vec![("admission", 2000, 1000, 0)],
        },
        SplitCase {
            row: "a zero-length span: not counted",
            metrics: loads(&[(0, 0.5), (1000, 0.95)]),
            waits: vec![("heph.admission", 500, Some(500))],
            want: vec![],
        },
        SplitCase {
            row: "a span across the first sample: the part before it unknown",
            metrics: loads(&[(1000, 0.95), (2000, 0.95)]),
            waits: vec![("heph.admission", 500, Some(1500))],
            want: vec![("admission", 500, 0, 500)],
        },
        SplitCase {
            row: "a gap of exactly 2.0 s: classified",
            metrics: loads(&[(0, 0.5), (2000, 0.95)]),
            waits: vec![("heph.admission", 0, Some(2000))],
            want: vec![("admission", 2000, 0, 0)],
        },
        SplitCase {
            row: "gaps of 2.5 s and 30 s: unknown",
            metrics: loads(&[(0, 0.95), (2500, 0.95), (3500, 0.95), (33_500, 0.95)]),
            waits: vec![("heph.admission", 0, Some(33_500))],
            want: vec![("admission", 1000, 0, 32_500)],
        },
        SplitCase {
            row: "a span after the last sample (after End, past the cap): unknown, never spare",
            metrics: loads(&[(0, 0.5), (1000, 0.5)]),
            waits: vec![("heph.admission", 500, Some(2500))],
            want: vec![("admission", 0, 500, 1500)],
        },
        SplitCase {
            row: "a span after the CPU source stopped (memory still read): unknown, never spare",
            metrics: [
                load(0, 0.5).to_vec(),
                load(1000, 0.5).to_vec(),
                vec![(USED, 2000, 10.0), (USED, 3000, 10.0)],
            ]
            .concat(),
            waits: vec![("heph.admission", 500, Some(3000))],
            want: vec![("admission", 0, 500, 2000)],
        },
        SplitCase {
            row: "the memory source stopped, CPU below 0.9: unknown, never spare (busy still busy)",
            metrics: vec![
                (BUSY, 0, 0.5),
                (USED, 0, 10.0),
                (BUSY, 1000, 0.5),
                (BUSY, 2000, 0.95),
            ],
            waits: vec![("heph.admission", 0, Some(2000))],
            want: vec![("admission", 1000, 0, 1000)],
        },
        SplitCase {
            row: "a span with no end: cut at the last sample; one starting after it: not counted",
            metrics: loads(&[(0, 0.95), (1000, 0.95), (2000, 0.5)]),
            waits: vec![("heph.admission", 500, None), ("heph.deps", 2500, None)],
            want: vec![("admission", 500, 1000, 0)],
        },
        SplitCase {
            row: "two samples with the same time_us: the busier wins, no zero-length interval",
            metrics: vec![
                (BUSY, 0, 0.5),
                (USED, 0, 10.0),
                (BUSY, 1000, 0.95),
                (BUSY, 1000, 0.5),
                (USED, 1000, 10.0),
                (USED, 1000, 10.0),
                (BUSY, 2000, 0.5),
                (USED, 2000, 10.0),
            ],
            waits: vec![("heph.admission", 0, Some(2000))],
            want: vec![("admission", 1000, 1000, 0)],
        },
        SplitCase {
            row: "busy 1.5, NaN, or -0.1 in a foreign recording: that interval unknown",
            metrics: loads(&[
                (0, 0.5),
                (1000, 1.5),
                (2000, f64::NAN),
                (3000, -0.1),
                (4000, 0.95),
            ]),
            waits: vec![("heph.admission", 0, Some(4000))],
            want: vec![("admission", 1000, 0, 3000)],
        },
        SplitCase {
            row: "a recording with no machine rows: all unknown, and the recipe runs",
            metrics: vec![],
            waits: vec![("heph.admission", 0, Some(1000))],
            want: vec![("admission", 0, 0, 1000)],
        },
        SplitCase {
            row: "another process saturating the machine (heph's CPU time flat): busy",
            metrics: [
                load(0, 1.0).to_vec(),
                load(1000, 1.0).to_vec(),
                vec![
                    ("heph.process.cpu.time", 0, 0.25),
                    ("heph.process.cpu.time", 1000, 0.25),
                ],
            ]
            .concat(),
            waits: vec![("heph.admission", 0, Some(1000))],
            want: vec![("admission", 1000, 0, 0)],
        },
        SplitCase {
            row: "two targets waiting on deps at once for 1 s each: 2 s of deps time",
            metrics: loads(&[(0, 0.95), (1000, 0.95)]),
            waits: vec![("heph.deps", 0, Some(1000)), ("heph.deps", 0, Some(1000))],
            want: vec![("deps", 2000, 0, 0)],
        },
        SplitCase {
            row: "a deps wait at CPU 0.2: spare",
            metrics: loads(&[(0, 0.2), (1000, 0.2)]),
            waits: vec![("heph.deps", 0, Some(1000))],
            want: vec![("deps", 0, 1000, 0)],
        },
    ];

    let all = split_db();
    let mut want_all = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let db = split_db();
        write_split_case(&db, "s", case);
        assert_eq!(
            recipe_rows("waiting_by_load", &db),
            split_want("s", case),
            "{}",
            case.row
        );
        let session = format!("s{i:02}");
        write_split_case(&all, &session, case);
        want_all.extend(split_want(&session, case));
    }
    assert_eq!(
        recipe_rows("waiting_by_load", &all),
        want_all,
        "all at once"
    );
}

/// One row of the spec's "Resources vs machine" table.
struct VersusCase {
    row: &'static str,
    /// The session's end, ms after its start at 0; `None` while it runs.
    end_ms: Option<i64>,
    /// `(pool, ms, held, capacity)`: `heph.pool.held` and
    /// `heph.pool.capacity` rows.
    pools: Vec<(&'static str, i64, f64, f64)>,
    /// `(metric, ms, value)`.
    metrics: Vec<(&'static str, i64, f64)>,
    /// `(pool, declared above use ms, use above declared ms, matched ms,
    /// unknown ms)`, by pool.
    want: Vec<(&'static str, i64, i64, i64, i64)>,
}

/// CPU busy `cpu` at each of `ms`.
fn busy_at(ms: impl IntoIterator<Item = i64>, cpu: f64) -> Vec<(&'static str, i64, f64)> {
    ms.into_iter().map(|t| (BUSY, t, cpu)).collect()
}

/// A pool row: `held` of `capacity`.
fn held(pool: &'static str, ms: i64, held: f64, capacity: f64) -> (&'static str, i64, f64, f64) {
    (pool, ms, held, capacity)
}

fn versus_db() -> rusqlite::Connection {
    let db = rusqlite::Connection::open_in_memory().expect("in-memory db");
    db.execute_batch(
        "CREATE TABLE metrics (session_id, name, time_us, value, attributes);
         CREATE TABLE sessions (session_id, started_ms, ended_ms);",
    )
    .expect("schema");
    db
}

fn write_versus_case(db: &rusqlite::Connection, session: &str, case: &VersusCase) {
    db.execute(
        "INSERT INTO sessions VALUES (?1, 0, ?2)",
        rusqlite::params![session, case.end_ms],
    )
    .expect("the session");
    for &(pool, ms, held, capacity) in &case.pools {
        for (name, value) in [("heph.pool.held", held), ("heph.pool.capacity", capacity)] {
            db.execute(
                "INSERT INTO metrics VALUES (?1, ?2, ?3, ?4, json_object('heph.pool', ?5))",
                rusqlite::params![session, name, ms * 1_000, value, pool],
            )
            .expect("a pool row");
        }
    }
    for &(name, ms, value) in &case.metrics {
        db.execute(
            "INSERT INTO metrics VALUES (?1, ?2, ?3, ?4, NULL)",
            rusqlite::params![session, name, ms * 1_000, value],
        )
        .expect("a metric");
    }
}

fn versus_want(session: &str, case: &VersusCase) -> Vec<Vec<String>> {
    case.want
        .iter()
        .map(|&(pool, declared, used, matched, unknown)| {
            let mut row = vec![session.to_owned(), pool.to_owned()];
            row.extend([declared, used, matched, unknown].map(|ms| (ms * 1_000).to_string()));
            row
        })
        .collect()
}

/// Spec row `resources_vs_machine`: every row of the spec's "Resources vs
/// machine" table, each as its own session, then all of them in one
/// database. Sessions start at 0; a sample covers the interval since the
/// one before it, so the first sample of each case only opens the timeline.
#[test]
fn resources_vs_machine() {
    const LIMIT: &str = "heph.machine.cpu.limit";
    const MEM_LIMIT: &str = "heph.machine.memory.limit";
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let cases = [
        VersusCase {
            row: "cpu pool 8/8 held, machine 30% busy, for 10 s: 10 s declared above use",
            end_ms: Some(10_000),
            pools: vec![held("cpu", 0, 8.0, 8.0)],
            metrics: busy_at((0..=10).map(|s| s * 1000), 0.3),
            want: vec![("cpu", 10_000, 0, 0, 0)],
        },
        VersusCase {
            row: "cpu pool 2/8 held, machine 95% busy: use above declared",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 2.0, 8.0)],
            metrics: busy_at([0, 1000, 2000], 0.95),
            want: vec![("cpu", 0, 2000, 0, 0)],
        },
        VersusCase {
            row: "cpu pool 8/8 held, machine 95% busy: matched",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 8.0, 8.0)],
            metrics: busy_at([0, 1000, 2000], 0.95),
            want: vec![("cpu", 0, 0, 2000, 0)],
        },
        VersusCase {
            row: "held exactly 90% and busy exactly 50%: matched (and held at 90% \
                  against busy 30% is declared above use)",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 9.0, 10.0)],
            metrics: vec![(BUSY, 0, 0.5), (BUSY, 1000, 0.5), (BUSY, 2000, 0.3)],
            want: vec![("cpu", 1000, 0, 1000, 0)],
        },
        VersusCase {
            row: "a pool capacity of 16 on a quota of 4 CPUs: 8/16 held against busy \
                  over the quota (8 held is more than the 4 CPUs, yet half the pool)",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 8.0, 16.0)],
            metrics: [vec![(LIMIT, 0, 4.0)], busy_at([0, 1000, 2000], 0.3)].concat(),
            want: vec![("cpu", 0, 0, 2000, 0)],
        },
        VersusCase {
            row: "memory pool 30/32 GiB held, machine memory 20% used: declared above use, \
                  for the memory pool",
            end_ms: Some(2000),
            pools: vec![held("memory", 0, 30.0 * GIB, 32.0 * GIB)],
            metrics: vec![
                (MEM_LIMIT, 0, 64.0 * GIB),
                (USED, 0, 12.8 * GIB),
                (USED, 1000, 12.8 * GIB),
                (USED, 2000, 12.8 * GIB),
                (BUSY, 0, 0.95),
                (BUSY, 1000, 0.95),
                (BUSY, 2000, 0.95),
            ],
            want: vec![("memory", 2000, 0, 0, 0)],
        },
        VersusCase {
            row: "no pool rows (an all-hit build): no rows",
            end_ms: Some(2000),
            pools: vec![],
            metrics: busy_at([0, 1000, 2000], 0.3),
            want: vec![],
        },
        VersusCase {
            row: "pool rows but the CPU source stopped: unknown from the stop on \
                  (memory samples go on, and are not CPU)",
            end_ms: Some(10_000),
            pools: vec![held("cpu", 0, 8.0, 8.0)],
            metrics: [
                vec![(MEM_LIMIT, 0, 100.0)],
                busy_at([0, 1000, 2000, 3000], 0.3),
                (0..=10).map(|s| (USED, s * 1000, 10.0)).collect(),
            ]
            .concat(),
            want: vec![("cpu", 3000, 0, 0, 7000)],
        },
        VersusCase {
            row: "a pool row every 250 ms and a sample every 1 s: each interval uses the \
                  pool value holding over it, time-weighted (neither its first nor last value)",
            end_ms: Some(2000),
            pools: vec![
                held("cpu", 0, 8.0, 8.0),
                held("cpu", 250, 8.0, 8.0),
                held("cpu", 500, 0.0, 8.0),
                held("cpu", 750, 0.0, 8.0),
                held("cpu", 1000, 8.0, 8.0),
                held("cpu", 1250, 8.0, 8.0),
                held("cpu", 1500, 8.0, 8.0),
                held("cpu", 1750, 6.0, 8.0),
            ],
            metrics: busy_at([0, 1000, 2000], 0.3),
            // (0, 1 s]: a mean of 0.5 held (matched); (1 s, 2 s]: 0.9375.
            want: vec![("cpu", 1000, 0, 1000, 0)],
        },
        VersusCase {
            row: "a pool whose last row came before the last sample: the last value holds \
                  until End",
            end_ms: Some(4000),
            pools: vec![held("cpu", 0, 2.0, 8.0), held("cpu", 1500, 8.0, 8.0)],
            metrics: busy_at([0, 1000, 2000, 3000, 4000], 0.3),
            // (1 s, 2 s] holds 0.625 on average: matched, like (0, 1 s].
            want: vec![("cpu", 2000, 0, 2000, 0)],
        },
        VersusCase {
            row: "a pool the machine has no counterpart for (docker, sandbox): no row",
            end_ms: Some(2000),
            pools: vec![held("docker", 0, 2.0, 2.0), held("sandbox", 0, 4.0, 4.0)],
            metrics: busy_at([0, 1000, 2000], 0.3),
            want: vec![],
        },
        VersusCase {
            row: "time before the pool's first row and before the first sample, and a \
                  gap over 2 s: unknown",
            end_ms: Some(10_000),
            pools: vec![held("cpu", 1000, 8.0, 8.0)],
            metrics: busy_at([2000, 3000, 6000, 7000], 0.3),
            // Classified: (2, 3] and (6, 7]; (3, 6] is a 3 s gap.
            want: vec![("cpu", 2000, 0, 0, 8000)],
        },
        VersusCase {
            row: "cpu and memory pools in one session: a row each, each against its own \
                  machine figure (cpu 8/8 against busy 30%, memory 2/32 GiB against \
                  60 of 64 GiB used)",
            end_ms: Some(2000),
            pools: vec![
                held("cpu", 0, 8.0, 8.0),
                held("memory", 0, 2.0 * GIB, 32.0 * GIB),
            ],
            metrics: [
                vec![(MEM_LIMIT, 0, 64.0 * GIB)],
                busy_at([0, 1000, 2000], 0.3),
                [0, 1000, 2000].map(|t| (USED, t, 60.0 * GIB)).to_vec(),
            ]
            .concat(),
            want: vec![("cpu", 2000, 0, 0, 0), ("memory", 0, 2000, 0, 0)],
        },
        VersusCase {
            row: "a pool of capacity 0: no held fraction, so no row (and no division error)",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 0.0, 0.0), held("cpu", 1000, 2.0, 0.0)],
            metrics: busy_at([0, 1000, 2000], 0.3),
            want: vec![],
        },
        VersusCase {
            row: "held above capacity (10/8): held over 90%, so against busy 30% it is \
                  declared above use, never clamped into matched",
            end_ms: Some(2000),
            pools: vec![held("cpu", 0, 10.0, 8.0)],
            metrics: busy_at([0, 1000, 2000], 0.3),
            want: vec![("cpu", 2000, 0, 0, 0)],
        },
        VersusCase {
            row: "a live session: up to its last row",
            end_ms: None,
            pools: vec![held("cpu", 0, 8.0, 8.0)],
            metrics: busy_at([0, 1000, 2000], 0.3),
            want: vec![("cpu", 2000, 0, 0, 0)],
        },
    ];

    let all = versus_db();
    let mut want_all = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let db = versus_db();
        write_versus_case(&db, "s", case);
        assert_eq!(
            recipe_rows("resources_vs_machine", &db),
            versus_want("s", case),
            "{}",
            case.row
        );
        let session = format!("s{i:02}");
        write_versus_case(&all, &session, case);
        want_all.extend(versus_want(&session, case));
    }
    assert_eq!(
        recipe_rows("resources_vs_machine", &all),
        want_all,
        "all at once"
    );
}
