//! `heph session query`: read-only SQL over the explorer names (D20), from
//! the catalog, the recordings kept (`--full`) or the recordings named
//! (`--from`). The engine is `htui::tui::session::query`; this module picks
//! the sources, wires SIGINT to its stop flag, and prints the answer.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal as _, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use base64::Engine as _;
use rusqlite::types::Value;
use serde::Serialize;
use tokio::signal::unix::{SignalKind, signal};

use super::reference::{self, Opener, RefKind, Resolved};
use super::{QueryArgs, QueryFormat, SessionExit, ViewOpts, escape, escape_text, list_all};
use crate::tui::session::query::{
    self as engine, Answer, Coverage, Limits, QueryError, RecordingFile, Scope, Source,
};
use crate::tui::session::{EntryMeta, SessionId, SessionSource, catalog, store};

/// `--format json`'s envelope version. Only gains fields within a version.
pub const QUERY_JSON_VERSION: u32 = 1;

/// Where a query runs, as `execute` resolved it.
pub(crate) struct QueryEnv<'a> {
    /// The sessions root (`~/.heph/sessions`).
    pub root: &'a Path,
    pub sources: &'a [&'a dyn SessionSource],
    pub opts: ViewOpts,
    pub opener: &'a Opener,
    /// The workspace the window is scoped to; every workspace's when `None`.
    pub workspace: Option<String>,
    /// Where notes go, each a full line (stderr).
    pub note: Arc<dyn Fn(&str) + Send + Sync>,
}

/// Run `heph session query` as `args` asks, reading `-` SQL from `stdin`.
pub(crate) async fn query(
    args: &QueryArgs,
    env: QueryEnv<'_>,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    query_until(args, env, stdin, out, Arc::default()).await
}

/// [`query`], stopped (exit 130) once `stop` is set: by a SIGINT, or by
/// the caller.
pub(crate) async fn query_until(
    args: &QueryArgs,
    env: QueryEnv<'_>,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    if args.schema {
        return write_schema(out);
    }
    let sql = read_sql(args, stdin)?;
    let limits = Limits {
        rows: match args.limit {
            None => Some(engine::ROW_CAP),
            Some(0) => None,
            Some(n) => Some(n),
        },
        timeout: args.timeout,
        stop,
    };
    // Armed before any source is opened: a Ctrl-C stops the query and lets
    // its temp files go, rather than killing heph with them on disk.
    let _sigint = StopOn::interrupt(Arc::clone(&limits.stop));

    let (sources, skipped) = sources(args, &env, &limits.stop).await?;
    // The engine looks at the flag only every so many steps, which a short
    // statement never takes.
    interrupted(&limits.stop)?;
    let tmp = store::tmp_dir(env.root);
    if sources.len() > 1 {
        std::fs::create_dir_all(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
    }
    let run_limits = limits.clone();
    let mut answer = hcore::blocking::run(move || engine::run(&sql, sources, &tmp, &run_limits))
        .await
        .map_err(query_error)?;
    interrupted(&limits.stop)?;
    answer.coverage.skipped += skipped;
    notes(&answer.coverage, &*env.note);
    match args.format {
        QueryFormat::Table => write_table(&answer, out),
        QueryFormat::Csv => write_csv(&answer, std::io::stdout().is_terminal(), out),
        QueryFormat::Json => write_json(&answer, out),
    }
}

/// The SQL: the positional argument, stdin for `-`, or `-f FILE`.
fn read_sql(args: &QueryArgs, stdin: &mut dyn Read) -> anyhow::Result<String> {
    match (&args.sql, &args.file) {
        (Some(s), _) if s == "-" => {
            let mut sql = String::new();
            stdin
                .read_to_string(&mut sql)
                .context("reading the SQL from stdin")?;
            Ok(sql)
        }
        (Some(s), _) => Ok(s.clone()),
        (None, Some(f)) => std::fs::read_to_string(f)
            .with_context(|| format!("reading the SQL in {}", f.display())),
        (None, None) => Err(anyhow::anyhow!(
            "no SQL: give it as an argument, `-` for stdin, or `-f FILE`"
        )),
    }
}

/// The sources `args` select, and how many recordings were skipped picking
/// them (`--full`: a format this heph does not know, or unreadable).
async fn sources(
    args: &QueryArgs,
    env: &QueryEnv<'_>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<(Vec<Source>, usize)> {
    let note = Arc::clone(&env.note);
    if !args.from.is_empty() {
        let mut out = Vec::with_capacity(args.from.len());
        // Each recording is read once: a second `--from` naming one already
        // read is dropped with a note, so its rows are never counted twice.
        let mut seen: HashMap<Identity, String> = HashMap::with_capacity(args.from.len());
        let dropped = |first: &str, again: &str| {
            note(&format!(
                "heph: --from {again} names the same recording as --from {first}; reading it once\n"
            ));
        };
        for r in &args.from {
            interrupted(stop)?;
            // A path or a URL is known by what it names before it is opened,
            // so a duplicate is never downloaded or decompressed twice.
            let (early, shown) = match reference::classify(r.trim())? {
                RefKind::Path(p) => {
                    let canonical = tokio::fs::canonicalize(p)
                        .await
                        .unwrap_or_else(|_| p.to_owned());
                    (Some(Identity::Path(canonical)), r.clone())
                }
                RefKind::Url(u) => {
                    let url = url::Url::parse(u).map_or_else(|_| u.to_owned(), String::from);
                    (Some(Identity::Url(url)), reference::redact_url(u))
                }
                RefKind::Id(_) => (None, r.clone()),
            };
            if let Some(first) = early.as_ref().and_then(|k| seen.get(k)) {
                dropped(first, &shown);
                continue;
            }
            let file = match reference::open_ref(env.sources, r, env.opts, env.opener).await? {
                Resolved::Local { entry, .. } => {
                    match seen.entry(Identity::Local(entry.id)) {
                        Entry::Occupied(first) => {
                            dropped(first.get(), &shown);
                            continue;
                        }
                        Entry::Vacant(v) => {
                            v.insert(shown);
                        }
                    }
                    let (root, dir) = (env.root.to_owned(), env.root.join(entry.id.encode()));
                    let note = Arc::clone(&note);
                    hcore::blocking::run(move || RecordingFile::local(&root, &dir, &*note))
                        .await
                        .with_context(|| format!("opening session {r}"))?
                }
                Resolved::Foreign(recording) => {
                    if let Some(key) = early {
                        seen.insert(key, shown);
                    }
                    RecordingFile::foreign(recording)
                        .with_context(|| format!("opening {}", reference::redact_url(r)))?
                }
            };
            out.push(Source::Recording(file));
        }
        return Ok((out, 0));
    }
    let (since_ms, until_ms) =
        super::list::window(&args.since, args.until.as_deref(), env.opts.now_ms)?;
    if args.full {
        return full(env, since_ms, until_ms, Arc::clone(stop)).await;
    }
    if !env.root.is_dir() {
        // No session was ever recorded here: the empty schema answers.
        return Ok((Vec::new(), 0));
    }
    let (root, now_ms) = (env.root.to_owned(), env.opts.now_ms);
    let catalog = hcore::blocking::run(move || catalog::open(&root, now_ms))
        .await
        .context("opening the session catalog")?;
    for n in &catalog.notes {
        note(n);
    }
    let scope = Scope {
        since_ms: Some(since_ms),
        until_ms,
        workspace: env.workspace.clone(),
    };
    Ok((vec![Source::Catalog { catalog, scope }], 0))
}

/// What a `--from` ref names, for reading each recording once: a local
/// session by id, a file by its canonical path, a download by its URL.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Identity {
    Local(SessionId),
    Path(PathBuf),
    Url(String),
}

/// `--full`: every recording kept that started in the window, in the
/// workspace.
async fn full(
    env: &QueryEnv<'_>,
    since_ms: u64,
    until_ms: Option<u64>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<(Vec<Source>, usize)> {
    let in_window = |t: u64| t >= since_ms && until_ms.is_none_or(|u| t <= u);
    let mut dirs = Vec::new();
    let mut skipped = 0;
    for (_, entry) in list_all(env.sources).await? {
        match &entry.meta {
            EntryMeta::Known(m) => {
                let here = env
                    .workspace
                    .as_deref()
                    .is_none_or(|w| m.workspace_root.as_deref() == Some(w));
                if here && in_window(m.started_unix_ms) {
                    dirs.push(env.root.join(entry.id.encode()));
                }
            }
            // Its workspace is unknown; counted wherever it is asked for.
            EntryMeta::TooNew { .. } => {
                skipped += usize::from(in_window(entry.id.started_unix_ms()))
            }
        }
    }
    let (root, note) = (env.root.to_owned(), Arc::clone(&env.note));
    hcore::blocking::run(move || {
        let mut out = Vec::with_capacity(dirs.len());
        for dir in dirs {
            // Opening one may re-derive it: seconds each, over many.
            interrupted(&stop)?;
            match RecordingFile::local(&root, &dir, &*note) {
                Ok(file) => out.push(Source::Recording(file)),
                Err(e) => {
                    tracing::debug!(dir = %dir.display(), error = %format!("{e:#}"), "skipping a recording");
                    skipped += 1;
                }
            }
        }
        Ok((out, skipped))
    })
    .await
}

/// `e` as the command reports it: a Ctrl-C exits 130, as an interrupted
/// command does; everything else is 125 (`wrap_exit`).
pub(crate) fn query_error(e: QueryError) -> anyhow::Error {
    match e {
        QueryError::Sql(m) => anyhow::anyhow!(m),
        QueryError::Interrupted => SessionExit {
            code: SessionExit::INTERRUPTED,
            error: Some(anyhow::anyhow!("query interrupted")),
        }
        .into(),
        QueryError::Io(e) | QueryError::Other(e) => e.context("running the query"),
    }
}

/// The `interrupted` error once `stop` is set.
fn interrupted(stop: &AtomicBool) -> anyhow::Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Err(query_error(QueryError::Interrupted));
    }
    Ok(())
}

/// The stderr lines saying the answer is incomplete.
fn notes(c: &Coverage, note: &(dyn Fn(&str) + Send + Sync)) {
    if !c.partial.is_empty() {
        let what: Vec<String> = c
            .partial
            .iter()
            .map(|(t, why)| format!("{t}: {why}"))
            .collect();
        note(&format!(
            "heph: the catalog holds part of what was read ({}); --full or --from reads the \
             recordings whole\n",
            what.join("; ")
        ));
    }
    if c.skipped > 0 {
        note(&format!(
            "heph: skipped {} recording(s): a format this heph does not read, or unreadable\n",
            c.skipped
        ));
    }
    if c.truncated > 0 {
        note(&format!(
            "heph: {} session(s) read stopped recording at the size ceiling; their rows are \
             incomplete\n",
            c.truncated
        ));
    }
}

/// Sets `stop` on every SIGINT while held.
pub(crate) struct StopOn(Option<tokio::task::JoinHandle<()>>);

impl StopOn {
    fn interrupt(stop: Arc<AtomicBool>) -> Self {
        Self::on(SignalKind::interrupt(), stop)
    }

    /// The listener is armed here, before the task is spawned: a signal
    /// that arrives before the task first runs is not lost. The kind is a
    /// parameter only so the test can raise a signal nobody else in the
    /// test binary listens for.
    pub(crate) fn on(kind: SignalKind, stop: Arc<AtomicBool>) -> Self {
        match signal(kind) {
            Ok(mut sig) => Self(Some(tokio::spawn(async move {
                while sig.recv().await.is_some() {
                    stop.store(true, Ordering::Relaxed);
                }
            }))),
            Err(e) => {
                tracing::warn!(error = %e, "listening for ctrl-c; it will kill heph instead of stopping the query");
                Self(None)
            }
        }
    }
}

impl Drop for StopOn {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// `--schema`: the six names, a line per column, the contract version, and
/// the built-in recipes as runnable SQL, each under a `-- name: purpose`
/// line.
fn write_schema(out: &mut dyn Write) -> anyhow::Result<()> {
    let mut text = format!("explorer schema version {}\n", store::SCHEMA_VERSION);
    for t in engine::TABLES {
        text.push('\n');
        text.push_str(t.name);
        text.push('\n');
        let width = t.columns.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
        for (c, ty) in t.columns {
            text.push_str(&format!("  {c:width$}  {ty}\n"));
        }
    }
    text.push_str("\nrecipes\n");
    for r in engine::RECIPES {
        text.push_str(&format!("\n{}{};\n", r.comment(), r.sql));
    }
    out.write_all(text.as_bytes()).context("writing the schema")
}

/// A value as `--format table|csv` print it: NULL empty, BLOB base64.
fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => base64::engine::general_purpose::STANDARD.encode(b),
    }
}

/// Aligned columns under a header, every cell escaped for the terminal.
fn write_table(answer: &Answer, out: &mut dyn Write) -> anyhow::Result<()> {
    let header: Vec<String> = answer
        .columns
        .iter()
        .map(|c| escape(c).into_owned())
        .collect();
    let rows: Vec<Vec<String>> = answer
        .rows
        .iter()
        .map(|r| r.iter().map(|v| escape(&plain(v)).into_owned()).collect())
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in &rows {
        for (w, cell) in widths.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for r in std::iter::once(&header).chain(&rows) {
        let start = text.len();
        for (i, (cell, w)) in r.iter().zip(&widths).enumerate() {
            text.push_str(cell);
            if i + 1 < r.len() {
                text.extend(std::iter::repeat_n(' ', w - cell.chars().count() + 2));
            }
        }
        let end = text.trim_end().len().max(start);
        text.truncate(end);
        text.push('\n');
    }
    out.write_all(text.as_bytes())
        .context("writing the query's rows")
}

/// RFC 4180 rows under a header line, `\n`-terminated. A terminal gets
/// control characters escaped, newlines and tabs kept; a pipe gets the
/// values as they are.
fn write_csv(answer: &Answer, terminal: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let field = |s: &str, text: &mut String| {
        let s = if terminal { escape_text(s) } else { s.into() };
        if s.contains([',', '"', '\n', '\r']) {
            text.push('"');
            text.push_str(&s.replace('"', "\"\""));
            text.push('"');
        } else {
            text.push_str(&s);
        }
    };
    let mut text = String::new();
    let lines = std::iter::once(answer.columns.clone())
        .chain(answer.rows.iter().map(|r| r.iter().map(plain).collect()));
    for line in lines {
        for (i, cell) in line.iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            field(cell, &mut text);
        }
        text.push('\n');
    }
    out.write_all(text.as_bytes())
        .context("writing the query's rows")
}

#[derive(Serialize)]
struct JsonAnswer<'a> {
    version: u32,
    columns: &'a [String],
    types: &'a [Option<String>],
    rows: Vec<Vec<serde_json::Value>>,
    coverage: JsonCoverage<'a>,
}

#[derive(Serialize)]
struct JsonCoverage<'a> {
    catalog: &'a [&'static str],
    recordings: usize,
    skipped: usize,
    truncated: usize,
    reaches_ms: Option<u64>,
    partial: &'a BTreeMap<&'static str, &'static str>,
}

/// A value under `--format json`: INTEGER a string, REAL a number (NaN and
/// infinities null), BLOB base64, NULL null.
fn json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Integer(i) => serde_json::Value::String(i.to_string()),
        Value::Real(f) => {
            serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into)
        }
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Blob(b) => {
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
    }
}

/// `{version, columns, types, rows, coverage}` on one line.
fn write_json(answer: &Answer, out: &mut dyn Write) -> anyhow::Result<()> {
    let doc = JsonAnswer {
        version: QUERY_JSON_VERSION,
        columns: &answer.columns,
        types: &answer.types,
        rows: answer
            .rows
            .iter()
            .map(|r| r.iter().map(json_value).collect())
            .collect(),
        coverage: JsonCoverage {
            catalog: &answer.coverage.catalog,
            recordings: answer.coverage.recordings,
            skipped: answer.coverage.skipped,
            truncated: answer.coverage.truncated,
            reaches_ms: answer.coverage.reaches_ms,
            partial: &answer.coverage.partial,
        },
    };
    serde_json::to_writer(&mut *out, &doc).context("writing the query's rows")?;
    writeln!(out).context("writing the query's rows")
}

/// `--timeout`: a duration (`30s`, `2m`).
pub(crate) fn parse_timeout(s: &str) -> Result<Duration, String> {
    hcore::units::parse_duration(s).map_err(|e| format!("{e:#}"))
}
