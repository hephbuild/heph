//! End-of-execution error rendering. Failing targets are collected in the
//! per-request registry as [`TargetFailure`]s and rendered here with a small
//! hand-rolled format: a `×` title, a `╰─▶` cause line, and (when present) the
//! process log tail in a framed `log` box.

use std::io::IsTerminal;
use std::sync::Arc;

use crossterm::style::Stylize;

use crate::engine::error::{CancelledError, TargetFailure};
use crate::hmemoizer::downcast_chain_ref;

/// Whether to emit ANSI color: only when stderr is a terminal and `NO_COLOR`
/// (https://no-color.org) is unset.
fn color_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

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
/// `color`: border + `[log]` white, line numbers dim.
/// ```text
///   ╭─▶[log]
///   1 │  line one
///   2 │  line two
///   ╰────
/// ```
fn render_log_box(out: &mut String, log: &str, color: bool) {
    let lines: Vec<&str> = log.lines().collect();
    let width = lines.len().to_string().len();
    // The line-number gutter sits OUTSIDE the box; the box corners/border line up
    // one column past it (number field + the separating space).
    let pad = " ".repeat(width + 1);

    if color {
        out.push_str(&format!("  {pad}{}\n", "╭─[log]".white()));
    } else {
        out.push_str(&format!("  {pad}╭─[log]\n"));
    }
    for (i, line) in lines.iter().enumerate() {
        let num = format!("{:>width$}", i + 1, width = width);
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

/// Render a single target failure to a string. The `×` and `╰─▶` markers are red
/// when `color`.
fn render_target_failure(f: &TargetFailure, color: bool) -> String {
    let cross = if color {
        format!("{}", "×".red())
    } else {
        "×".to_string()
    };
    let mut out = format!("{cross} target failed: {}\n", f.addr.format());
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
        render_log_box(&mut out, log, color);
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
/// success output), it resolves them with the TUI paused:
///
/// - registry non-empty → render the rich boxes and exit with silent
///   [`AlreadyRendered`]; the returned `res` is a collateral marker, dropped;
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
        let printed: ::anyhow::Result<()> = $crate::tui::paused!($ctx, {
            if !failures.is_empty() {
                $crate::commands::errors::render_failures(&failures);
                Ok(())
            } else {
                match res {
                    Ok($val) => $body,
                    // A cancellation is surfaced by `finish_exit`; any other error
                    // is a genuine non-registry failure → `render_anyhow`.
                    Err(e) if !cancelled => Err(e),
                    Err(_) => Ok(()),
                }
            }
        });
        printed?;
        $crate::commands::errors::finish_exit(failures.is_empty(), cancelled)
    }};
}
pub(crate) use finalize;

/// Map the (no-failures, cancelled) state to an exit result. Failures → silent
/// [`AlreadyRendered`]; else cancellation → `cancelled`; else `Ok(())`.
pub(crate) fn finish_exit(no_failures: bool, cancelled: bool) -> anyhow::Result<()> {
    if !no_failures {
        return Err(AlreadyRendered.into());
    }
    if cancelled {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// A failure that has already been rendered (e.g. the per-target diagnostics
/// printed by `render_failures`). Carried up only to set a non-zero exit code —
/// `render_anyhow` swallows it so nothing extra is printed.
#[derive(Debug)]
pub struct AlreadyRendered;

impl std::fmt::Display for AlreadyRendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build failed")
    }
}

impl std::error::Error for AlreadyRendered {}

/// Render an `anyhow::Error` to stderr. If a [`TargetFailure`] is in the chain
/// it gets the rich box treatment; otherwise the error's cause chain is printed
/// in the same `×` / `╰─▶` style. Always renders something, so callers need no
/// fallback.
pub fn render_anyhow(e: &anyhow::Error) -> bool {
    // Already-rendered failures carry no extra output — swallow them.
    if downcast_chain_ref::<AlreadyRendered>(e).is_some() {
        return true;
    }
    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        eprint!("{}", render_target_failure(tf, color_enabled()));
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
    use crate::engine::error::ProcessFailed;
    use anyhow::Context as _;

    #[test]
    fn is_cancelled_detects_cancellation_in_chain() {
        use crate::engine::error::CancelledError;
        let cancelled = anyhow::Error::new(CancelledError).context("running //pkg:a");
        assert!(is_cancelled(&cancelled));
        let other = anyhow::anyhow!("boom").context("running //pkg:a");
        assert!(!is_cancelled(&other));
    }

    #[test]
    fn finish_exit_maps_state_to_exit() {
        // Recorded failures → silent AlreadyRendered (boxes already printed),
        // and take priority over cancellation.
        let e = finish_exit(false, false).unwrap_err();
        assert!(downcast_chain_ref::<AlreadyRendered>(&e).is_some());
        let e = finish_exit(false, true).unwrap_err();
        assert!(downcast_chain_ref::<AlreadyRendered>(&e).is_some());

        // No failures + cancellation → `cancelled`.
        let e = finish_exit(true, true).unwrap_err();
        assert_eq!(e.to_string(), "cancelled");

        // No failures, not cancelled → Ok.
        assert!(finish_exit(true, false).is_ok());
    }

    #[test]
    fn already_rendered_is_swallowed_but_handled() {
        // The marker counts as handled (no fallback log in main.rs) and prints
        // nothing extra — the diagnostics were already shown by render_failures.
        let e = anyhow::Error::new(AlreadyRendered).context("3 target(s) failed");
        assert!(render_anyhow(&e));
    }

    #[test]
    fn renders_target_failure_with_log_box() {
        let addr = crate::htaddr::parse_addr("//simple_fail:d1").unwrap();
        let source = anyhow::Error::new(ProcessFailed {
            status: "exit status: 1".to_string(),
            log_tail: String::new(),
        })
        .context("driver run")
        .context("run")
        .context("execute //simple_fail:d1");
        let log = "stuff\nstuff\nstuff\nstuff\nstuff\nstuff\nstuff\nstuff\nnot gucci";
        let f = TargetFailure::new(addr, Some(log.to_string()), source);

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
    fn color_styles_markers_red_border_white_numbers_dim() {
        let addr = crate::htaddr::parse_addr("//pkg:a").unwrap();
        let f = TargetFailure::new(addr, Some("oops".to_string()), anyhow::anyhow!("boom"));
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
