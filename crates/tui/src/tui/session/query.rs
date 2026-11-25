//! `heph session query` (spec "Round 1 hardening", D20, I9): one read-only
//! statement over the six explorer names, answered from the catalog, one
//! recording, or many.
//!
//! - The statement is first prepared against empty tables of the explorer
//!   schema, under the authorizer: that refuses what is not a read, names the
//!   columns and their declared types, and says which tables it reads.
//! - One source is queried in place, read-only, through heph's own TEMP views
//!   of the six names: never a file's stored views, and the catalog scoped
//!   to the window and workspace by those views.
//! - Several sources are copied into one temp database, only the tables the
//!   statement reads, then queried there. The bundled SQLite attaches 10
//!   databases at most, and a week holds hundreds of recordings.
//!
//! Read-only by construction: exactly one statement, `readonly()`, an
//! authorizer that allows reads (of the six names only), functions outside a
//! denylist and three schema pragmas; `trusted_schema` off, `DEFENSIVE` on, a
//! 64 MiB value limit, and a progress handler that stops it at its timeout.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::Value;
use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::catalog::{self, Catalog};
use super::store;

/// One explorer name and its columns with their declared types, in order.
#[derive(Debug)]
pub struct Table {
    pub name: &'static str,
    pub columns: &'static [(&'static str, &'static str)],
}

/// The six explorer names (I9), as every source presents them.
pub const TABLES: &[Table] = &[
    Table {
        name: "sessions",
        columns: &[
            ("session_id", "TEXT"),
            ("schema_version", "INTEGER"),
            ("format", "INTEGER"),
            ("heph_version", "TEXT"),
            ("owner_pid", "INTEGER"),
            ("host", "TEXT"),
            ("user", "TEXT"),
            ("actor_kind", "TEXT"),
            ("actor_name", "TEXT"),
            ("ci_run_url", "TEXT"),
            ("command", "TEXT"),
            ("argv", "TEXT"),
            ("cwd", "TEXT"),
            ("workspace_root", "TEXT"),
            ("home", "TEXT"),
            ("git_head", "TEXT"),
            ("label", "TEXT"),
            ("interactive", "INTEGER"),
            ("started_ms", "INTEGER"),
            ("ended_ms", "INTEGER"),
            ("exit_code", "INTEGER"),
            ("panicked", "INTEGER"),
            ("truncated", "INTEGER"),
            ("truncated_seq", "INTEGER"),
            ("sealed", "INTEGER"),
            ("recorded", "INTEGER"),
            ("snapshot", "INTEGER"),
        ],
    },
    Table {
        name: "spans",
        columns: &[
            ("session_id", "TEXT"),
            ("span_id", "INTEGER"),
            ("parent_span_id", "INTEGER"),
            ("name", "TEXT"),
            ("class", "TEXT"),
            ("addr", "TEXT"),
            ("driver", "TEXT"),
            ("outcome", "TEXT"),
            ("start_us", "INTEGER"),
            ("end_us", "INTEGER"),
            ("duration_us", "INTEGER"),
            ("status", "TEXT"),
            ("attributes", "TEXT"),
        ],
    },
    Table {
        name: "span_events",
        columns: &[
            ("session_id", "TEXT"),
            ("span_id", "INTEGER"),
            ("name", "TEXT"),
            ("time_us", "INTEGER"),
            ("attributes", "TEXT"),
        ],
    },
    Table {
        name: "metrics",
        columns: &[
            ("session_id", "TEXT"),
            ("name", "TEXT"),
            ("kind", "TEXT"),
            ("unit", "TEXT"),
            ("time_us", "INTEGER"),
            ("value", "REAL"),
            ("attributes", "TEXT"),
        ],
    },
    Table {
        name: "logs",
        columns: &[
            ("session_id", "TEXT"),
            ("seq", "INTEGER"),
            ("time_us", "INTEGER"),
            ("severity", "TEXT"),
            ("body", "TEXT"),
        ],
    },
    Table {
        name: "targets",
        columns: &[
            ("session_id", "TEXT"),
            ("addr", "TEXT"),
            ("outcome", "TEXT"),
            ("duration_us", "INTEGER"),
        ],
    },
];

/// A built-in recipe: a question asked often, and the SQL that answers it
/// over any source. `--schema` prints them; `docs/SESSIONS.md` carries the
/// same text, which `recipes_run` holds it to.
#[derive(Debug)]
pub struct Recipe {
    pub name: &'static str,
    /// One line: the question it answers.
    pub purpose: &'static str,
    /// What to know before trusting its answer, a comment line each. No
    /// `;`: `recipes_run` splits the docs' SQL blocks on it.
    pub notes: &'static [&'static str],
    /// One statement, no trailing `;`. Keys on `class`, never on a span's
    /// name (E5).
    pub sql: &'static str,
}

impl Recipe {
    /// Its comment, as `--schema` prints it above the SQL: `-- name:
    /// purpose`, then a `-- ` line per note, each `\n`-ended.
    pub fn comment(&self) -> String {
        let mut out = format!("-- {}: {}\n", self.name, self.purpose);
        for n in self.notes {
            out.push_str("-- ");
            out.push_str(n);
            out.push('\n');
        }
        out
    }
}

/// The note of a recipe over `spans`: the catalog does not keep a cache
/// hit's spans, so over it a warm run looks empty.
const SPANS_NOTE: &[&str] = &[
    "The catalog keeps the spans of targets that ran, not of cache hits:",
    "for a warm run, use --full or --from.",
];

/// The built-in recipes, in the order `--schema` prints them.
pub const RECIPES: &[Recipe] = &[
    Recipe {
        name: "time_by_phase",
        purpose: "Where did time go, by class and phase (busy time, not wall time)?",
        notes: SPANS_NOTE,
        sql: "\
SELECT class, name, count(*) AS n, sum(duration_us) / 1e6 AS busy_s
FROM spans WHERE class IS NOT NULL
GROUP BY class, name ORDER BY busy_s DESC, class, name",
    },
    Recipe {
        name: "uncovered_wall",
        purpose: "How much of each session's wall time had no target running?",
        notes: SPANS_NOTE,
        sql: "\
WITH runs AS (
  SELECT session_id, start_us, end_us,
         max(end_us) OVER (PARTITION BY session_id ORDER BY start_us, end_us
                           ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS before_end
  FROM spans WHERE class = 'target' AND end_us IS NOT NULL
), islands AS (
  SELECT session_id, start_us, end_us,
         sum(before_end IS NULL OR start_us > before_end)
           OVER (PARTITION BY session_id ORDER BY start_us, end_us) AS island
  FROM runs
), covered AS (
  SELECT session_id, sum(island_us) AS target_us
  FROM (SELECT session_id, max(end_us) - min(start_us) AS island_us
        FROM islands GROUP BY session_id, island)
  GROUP BY session_id
)
SELECT s.session_id, (s.ended_ms - s.started_ms) / 1e3 AS wall_s,
       coalesce(c.target_us, 0) / 1e6 AS target_s,
       round(100 - coalesce(c.target_us, 0) / 10.0 / (s.ended_ms - s.started_ms), 1)
         AS uncovered_pct
FROM sessions s LEFT JOIN covered c USING (session_id)
WHERE s.ended_ms > s.started_ms
ORDER BY s.started_ms DESC",
    },
    Recipe {
        name: "phase_percentiles",
        purpose: "How long does each phase take: p50, p90, p99 and max, in ms?",
        notes: SPANS_NOTE,
        sql: "\
SELECT class, name, count(*) AS n,
       min(CASE WHEN cd >= 0.5 THEN duration_us END) / 1e3 AS p50_ms,
       min(CASE WHEN cd >= 0.9 THEN duration_us END) / 1e3 AS p90_ms,
       min(CASE WHEN cd >= 0.99 THEN duration_us END) / 1e3 AS p99_ms,
       max(duration_us) / 1e3 AS max_ms
FROM (SELECT class, name, duration_us,
             cume_dist() OVER (PARTITION BY class, name ORDER BY duration_us) AS cd
      FROM spans WHERE class IS NOT NULL AND duration_us IS NOT NULL)
GROUP BY class, name ORDER BY p90_ms DESC, class, name",
    },
    Recipe {
        name: "hit_rate_by_package",
        purpose: "Which packages miss the cache most?",
        notes: &[
            "Failed targets count as misses, those whose dependency failed included.",
            "A target with cache = False never hits.",
        ],
        sql: "\
SELECT substr(addr, 1, instr(addr, ':') - 1) AS package,
       count(*) AS targets, sum(outcome = 'hit') AS hits,
       round(100.0 * sum(outcome = 'hit') / count(*), 1) AS hit_pct
FROM targets WHERE outcome IN ('ran', 'hit', 'failed')
GROUP BY package ORDER BY hit_pct, targets DESC, package",
    },
];

/// The explorer names the catalog holds: it keeps no logs.
const CATALOG_HOLDS: &[&str] = &["sessions", "spans", "span_events", "metrics", "targets"];
/// What the catalog holds of a table only in part (`coverage.partial`):
/// the spans of what ran, not of cache hits, and no logs.
const CATALOG_PARTIAL: &[(&str, &str)] = &[
    ("spans", "ran targets only"),
    ("span_events", "ran targets only"),
    ("logs", "not kept"),
];

/// What a recording's explorer names derive from. Read only when each is a
/// table, so a file whose `spans` is a view is not read at all.
const RECORDING_BASES: &[&str] = &["session", "records", "spans", "span_events", "metrics"];
const CATALOG_BASES: &[&str] = &[
    "sessions",
    "addrs",
    "target_rows",
    "spans",
    "span_events",
    "metrics",
];

/// Rows a query returns at most, unless `--limit` says otherwise.
pub const ROW_CAP: usize = 100_000;
/// `--timeout`'s default.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// `SQLITE_LIMIT_LENGTH`: the largest string or blob a query makes.
const LENGTH_LIMIT: i32 = 64 << 20;
/// The temp database of a query over several sources.
const COPY_CAP: u64 = 2 << 30;
/// SQLite VM steps between two looks at the clock.
const PROGRESS_STEPS: i32 = 10_000;

const DENIED_FUNCTIONS: &[&str] = &["load_extension", "readfile", "writefile", "edit"];
const ALLOWED_PRAGMAS: &[&str] = &["table_info", "table_list", "index_list"];
/// The table-valued functions a statement may read, over a JSON value.
const JSON_WALKS: &[&str] = &["json_each", "json_tree"];

/// The window and workspace the catalog is scoped to. Sessions are in it by
/// their start; every row of a session in it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Sessions started at or after this (unix ms).
    pub since_ms: Option<u64>,
    /// Sessions started at or before this (unix ms).
    pub until_ms: Option<u64>,
    /// Only this workspace root's sessions; every workspace's when `None`.
    pub workspace: Option<String>,
}

impl Scope {
    /// The scope as a condition on a `sessions` row, its columns behind
    /// `prefix`.
    fn sql(&self, prefix: &str) -> String {
        let mut parts = vec!["1".to_owned()];
        if let Some(s) = self.since_ms {
            parts.push(format!("{prefix}started_ms >= {s}"));
        }
        if let Some(u) = self.until_ms {
            parts.push(format!("{prefix}started_ms <= {u}"));
        }
        if let Some(w) = &self.workspace {
            parts.push(format!("{prefix}workspace_root = {}", quote(w)));
        }
        parts.join(" AND ")
    }
}

/// `s` as an SQL string literal.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// What a query reads.
#[derive(Debug)]
pub enum Source {
    /// The catalog, as `catalog::open` repaired it, scoped by `scope`.
    Catalog { catalog: Catalog, scope: Scope },
    /// One recording's file.
    Recording(RecordingFile),
}

/// A recording's file, read-only, and whatever keeps it readable (a temp
/// copy, a download), dropped with this.
pub struct RecordingFile {
    path: PathBuf,
    /// Opened `immutable=1`: nothing writes it while it is read (sealed,
    /// foreign or a copy). A live recording is read through its WAL.
    immutable: bool,
    _keep: Box<dyn Send>,
}

impl fmt::Debug for RecordingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordingFile")
            .field("path", &self.path)
            .field("immutable", &self.immutable)
            .finish_non_exhaustive()
    }
}

impl RecordingFile {
    /// The local recording in `dir` under the sessions `root`: re-derived
    /// first when an older heph derived it (said through `note`), and given
    /// its indexes on this first need (spec "Disk"), both under its lock.
    pub fn local(
        root: &Path,
        dir: &Path,
        note: &(dyn Fn(&str) + Send + Sync),
    ) -> anyhow::Result<Self> {
        let opened =
            super::source::open_repaired_with(root, dir, store::Need { indexes: true }, note)?;
        let path = conn_path(&opened.conn)
            .with_context(|| format!("reading the recording in {}", dir.display()))?;
        Ok(Self {
            path,
            immutable: !opened.live,
            _keep: Box::new(opened),
        })
    }

    /// A recording opened by path or URL (already re-derived into a copy
    /// when it must be), read without indexes: a foreign file is never
    /// written.
    pub fn foreign(recording: super::recording::Recording) -> anyhow::Result<Self> {
        let path = conn_path(recording.conn()).context("reading a foreign recording")?;
        Ok(Self {
            path,
            immutable: true,
            _keep: Box::new(recording),
        })
    }

    /// A local recording still being written: what is read of it is the
    /// session so far.
    pub fn is_live(&self) -> bool {
        !self.immutable
    }
}

/// One recording's six explorer names, for heph's own statements (the OTLP
/// export): the same TEMP views over heph's definitions a query reads in
/// place, on a hardened read-only connection, with no authorizer and no row
/// cap. Nothing a user wrote runs here.
pub struct Explorer {
    conn: Connection,
    _file: RecordingFile,
}

impl fmt::Debug for Explorer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Explorer")
            .field("file", &self._file)
            .finish_non_exhaustive()
    }
}

impl Explorer {
    pub fn open(file: RecordingFile) -> anyhow::Result<Self> {
        let conn = open_read_only(&file.path, file.immutable)?;
        harden(&conn)?;
        require_tables(&conn, "main", RECORDING_BASES).map_err(|e| match e {
            QueryError::Other(e) => e,
            other => anyhow::anyhow!("{other}"),
        })?;
        define_views(&conn, &|name| format!("heph_x_{name}"), |name| {
            recording_select(name, "main")
        })?;
        Ok(Self { conn, _file: file })
    }

    /// The connection; an unqualified explorer name reads heph's view.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

fn conn_path(conn: &Connection) -> anyhow::Result<PathBuf> {
    conn.path()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .context("the recording has no file")
}

/// How far a query may go.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Rows returned at most; more is an error. `None`: no cap.
    pub rows: Option<usize>,
    /// The query stops after this long, copies included.
    pub timeout: Duration,
    /// Set (by a SIGINT) to stop the query now.
    pub stop: Arc<AtomicBool>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rows: Some(ROW_CAP),
            timeout: TIMEOUT,
            stop: Arc::default(),
        }
    }
}

/// Where an answer's rows came from (`--json` `coverage`; only gains
/// fields).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// The explorer tables the query read from the catalog.
    pub catalog: Vec<&'static str>,
    /// Recordings read.
    pub recordings: usize,
    /// Recordings skipped: a format this heph does not know, or unreadable.
    pub skipped: usize,
    /// Sessions read whose recording stopped at its size ceiling, so their
    /// rows are incomplete.
    pub truncated: usize,
    /// When the catalog was read: the start (unix ms) of the oldest session
    /// it holds. Nothing older is in the answer, whatever the window.
    pub reaches_ms: Option<u64>,
    /// The tables read from the catalog that hold only part of what a
    /// recording does, each with what is missing.
    pub partial: BTreeMap<&'static str, &'static str>,
}

/// A query's answer.
#[derive(Debug)]
pub struct Answer {
    pub columns: Vec<String>,
    /// Each column's declared type; `None` for an expression.
    pub types: Vec<Option<String>>,
    pub rows: Vec<Vec<Value>>,
    pub coverage: Coverage,
    /// Queried in place (one source) rather than in a copy.
    pub in_place: bool,
    /// The explorer tables copied, in schema order (empty in place).
    pub copied: Vec<&'static str>,
}

/// Why a query failed.
#[derive(Debug)]
pub enum QueryError {
    /// The statement: refused, invalid, or stopped.
    Sql(String),
    /// Stopped by a SIGINT.
    Interrupted,
    /// The disk holding the temp copy is full, not the 2 GiB cap.
    Io(anyhow::Error),
    /// Anything else: a source that cannot be read, the temp database.
    Other(anyhow::Error),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(m) => f.write_str(m),
            Self::Interrupted => f.write_str("interrupted"),
            Self::Io(e) | Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<anyhow::Error> for QueryError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// What [`check`] learnt of a statement.
struct Checked {
    columns: Vec<String>,
    types: Vec<Option<String>>,
    /// The explorer tables it reads, in schema order.
    reads: Vec<&'static str>,
    /// The empty schema it was prepared against.
    conn: Connection,
}

/// Run `sql` over `sources`, temp files in `tmp`.
pub fn run(
    sql: &str,
    sources: Vec<Source>,
    tmp: &Path,
    limits: &Limits,
) -> Result<Answer, QueryError> {
    let checked = check(sql)?;
    let deadline = Instant::now() + limits.timeout;
    let mut sources = sources;
    match (sources.pop(), sources.is_empty()) {
        (None, _) => {
            let Checked {
                columns,
                types,
                mut conn,
                ..
            } = checked;
            let rows = execute(&mut conn, sql, Allowed::Tables, limits, deadline)?;
            Ok(Answer {
                columns,
                types,
                rows,
                coverage: Coverage::default(),
                in_place: true,
                copied: Vec::new(),
            })
        }
        (Some(only), true) => in_place(sql, only, checked, limits, deadline),
        (Some(last), false) => {
            sources.push(last);
            copied(sql, sources, checked, tmp, limits, deadline)
        }
    }
}

/// The empty explorer schema, hardened, to prepare a statement against.
fn check(sql: &str) -> Result<Checked, QueryError> {
    let mut conn = Connection::open_in_memory().context("opening an empty explorer schema")?;
    harden(&conn)?;
    create_tables(&conn)?;
    let reads = Arc::new(Mutex::new(BTreeSet::new()));
    conn.authorizer(Some(authorizer(Allowed::Tables, Some(Arc::clone(&reads)))))
        .context("installing the authorizer")?;
    let (columns, types) = {
        let stmt = prepare(&conn, sql).map_err(|e| more_than_one(sql).unwrap_or(e))?;
        stmt.columns()
            .map(|c| (c.name().to_owned(), c.decl_type().map(str::to_owned)))
            .unzip()
    };
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .context("removing the authorizer")?;
    let read = reads.lock().unwrap_or_else(PoisonError::into_inner);
    let reads = TABLES
        .iter()
        .map(|t| t.name)
        .filter(|n| read.contains(*n))
        .collect();
    Ok(Checked {
        columns,
        types,
        reads,
        conn,
    })
}

/// The refusal to give when `sql` holds more than one statement. rusqlite
/// finds a second statement by preparing it, which the authorizer refuses
/// first ("not authorized" for `SELECT 1; DELETE …`); so a refused `sql` is
/// prepared again on a throwaway schema whose authorizer allows everything
/// but a pragma. Some pragmas act when prepared, not when stepped, and
/// some of those reach past the connection (`soft_heap_limit` and
/// `hard_heap_limit` set the process's): the authorizer ignores every
/// pragma, which prepares it as a no-op, so the second statement is still
/// found and nothing is changed.
fn more_than_one(sql: &str) -> Option<QueryError> {
    let mut conn = Connection::open_in_memory().ok()?;
    create_tables(&conn).ok()?;
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Pragma { .. } => Authorization::Ignore,
        _ => Authorization::Allow,
    }))
    .ok()?;
    match conn.prepare(sql) {
        Err(e @ rusqlite::Error::MultipleStatement) => Some(sql_error(&e)),
        _ => None,
    }
}

/// `sql` prepared on `conn`, refused unless it is exactly one read-only
/// statement.
fn prepare<'c>(conn: &'c Connection, sql: &str) -> Result<rusqlite::Statement<'c>, QueryError> {
    let stmt = conn.prepare(sql).map_err(|e| sql_error(&e))?;
    if stmt.expanded_sql().is_none() {
        return Err(QueryError::Sql(
            "no statement: give one SELECT over the explorer tables".to_owned(),
        ));
    }
    if !stmt.readonly() {
        return Err(QueryError::Sql(
            "refused: the statement writes; heph session query only reads".to_owned(),
        ));
    }
    Ok(stmt)
}

/// A prepare or step error as the user reads it: an unknown table or
/// column lists the real ones, a refusal says why.
fn sql_error(e: &rusqlite::Error) -> QueryError {
    let msg = match e {
        rusqlite::Error::MultipleStatement => {
            return QueryError::Sql(
                "more than one statement: heph session query runs exactly one".to_owned(),
            );
        }
        rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
        rusqlite::Error::SqlInputError { msg, .. } => msg.clone(),
        other => other.to_string(),
    };
    let hint = if msg.starts_with("no such table") {
        format!(
            "; the tables are {}",
            TABLES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
        )
    } else if msg.starts_with("no such column") {
        let tables: Vec<String> = TABLES
            .iter()
            .map(|t| {
                let cols: Vec<&str> = t.columns.iter().map(|(c, _)| *c).collect();
                format!("{}({})", t.name, cols.join(", "))
            })
            .collect();
        format!("; the columns are {}", tables.join("; "))
    } else if msg == "not authorized" || msg.contains("prohibited") {
        "; heph session query only reads the explorer tables (`--schema` lists them)".to_owned()
    } else {
        String::new()
    };
    QueryError::Sql(format!("{msg}{hint}"))
}

/// What the authorizer lets a statement read.
#[derive(Debug, Clone)]
enum Allowed {
    /// The explorer tables of `main` (the empty schema, the copy).
    Tables,
    /// Heph's own views (in `temp`), and what their inner views read from
    /// the source; nothing else.
    Views {
        /// The inner views' nonce'd names.
        inner: Arc<[String]>,
        /// The source's tables the inner views read.
        bases: &'static [&'static str],
        /// Whether `json_each`/`json_tree` are SQLite's: the source holds
        /// nothing by those names, which would shadow them.
        walks: bool,
    },
}

/// Whether a statement may read `table.column`. SQLite also authorizes a
/// "null read" (`column` empty, no accessor) of each FROM item no column
/// of which is read (`count(*)`), naming its schema only as the statement
/// wrote it; and once the flattener has folded heph's views into the
/// statement, of the base table an inner view named. A null read gives row
/// counts, never values.
fn may_read(
    allowed: &Allowed,
    table: &str,
    column: &str,
    db: Option<&str>,
    accessor: Option<&str>,
) -> bool {
    let null_read = column.is_empty();
    // `json_each(x)` and `json_tree(x)` walk a value the statement already
    // read, and read nothing of a source: allowed unless the source holds
    // a table or view by that name, which SQLite would read instead.
    let walk = db == Some("main") && JSON_WALKS.iter().any(|f| f.eq_ignore_ascii_case(table));
    match allowed {
        // `check` resolved every name against the explorer tables, so an
        // unqualified null read is one of them or a CTE.
        Allowed::Tables => match db {
            None => null_read,
            Some(db) => walk || (db == "main" && TABLES.iter().any(|t| t.name == table)),
        },
        // `temp` holds heph's views alone (a statement may create nothing),
        // and only an inner view, by its nonce'd name, reads the source.
        Allowed::Views {
            inner,
            bases,
            walks,
        } => {
            (walk && *walks)
                || db == Some("temp")
                || accessor.is_some_and(|a| inner.iter().any(|n| n == a))
                || (null_read && db.is_none_or(|db| db == "main" && bases.contains(&table)))
        }
    }
}

fn authorizer(
    allowed: Allowed,
    reads: Option<Arc<Mutex<BTreeSet<String>>>>,
) -> impl FnMut(AuthContext<'_>) -> Authorization + Send + 'static {
    move |ctx| match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read {
            table_name,
            column_name,
        } => {
            if may_read(
                &allowed,
                table_name,
                column_name,
                ctx.database_name,
                ctx.accessor,
            ) {
                if let Some(r) = &reads {
                    r.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(table_name.to_owned());
                }
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if DENIED_FUNCTIONS
                .iter()
                .any(|d| d.eq_ignore_ascii_case(function_name))
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        AuthAction::Pragma { pragma_name, .. } => {
            if ALLOWED_PRAGMAS
                .iter()
                .any(|p| p.eq_ignore_ascii_case(pragma_name))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

/// What every query connection sets before it reads anything.
fn harden(conn: &Connection) -> anyhow::Result<()> {
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .context("turning trusted_schema off")?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .context("turning defensive on")?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, LENGTH_LIMIT)
        .context("limiting value length")?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .context("keeping temp tables in memory")?;
    Ok(())
}

/// The six explorer names as empty tables of `main`, declared types and
/// all.
fn create_tables(conn: &Connection) -> anyhow::Result<()> {
    let mut sql = String::new();
    for t in TABLES {
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|(c, ty)| format!("{c} {ty}"))
            .collect();
        sql.push_str(&format!(
            "CREATE TABLE main.{} ({});\n",
            t.name,
            cols.join(", ")
        ));
    }
    conn.execute_batch(&sql)
        .context("creating the explorer tables")
}

fn table(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|t| t.name == name)
}

fn column_list(t: &Table) -> String {
    t.columns
        .iter()
        .map(|(c, _)| *c)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Explorer name `name` derived from the recording in schema `s`: heph's
/// definitions of the views a recording stores (`store::SCHEMA`), over its
/// tables only.
fn recording_select(name: &str, s: &str) -> String {
    match name {
        "sessions" => format!(
            "SELECT id AS session_id, {v} AS schema_version, format, heph_version, owner_pid, \
             host, user, actor_kind, actor_name, ci_run_url, command, argv, cwd, workspace_root, \
             home, git_head, label, interactive, started_ms, ended_ms, exit_code, panicked, \
             truncated, truncated_seq, sealed, 1 AS recorded, snapshot FROM {s}.session",
            v = store::SCHEMA_VERSION
        ),
        "logs" => format!(
            "SELECT s.id AS session_id, r.seq AS seq, r.at_ms * 1000 AS time_us, \
             CASE r.kind WHEN 'Output' THEN 'info' \
               ELSE coalesce(nullif(lower(json_extract(r.body, '$.session.level')), ''), 'info') \
             END AS severity, \
             CASE r.kind WHEN 'Output' THEN json_extract(r.body, '$.session.text') \
               ELSE json_extract(r.body, '$.session.msg') END AS body \
             FROM {s}.records r, {s}.session s WHERE r.kind IN ('Log', 'Output')"
        ),
        "targets" => format!(
            "SELECT session_id, addr, outcome, duration_us FROM ( \
               SELECT session_id, addr, outcome, duration_us, row_number() OVER ( \
                 PARTITION BY session_id, addr \
                 ORDER BY CASE outcome WHEN 'failed' THEN 0 WHEN 'ran' THEN 1 \
                   WHEN 'cancelled' THEN 2 WHEN 'hit' THEN 3 ELSE 4 END, span_id \
               ) AS n \
               FROM {s}.spans WHERE name = 'heph.result' \
             ) WHERE n = 1"
        ),
        other => match table(other) {
            Some(t) => format!("SELECT {} FROM {s}.{}", column_list(t), t.name),
            None => format!("SELECT NULL WHERE 0 -- {other}"),
        },
    }
}

/// Explorer name `name` from the catalog in schema `c`, scoped by `scope`.
fn catalog_select(name: &str, c: &str, scope: &Scope) -> String {
    let Some(t) = table(name) else {
        return format!("SELECT NULL WHERE 0 -- {name}");
    };
    let in_scope = format!(
        "session_id IN (SELECT session_id FROM {c}.sessions WHERE {})",
        scope.sql("")
    );
    match name {
        "sessions" => format!(
            "SELECT {} FROM {c}.sessions WHERE {}",
            column_list(t),
            scope.sql("")
        ),
        "targets" => format!(
            "SELECT s.session_id AS session_id, a.addr AS addr, \
               CASE t.outcome WHEN 0 THEN 'hit' WHEN 1 THEN 'ran' WHEN 2 THEN 'failed' \
                 WHEN 3 THEN 'cancelled' ELSE t.outcome END AS outcome, \
               t.duration_us AS duration_us \
             FROM {c}.target_rows t JOIN {c}.sessions s ON s.sid = t.sid \
               JOIN {c}.addrs a ON a.id = t.addr_id WHERE {}",
            scope.sql("s.")
        ),
        "logs" => {
            let cols: Vec<String> = t
                .columns
                .iter()
                .map(|(col, ty)| format!("CAST(NULL AS {ty}) AS {col}"))
                .collect();
            format!("SELECT {} WHERE 0", cols.join(", "))
        }
        _ => format!(
            "SELECT {} FROM {c}.{} WHERE {in_scope}",
            column_list(t),
            t.name
        ),
    }
}

/// Fail unless each of `names` in schema `s` is a table: a file whose
/// `spans` is a view (or missing) is not read.
fn require_tables(conn: &Connection, s: &str, names: &[&str]) -> Result<(), QueryError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT name FROM {s}.sqlite_schema WHERE type = 'table'"
        ))
        .context("reading a source's schema")?;
    let have: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .context("reading a source's schema")?;
    match names.iter().find(|n| !have.contains(**n)) {
        Some(missing) => Err(QueryError::Other(anyhow::anyhow!(
            "not a heph session: its `{missing}` is not a table"
        ))),
        None => Ok(()),
    }
}

fn open_read_only(path: &Path, immutable: bool) -> anyhow::Result<Connection> {
    let query = if immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    };
    Connection::open_with_flags(
        store::file_uri(path, query),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))
}

/// Stop `conn`'s statements at `deadline`, or when `limits.stop` is set.
fn bound(conn: &mut Connection, limits: &Limits, deadline: Instant) -> anyhow::Result<()> {
    let stop = Arc::clone(&limits.stop);
    conn.progress_handler(
        PROGRESS_STEPS,
        Some(move || stop.load(Ordering::Relaxed) || Instant::now() >= deadline),
    )
    .context("installing the query's timeout")
}

/// An error of a statement `bound` may have stopped.
fn stopped(e: &rusqlite::Error, limits: &Limits) -> QueryError {
    if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) {
        if limits.stop.load(Ordering::Relaxed) {
            return QueryError::Interrupted;
        }
        return QueryError::Sql(format!(
            "stopped after --timeout {}",
            humantime_secs(limits.timeout)
        ));
    }
    sql_error(e)
}

fn humantime_secs(d: Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Run the checked `sql` on `conn` under `allowed`, collecting its rows.
fn execute(
    conn: &mut Connection,
    sql: &str,
    allowed: Allowed,
    limits: &Limits,
    deadline: Instant,
) -> Result<Vec<Vec<Value>>, QueryError> {
    bound(conn, limits, deadline)?;
    conn.authorizer(Some(authorizer(allowed, None)))
        .context("installing the authorizer")?;
    let mut stmt = prepare(conn, sql)?;
    let n = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| stopped(&e, limits))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| stopped(&e, limits))? {
        if limits.rows.is_some_and(|cap| out.len() >= cap) {
            let cap = limits.rows.unwrap_or_default();
            return Err(QueryError::Sql(format!(
                "the query returns more than {cap} rows: narrow it (LIMIT, WHERE), or raise \
                 --limit (0 for no cap)"
            )));
        }
        let values = (0..n)
            .map(|i| row.get::<_, Value>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| sql_error(&e))?;
        out.push(values);
    }
    Ok(out)
}

/// One source, read in place through heph's own TEMP views.
fn in_place(
    sql: &str,
    source: Source,
    checked: Checked,
    limits: &Limits,
    deadline: Instant,
) -> Result<Answer, QueryError> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let inner = |name: &str| format!("heph_q_{nonce}_{name}");
    let mut coverage = Coverage::default();
    // The catalog stays open (its lock held) until the answer is read.
    let mut catalog: Catalog;
    let mut file_conn: Connection;
    let (conn, bases): (&mut Connection, &'static [&'static str]) = match source {
        Source::Catalog { catalog: c, scope } => {
            catalog = c;
            let conn = &mut catalog.conn;
            harden(conn)?;
            require_tables(conn, "main", CATALOG_BASES)?;
            define_views(conn, &inner, |name| catalog_select(name, "main", &scope))?;
            coverage.catalog = checked
                .reads
                .iter()
                .copied()
                .filter(|t| CATALOG_HOLDS.contains(t))
                .collect();
            coverage.partial = CATALOG_PARTIAL
                .iter()
                .copied()
                .filter(|(t, _)| checked.reads.contains(t))
                .collect();
            coverage.reaches_ms =
                catalog::reaches_ms(conn).context("reading how far the catalog reaches")?;
            coverage.truncated = count(
                conn,
                &format!(
                    "SELECT count(*) FROM main.sessions WHERE truncated = 1 AND {}",
                    scope.sql("")
                ),
            )?;
            (conn, CATALOG_BASES)
        }
        Source::Recording(f) => {
            file_conn = open_read_only(&f.path, f.immutable)?;
            let conn = &mut file_conn;
            harden(conn)?;
            require_tables(conn, "main", RECORDING_BASES)?;
            define_views(conn, &inner, |name| recording_select(name, "main"))?;
            coverage.recordings = 1;
            coverage.truncated = count(
                conn,
                "SELECT count(*) FROM main.session WHERE truncated = 1",
            )?;
            (conn, RECORDING_BASES)
        }
    };
    let shadows: i64 = conn
        .query_row(
            "SELECT count(*) FROM main.sqlite_schema \
             WHERE lower(name) IN ('json_each', 'json_tree')",
            [],
            |r| r.get(0),
        )
        .context("reading a source's schema")?;
    let allowed = Allowed::Views {
        inner: TABLES.iter().map(|t| inner(t.name)).collect(),
        bases,
        walks: shadows == 0,
    };
    let rows = execute(conn, sql, allowed, limits, deadline)?;
    Ok(Answer {
        columns: checked.columns,
        types: checked.types,
        rows,
        coverage,
        in_place: true,
        copied: Vec::new(),
    })
}

fn count(conn: &Connection, sql: &str) -> Result<usize, QueryError> {
    let n: i64 = conn
        .query_row(sql, [], |r| r.get(0))
        .context("counting a source's truncated sessions")?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The six names as TEMP views: `<name>` over `<inner(name)>` over
/// `select(name)`. The authorizer allows reads made inside an inner view
/// alone, and the nonce in its name keeps a stored view or a CTE from
/// passing for one.
fn define_views(
    conn: &Connection,
    inner: &dyn Fn(&str) -> String,
    select: impl Fn(&str) -> String,
) -> anyhow::Result<()> {
    let mut sql = String::new();
    for t in TABLES {
        let i = inner(t.name);
        sql.push_str(&format!(
            "CREATE TEMP VIEW {i} AS {};\nCREATE TEMP VIEW {} ({}) AS SELECT * FROM temp.{i};\n",
            select(t.name),
            t.name,
            column_list(t),
        ));
    }
    conn.execute_batch(&sql)
        .context("defining the explorer views")
}

/// Several sources, copied into one temp database: only the tables the
/// statement reads.
fn copied(
    sql: &str,
    sources: Vec<Source>,
    checked: Checked,
    tmp: &Path,
    limits: &Limits,
    deadline: Instant,
) -> Result<Answer, QueryError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let file = tempfile::Builder::new()
        .prefix("query-")
        .suffix(".db")
        .tempfile_in(tmp)
        .with_context(|| format!("creating a temp database in {}", tmp.display()))?
        .into_temp_path();
    let mut conn = Connection::open_with_flags(
        &file,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening the query's temp database")?;
    harden(&conn)?;
    conn.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;")
        .context("setting up the query's temp database")?;
    let page: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .context("reading the page size")?;
    let cap = i64::try_from(COPY_CAP).unwrap_or(i64::MAX);
    conn.pragma_update(None, "max_page_count", cap / page.max(512))
        .context("bounding the query's temp database")?;
    create_tables(&conn)?;
    bound(&mut conn, limits, deadline)?;
    let mut coverage = Coverage::default();
    for source in sources {
        match source {
            Source::Recording(f) => {
                copy_recording(&conn, &f, &checked.reads, limits)?;
                coverage.recordings += 1;
                coverage.truncated += count(
                    &conn,
                    "SELECT count(*) FROM src.session WHERE truncated = 1",
                )
                .and_then(|n| detach(&conn).map(|()| n))?;
            }
            Source::Catalog { .. } => {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "the catalog is queried alone, not with recordings"
                )));
            }
        }
    }
    let rows = execute(&mut conn, sql, Allowed::Tables, limits, deadline)?;
    drop(conn);
    drop(file);
    Ok(Answer {
        columns: checked.columns,
        types: checked.types,
        rows,
        coverage,
        in_place: false,
        copied: checked.reads,
    })
}

/// Attach `f` as `src` and copy `reads` from it; `src` stays attached for
/// the caller to read its session row, then [`detach`].
fn copy_recording(
    conn: &Connection,
    f: &RecordingFile,
    reads: &[&'static str],
    limits: &Limits,
) -> Result<(), QueryError> {
    let query = if f.immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    };
    conn.execute("ATTACH ?1 AS src", [store::file_uri(&f.path, query)])
        .with_context(|| format!("opening {}", f.path.display()))?;
    let copy = || -> Result<(), QueryError> {
        require_tables(conn, "src", RECORDING_BASES)?;
        for name in reads {
            conn.execute(
                &format!(
                    "INSERT INTO main.{name} SELECT * FROM ({})",
                    recording_select(name, "src")
                ),
                [],
            )
            .map_err(|e| copy_error(&e, conn, limits))?;
        }
        Ok(())
    };
    copy().inspect_err(|_| {
        if let Err(e) = detach(conn) {
            tracing::debug!(error = %e, "cannot detach a query source");
        }
    })
}

fn detach(conn: &Connection) -> Result<(), QueryError> {
    conn.execute_batch("DETACH src")
        .context("closing a query source")?;
    Ok(())
}

/// An error copying into `conn`, the temp database. SQLite says "full"
/// alike for its `max_page_count` (the 2 GiB cap) and for a full disk.
fn copy_error(e: &rusqlite::Error, conn: &Connection, limits: &Limits) -> QueryError {
    if e.sqlite_error_code() != Some(ErrorCode::DiskFull) {
        return stopped(e, limits);
    }
    match at_cap(conn) {
        Ok(true) => QueryError::Sql(format!(
            "the query's copy of its sources reached {} GiB: narrow the window or the sources",
            COPY_CAP >> 30
        )),
        Ok(false) => QueryError::Io(anyhow::anyhow!(
            "the disk holding the query's temp copy ({}) is full",
            conn.path().unwrap_or("?")
        )),
        Err(e) => QueryError::Io(e.context("the query's temp copy is full")),
    }
}

/// Whether `conn`'s file is at its `max_page_count`. The failed statement
/// leaves `page_count` where it began (the pager drops what it held), but
/// with no journal what it wrote stays in the file: at the cap, that is
/// all of it but what the page cache still held.
fn at_cap(conn: &Connection) -> anyhow::Result<bool> {
    let pragma = |name: &str| -> anyhow::Result<i64> {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .with_context(|| format!("reading the temp copy's {name}"))
    };
    let page = u64::try_from(pragma("page_size")?).unwrap_or(0);
    let cap = u64::try_from(pragma("max_page_count")?)
        .unwrap_or(0)
        .saturating_mul(page);
    // Negative: KiB; positive: pages.
    let cache = match pragma("cache_size")? {
        n if n < 0 => n.unsigned_abs().saturating_mul(1024),
        n => n.unsigned_abs().saturating_mul(page),
    };
    let path = conn
        .path()
        .filter(|p| !p.is_empty())
        .context("the temp copy has no file")?;
    let len = std::fs::metadata(path)
        .with_context(|| format!("reading the size of {path}"))?
        .len();
    // Twice the cache, and a page: the margin is SQLite's, not the disk's.
    Ok(len
        .saturating_add(cache.saturating_mul(2))
        .saturating_add(page)
        >= cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp database set up as `copied` sets up its copy, capped at
    /// `max_pages`, with a 64 KiB page cache.
    fn copy_db(path: &Path, max_pages: Option<i64>) -> Connection {
        let conn = Connection::open(path).expect("open");
        conn.execute_batch(
            "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA cache_size = -64;
             CREATE TABLE t (b BLOB);",
        )
        .expect("set up");
        if let Some(n) = max_pages {
            conn.pragma_update(None, "max_page_count", n).expect("cap");
        }
        conn
    }

    /// SQLite says "full" alike for the copy's cap (`max_page_count`) and
    /// for a full disk. Past the cap it is the statement's to narrow
    /// (`sql`); short of it, the disk is full (`io`), and saying "reached
    /// 2 GiB" would send the user after the wrong thing.
    #[test]
    fn copy_error_tells_the_cap_from_a_full_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let limits = Limits::default();

        let capped = copy_db(&dir.path().join("capped.db"), Some(2_000));
        let e = capped
            .execute(
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 100000)
                 INSERT INTO t SELECT randomblob(1000) FROM r",
                [],
            )
            .expect_err("past the cap");
        assert_eq!(e.sqlite_error_code(), Some(ErrorCode::DiskFull), "{e}");
        match copy_error(&e, &capped, &limits) {
            QueryError::Sql(m) => assert!(m.contains("reached"), "{m}"),
            other => panic!("the cap: {other:?}"),
        }

        // Far below its cap: what the disk says when it fills.
        let roomy = copy_db(&dir.path().join("roomy.db"), None);
        let full = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_owned()),
        );
        match copy_error(&full, &roomy, &limits) {
            QueryError::Io(e) => assert!(format!("{e:#}").contains("full"), "{e:#}"),
            other => panic!("a full disk: {other:?}"),
        }
    }
}
