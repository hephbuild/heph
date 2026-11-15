//! The interactive failure report: printed once the TUI is torn down, one
//! block per root failure, each naming its log by a `heph log` command and
//! never by a path into heph's home (two-mode TUI I8, I9).
//!
//! ```text
//! ✗ //web:lint · exit status: 1
//!   │ web/a.ts:14:3  error  'useState' is defined but never used
//!   ├ fixes written: web/a.ts, web/b.ts · the target still failed; re-run to verify
//!   ├ restore:       git -C /src/acme --literal-pathspecs checkout -- web/a.ts web/b.ts
//!   ├ kept:          report/ (3 files) · heph inspect failure //web:lint
//!   ╰ full log:      heph log //web:lint (37 lines)
//! ```
//!
//! Everything that touches the filesystem — the log tail, the walk of the
//! kept `failure_out` files, resolving the home — happens in
//! [`ReportContext::gather`], called where `finalize!` builds the error, so
//! rendering is pure and never waits on a disk.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crossterm::style::Stylize;

use super::{FIXES_SHOWN, cause_chain, not_written_reason, shell_quote};
use crate::engine::error::{
    FailureFixes, FrozenCheckError, ProcessFailed, SandboxFate, TargetFailure,
};
use crate::engine::request_state::TookDown;
use crate::hmemoizer::downcast_chain_ref;
use crate::tui::text::{Redact, clean_inline, cut_right, expand_tabs, sanitize_log};

/// How many kept `failure_out` groups a block names before `+N more`.
const KEPT_GROUPS_SHOWN: usize = 5;

/// The width a report is cut to when the terminal's is unknown.
const FALLBACK_WIDTH: usize = 120;

/// What the report needs beyond the failures themselves, read before the
/// render: one entry per block, in failure order.
#[derive(Debug)]
pub struct ReportContext {
    blocks: Vec<Block>,
    /// Failures that were a stop (Ctrl-C), not a failure: counted, no block.
    stopped: usize,
    /// heph's home, redacted from every error text and log line.
    redact: Redact,
    /// The terminal's width: a tail line is cut to it.
    width: usize,
}

/// One root failure's facts, gathered off the render path.
#[derive(Debug)]
struct Block {
    failure: Arc<TargetFailure>,
    took_down: TookDown,
    /// The log's last lines and the 1-based number of the first.
    tail: Option<(String, usize)>,
    /// The kept `failure_out` files, by first path component.
    kept: Vec<(String, usize)>,
}

impl ReportContext {
    /// Gather the report for `failures`: what each took down (`took_down`),
    /// which were a stop (`was_cancelled`), its log tail and kept files,
    /// with `home` redacted. Reads the filesystem; call it where the
    /// failures are collected, never from a render.
    pub fn gather(
        failures: &[Arc<TargetFailure>],
        took_down: impl Fn(&str) -> TookDown,
        was_cancelled: impl Fn(&str) -> bool,
        home: Option<&Path>,
    ) -> Self {
        let width = crossterm::terminal::size()
            .map(|(w, _)| usize::from(w))
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or(FALLBACK_WIDTH);
        let mut stopped = 0;
        let blocks = failures
            .iter()
            .filter(|f| {
                let stop = was_cancelled(&f.addr.format());
                stopped += usize::from(stop);
                !stop
            })
            .map(|f| Block {
                failure: Arc::clone(f),
                took_down: took_down(&f.addr.format()),
                tail: read_tail(f),
                // `failure_out` is package-relative and the slot mirrors the
                // workspace, so every kept file is under `out/<package>/`:
                // grouped from there, `//web:lint` reads `report/`, not `web/`.
                kept: f
                    .outputs
                    .as_deref()
                    .map(|out| kept_groups(&out.join(&f.addr.package)))
                    .unwrap_or_default(),
            })
            .collect();
        Self {
            blocks,
            stopped,
            redact: home.map(Redact::heph_home).unwrap_or_default(),
            width,
        }
    }

    /// Cut tail lines to `width` columns instead of the terminal's.
    #[cfg(test)]
    pub(crate) fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// Redact `redact` instead of the home given to [`gather`](Self::gather).
    #[cfg(test)]
    pub(crate) fn with_redact(mut self, redact: Redact) -> Self {
        self.redact = redact;
        self
    }

    /// The report: a blank line, then each block; a closing line counts
    /// the stops left out. "stopped", not "by ctrl-c": fail-fast cancels
    /// the same way, and nobody pressed anything.
    pub fn render(&self, color: bool) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            out.push('\n');
            self.render_block(&mut out, block, color);
        }
        if self.stopped > 0 {
            out.push_str(&format!(
                "\n{} stopped, not failed: no block\n",
                self.stopped
            ));
        }
        out
    }

    fn render_block(&self, out: &mut String, block: &Block, color: bool) {
        let f = &block.failure;
        let addr = f.addr.format();
        let cross = if color {
            format!("{}", "✗".red().bold())
        } else {
            "✗".to_owned()
        };
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        let mut body: Vec<String> = Vec::new();

        let status = if let Some(t) = crate::engine::run_timeout::timed_out(&f.source) {
            if let Some(grace) = t.abandoned_after {
                rows.push((
                    "abandoned:",
                    format!(
                        "the run did not stop within {} of being cancelled and was abandoned, \
                         so its process may still be running",
                        hcore::units::format_duration(grace)
                    ),
                ));
            }
            Some(format!("timed out after {} (its timeout)", t.timeout))
        } else if let Some(fc) = downcast_chain_ref::<FrozenCheckError>(&f.source) {
            body.extend(fc.diff.lines().map(|l| self.cut_row(l)));
            Some("generated output differs from tree".to_owned())
        } else {
            // Through the memoizer's shared wrapper: a recorded failure's
            // source is one, and `ErrorDetail`'s plain chain walk stops at it.
            downcast_chain_ref::<ProcessFailed>(&f.source)
                .map(|p| p.status.clone())
                .or_else(|| crate::engine::event::ErrorDetail::from_error(&f.source).exit_status)
        };
        if status.is_none() {
            // No status to name it: the error says what went wrong.
            let cause = cause_chain(&f.source, &addr);
            body.extend(
                sanitize_log(self.redact.apply(&cause).as_bytes())
                    .lines()
                    .map(|l| self.cut_row(l)),
            );
        }
        match &block.tail {
            Some((text, _)) if text.trim().is_empty() => body.push("empty log".to_owned()),
            Some((text, _)) => body.extend(
                sanitize_log(text.as_bytes())
                    .lines()
                    .map(|l| self.cut_row(&self.redact.apply(l))),
            ),
            None => {}
        }

        let td = &block.took_down;
        if td.total > 0 {
            let mut named = td.named.join(", ");
            let more = td.total.saturating_sub(td.named.len());
            if more > 0 {
                named.push_str(&format!(" +{more} more"));
            }
            rows.push(("took down:", named));
        }
        if let Some(fixes) = &f.fixes {
            fix_rows(&mut rows, fixes, &addr);
        }
        if !block.kept.is_empty() {
            let mut groups: Vec<String> = block
                .kept
                .iter()
                .take(KEPT_GROUPS_SHOWN)
                .map(|(name, n)| match n {
                    1 if !name.ends_with('/') => name.clone(),
                    1 => format!("{name} (1 file)"),
                    n => format!("{name} ({n} files)"),
                })
                .collect();
            let more = block.kept.len().saturating_sub(KEPT_GROUPS_SHOWN);
            if more > 0 {
                groups.push(format!("+{more} more"));
            }
            rows.push((
                "kept:",
                format!(
                    "{} · heph inspect failure {}",
                    groups.join(", "),
                    shell_quote(&addr)
                ),
            ));
        }
        match &f.sandbox {
            // The one path into heph's home a report shows: the user asked
            // for it with --keep-failed-sandboxes (I9).
            Some(SandboxFate::Kept(path)) => {
                rows.push(("sandbox kept:", path.display().to_string()));
            }
            Some(SandboxFate::LeftAbandoned) => rows.push((
                "sandbox left:",
                "the run was abandoned after its timeout".to_owned(),
            )),
            Some(SandboxFate::Removed | SandboxFate::RemovedMaterial) | None => {}
        }
        if f.log.as_ref().and_then(|l| l.path()).is_some() {
            let lines = block
                .tail
                .as_ref()
                .map(|(text, start)| start.saturating_sub(1) + text.lines().count())
                .unwrap_or(0);
            rows.push((
                "full log:",
                format!(
                    "heph log {} ({} {})",
                    shell_quote(&addr),
                    thousands(lines),
                    if lines == 1 { "line" } else { "lines" }
                ),
            ));
        }

        let head = match &status {
            Some(s) => format!("{cross} {addr} · {}\n", clean_inline(s)),
            None => format!("{cross} {addr}\n"),
        };
        out.push_str(&clean_inline(head.trim_end()));
        out.push('\n');
        let gutter = |s: &'static str| {
            if color {
                format!("{}", s.dim())
            } else {
                s.to_owned()
            }
        };
        for line in &body {
            out.push_str(&format!("  {} {line}\n", gutter("│")));
        }
        let pad = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        let last = rows.len().saturating_sub(1);
        for (i, (label, value)) in rows.iter().enumerate() {
            let mark = if i == last { "╰" } else { "├" };
            let value = clean_inline(value);
            let value = if *label == "sandbox kept:" {
                value
            } else {
                std::borrow::Cow::Owned(self.redact.apply(&value).into_owned())
            };
            out.push_str(&format!(
                "  {} {label:<pad$} {value}\n",
                gutter(mark),
                pad = pad
            ));
        }
    }

    /// One `│` row: tabs expanded, cut to the width past its 4-column gutter.
    fn cut_row(&self, line: &str) -> String {
        cut_right(&expand_tabs(line), self.width.saturating_sub(4).max(1)).into_owned()
    }
}

/// The `fixes written:` / `restore:` or `fixes not written:` rows.
fn fix_rows(rows: &mut Vec<(&'static str, String)>, fixes: &FailureFixes, addr: &str) {
    match fixes {
        FailureFixes::Written { paths, .. } if paths.is_empty() => {}
        FailureFixes::Written { root, paths, list } => {
            let mut named = paths
                .iter()
                .take(FIXES_SHOWN)
                .map(|p| clean_inline(p).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            let more = paths.len().saturating_sub(FIXES_SHOWN);
            if more > 0 {
                named.push_str(&format!(" +{more} more"));
            }
            named.push_str(" · the target still failed; re-run to verify");
            rows.push(("fixes written:", named));
            let git = format!(
                "git -C {} --literal-pathspecs checkout",
                shell_quote(&root.to_string_lossy())
            );
            // A path a terminal cannot show as typed (a control byte) or a
            // list too long to name goes through the slot's list, which
            // `heph inspect failure` names: never the slot's path itself.
            let unprintable = paths.iter().any(|p| p.chars().any(char::is_control));
            let restore = if list.is_some() && (more > 0 || unprintable) {
                format!(
                    "{git} --pathspec-from-file=<the fixes file heph inspect failure {} names>",
                    shell_quote(addr)
                )
            } else {
                let quoted: Vec<_> = paths.iter().map(|p| shell_quote(p)).collect();
                format!("{git} -- {}", quoted.join(" "))
            };
            rows.push(("restore:", restore));
        }
        FailureFixes::NotWritten(reason) => {
            rows.push(("fixes not written:", not_written_reason(reason)));
        }
    }
}

/// The log's tail as the block shows it: the open slot copy first, the tail
/// in memory when there is none or it cannot be read.
fn read_tail(f: &TargetFailure) -> Option<(String, usize)> {
    let tail = match &f.log {
        Some(log) => log.tail().ok().or_else(|| f.log_tail.clone()),
        None => f.log_tail.clone(),
    }?;
    Some((tail.text, tail.start_line))
}

/// The files under `out` (the slot's kept `failure_out`, at the target's
/// package), grouped by first path component: `report/` with its file count, or a top-level file on
/// its own. Sorted by name. An unreadable directory reads as empty.
fn kept_groups(out: &Path) -> Vec<(String, usize)> {
    let mut groups: HashMap<String, usize> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(out) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let n = count_files(&entry.path());
            if n > 0 {
                *groups.entry(format!("{name}/")).or_default() += n;
            }
        } else {
            *groups.entry(name).or_default() += 1;
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort();
    groups
}

/// Files (anything not a directory) under `dir`, recursively; symlinks are
/// counted, not followed.
fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(_) => n += 1,
                Err(_) => {}
            }
        }
    }
    n
}

/// `n` with `,` between each group of three digits: `1,204`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::error::{FailureLog, FixesNotWritten, LogTail, ProcessFailed};
    use crate::htaddr::parse_addr;

    const HOME: &str = "/home/u/.heph";

    /// A failure that exited 101, its log kept in a slot whose file holds
    /// `lines`, of which the report shows the last `cap` (`--log-lines`).
    fn exited(dir: &Path, addr: &str, lines: &[&str], cap: usize) -> TargetFailure {
        let path = dir.join(format!("{}.log", addr.replace(['/', ':'], "_")));
        std::fs::write(
            &path,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .expect("write log");
        let log = FailureLog::open(path.clone(), std::fs::File::open(&path).expect("open"), cap);
        let source = anyhow::Error::new(ProcessFailed::new("exit status: 101", None))
            .context(format!("execute {addr}"));
        TargetFailure::new(parse_addr(addr).expect("addr"), None, source).with_log(Some(log))
    }

    fn report(failures: Vec<TargetFailure>, took: &[(&str, &[&str], usize)]) -> String {
        let failures: Vec<_> = failures.into_iter().map(Arc::new).collect();
        ReportContext::gather(
            &failures,
            |root| {
                took.iter()
                    .find(|(r, ..)| *r == root)
                    .map(|(_, named, total)| TookDown {
                        named: named.iter().map(|s| (*s).to_owned()).collect(),
                        total: *total,
                    })
                    .unwrap_or_default()
            },
            |_| false,
            None,
        )
        .with_width(60)
        .with_redact(Redact::of([HOME.into()]))
        .render(false)
    }

    /// One root with what it took down and a capped tail, then a second
    /// root with no process behind it: two blocks, in failure order; the
    /// tail is the last `--log-lines`, the took-down list and the heph log
    /// command close the first, and no path is printed.
    #[test]
    fn failure_report_groups_by_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = exited(
            dir.path(),
            "//crates/engine:test",
            &[
                "compiling",
                "running 214 tests",
                "---- stale stdout ----",
                "panicked",
                "FAILED",
            ],
            3,
        );
        let second = TargetFailure::new(
            parse_addr("//web:gen").expect("addr"),
            None,
            anyhow::anyhow!("staging {HOME}/sandbox/ab/in: permission denied")
                .context("execute //web:gen"),
        );
        let out = report(
            vec![first, second],
            &[(
                "//crates/engine:test",
                &["//e2e:shard-1", "//e2e:shard-2"],
                2,
            )],
        );
        assert_eq!(
            out,
            "\n✗ //crates/engine:test · exit status: 101\
             \n  │ ---- stale stdout ----\
             \n  │ panicked\
             \n  │ FAILED\
             \n  ├ took down: //e2e:shard-1, //e2e:shard-2\
             \n  ╰ full log:  heph log //crates/engine:test (5 lines)\
             \n\
             \n✗ //web:gen\
             \n  │ staging <heph home>/sandbox/ab/in: permission denied\
             \n"
        );
        assert!(!out.contains(&*dir.path().to_string_lossy()), "{out}");

        // A tail shorter than the cap shows whole; a root that took down
        // more than it names says how many more.
        let short = exited(dir.path(), "//p:short", &["only line"], 3);
        let out = report(
            vec![short],
            &[(
                "//p:short",
                &["//a:1", "//a:2", "//a:3", "//a:4", "//a:5"],
                7,
            )],
        );
        assert!(out.contains("  │ only line\n"), "{out}");
        assert!(out.contains("//a:5 +2 more\n"), "{out}");
        assert!(out.contains("heph log //p:short (1 line)"), "{out}");
    }

    /// I13 for the report tail: each row of the log-text adversarial table
    /// leaves no control byte in the report, CR keeps the last segment,
    /// tabs expand, invalid UTF-8 shows U+FFFD, a long line is cut to the
    /// width by display width, and an empty log says so.
    #[test]
    fn control_bytes_never_reach_terminal_report_tail() {
        let long = "é".repeat(1 << 20);
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("empty", b"".to_vec(), "│ empty log"),
            ("blank lines", b"\n\n\n".to_vec(), "│ empty log"),
            ("no newline", b"last".to_vec(), "│ last"),
            ("crlf", b"a\r\nb\r\n".to_vec(), "│ a\n  │ b"),
            ("cr progress", b"10%\r20%\r30%\n".to_vec(), "│ 30%"),
            (
                "alt screen",
                b"x\x1b[?1049l\x1b[?1003h\x1b[2Jy\n".to_vec(),
                "│ xy",
            ),
            (
                "osc",
                b"\x1b]8;;http://e\x07link\x1b]8;;\x07 \x1b]52;c;aGk=\x07z\n".to_vec(),
                "│ link z",
            ),
            ("lone csi", "a\u{9b}b\n".as_bytes().to_vec(), "│ a"),
            ("invalid utf8", b"a\xffb\n".to_vec(), "│ a\u{fffd}b"),
            ("nul", b"a\0b\n".to_vec(), "│ ab"),
            ("tabs", b"a\tb\n".to_vec(), "│ a       b"),
            ("long", long.as_bytes().to_vec(), "…"),
            ("cut mid utf8", b"\xc3".to_vec(), "│ \u{fffd}"),
            (
                "wide",
                "日本語\u{301}\n".as_bytes().to_vec(),
                "│ 日本語\u{301}",
            ),
        ];
        for (name, bytes, want) in cases {
            // The slot's tail is read as text, lossily: the way a log's
            // bytes reach the report.
            let tail = LogTail {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                start_line: 1,
            };
            let f = TargetFailure::new(
                parse_addr("//p:t").expect("addr"),
                Some(tail),
                anyhow::Error::new(ProcessFailed::new("exit status: 1", None)),
            );
            let out = report(vec![f], &[]);
            assert!(
                !out.chars().any(|c| c.is_control() && c != '\n'),
                "{name}: {out:?}"
            );
            assert!(out.contains(want), "{name}: {out:?}");
            for line in out.lines() {
                assert!(crate::tui::text::width(line) <= 60, "{name}: {line:?}");
            }
        }
    }

    /// I9 for the report: heph's home is redacted from error text and log
    /// lines, a removed sandbox says nothing, and the one path it shows is
    /// the sandbox --keep-failed-sandboxes kept.
    #[test]
    fn no_home_path_on_any_screen_report() {
        let tail = LogTail {
            text: format!("error: {HOME}/sandbox/p/t/src/main.rs:3: bad\n"),
            start_line: 1,
        };
        let removed = TargetFailure::new(
            parse_addr("//p:t").expect("addr"),
            Some(tail.clone()),
            anyhow::Error::new(ProcessFailed::new("exit status: 1", None)),
        )
        .with_sandbox(Some(SandboxFate::Removed));
        let out = report(vec![removed], &[]);
        assert!(!out.contains(HOME), "{out}");
        assert!(out.contains("<heph home>/sandbox/p/t/src/main.rs"), "{out}");
        assert!(
            !out.contains("sandbox kept") && !out.contains("sandbox left"),
            "{out}"
        );

        let kept_at = format!("{HOME}/sandbox/p/__target_t");
        let kept = TargetFailure::new(
            parse_addr("//p:t").expect("addr"),
            Some(tail),
            anyhow::Error::new(ProcessFailed::new("exit status: 1", None)),
        )
        .with_sandbox(Some(SandboxFate::Kept(kept_at.clone().into())));
        let out = report(vec![kept], &[]);
        assert!(
            out.contains(&format!("╰ sandbox kept: {kept_at}\n")),
            "{out}"
        );
        assert_eq!(out.matches(HOME).count(), 1, "{out}");
    }

    /// A timeout names its limit instead of an exit status; an abandoned one
    /// says the run may still be running and that its sandbox was left.
    #[test]
    fn failure_report_abandoned_timeout() {
        let t = crate::engine::run_timeout::TimedOutError {
            addr: parse_addr("//e2e:it").expect("addr"),
            timeout: hplugin::driver::Timeout::new(std::time::Duration::from_secs(600))
                .expect("timeout"),
            abandoned_after: Some(std::time::Duration::from_secs(10)),
        };
        let f = TargetFailure::new(
            parse_addr("//e2e:it").expect("addr"),
            None,
            anyhow::Error::new(t),
        )
        .with_sandbox(Some(SandboxFate::LeftAbandoned));
        let out = report(vec![f], &[]);
        assert!(
            out.starts_with("\n✗ //e2e:it · timed out after 10m (its timeout)\n"),
            "{out}"
        );
        assert!(
            out.contains("├ abandoned:    the run did not stop within 10s"),
            "{out}"
        );
        assert!(
            out.contains("╰ sandbox left: the run was abandoned after its timeout\n"),
            "{out}"
        );
    }

    /// Fixes written back: named up to ten with a restore command that
    /// quotes every path; past ten, `+N more` and the restore goes through
    /// the list `heph inspect failure` names. Nothing written says why.
    #[test]
    fn failure_report_fixes() {
        let fixes = |paths: Vec<String>, list: bool| {
            TargetFailure::new(
                parse_addr("//web:lint").expect("addr"),
                None,
                anyhow::Error::new(ProcessFailed::new("exit status: 1", None)),
            )
            .with_fixes(Some(FailureFixes::Written {
                root: "/src/acme".into(),
                paths,
                list: list.then(|| format!("{HOME}/failures/web_lint/fixes.txt").into()),
            }))
        };
        let out = report(
            vec![fixes(vec!["web/a b.ts".into(), "web/é.ts".into()], true)],
            &[],
        );
        assert!(
            out.contains("├ fixes written: web/a b.ts, web/é.ts · the target still failed; re-run to verify\n"),
            "{out}"
        );
        assert!(
            out.contains("╰ restore:       git -C /src/acme --literal-pathspecs checkout -- 'web/a b.ts' 'web/é.ts'\n"),
            "{out}"
        );

        let ten: Vec<String> = (0..10).map(|i| format!("f{i}")).collect();
        let out = report(vec![fixes(ten, true)], &[]);
        assert!(out.contains("f9 · the target"), "{out}");
        assert!(!out.contains("more"), "{out}");
        assert!(out.contains("checkout -- f0 f1"), "{out}");

        let eleven: Vec<String> = (0..11).map(|i| format!("f{i}")).collect();
        let out = report(vec![fixes(eleven, true)], &[]);
        assert!(out.contains("f9 +1 more · the target"), "{out}");
        assert!(
            out.contains(
                "--pathspec-from-file=<the fixes file heph inspect failure //web:lint names>"
            ),
            "{out}"
        );
        assert!(!out.contains("fixes.txt") && !out.contains(HOME), "{out}");

        let killed = TargetFailure::new(
            parse_addr("//web:lint").expect("addr"),
            None,
            anyhow::Error::new(ProcessFailed::new("signal: 9 (SIGKILL)", None)),
        )
        .with_fixes(Some(FailureFixes::NotWritten(FixesNotWritten::Signal(9))));
        let out = report(vec![killed], &[]);
        assert!(
            out.contains("╰ fixes not written: killed by signal 9\n"),
            "{out}"
        );
    }
}
