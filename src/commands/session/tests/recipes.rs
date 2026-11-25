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
    let expected: [(&str, &[&[&str]]); 5] = [
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
