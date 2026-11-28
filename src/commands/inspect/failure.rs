use crate::tui::session::SessionSlot;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap_complete::engine::ArgValueCompleter;
use serde::Serialize;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::commands::completion::complete_target_addr;
use crate::commands::utils::parse_addr_arg;
use crate::engine::failure_slot::{FIXES, FailureSlots, LOG, OUT};
use crate::htaddr::Addr;
use crate::tui::LogSink;

#[derive(clap::Args, Clone)]
pub struct Args {
    /// Target address (e.g. //pkg:name)
    #[arg(add = ArgValueCompleter::new(complete_target_addr))]
    pub addr: String,
    #[command(flatten)]
    pub out: output::JsonArgs<FailureView>,
}

/// Where a target's last recorded failure is kept: its slot, and each part
/// of it that the failed run left. Paths are absolute; an absent part is
/// `None` (`null` in `--json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, output::Record)]
pub struct FailureView {
    /// The target's address. Always set today; nullable since contract 2,
    /// for a failure that is not a target's (a BUILD file's).
    pub addr: Option<String>,
    /// The failure slot's directory.
    #[serde(serialize_with = "output::ser::path")]
    pub slot: PathBuf,
    /// The input hash of the run that failed. It may no longer be the
    /// target's: nothing is resolved to compare. Always set today; nullable
    /// since contract 2, for a failure that had no run to hash.
    pub hashin: Option<String>,
    /// When it failed, in milliseconds since the Unix epoch. Always set
    /// today; nullable since contract 2.
    pub failed_at: Option<u64>,
    /// The failed run's full process log; null when it kept none.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub log: Option<PathBuf>,
    /// Its `failure_out` matches, laid out as in the workspace; null when
    /// none matched.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub out: Option<PathBuf>,
    /// The list of files an `in_place` write-back changed; null when none.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub fixes: Option<PathBuf>,
}

/// The failure recorded for `addr` in `slots`, read from its slot alone:
/// nothing is built or resolved. `None` when there is no complete slot for
/// it — the target never failed, or passed or resolved from cache since.
pub fn failure_view(slots: &FailureSlots, addr: &Addr) -> anyhow::Result<Option<FailureView>> {
    let Some(manifest) = slots.read(addr) else {
        return Ok(None);
    };
    let slot = std::path::absolute(slots.slot_dir(addr))
        .context("making the failure slot's path absolute")?;
    let part = |name: &str, is: fn(&std::path::Path) -> bool| {
        let p = slot.join(name);
        is(&p).then_some(p)
    };
    Ok(Some(FailureView {
        addr: Some(manifest.addr),
        hashin: manifest.hashin,
        failed_at: Some(manifest.failed_at),
        log: part(LOG, std::path::Path::is_file),
        out: part(OUT, std::path::Path::is_dir),
        fixes: part(FIXES, std::path::Path::is_file),
        slot,
    }))
}

/// The text form: the slot, then each part present, one labelled path per
/// line.
fn render_text(view: &FailureView) -> String {
    let mut out = format!("slot: {}\n", view.slot.display());
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
    let view = failure_view(engine.failure_slots(), &addr)?;
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

/// Print what the command prints on stdout for `view` on `w`, or return the
/// error it exits 1 with. With no recorded failure, text mode is an error
/// (`no recorded failure for <addr>` on stderr, exit 1), while `--json`
/// prints `null` and succeeds: a script tells "no failure" from "the command
/// broke" by the exit code alone.
fn print_to(
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
        None => anyhow::bail!("no recorded failure for {}", addr.format()),
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

    #[test]
    fn failure_prints_slot_paths() {
        let addr = htaddr::parse_addr("//web:lint").expect("addr");
        let (_home, slots) = slot(&addr, false);
        let view = failure_view(&slots, &addr).expect("read").expect("a slot");
        let dir = slots.slot_dir(&addr);
        assert_eq!(
            render_text(&view),
            format!(
                "slot: {}\nlog: {}\nout: {}\n",
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

    /// No recorded failure: text mode is an error naming the target (exit 1),
    /// `--json` is `null` on stdout and a success.
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
                    "no recorded failure for //web:lint"
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
            slot: PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff")),
            hashin: Some("h".into()),
            failed_at: Some(1),
            log: None,
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
                "slot": dir,
                "hashin": "h1",
                "failed_at": 42,
                "log": dir.join(LOG),
                "out": null,
                "fixes": dir.join(FIXES),
            })
        );
    }
}
