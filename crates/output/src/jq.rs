//! `--jq`: a jq filter, through jaq, run on a thread of its own.
//!
//! The filter compiles while arguments are parsed, so a bad one exits 2 before
//! any engine work (I2). The document reaches the filter as the bytes the
//! compact writer produced, parsed with `jaq_json::read::parse_single`: key
//! order is kept, and `--jq .` equals `--json | jq .` by construction.
//!
//! The evaluation runs on a dedicated `std::thread`: jaq's values are `Rc`
//! based (`!Send`), and jaq has no interrupt hook. On cancellation the thread
//! is abandoned, not joined — never `spawn_blocking`, whose pool the runtime
//! joins on drop, which would hang exit on a filter that never ends.
//!
//! The filter is pure (I7): `input`/`inputs` are never registered, and `env`,
//! `$ENV`, `halt` and `halt_error` are removed by name, so a filter cannot
//! read stdin or the environment, or exit the process. `now` and `debug` (to
//! stderr) stay.

use crate::error::OutputError;
use anyhow::Context as _;
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Native, Vars, compile, data, load};
use jaq_json::Val;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

type Data = data::JustLut<Val>;
type Filter = compile::Filter<Native<Data>>;

/// Builtins taken out of jaq's standard library: each reads the environment
/// or ends the process. `input`/`inputs` live in `jaq_std::input`, which is
/// never registered.
const REMOVED: &[&str] = &["env", "halt", "halt_error"];

/// How often the waiting side checks for cancellation while the filter runs
/// without producing.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// Results the eval thread may get ahead of the writer.
const CHANNEL_DEPTH: usize = 256;

/// The eval thread's stack. Parsing the document and evaluating a filter
/// both recurse, once per nesting level of the value or of the filter's own
/// calls, and a spawned thread's default (2 MiB) is a quarter of the main
/// thread's. Only what is touched is committed, so the reservation is cheap.
const EVAL_STACK: usize = 64 << 20;

/// A compiled `--jq` filter. Cheap to clone.
#[derive(Clone)]
pub struct Jq {
    source: Arc<str>,
    filter: Arc<Filter>,
}

impl std::fmt::Debug for Jq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Jq").field(&self.source).finish()
    }
}

impl Jq {
    /// Compile `code`. The error is a human message naming the column.
    pub fn compile(code: &str) -> Result<Self, String> {
        let filter = compile_filter(code)?;
        Ok(Self {
            source: code.into(),
            filter: Arc::new(filter),
        })
    }

    /// The filter as given.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Run the filter over `doc` (one JSON value) and write each result on
    /// its own line: a string raw, anything else as compact JSON (D6).
    ///
    /// A runtime error is [`OutputError::JqRuntime`], after the results
    /// before it were written and flushed; so is an eval thread that ended
    /// without saying it was done (a panic), rather than a short output that
    /// exits 0. `cancelled` is polled while the filter runs; once it is true
    /// the eval thread is abandoned and this returns
    /// [`OutputError::Cancelled`]. A broken pipe stops the filter and returns
    /// `Ok`.
    ///
    /// `doc` is moved to the eval thread and dropped once it is parsed, so
    /// the document's bytes and its parsed value are not both held while the
    /// filter runs.
    pub(crate) fn run(
        &self,
        doc: Vec<u8>,
        out: &mut impl Write,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::sync_channel::<Msg>(CHANNEL_DEPTH);
        let stop = Arc::new(AtomicBool::new(false));
        let filter = Arc::clone(&self.filter);
        let thread_stop = Arc::clone(&stop);
        // Detached on purpose: see the module docs.
        let _detached = std::thread::Builder::new()
            .name("heph-jq".into())
            .stack_size(EVAL_STACK)
            .spawn(move || eval(&filter, doc, &tx, &thread_stop))
            .context("starting the --jq thread")?;

        let result = drain(&rx, out, cancelled);
        // Whatever ended the wait, the thread has nothing more to do. The
        // flag only needs to become visible eventually: `Relaxed`.
        stop.store(true, Ordering::Relaxed);
        result
    }
}

/// What the eval thread sends.
enum Msg {
    /// One result, formatted, with its newline.
    Line(Vec<u8>),
    /// The filter failed; nothing follows.
    Error(String),
    /// The filter produced its last result; nothing follows. A channel that
    /// disconnects without it lost the eval thread to a panic.
    Done,
}

fn drain(
    rx: &mpsc::Receiver<Msg>,
    out: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    loop {
        // Checked on every turn, not only when the filter is quiet: a filter
        // that produces forever never lets the wait time out.
        if cancelled() {
            // Results so far stay on stdout; the reason goes to stderr
            // through the error.
            crate::write::closed(out.flush())?;
            return Err(OutputError::Cancelled.into());
        }
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(Msg::Line(line)) => {
                if crate::write::closed(out.write_all(&line))? {
                    return Ok(());
                }
            }
            Ok(Msg::Error(message)) => {
                crate::write::closed(out.flush())?;
                return Err(OutputError::JqRuntime(message).into());
            }
            Ok(Msg::Done) => {
                crate::write::closed(out.flush())?;
                return Ok(());
            }
            // The eval thread dropped its sender without `Done` or `Error`:
            // it panicked. What came so far stays on stdout, but the output
            // is short, so it must not read as a success.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                crate::write::closed(out.flush())?;
                return Err(OutputError::JqRuntime("the filter panicked".to_owned()).into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Nothing came in a while: let what came so far out.
                if crate::write::closed(out.flush())? {
                    return Ok(());
                }
            }
        }
    }
}

/// The eval thread's body.
fn eval(filter: &Filter, doc: Vec<u8>, tx: &mpsc::SyncSender<Msg>, stop: &AtomicBool) {
    let parsed = jaq_json::read::parse_single(&doc);
    // The parsed value is all the filter reads: free the bytes before it runs.
    drop(doc);
    let input = match parsed {
        Ok(input) => input,
        Err(e) => {
            // Our own writer produced `doc`; this is a bug, reported.
            let _sent = tx.send(Msg::Error(format!("reading the JSON document: {e}")));
            return;
        }
    };
    let ctx = Ctx::<Data>::new(&filter.lut, Vars::new([]));
    for result in filter.id.run((ctx, input)) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let msg = match result {
            Ok(value) => {
                let mut line = Vec::new();
                match format_result(&mut line, &value) {
                    Ok(()) => Msg::Line(line),
                    Err(e) => Msg::Error(format!("formatting a result: {e}")),
                }
            }
            // `halt` is removed, so an exception is always an error. Never
            // `unwrap_valr`: on a halt it exits the process.
            Err(exn) => Msg::Error(match exn.get_err() {
                Ok(e) => e.to_string(),
                Err(_) => "the filter stopped without a value".to_string(),
            }),
        };
        let last = matches!(msg, Msg::Error(_));
        if tx.send(msg).is_err() || last {
            return;
        }
    }
    // The receiver may be gone (a closed pipe, a cancellation): nothing to do.
    let _sent = tx.send(Msg::Done);
}

/// One result: a text string raw (the `gh --jq` rule), anything else as
/// compact JSON; then a newline.
fn format_result(out: &mut Vec<u8>, value: &Val) -> std::io::Result<()> {
    match value {
        Val::TStr(s) => out.extend_from_slice(s),
        other => jaq_json::write::write(out, &jaq_json::write::Pp::default(), 0, other)?,
    }
    out.push(b'\n');
    Ok(())
}

fn compile_filter(code: &str) -> Result<Filter, String> {
    let defs = jaq_core::defs()
        .chain(jaq_std::defs().filter(|d| !REMOVED.contains(&d.name)))
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs().filter(|(name, _, _)| !REMOVED.contains(name)))
        .chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, File { code, path: () })
        .map_err(|errors| render_load_errors(code, errors))?;
    Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errors| render_compile_errors(code, errors))
}

fn render_load_errors(code: &str, errors: load::Errors<&str, ()>) -> String {
    let mut messages = Vec::new();
    for (_file, error) in errors {
        match error {
            load::Error::Io(items) => {
                for (path, e) in items {
                    messages.push(at(code, path, &format!("cannot load `{path}`: {e}")));
                }
            }
            load::Error::Lex(items) => {
                for (expect, found) in items {
                    let what = match expect {
                        load::lex::Expect::Delim(open) => {
                            format!("a closing delimiter for `{open}`")
                        }
                        other => other.as_str().to_string(),
                    };
                    messages.push(at(code, found, &format!("expected {what}")));
                }
            }
            load::Error::Parse(items) => {
                for (expect, found) in items {
                    let found_text = if found.is_empty() {
                        "the end of the filter".to_string()
                    } else {
                        format!("`{found}`")
                    };
                    messages.push(at(
                        code,
                        found,
                        &format!("expected {}, found {found_text}", expect.as_str()),
                    ));
                }
            }
        }
    }
    messages.join("\n")
}

fn render_compile_errors(code: &str, errors: compile::Errors<&str, ()>) -> String {
    let mut messages = Vec::new();
    for (_file, items) in errors {
        for (name, undefined) in items {
            let what = match undefined {
                compile::Undefined::Filter(arity) => {
                    format!("unknown function `{name}/{arity}`")
                }
                other => format!("unknown {} `{name}`", other.as_str()),
            };
            let removed = REMOVED.contains(&name.trim_start_matches('$'))
                || matches!(name, "$ENV" | "input" | "inputs");
            let what = if removed {
                format!(
                    "{what}: --jq leaves out builtins that read stdin or the environment, or exit"
                )
            } else {
                what
            };
            messages.push(at(code, name, &what));
        }
    }
    messages.join("\n")
}

/// `message`, located at `part` (a slice of `code`): the line and column, and
/// the filter with a caret under the spot.
fn at(code: &str, part: &str, message: &str) -> String {
    let start = offset_in(code, part).unwrap_or(code.len());
    let before = code.get(..start).unwrap_or(code);
    let line_no = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let column = before
        .get(line_start..)
        .map_or(1, |s| s.chars().count() + 1);
    let line = code
        .get(line_start..)
        .and_then(|rest| rest.lines().next())
        .unwrap_or("");
    let width = part.chars().count().max(1);
    let location = if code.contains('\n') {
        format!("line {line_no}, column {column}")
    } else {
        format!("column {column}")
    };
    format!(
        "{message} (at {location})\n  {line}\n  {}{}",
        " ".repeat(column.saturating_sub(1)),
        "^".repeat(width)
    )
}

/// Where `part` starts inside `code`, if it is a slice of it.
fn offset_in(code: &str, part: &str) -> Option<usize> {
    let base = code.as_ptr() as usize;
    let ptr = part.as_ptr() as usize;
    (ptr >= base && ptr + part.len() <= base + code.len()).then(|| ptr - base)
}

#[cfg(test)]
mod tests;
