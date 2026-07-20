//! End-of-execution error rendering. Failing targets are collected in the
//! per-request registry as [`TargetFailure`]s and rendered here with a small
//! hand-rolled format: a `×` title, a `╰─▶` cause line, and (when present) the
//! process log tail in a framed `log` box.

use std::sync::Arc;

use crossterm::style::Stylize;

use crate::engine::error::{CancelledError, FrozenCheckError, TargetFailure};
use crate::hmemoizer::downcast_chain_ref;
use crate::tui::color::stderr_color_enabled as color_enabled;

/// Render the cause chain of an `anyhow::Error`, dropping the leading frames that
/// merely restate the target address (engine boundaries like `execute //addr`,
/// already shown in the title) and joining the rest the way anyhow's `{:#}`
/// would. If every frame mentions the addr (e.g. a bare `target not found`), the
/// whole chain is kept.
fn cause_chain(source: &anyhow::Error, addr: &str) -> String {
    let frames: Vec<String> = source.chain().map(|c| c.to_string()).collect();
    let start = frames.iter().position(|f| !f.contains(addr)).unwrap_or(0);
    frames.get(start..).unwrap_or(&[]).join(": ")
}

/// Render the log tail inside a framed box, indented two spaces. The `▶` of the
/// header sits in the same column as the `│` gutter bar so they connect. With
/// `color`: border + `[log]` white, line numbers dim. Line numbers start at
/// `start_line` — the real position of the first shown line in the full log — so
/// the last 10 lines of a 100-line log read 91–100, not 1–10.
/// ```text
///   ╭─▶[log]
///   91 │  line one
///   92 │  line two
///   ╰────
/// ```
fn render_log_box(out: &mut String, log: &str, start_line: usize, color: bool) {
    let lines: Vec<&str> = log.lines().collect();
    // Width is driven by the largest (last) line number, not the line count.
    let last_no = start_line + lines.len().saturating_sub(1);
    let width = last_no.to_string().len();
    // The line-number gutter sits OUTSIDE the box; the box corners/border line up
    // one column past it (number field + the separating space).
    let pad = " ".repeat(width + 1);

    if color {
        out.push_str(&format!("  {pad}{}\n", "╭─[log]".white()));
    } else {
        out.push_str(&format!("  {pad}╭─[log]\n"));
    }
    for (i, line) in lines.iter().enumerate() {
        let num = format!("{:>width$}", start_line + i, width = width);
        if color {
            out.push_str(&format!("  {} {} {line}\n", num.dim(), "│".white()));
        } else {
            out.push_str(&format!("  {num} │ {line}\n"));
        }
    }
    if color {
        out.push_str(&format!("  {pad}{}\n", "╰────".white()));
    } else {
        out.push_str(&format!("  {pad}╰────\n"));
    }
}

/// Render a unified diff inside a framed box, indented two spaces. With `color`,
/// addition lines (`+`) are green and deletion lines (`-`) are red; the box
/// border + `[diff]` header are white. Hunk/context lines pass through unstyled.
/// ```text
///   ╭─[diff]
///   │ --- tree
///   │ +++ generated
///   │ -old line
///   │ +new line
///   ╰────
/// ```
fn render_diff_box(out: &mut String, diff: &str, color: bool) {
    if color {
        out.push_str(&format!("  {}\n", "╭─[diff]".white()));
    } else {
        out.push_str("  ╭─[diff]\n");
    }
    for line in diff.lines() {
        // A leading `+`/`-` marks an addition/deletion; `+++`/`---` file headers
        // are left unstyled so the green/red is reserved for content lines.
        let is_header = line.starts_with("+++") || line.starts_with("---");
        let styled = if !color || is_header {
            line.to_string()
        } else if line.starts_with('+') {
            format!("{}", line.green())
        } else if line.starts_with('-') {
            format!("{}", line.red())
        } else {
            line.to_string()
        };
        if color {
            out.push_str(&format!("  {} {styled}\n", "│".white()));
        } else {
            out.push_str(&format!("  │ {styled}\n"));
        }
    }
    if color {
        out.push_str(&format!("  {}\n", "╰────".white()));
    } else {
        out.push_str("  ╰────\n");
    }
}

/// Render a single target failure to a string. The `×` and `╰─▶` markers are red
/// when `color`.
fn render_target_failure(f: &TargetFailure, color: bool) -> String {
    let cross = if color {
        format!("{}", "×".red())
    } else {
        "×".to_string()
    };
    let mut out = format!("{cross} target failed: {}\n", f.addr.format());
    // A frozen-check failure carries its diff in a dedicated box; render a clean
    // one-line cause (the addr is already in the title) plus the framed diff,
    // instead of dumping the whole multi-line diff into the inline cause chain.
    if let Some(fc) = downcast_chain_ref::<FrozenCheckError>(&f.source) {
        let arrow = if color {
            format!("{}", "╰─▶".red())
        } else {
            "╰─▶".to_string()
        };
        out.push_str(&format!("{arrow} generated output differs from tree\n"));
        render_diff_box(&mut out, &fc.diff, color);
        return out;
    }
    let cause = cause_chain(&f.source, &f.addr.format());
    if !cause.is_empty() {
        let arrow = if color {
            format!("{}", "╰─▶".red())
        } else {
            "╰─▶".to_string()
        };
        out.push_str(&format!("{arrow} {cause}\n"));
    }
    if let Some(log) = &f.log_tail {
        render_log_box(&mut out, &log.text, log.start_line, color);
    }
    out
}

/// Render each recorded target failure to stderr, separated by a blank line.
pub fn render_failures(failures: &[Arc<TargetFailure>]) {
    let color = color_enabled();
    for (i, f) in failures.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }
        eprint!("{}", render_target_failure(f, color));
    }
}

/// True when the error chain carries a Ctrl-C cancellation. A cancellation is
/// never recorded as a [`TargetFailure`]; it aborts the build but is not a target
/// fault, so commands surface it separately from the failure registry.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    downcast_chain_ref::<CancelledError>(e).is_some()
}

/// Paved-road command finalizer. The single construct every command ends with.
///
/// One place owns the awkward truth that a command's *outcome* lives in two
/// channels: the per-request failure registry (rich, deduped `TargetFailure`s,
/// recorded when a provider/driver runs a target) and the returned `res`. Given
/// `ctx`, the request state `rs`, an engine `res`, and `$val => $body` (the
/// success output), it resolves them (pausing the TUI only for the success/error
/// output that prints inline):
///
/// - registry non-empty → carry the failures out as [`PendingFailures`], to be
///   rendered by `render_anyhow` *after* the viewport is torn down; the returned
///   `res` is a collateral marker, dropped;
/// - registry empty, `res` Ok → bind the value as `$val` and run `$body` (may use
///   `?`), then exit `Ok`;
/// - registry empty, `res` Err → a cancellation exits `cancelled`; any other error
///   is a genuine non-registry failure propagated to `render_anyhow`.
///
/// `$body` must evaluate to `anyhow::Result<()>`. Commands whose outcome isn't a
/// single `Result` (e.g. `run`'s fail-fast batch) fold it into one `res` before
/// calling — a cancellation among batch errors becomes `Err(CancelledError)`.
///
/// The `$val => $body` clause is optional: omit it for commands that print
/// incrementally and need no end-of-run output (the success value is discarded).
macro_rules! finalize {
    ($ctx:expr, $rs:expr, $res:expr $(,)?) => {
        $crate::commands::errors::finalize!($ctx, $rs, $res, _ => { Ok(()) })
    };
    ($ctx:expr, $rs:expr, $res:expr, $val:pat => $body:block) => {{
        let res = $res;
        let cancelled = res
            .as_ref()
            .err()
            .is_some_and($crate::commands::errors::is_cancelled);
        let failures = $rs.take_failures();
        if !failures.is_empty() {
            // Do NOT render here: mid-run the TUI is only *paused*, so the resume
            // re-anchor and the final viewport collapse would wipe the boxes off
            // screen. Carry the failures out; `render_anyhow` in `main` prints
            // them once the viewport is fully torn down. `res` is a collateral
            // marker, dropped.
            ::core::result::Result::<(), ::anyhow::Error>::Err(
                $crate::commands::errors::PendingFailures(failures).into(),
            )
        } else {
            let printed: ::anyhow::Result<()> = $crate::tui::paused!($ctx, {
                match res {
                    Ok($val) => $body,
                    // A cancellation is surfaced by `finish_exit`; any other error
                    // is a genuine non-registry failure → `render_anyhow`.
                    Err(e) if !cancelled => Err(e),
                    Err(_) => Ok(()),
                }
            });
            printed?;
            $crate::commands::errors::finish_exit(cancelled)
        }
    }};
}
pub(crate) use finalize;

/// Map the cancelled flag to an exit result for the no-failures path. Registry
/// failures never reach here — they are carried out as [`PendingFailures`] and
/// rendered by `render_anyhow`.
pub(crate) fn finish_exit(cancelled: bool) -> anyhow::Result<()> {
    if cancelled {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// Per-target failures collected in the request registry, carried up so
/// [`render_anyhow`] can render their rich boxes *after* the interactive viewport
/// is fully torn down. Rendering them mid-run (while the TUI is only paused) let
/// the resume re-anchor and the final viewport collapse wipe the boxes off
/// screen; deferring to `main`'s post-teardown `render_anyhow` prints them on a
/// terminal nothing repaints over. Carrying the failures — rather than a bare
/// "already rendered" marker — is what lets that late render reproduce the boxes.
#[derive(Debug)]
pub struct PendingFailures(pub Vec<Arc<TargetFailure>>);

impl std::fmt::Display for PendingFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build failed")
    }
}

impl std::error::Error for PendingFailures {}

/// Render an `anyhow::Error` to stderr. If a [`TargetFailure`] is in the chain
/// it gets the rich box treatment; otherwise the error's cause chain is printed
/// in the same `×` / `╰─▶` style. Always renders something, so callers need no
/// fallback.
pub fn render_anyhow(e: &anyhow::Error) -> bool {
    // Registry failures deferred from `finalize!`: render their rich boxes now
    // that the viewport is torn down and nothing repaints over stderr.
    if let Some(pf) = downcast_chain_ref::<PendingFailures>(e) {
        render_failures(&pf.0);
        return true;
    }
    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        eprint!("{}", render_target_failure(tf, color_enabled()));
        return true;
    }
    // A frozen-check failure that arrived outside a `TargetFailure` still gets the
    // framed diff treatment so CI output is legible.
    if let Some(fc) = downcast_chain_ref::<FrozenCheckError>(e) {
        let color = color_enabled();
        let cross = if color {
            format!("{}", "×".red())
        } else {
            "×".to_string()
        };
        let mut out = format!("{cross} frozen check failed: {}\n", fc.addr.format());
        render_diff_box(&mut out, &fc.diff, color);
        eprint!("{out}");
        return true;
    }
    let frames: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let Some(top) = frames.first() else {
        return false;
    };
    let mut out = format!("× {top}\n");
    let rest = frames.get(1..).unwrap_or(&[]);
    if !rest.is_empty() {
        out.push_str(&format!("╰─▶ {}\n", rest.join(": ")));
    }
    eprint!("{out}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::error::{LogTail, ProcessFailed};
    use anyhow::Context as _;
    use std::sync::Arc;

    /// A `ProcessFailed` whose log handle is never read (tests that only exercise
    /// the cause chain / rendering, not log extraction).
    fn dummy_process_failed() -> ProcessFailed {
        ProcessFailed {
            status: "exit status: 1".to_string(),
            log: Arc::new(hcore::hartifactcontent::FileContent::new("/dev/null")),
        }
    }

    #[test]
    fn is_cancelled_detects_cancellation_in_chain() {
        use crate::engine::error::CancelledError;
        let cancelled = anyhow::Error::new(CancelledError).context("running //pkg:a");
        assert!(is_cancelled(&cancelled));
        let other = anyhow::anyhow!("boom").context("running //pkg:a");
        assert!(!is_cancelled(&other));
    }

    #[test]
    fn finish_exit_maps_cancelled_to_exit() {
        // Cancellation (no registry failures) → `cancelled`.
        let e = finish_exit(true).unwrap_err();
        assert_eq!(e.to_string(), "cancelled");

        // Not cancelled → Ok.
        assert!(finish_exit(false).is_ok());
    }

    #[test]
    fn render_anyhow_renders_deferred_pending_failures() {
        // Regression: in interactive TUI mode the registry failures used to be
        // printed mid-run inside a `paused!` block, where the resume re-anchor +
        // final viewport collapse wiped them. `finalize!` now carries them out as
        // `PendingFailures`; `render_anyhow` (called post-teardown in `main`) must
        // reproduce the rich boxes for every carried failure and claim the error
        // as handled so nothing extra is logged.
        let a = crate::htaddr::parse_addr("//simple_fail:d1").unwrap();
        let b = crate::htaddr::parse_addr("//simple_fail:d2").unwrap();
        let fa = TargetFailure::new(a, None, anyhow::anyhow!("boom a"));
        let fb = TargetFailure::new(b, None, anyhow::anyhow!("boom b"));
        let e = anyhow::Error::new(PendingFailures(vec![Arc::new(fa), Arc::new(fb)]))
            .context("2 target(s) failed");
        assert!(render_anyhow(&e));
        // The carried failures survive the wrapping context so the late render can
        // reach them (they would be lost if `finalize!` had only kept a marker).
        let pf = downcast_chain_ref::<PendingFailures>(&e).expect("carried failures");
        assert_eq!(pf.0.len(), 2);
    }

    #[test]
    fn renders_target_failure_with_log_box() {
        let addr = crate::htaddr::parse_addr("//simple_fail:d1").unwrap();
        let source = anyhow::Error::new(dummy_process_failed())
            .context("driver run")
            .context("run")
            .context("execute //simple_fail:d1");
        let log = "stuff\nstuff\nstuff\nstuff\nstuff\nstuff\nstuff\nstuff\nnot gucci";
        let f = TargetFailure::new(
            addr,
            Some(LogTail {
                text: log.to_string(),
                start_line: 1,
            }),
            source,
        );

        let rendered = render_target_failure(&f, false);
        let expected = "\
× target failed: //simple_fail:d1
╰─▶ run: driver run: process exited with status: exit status: 1
    ╭─[log]
  1 │ stuff
  2 │ stuff
  3 │ stuff
  4 │ stuff
  5 │ stuff
  6 │ stuff
  7 │ stuff
  8 │ stuff
  9 │ not gucci
    ╰────
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn log_box_numbers_reflect_real_file_positions() {
        // The last 3 lines of a 100-line log render with numbers 98–100, not 1–3,
        // and the gutter widens to fit the largest number.
        let addr = crate::htaddr::parse_addr("//simple_fail:d1").unwrap();
        let f = TargetFailure::new(
            addr,
            Some(LogTail {
                text: "line98\nline99\nboom".to_string(),
                start_line: 98,
            }),
            anyhow::anyhow!("boom"),
        );
        let rendered = render_target_failure(&f, false);
        let expected = "\
× target failed: //simple_fail:d1
╰─▶ boom
      ╭─[log]
   98 │ line98
   99 │ line99
  100 │ boom
      ╰────
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renders_target_failure_without_log() {
        let addr = crate::htaddr::parse_addr("//pkg:a").unwrap();
        let source = anyhow::anyhow!("target not found: //pkg:a");
        let f = TargetFailure::new(addr, None, source);
        // Single-frame cause: not skipped.
        assert_eq!(
            render_target_failure(&f, false),
            "× target failed: //pkg:a\n╰─▶ target not found: //pkg:a\n"
        );
    }

    #[test]
    fn renders_frozen_check_failure_with_diff_box() {
        // A FrozenCheckError carried inside a TargetFailure's source chain renders
        // a clean one-line cause plus the diff framed in a [diff] box — not the
        // raw multi-line Display dumped into the inline cause.
        let addr = crate::htaddr::parse_addr("//gen:proto").unwrap();
        let diff = "--- tree\n+++ generated\n-old line\n+new line\n";
        let source = anyhow::Error::new(FrozenCheckError {
            addr: addr.clone(),
            diff: diff.to_string(),
        })
        .context("execute //gen:proto");
        let f = TargetFailure::new(addr, None, source);

        let rendered = render_target_failure(&f, false);
        let expected = "\
× target failed: //gen:proto
╰─▶ generated output differs from tree
  ╭─[diff]
  │ --- tree
  │ +++ generated
  │ -old line
  │ +new line
  ╰────
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renders_bare_frozen_check_error() {
        // A FrozenCheckError surfaced directly (not wrapped in a TargetFailure)
        // still gets the framed diff treatment via render_anyhow.
        let addr = crate::htaddr::parse_addr("//gen:proto").unwrap();
        let diff = "-old\n+new\n";
        let e = anyhow::Error::new(FrozenCheckError {
            addr,
            diff: diff.to_string(),
        });
        // render_anyhow prints to stderr; assert it claims the error as handled.
        assert!(render_anyhow(&e));
    }

    #[test]
    fn frozen_diff_box_colors_additions_green_deletions_red() {
        let mut out = String::new();
        render_diff_box(&mut out, "-gone\n+added\n context\n", true);
        // Additions green, deletions red, context unstyled.
        assert!(out.contains(&format!("{}", "+added".green())));
        assert!(out.contains(&format!("{}", "-gone".red())));
        assert!(out.contains(" context"));
        // Border + header white.
        assert!(out.contains(&format!("{}", "╭─[diff]".white())));
        assert!(out.contains(&format!("{}", "╰────".white())));
    }

    #[test]
    fn color_styles_markers_red_border_white_numbers_dim() {
        let addr = crate::htaddr::parse_addr("//pkg:a").unwrap();
        let f = TargetFailure::new(
            addr,
            Some(LogTail {
                text: "oops".to_string(),
                start_line: 1,
            }),
            anyhow::anyhow!("boom"),
        );
        let rendered = render_target_failure(&f, true);
        // × and ╰─▶ markers red.
        assert!(rendered.contains(&format!("{}", "×".red())));
        assert!(rendered.contains(&format!("{}", "╰─▶".red())));
        // Border + [log] white, line numbers dim.
        assert!(rendered.contains(&format!("{}", "╭─[log]".white())));
        assert!(rendered.contains(&format!("{}", "╰────".white())));
        assert!(rendered.contains(&format!("{}", "│".white())));
        assert!(rendered.contains(&format!("{}", "1".dim())));
        // The log text itself stays unstyled.
        assert!(rendered.contains(" oops\n"));
    }
}
