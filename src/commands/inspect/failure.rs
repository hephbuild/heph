use crate::tui::session::SessionSlot;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use hcore::diagnostic::{Hint, SourceLoc, Stage};
use hcore::failure::{FailureRecord, LogRef};
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::utils::parse_addr_arg;
use crate::engine::Engine;
use crate::engine::failure_slot::{FILE_ADDR_PREFIX, FIXES, FailureSlots, LOG, OUT, SlotManifest};
use crate::htaddr::Addr;
use crate::tui::LogSink;
use crate::tui::text::{Redact, WorkspaceRel};

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    #[command(flatten)]
    pub out: output::JsonArgs<FailureView>,
}

/// A culprit's last recorded failure: what went wrong, at which stage and
/// where, and its slot with each part the failure left. Paths are absolute;
/// an absent part is `None` (`null` in `--json`). Only a culprit has one: a
/// target skipped because a dependency failed keeps no record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, output::Record)]
pub struct FailureView {
    /// The target's address; null when the failure is a BUILD or `.bzl`
    /// file's (`file`).
    pub addr: Option<String>,
    /// The BUILD or `.bzl` file that did not evaluate, workspace-relative;
    /// null when the failure is a target's.
    pub file: Option<String>,
    /// The failure slot's directory.
    #[serde(serialize_with = "output::ser::path")]
    pub slot: PathBuf,
    /// The stage of the build that raised it (eval, spec, parse, link, run,
    /// cache, config or cli).
    pub stage: Stage,
    /// What went wrong, on one line. Its wording is not contract.
    pub message: String,
    /// Where in a BUILD or `.bzl` file it was raised, the file
    /// workspace-relative; null when it names no place.
    pub location: Option<SourceLoc>,
    /// What the user can do about it, each with its kind. A hint's wording
    /// is not contract.
    pub hints: Vec<Hint>,
    /// A cap cut something from the record (the message, its causes, a
    /// source line).
    pub truncated: bool,
    /// The input hash of the run that failed. It may no longer be the
    /// target's: nothing is resolved to compare. Null for a failure before
    /// the run, which had no run to hash.
    pub hashin: Option<String>,
    /// When it failed, in milliseconds since the Unix epoch. Null for a
    /// failure before the run.
    pub failed_at: Option<u64>,
    /// The failed run's full process log; null when it kept none (a failure
    /// before the run keeps none).
    #[serde(serialize_with = "output::ser::opt_path")]
    pub log: Option<PathBuf>,
    /// How many lines `log` holds; null with no log.
    pub log_lines: Option<u64>,
    /// The slot was filled again by a newer failed run while it was read:
    /// `log` is that run's.
    pub log_replaced: bool,
    /// Its `failure_out` matches, laid out as in the workspace; null when
    /// none matched.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub out: Option<PathBuf>,
    /// The list of files an `in_place` write-back changed; null when none.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub fixes: Option<PathBuf>,
}

impl FailureView {
    /// The failure happened before the target's process ran: no revision,
    /// no log.
    pub fn before_run(&self) -> bool {
        self.failed_at.is_none()
    }
}

/// What a view's message is scrubbed with, as `run --json`'s `error` is:
/// heph's home redacted, the workspace's paths made relative.
#[derive(Debug, Clone, Default)]
pub struct ViewText {
    redact: Redact,
    rel: WorkspaceRel,
}

impl ViewText {
    /// For `engine`'s home and workspace. Reads the filesystem once.
    pub fn for_engine(engine: &Engine) -> Self {
        Self {
            redact: Redact::heph_home(&engine.home),
            rel: WorkspaceRel::workspace(engine.root()),
        }
    }
}

/// The failure recorded in `addr`'s own slot in `slots`, read from the slot
/// alone: nothing is built or resolved. `None` when there is no complete
/// slot for it — the target never failed, or passed or resolved from cache
/// since. The message is not scrubbed: [`inspect_view`] is what the command
/// prints.
pub fn failure_view(slots: &FailureSlots, addr: &Addr) -> anyhow::Result<Option<FailureView>> {
    slots
        .read(addr)
        .map(|m| slot_view(&slots.slot_dir(addr), m, &ViewText::default()))
        .transpose()
}

/// What `heph inspect failure <addr>` reports: `addr`'s own slot, else the
/// slot of the BUILD file its package did not evaluate in (D5, Q5). `None`
/// for a target with neither: it never failed, passed since, or was skipped.
pub fn inspect_view(engine: &Engine, addr: &Addr) -> anyhow::Result<Option<FailureView>> {
    let slots = engine.failure_slots();
    let found = match slots.read(addr) {
        Some(m) => Some((slots.slot_dir(addr), m)),
        None => slots.read_file_of_package(addr.package.as_str()),
    };
    found
        .map(|(dir, m)| slot_view(&dir, m, &ViewText::for_engine(engine)))
        .transpose()
}

/// The view of the complete slot at `dir`, whose manifest is `manifest`.
fn slot_view(dir: &Path, manifest: SlotManifest, text: &ViewText) -> anyhow::Result<FailureView> {
    let slot = std::path::absolute(dir).context("making the failure slot's path absolute")?;
    let part = |name: &str, is: fn(&Path) -> bool| {
        let p = slot.join(name);
        is(&p).then_some(p)
    };
    let log = part(LOG, Path::is_file);
    let log_lines = log
        .as_deref()
        .map(|p| {
            std::fs::File::open(p)
                .and_then(|f| hcore::failure_log::count_lines(&f))
                .with_context(|| format!("counting the lines of {}", p.display()))
        })
        .transpose()?;
    let file = manifest
        .addr
        .strip_prefix(FILE_ADDR_PREFIX)
        .map(str::to_owned);
    let record = hcore::failure_log::read_diagnostic(&slot).map(|mut rec| {
        // The record's log is the slot's, at the fill the manifest names:
        // `json_facts` tells, as `run --json` does, whether it was replaced.
        rec.log = log.as_ref().map(|p| LogRef {
            path: Some(p.to_string_lossy().into_owned()),
            fill_id: manifest.fill_id.clone(),
            ..LogRef::default()
        });
        rec
    });
    // The slot says whether the target ran, never the record (compat F2): a
    // cdylib's failure after its run can be recorded `before_run`, and an
    // older slot has no record. A run leaves a revision or a log; a failure
    // before it leaves neither.
    let before_run = file.is_some() || (manifest.hashin.is_none() && log.is_none());
    let (stage, message, location, hints, truncated, log_replaced) = match &record {
        Some(rec) => {
            let facts = crate::tui::failure_report::json_facts(rec);
            let hints = facts
                .hints
                .into_iter()
                .map(|h| Hint::new(h.kind, line(&h.text, text)))
                .collect();
            (
                facts.stage,
                line(&message_of(rec), text),
                facts.location,
                hints,
                facts.truncated,
                facts.log_replaced,
            )
        }
        // A slot from an older heph, which kept no diagnostic.
        None => (
            if before_run { Stage::Eval } else { Stage::Run },
            "no diagnostic kept (the slot is from an older heph)".to_owned(),
            None,
            Vec::new(),
            false,
            false,
        ),
    };
    Ok(FailureView {
        addr: file.is_none().then_some(manifest.addr),
        file,
        stage,
        message,
        location,
        hints,
        truncated,
        hashin: manifest.hashin.filter(|_| !before_run),
        failed_at: (!before_run).then_some(manifest.failed_at),
        log,
        log_lines,
        log_replaced,
        out: part(OUT, Path::is_dir),
        fixes: part(FIXES, Path::is_file),
        slot,
    })
}

/// The record's message, else its first cause.
fn message_of(rec: &FailureRecord) -> String {
    match rec.diagnostic.message.trim() {
        "" => rec.causes.first().cloned().unwrap_or_default(),
        m => m.to_owned(),
    }
}

/// `s` on one line, scrubbed as `run --json`'s `error` is.
fn line(s: &str, text: &ViewText) -> String {
    let one = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    crate::tui::text::message(&one, &text.redact, &text.rel).into_owned()
}

/// The text form: what failed and where, then the slot and each part
/// present, one labelled line each.
fn render_text(view: &FailureView) -> String {
    let mut out = String::new();
    let name = view.addr.as_deref().or(view.file.as_deref()).unwrap_or("");
    out.push_str(&format!(
        "failure: {name} ({} stage): {}\n",
        view.stage.as_str(),
        view.message
    ));
    if let Some(loc) = &view.location {
        out.push_str(&format!(
            "location: {}:{}:{}\n",
            loc.file, loc.line, loc.col
        ));
    }
    for hint in &view.hints {
        out.push_str(&format!("hint: {}\n", hint.text));
    }
    if view.before_run() {
        out.push_str("run: never (it failed before running)\n");
    }
    out.push_str(&format!("slot: {}\n", view.slot.display()));
    for (label, part) in [
        ("log", &view.log),
        ("out", &view.out),
        ("fixes", &view.fixes),
    ] {
        if let Some(p) = part {
            out.push_str(&format!("{label}: {}\n", p.display()));
        }
    }
    out
}

pub fn execute(
    args: &Args,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    args: Args,
    _sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let addr = parse_addr_arg(&args.addr)?;
    let (engine, _shutdown) = bootstrap::new_engine(&global, session)?;
    let view = inspect_view(&engine, &addr)?;
    let mut stdout = std::io::BufWriter::new(std::io::stdout());
    print_to(
        &args.out,
        &addr,
        view.as_ref(),
        &mut stdout,
        output::Style::for_stdout(),
    )?;
    stdout.flush().context("printing the failure")
}

/// The text-mode error for a target with no recorded failure. It does not
/// read as "never failed": a skipped target has no record either (U16).
pub fn no_record_message(addr: &Addr) -> String {
    format!(
        "no failure recorded for {} (skipped targets keep no record; the run's --json carries root and path)",
        addr.format()
    )
}

/// Print what the command prints on stdout for `view` on `w`, or return the
/// error it exits 1 with. With no recorded failure, text mode is an error
/// ([`no_record_message`] on stderr, exit 1), while `--json` prints `null`
/// and succeeds: a script tells "no failure" from "the command broke" by the
/// exit code alone.
pub fn print_to(
    out: &output::JsonArgs<FailureView>,
    addr: &Addr,
    view: Option<&FailureView>,
    w: &mut impl Write,
    style: output::Style,
) -> anyhow::Result<()> {
    if out.enabled() {
        // No request: SIGINT ends the process as is.
        let printer = out.printer_to(w, style);
        return match view {
            Some(view) => printer.record(view, &output::never_cancelled),
            None => printer.null(&output::never_cancelled),
        };
    }
    match view {
        Some(view) => w
            .write_all(render_text(view).as_bytes())
            .context("printing the failure"),
        None => anyhow::bail!("{}", no_record_message(addr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr;

    /// A slot for `addr` under a fresh home, holding a log, an `out/` and,
    /// when `fixes`, a `fixes.txt`.
    fn slot(addr: &Addr, fixes: bool) -> (tempfile::TempDir, FailureSlots) {
        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        let dir = slots.slot_dir(addr);
        std::fs::create_dir_all(dir.join(OUT)).expect("mkdir out");
        std::fs::write(dir.join(LOG), "boom\n").expect("write log");
        if fixes {
            std::fs::write(dir.join(FIXES), "web/a.ts\n").expect("write fixes");
        }
        let manifest = serde_json::json!({
            "version": 1, "addr": addr.format(), "hashin": "h1", "failed_at": 42,
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("write manifest");
        (home, slots)
    }

    /// "Before the run" is the slot's to say, not the record's (compat F2):
    /// a slot with a revision and a log ran, though its record (a cdylib's
    /// after-run failure) says `before_run`. Its revision and time are kept.
    #[test]
    fn before_run_comes_from_the_slot() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, false);
        let dir = slots.slot_dir(&addr);
        std::fs::write(
            dir.join("diagnostic.json"),
            r#"{"version":1,"diagnostic":{"stage":"run","message":"boom"},"addr":"//web:lint"}"#,
        )
        .expect("write diagnostic");
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        assert_eq!(view.message, "boom", "the record was read");
        assert!(!view.before_run(), "{view:?}");
        assert_eq!(view.hashin.as_deref(), Some("h1"));
        assert_eq!(view.failed_at, Some(42));
    }

    #[test]
    fn failure_prints_slot_paths() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, false);
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let dir = slots.slot_dir(&addr);
        assert_eq!(
            render_text(&view),
            format!(
                "failure: //web:lint (run stage): no diagnostic kept (the slot is from an older heph)\n\
                 slot: {}\nlog: {}\nout: {}\n",
                dir.display(),
                dir.join(LOG).display(),
                dir.join(OUT).display()
            )
        );

        // Another address has no slot: nothing to print, and no slot made.
        let other = htaddr::parse_addr("//web:other").expect("addr");
        assert_eq!(failure_view(&slots, &other).expect("read"), None);
        assert!(!slots.slot_dir(&other).exists());
    }

    /// No recorded failure: text mode is an error naming the target (exit 1)
    /// that does not read as "never failed", `--json` is `null` on stdout
    /// and a success.
    #[test]
    fn no_recorded_failure_text_errors_json_is_null() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }

        let home = tempfile::tempdir().expect("tempdir");
        let slots = FailureSlots::new(home.path());
        for (argv, want) in [
            (&["failure", "//web:lint"][..], None),
            (&["failure", "//web:lint", "--json"][..], Some("null\n")),
            (&["failure", "//web:lint", "--jq", "."][..], Some("null\n")),
            (
                &["failure", "//web:lint", "--fields", "log"][..],
                Some("null\n"),
            ),
        ] {
            let args = Cli::try_parse_from(argv).expect("parse").args;
            let addr = htaddr::parse_addr(&args.addr).expect("addr");
            let view = failure_view(&slots, &addr).expect("read");
            assert_eq!(view, None, "{argv:?}");
            let (got, printed) = print(&args, &addr, view.as_ref());
            match want {
                Some(want) => {
                    got.expect("json succeeds");
                    assert_eq!(printed, want, "{argv:?}");
                }
                None => assert_eq!(
                    got.expect_err("text mode exits 1").to_string(),
                    "no failure recorded for //web:lint (skipped targets keep no record; \
                     the run's --json carries root and path)"
                ),
            }
        }
    }

    /// `print_to` on a buffer, piped: the result, and what it printed.
    fn print(args: &Args, addr: &Addr, view: Option<&FailureView>) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let got = print_to(&args.out, addr, view, &mut out, output::Style::Compact);
        (got, String::from_utf8(out).expect("utf-8"))
    }

    fn args(flags: &[&str]) -> Args {
        Args {
            addr: "//web:lint".into(),
            out: output::JsonArgs::try_parse_from(flags).expect("flags"),
        }
    }

    /// A recorded failure prints the text form, or the view as one compact
    /// JSON line when piped.
    #[test]
    fn print_prints_the_view() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, true);
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let (got, text) = print(&args(&[]), &addr, Some(&view));
        got.expect("text");
        assert_eq!(text, render_text(&view));

        let (got, json) = print(&args(&["--json"]), &addr, Some(&view));
        got.expect("json");
        assert_eq!(json.lines().count(), 1, "{json}");
        let json: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(json, serde_json::to_value(&view).expect("serialize"));

        // `--jq` reads the log's path straight out, unquoted.
        let (got, log) = print(&args(&["--jq", ".log"]), &addr, Some(&view));
        got.expect("jq");
        assert_eq!(
            log,
            format!("{}\n", view.log.as_ref().expect("log").display())
        );
    }

    #[test]
    fn record_keys_subset_of_fields() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, true);
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        output::check_keys(&view).unwrap();
    }

    /// A slot under a non-UTF-8 directory fails naming the path, instead of
    /// printing a lossy one a script would act on.
    #[test]
    fn non_utf8_path_is_an_error() {
        use std::os::unix::ffi::OsStrExt as _;
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let view = FailureView {
            addr: Some(addr.format()),
            file: None,
            slot: PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff")),
            stage: Stage::Run,
            message: "boom".into(),
            location: None,
            hints: Vec::new(),
            truncated: false,
            hashin: Some("h".into()),
            failed_at: Some(1),
            log: None,
            log_lines: None,
            log_replaced: false,
            out: None,
            fixes: None,
        };
        let (got, printed) = print(&args(&["--json"]), &addr, Some(&view));
        let err = format!("{:#}", got.expect_err("a lossy path is refused"));
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert_eq!(printed, "");
    }

    #[test]
    fn failure_json_shape() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, true);
        // A slot whose run kept no `out/`: that part is `null`.
        std::fs::remove_dir(slots.slot_dir(&addr).join(OUT)).expect("rm out");
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let dir = slots.slot_dir(&addr);
        assert!(dir.is_absolute());
        let json = serde_json::to_value(&view).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "addr": "//web:lint",
                "file": null,
                "slot": dir,
                "stage": "run",
                "message": "no diagnostic kept (the slot is from an older heph)",
                "location": null,
                "hints": [],
                "truncated": false,
                "hashin": "h1",
                "failed_at": 42,
                "log": dir.join(LOG),
                "log_lines": 1,
                "log_replaced": false,
                "out": null,
                "fixes": dir.join(FIXES),
            })
        );
    }
}
