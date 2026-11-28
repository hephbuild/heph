//! The interactive failure report: printed once the TUI is torn down, one
//! box per culprit, drawn by [`render_report`] from each failure's record
//! (error-reporting D5), so it says what the CI box and the line backend
//! say. It names a log by a `heph log` command and the kept outputs by
//! `heph inspect failure`, never by a path into heph's home (two-mode TUI
//! I8, I9).
//!
//! ```text
//! ✗ //web:lint · exit status 1
//!   reached by: //app:all → //web:lint
//!   took down: //app:bundle
//!      ╭─[log · 37 lines]
//!   37 │ web/a.ts:14:3  error  'useState' is defined but never used
//!      ╰────
//!   outputs: report/ (3 files) · heph inspect failure //web:lint
//!   log: heph log //web:lint
//! ```
//!
//! What it needs from the request is read in [`ReportContext::gather`],
//! where `finalize` builds the error; the terminal's width is read when the
//! report is drawn, after the teardown.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::engine::error::{FailureRecord, TargetFailure};
use crate::tui::failure_report::{RenderCtx, RenderMode, render_report};
use crate::tui::text::{Redact, WorkspaceRel};

/// The width a report wraps at when the terminal's is unknown.
const FALLBACK_WIDTH: usize = 120;

/// What the report needs beyond the failures themselves, read before the
/// render: one record per culprit, in failure order.
#[derive(Debug)]
pub struct ReportContext {
    records: Vec<Arc<FailureRecord>>,
    /// heph's home, redacted from every error text and log line.
    redact: Redact,
    /// The workspace, which error text names paths relative to; a log line
    /// keeps the tool's own paths.
    rel: WorkspaceRel,
    /// Where an excerpt's file is read from: the workspace root.
    source_root: Option<PathBuf>,
    /// The most log lines a box shows (`--log-lines`).
    log_lines: usize,
    /// The width to wrap at instead of the terminal's (tests).
    width: Option<usize>,
}

impl ReportContext {
    /// Gather the report for `failures`: each one's record, leaving out a
    /// stop (`was_cancelled`, or a record that says so), with `home`
    /// redacted and at most `log_lines` lines of each log. Call it where
    /// the failures are collected.
    pub fn gather(
        failures: &[Arc<TargetFailure>],
        was_cancelled: impl Fn(&str) -> bool,
        home: Option<&Path>,
        log_lines: usize,
    ) -> Self {
        // A target a stop ended did not fail: no box, and no word about it.
        let records = failures
            .iter()
            .filter(|f| !f.record.cancelled && !was_cancelled(&f.addr.format()))
            .map(|f| Arc::clone(&f.record))
            .collect();
        Self {
            records,
            redact: home.map(Redact::heph_home).unwrap_or_default(),
            rel: WorkspaceRel::default(),
            source_root: None,
            log_lines,
            width: None,
        }
    }

    /// Name paths under `root` relative to it in error text (D2), and read
    /// excerpts from it. Reads the filesystem once, as
    /// [`gather`](Self::gather) does: call it there.
    pub fn with_workspace(mut self, root: Option<&Path>) -> Self {
        self.rel = root.map(WorkspaceRel::workspace).unwrap_or_default();
        self.source_root = root.map(Path::to_path_buf);
        self
    }

    /// Wrap at `width` columns instead of the terminal's: a test's report
    /// must not depend on the terminal it ran in.
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = Some(width);
        self
    }

    /// Redact `redact` instead of the home given to [`gather`](Self::gather).
    #[cfg(test)]
    pub(crate) fn with_redact(mut self, redact: Redact) -> Self {
        self.redact = redact;
        self
    }

    /// The report: a blank line, then the boxes ([`render_report`]: the
    /// first 50 in full, one line for each further one). Empty when every
    /// failure was a stop. Wraps at the terminal's width as it is now.
    pub fn render(&self, color: bool) -> String {
        if self.records.is_empty() {
            return String::new();
        }
        let width = self.width.unwrap_or_else(|| {
            crossterm::terminal::size()
                .map(|(w, _)| usize::from(w))
                .ok()
                .filter(|w| *w > 0)
                .unwrap_or(FALLBACK_WIDTH)
        });
        let ctx = RenderCtx {
            rel: self.rel.clone(),
            redact: self.redact.clone(),
            source_root: self.source_root.clone(),
            width: Some(width),
            color,
            log_lines: self.log_lines,
            mode: RenderMode::Report,
        };
        let mut out = String::from("\n");
        out.push_str(&render_report(self.records.iter().map(AsRef::as_ref), &ctx));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::htaddr::parse_addr;
    use hcore::diagnostic::{Diagnostic, Stage};
    use hcore::events::{ExitData, FailedIn, LeftBehindData, LogRef, TookDownData};

    const HOME: &str = "/home/u/.heph";

    /// A culprit's failure carrying `rec` as its record.
    fn failure(addr: &str, rec: FailureRecord) -> Arc<TargetFailure> {
        Arc::new(
            TargetFailure::new(parse_addr(addr).expect("addr"), None, anyhow::anyhow!("x"))
                .with_record(Arc::new(rec)),
        )
    }

    /// A record of `message` against `addr`, requested itself.
    fn rec(addr: &str, message: &str) -> FailureRecord {
        let mut r = FailureRecord::new(Diagnostic::new(Stage::Run, message).with_addr(addr));
        r.addr = Some(addr.to_owned());
        r.entry = addr.to_owned();
        r.path = vec![addr.to_owned()];
        r
    }

    /// A record of a process that exited 101, its log kept in a slot at
    /// `dir` holding `lines`.
    fn exited(dir: &Path, addr: &str, lines: &[&str]) -> FailureRecord {
        let slot = dir.join(addr.replace(['/', ':'], "_"));
        std::fs::create_dir_all(&slot).expect("slot");
        std::fs::write(
            slot.join(hcore::failure_log::MANIFEST),
            r#"{"version":1,"fill_id":"f"}"#,
        )
        .expect("manifest");
        let log = slot.join(hcore::failure_log::LOG);
        std::fs::write(
            &log,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .expect("log");
        let mut r = rec(addr, "process failed");
        r.failed_in = FailedIn::Exec;
        r.exit = Some(ExitData {
            code: Some(101),
            signal: None,
            status: "exit status: 101".into(),
        });
        r.log = Some(LogRef {
            path: Some(log.display().to_string()),
            fill_id: Some("f".into()),
            lines: lines.len() as u64,
            memory_tail: None,
        });
        r.left_behind = Some(LeftBehindData {
            log_path: Some(log.display().to_string()),
            log_readable: Some(true),
            ..LeftBehindData::default()
        });
        r
    }

    fn report(failures: &[Arc<TargetFailure>], width: usize) -> String {
        ReportContext::gather(failures, |_| false, None, 3)
            .with_width(width)
            .with_redact(Redact::of([HOME.into()]))
            .render(false)
    }

    /// One box per culprit, drawn from its record in report mode: what it
    /// took down, its log by `heph log` and never by its path, heph's home
    /// redacted from heph's text, at most `--log-lines` of the log.
    #[test]
    fn failure_report_groups_by_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut first = exited(
            dir.path(),
            "//crates/engine:test",
            &["compiling", "running", "stale", "panicked", "FAILED"],
        );
        first.took_down = Some(TookDownData {
            named: vec!["//e2e:shard-1".into(), "//e2e:shard-2".into()],
            total: 2,
        });
        let second = rec(
            "//web:gen",
            &format!("staging {HOME}/sandbox/ab/in: permission denied"),
        );
        let out = report(
            &[
                failure("//crates/engine:test", first),
                failure("//web:gen", second),
            ],
            80,
        );
        assert_eq!(
            out,
            "\n✗ //crates/engine:test · exit status 101\
             \n  took down: //e2e:shard-1, //e2e:shard-2\
             \n    ╭─[log · 5 lines]\
             \n    │ … 2 earlier lines\
             \n  3 │ stale\
             \n  4 │ panicked\
             \n  5 │ FAILED\
             \n    ╰────\
             \n  log: heph log //crates/engine:test\
             \n\
             \n✗ //web:gen\
             \n  staging <heph home>/sandbox/ab/in: permission denied\
             \n"
        );
        assert!(!out.contains(&*dir.path().to_string_lossy()), "{out}");
    }

    /// A stop is not a failure: neither a record that says so nor a target
    /// the request remembers as stopped gets a box, and a report of stops
    /// alone is empty.
    #[test]
    fn report_leaves_out_stops() {
        let mut stopped = rec("//p:stopped", "cancelled");
        stopped.cancelled = true;
        let failures = [
            failure("//p:stopped", stopped),
            failure("//p:remembered", rec("//p:remembered", "cancelled")),
            failure("//p:t", rec("//p:t", "boom")),
        ];
        let out = ReportContext::gather(&failures, |a| a == "//p:remembered", None, 3)
            .with_width(80)
            .render(false);
        assert_eq!(out, "\n✗ //p:t\n  boom\n");
        let out =
            ReportContext::gather(&failures[..2], |a| a == "//p:remembered", None, 3).render(false);
        assert_eq!(out, "");
    }

    /// The report draws 50 boxes, then one line per further failure, then
    /// how to read them all (E5; the cap is the user's, 2026-11-27).
    #[test]
    fn report_caps_boxes_at_fifty() {
        let failures: Vec<_> = (0..53)
            .map(|i| {
                let a = format!("//p:t{i}");
                failure(&a, rec(&a, "boom"))
            })
            .collect();
        let out = report(&failures, 80);
        assert_eq!(out.matches("  boom\n").count(), 50, "{out}");
        assert!(
            out.ends_with(
                "✗ //p:t49\n  boom\n\n✗ //p:t50 · boom\n✗ //p:t51 · boom\n✗ //p:t52 · boom\n\
                 3 more: heph inspect failure --json\n"
            ),
            "{out}"
        );
    }

    /// Text wraps at the width instead of being cut: every word is shown.
    #[test]
    fn report_wraps_instead_of_cutting() {
        let words: Vec<String> = (0..30).map(|i| format!("word{i}")).collect();
        let out = report(&[failure("//p:t", rec("//p:t", &words.join(" ")))], 40);
        for w in &words {
            assert!(out.contains(w.as_str()), "{w}: {out}");
        }
        assert!(
            out.lines().all(|l| l.chars().count() <= 40),
            "every row within the width: {out}"
        );
    }

    /// D2 in the interactive report: the message names workspace paths
    /// relative and escapes control bytes, through a symlinked root too.
    /// Without a workspace, nothing is rewritten.
    #[test]
    fn report_cause_names_workspace_paths_relative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("a")).expect("mkdir");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let real = std::fs::canonicalize(&real).expect("canonical");
        let file = real.join("a/BUILD");
        let failures = [failure(
            "//p:t",
            rec(
                "//p:t",
                &format!("reading {}: denied\x1b[2J", file.display()),
            ),
        )];
        let render = |root: Option<&Path>| {
            ReportContext::gather(&failures, |_| false, None, 3)
                .with_width(400)
                .with_workspace(root)
                .render(false)
        };
        for root in [real.as_path(), link.as_path()] {
            let out = render(Some(root));
            assert!(
                out.contains("reading a/BUILD: denied\\x1b[2J"),
                "{root:?}: {out}"
            );
            assert!(!out.contains('\x1b'), "{out:?}");
        }
        let out = render(None);
        assert!(
            out.contains(&format!("reading {}: denied", file.display())),
            "{out}"
        );
    }

    /// I9: the report names no path into heph's home but the sandbox the
    /// user asked to keep, and the kept outputs by `heph inspect failure`.
    #[test]
    fn no_home_path_on_any_screen_report() {
        let kept_at = format!("{HOME}/sandbox/p/__target_t");
        let mut r = rec("//p:t", "boom");
        r.failed_in = FailedIn::Exec;
        r.left_behind = Some(LeftBehindData {
            log_path: Some(format!("{HOME}/failures/p_t/log.txt")),
            sandbox: Some(format!("{kept_at} (kept)")),
            outputs: Some(format!("{HOME}/failures/p_t/out")),
            log_readable: Some(true),
            ..LeftBehindData::default()
        });
        let out = report(&[failure("//p:t", r)], 400);
        assert!(
            out.contains(&format!("  sandbox: {kept_at} (kept)\n")),
            "{out}"
        );
        assert!(
            out.contains("  outputs: kept · heph inspect failure //p:t\n"),
            "{out}"
        );
        assert_eq!(out.matches(HOME).count(), 1, "{out}");
    }
}
