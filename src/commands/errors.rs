//! End-of-execution error rendering. Failing targets are collected in the
//! per-request registry as [`TargetFailure`]s and rendered here with a small
//! hand-rolled format: a `×` title, a `╰─▶` cause line, and (when present) the
//! process log tail in a framed `log` box.

use std::io;
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

/// Render each recorded target failure, separated by a blank line. Pure, so the
/// exact bytes the user sees are assertable without capturing stderr.
fn render_failures_to_string(failures: &[Arc<TargetFailure>], color: bool) -> String {
    let mut out = String::new();
    for (i, f) in failures.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&render_target_failure(f, color));
    }
    out
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
/// - registry non-empty → exit with [`FailedTargets`]; interactive mode defers the
///   render to `render_anyhow` (after the viewport is torn down), non-interactive
///   mode renders here. The returned `res` is a collateral marker, dropped;
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
/// Fold a keep-going batch into the single `res` that [`finalize!`] expects.
///
/// `Engine::result` reports keep-going failures INSIDE its `Ok`
/// (`BatchResult::errors`); they are normally duplicated into the failure
/// registry, which `finalize!` renders. This fold must still exist: an error
/// class the registry declines to record (a request-property marker leaking
/// out of a conversion boundary) would otherwise be dropped on the floor —
/// the run prints its "err N" summary, then exits 0 in silence, which a
/// 308-failure lint run did. The registry branch still wins the rendering
/// whenever it has entries; this is the belt for when it does not.
///
/// Cancellation collapses to [`CancelledError`] only when nothing else
/// failed — a genuine failure must never be masked by the cancellations it
/// caused.
pub fn fold_batch(
    batch: crate::engine::BatchResult,
) -> anyhow::Result<Vec<Arc<crate::engine::EResult>>> {
    if batch.errors.is_empty() {
        return Ok(batch.ok);
    }
    let (cancelled, genuine): (Vec<_>, Vec<_>) =
        batch.errors.into_iter().partition(|(_, e)| is_cancelled(e));
    if genuine.is_empty() {
        drop(cancelled);
        return Err(CancelledError.into());
    }
    Err(crate::engine::error::MultiError(
        genuine
            .into_iter()
            .map(|(addr, e)| e.context(addr.format()))
            .collect(),
    )
    .into())
}

/// Reject a run whose selector chose nothing.
///
/// `heph run //pkg:gone` already exits 1 — the addr arm resolves the addr and
/// gets `TargetNotFound`. Only the *selector* arm was silent: a label no target
/// carries, an unbuilt variant, or a scope that misses the tree produced an
/// empty batch with no errors, which folded to `Ok` and exited 0. A run that
/// built nothing is indistinguishable from a run that had nothing to do, which
/// is how a CI job goes green having done no work.
///
/// Scoped to `run`. A `query` that matches nothing is a legitimate answer, and
/// `validate` over an empty scope has nothing to prove.
pub fn require_non_empty(
    results: Vec<Arc<crate::engine::EResult>>,
) -> anyhow::Result<Vec<Arc<crate::engine::EResult>>> {
    if results.is_empty() {
        anyhow::bail!(
            "no targets matched — nothing was run.\n\
             Check the selector with `heph query -e '<expr>'`; a label that no target \
             carries, or a package matcher outside the workspace, matches nothing."
        );
    }
    Ok(results)
}

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
            // `res` is a collateral marker here — dropped either way.
            if $ctx.interactive() {
                // Do NOT render here: mid-run the TUI is only *paused*, so the
                // resume re-anchor and the final viewport collapse would wipe the
                // boxes off screen. Carry the failures out; `render_anyhow` in
                // `main` prints them once the viewport is fully torn down.
                ::anyhow::Result::<()>::Err(
                    $crate::commands::errors::FailedTargets::deferred(failures).into_error(),
                )
            } else {
                // No viewport, so nothing ever repaints stderr and there is no
                // reason to defer. Print at the failure site: deferring to `main`
                // would push the diagnostics behind the backend's end-of-run
                // summary *and* its unbounded background-upload drain, invert the
                // errors-then-verdict order every other build tool prints, and lose
                // them entirely if a second ctrl-c aborts during that drain.
                // (Interactive has no such choice — teardown must come first, so it
                // necessarily prints verdict-then-errors.)
                ::anyhow::Result::<()>::Err(
                    $crate::commands::errors::FailedTargets::reported_to_stderr(failures)
                        .into_error(),
                )
            }
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
/// failures never reach here — they exit as [`FailedTargets`].
pub(crate) fn finish_exit(cancelled: bool) -> anyhow::Result<()> {
    if cancelled {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// The per-target failures collected in the request registry, carried up so the
/// process exits non-zero with its diagnostics on screen. The two constructors
/// differ only in *when* the boxes are printed:
///
/// - [`FailedTargets::deferred`] (interactive) — nothing is printed yet. Mid-run
///   the TUI is only *paused*, so a render there is re-anchored over by the resume
///   and then erased by the final viewport collapse. [`render_anyhow`], called
///   from `main` after full teardown, prints the boxes on a terminal nothing
///   repaints over.
/// - [`FailedTargets::reported`] (non-interactive) — the constructor itself does
///   the printing, at the failure site so the boxes precede the end-of-run summary
///   and the background-work drain. `render_anyhow` only claims the error.
///
/// Either way the failures ride *in* the error rather than behind an opaque
/// "already rendered" marker, so the late render can reproduce them and
/// downcasting callers (telemetry's failure classifier) still see a target
/// failure.
#[derive(Debug)]
pub struct FailedTargets {
    failures: Vec<Arc<TargetFailure>>,
    /// Already printed at the failure site; `render_anyhow` must not print twice.
    rendered: bool,
}

impl FailedTargets {
    /// Failures not yet printed — [`render_anyhow`] renders them.
    pub fn deferred(failures: Vec<Arc<TargetFailure>>) -> Self {
        Self {
            failures,
            rendered: false,
        }
    }

    /// Print the failures to `out` and record that it happened, so
    /// [`render_anyhow`] stays quiet later.
    ///
    /// Printing lives *in* the constructor on purpose: `rendered` is a promise
    /// about what is already on the terminal, and a caller that sets it without
    /// printing produces a non-zero exit with no diagnostics at all — the exact
    /// bug this module exists to prevent, and one no test of the marker can see.
    pub fn reported(
        failures: Vec<Arc<TargetFailure>>,
        out: &mut dyn io::Write,
        color: bool,
    ) -> Self {
        // A failed write to the terminal is not actionable, and the exit code still
        // has to carry the failure out.
        drop(write!(
            out,
            "{}",
            render_failures_to_string(&failures, color)
        ));
        Self {
            failures,
            rendered: true,
        }
    }

    /// [`FailedTargets::reported`] against the real stderr — what `finalize!` uses.
    pub fn reported_to_stderr(failures: Vec<Arc<TargetFailure>>) -> Self {
        Self::reported(failures, &mut io::stderr(), color_enabled())
    }

    pub fn failures(&self) -> &[Arc<TargetFailure>] {
        &self.failures
    }

    /// Build the `anyhow::Error` that carries these failures out of `finalize!`.
    ///
    /// A representative [`TargetFailure`] is the chain's root, with `FailedTargets`
    /// layered on top as context.
    ///
    /// The asymmetry that makes this necessary: `anyhow::Error::chain()` traverses
    /// `Error::source()`, but `downcast_ref` does *not* — it only walks the context
    /// layers anyhow itself built. So a failure reachable only through `source()`,
    /// or held in a struct field, is invisible to every caller that asks "did a
    /// target fail?" (telemetry's classifier), which would bucket the tool's most
    /// common failure as a generic error. `TargetFailure` is `Clone` for exactly
    /// this kind of resurfacing.
    pub fn into_error(self) -> anyhow::Error {
        match self.failures.first().map(|f| f.as_ref().clone()) {
            Some(representative) => anyhow::Error::new(representative).context(self),
            None => anyhow::Error::new(self),
        }
    }
}

impl std::fmt::Display for FailedTargets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} target(s) failed", self.failures.len())
    }
}

impl std::error::Error for FailedTargets {}

/// Render an `anyhow::Error` to stderr, returning whether it was claimed. If a
/// [`TargetFailure`] is in the chain it gets the rich box treatment; otherwise the
/// error's cause chain is printed in the same `×` / `╰─▶` style. `false` means
/// nothing renderable was found and the caller should fall back to its own log; a
/// claimed error may still print nothing, when it was already reported at the
/// failure site.
pub fn render_anyhow(e: &anyhow::Error) -> bool {
    match render_anyhow_to_string(e, color_enabled()) {
        Some(out) => {
            eprint!("{out}");
            true
        }
        None => false,
    }
}

/// The bytes [`render_anyhow`] would write, or `None` when the error carries
/// nothing renderable and the caller should fall back to its own logging. Pure
/// and colour-explicit so each arm is assertable without capturing stderr.
///
/// `Some("")` is a deliberate case: [`FailedTargets::reported`] already
/// printed at the failure site, so the error is handled but adds no output.
fn render_anyhow_to_string(e: &anyhow::Error, color: bool) -> Option<String> {
    // Registry failures from `finalize!`. Interactive runs deferred their render
    // to here — the viewport is torn down and nothing repaints over stderr now.
    if let Some(ft) = downcast_chain_ref::<FailedTargets>(e) {
        // An empty set is not a failure anyone can act on: claim nothing so the
        // caller logs rather than exiting non-zero in silence.
        if ft.failures().is_empty() {
            return None;
        }
        if ft.rendered {
            return Some(String::new());
        }
        return Some(render_failures_to_string(ft.failures(), color));
    }
    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        return Some(render_target_failure(tf, color));
    }
    // A frozen-check failure that arrived outside a `TargetFailure` still gets the
    // framed diff treatment so CI output is legible.
    if let Some(fc) = downcast_chain_ref::<FrozenCheckError>(e) {
        let cross = if color {
            format!("{}", "×".red())
        } else {
            "×".to_string()
        };
        let mut out = format!("{cross} frozen check failed: {}\n", fc.addr.format());
        render_diff_box(&mut out, &fc.diff, color);
        return Some(out);
    }
    let frames: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let top = frames.first()?;
    let mut out = format!("× {top}\n");
    let rest = frames.get(1..).unwrap_or(&[]);
    if !rest.is_empty() {
        out.push_str(&format!("╰─▶ {}\n", rest.join(": ")));
    }
    Some(out)
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

    /// Two registry failures, wrapped in a context frame the way `finalize!`'s
    /// caller chain wraps them.
    fn two_failures() -> Vec<Arc<TargetFailure>> {
        let a = crate::htaddr::parse_addr("//simple_fail:d1").unwrap();
        let b = crate::htaddr::parse_addr("//simple_fail:d2").unwrap();
        vec![
            Arc::new(TargetFailure::new(a, None, anyhow::anyhow!("boom a"))),
            Arc::new(TargetFailure::new(b, None, anyhow::anyhow!("boom b"))),
        ]
    }

    #[test]
    fn render_anyhow_reproduces_every_deferred_failure_box() {
        // Regression: in interactive TUI mode the registry failures used to be
        // printed mid-run inside a `paused!` block, where the resume re-anchor +
        // final viewport collapse wiped them. `finalize!` now carries them out and
        // `render_anyhow` (called post-teardown in `main`) reproduces the boxes.
        //
        // Asserting the exact bytes is the point: a `render_anyhow(&e) == true`
        // check passes even with the `FailedTargets` arm deleted, because the
        // generic cause-chain fallback also claims the error — it just prints one
        // plain line instead of two boxes.
        let e = FailedTargets::deferred(two_failures()).into_error();
        let out = render_anyhow_to_string(&e, false).expect("handled");
        assert_eq!(
            out,
            "\
× target failed: //simple_fail:d1
╰─▶ boom a

× target failed: //simple_fail:d2
╰─▶ boom b
"
        );
    }

    #[test]
    fn render_anyhow_stays_quiet_for_failures_rendered_at_the_site() {
        // Non-interactive runs print the boxes at the failure site so they land
        // ahead of the summary and the background drain. The error still travels up
        // for the exit code, and `render_anyhow` must claim it — but print nothing,
        // or every failing CI run shows its diagnostics twice.
        let mut sink: Vec<u8> = Vec::new();
        let e = FailedTargets::reported(two_failures(), &mut sink, false).into_error();
        // The constructor is what printed them — the only way to make the `rendered`
        // promise — so the flag cannot drift from what is actually on the terminal.
        assert_eq!(
            String::from_utf8(sink).expect("utf8"),
            render_failures_to_string(&two_failures(), false),
            "the boxes go out at the failure site, in full"
        );
        assert_eq!(render_anyhow_to_string(&e, false), Some(String::new()));
        assert!(render_anyhow(&e), "still counts as handled");
    }

    #[test]
    fn empty_failed_targets_is_not_claimed_as_handled() {
        // `finalize!` guards on a non-empty registry, so this is latent — but an
        // empty set used to render nothing *and* report itself handled, which is a
        // non-zero exit with no output at all. Fall through to `main`'s log instead.
        let e = FailedTargets::deferred(vec![]).into_error();
        assert_eq!(render_anyhow_to_string(&e, false), None);
    }

    #[test]
    fn failed_targets_stays_downcastable_as_a_target_failure() {
        // Telemetry's classifier asks `downcast_chain_ref::<TargetFailure>`, which
        // searches anyhow's context layers and not `Error::source()`. Holding the
        // failures in a struct field alone would therefore report the tool's most
        // common failure mode as a generic error; `into_error` keeps a
        // representative failure in the chain so both downcasts land.
        let e = FailedTargets::deferred(two_failures());
        assert_eq!(e.to_string(), "2 target(s) failed");
        let e = e.into_error();
        let tf = downcast_chain_ref::<TargetFailure>(&e).expect("classifiable");
        assert_eq!(tf.addr.format(), "//simple_fail:d1");
        let ft = downcast_chain_ref::<FailedTargets>(&e).expect("all failures carried");
        assert_eq!(ft.failures().len(), 2);
        // And the render still goes through the `FailedTargets` arm — the whole set,
        // not just the representative that makes the downcast work.
        assert_eq!(
            render_anyhow_to_string(&e, false),
            Some(render_failures_to_string(&two_failures(), false))
        );
    }

    /// `finalize!` is duck-typed over the app context and the request state — it
    /// only ever calls `interactive()`, `pause()` and `take_failures()` — so the
    /// branch it takes can be driven without an engine or a terminal.
    struct FakeCtx {
        interactive: bool,
        pauses: std::cell::Cell<usize>,
    }

    struct FakeGuard;

    impl FakeCtx {
        fn new(interactive: bool) -> Self {
            Self {
                interactive,
                pauses: std::cell::Cell::new(0),
            }
        }
        fn interactive(&self) -> bool {
            self.interactive
        }
        async fn pause(&self) -> FakeGuard {
            self.pauses.set(self.pauses.get() + 1);
            FakeGuard
        }
    }

    fn addr(s: &str) -> crate::htaddr::Addr {
        hmodel::htaddr::parse_addr(s).expect("addr")
    }

    /// Keep-going failures inside an `Ok` batch must fold into `res` — the
    /// registry normally duplicates them, but an unrecorded class (the
    /// silent-exit-0 lint incident) must still fail the run.
    #[test]
    fn empty_result_set_is_a_failed_run() {
        // The bug: a selector matching nothing folded to `Ok(vec![])` and exited
        // 0, so a typo'd label or an out-of-scope matcher "succeeded" having
        // built nothing.
        let err = require_non_empty(vec![]).err().expect("empty must fail");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains("no targets matched"),
            "the message must say nothing ran, got: {rendered}"
        );
        assert!(
            rendered.contains("heph query"),
            "and point at how to debug the selector, got: {rendered}"
        );
    }

    #[test]
    fn a_non_empty_result_set_passes_through_untouched() {
        let batch = crate::engine::BatchResult {
            ok: vec![],
            errors: vec![],
        };
        // Guard the shape rather than the emptiness: `fold_batch` on a clean
        // batch is `Ok`, and only `require_non_empty` decides emptiness — so a
        // future change that makes `fold_batch` itself reject empty would be
        // caught here as a double rejection.
        assert!(
            fold_batch(batch).is_ok(),
            "fold_batch must not judge emptiness"
        );
    }

    #[test]
    fn fold_batch_surfaces_keep_going_errors() {
        let batch = crate::engine::BatchResult {
            ok: vec![],
            errors: vec![(addr("//pkg:a"), anyhow::anyhow!("boom"))],
        };
        let err = fold_batch(batch).err().expect("errors must fold into Err");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("//pkg:a"), "{rendered}");
        assert!(rendered.contains("boom"), "{rendered}");
    }

    #[test]
    fn fold_batch_passes_a_clean_batch_through() {
        let batch = crate::engine::BatchResult {
            ok: vec![],
            errors: vec![],
        };
        assert!(fold_batch(batch).is_ok());
    }

    /// All-cancelled collapses to the cancellation `finalize!` recognises…
    #[test]
    fn fold_batch_collapses_pure_cancellation() {
        let batch = crate::engine::BatchResult {
            ok: vec![],
            errors: vec![(addr("//pkg:a"), CancelledError.into())],
        };
        let err = fold_batch(batch).err().expect("cancelled folds to Err");
        assert!(is_cancelled(&err), "{err:#}");
    }

    /// …but a genuine failure is never masked by the cancellations it caused.
    #[test]
    fn fold_batch_prefers_genuine_failures_over_cancellation() {
        let batch = crate::engine::BatchResult {
            ok: vec![],
            errors: vec![
                (addr("//pkg:a"), CancelledError.into()),
                (addr("//pkg:b"), anyhow::anyhow!("boom")),
            ],
        };
        let err = fold_batch(batch).err().expect("must fail");
        assert!(!is_cancelled(&err), "{err:#}");
        assert!(format!("{err:#}").contains("//pkg:b"));
    }

    struct FakeRs(std::cell::RefCell<Vec<Arc<TargetFailure>>>);

    impl FakeRs {
        fn new(failures: Vec<Arc<TargetFailure>>) -> Self {
            Self(std::cell::RefCell::new(failures))
        }
        fn take_failures(&self) -> Vec<Arc<TargetFailure>> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
    }

    #[tokio::test]
    async fn finalize_defers_the_render_only_when_interactive() -> anyhow::Result<()> {
        // Interactive: the boxes must NOT be printed here. Mid-run the TUI is only
        // paused, so anything printed under `paused!` is re-anchored over by the
        // resume and erased by the final viewport collapse. Carry them out instead,
        // without pausing at all.
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(two_failures());
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize!(ctx, rs, res, _v => { Ok(()) }).unwrap_err();
        let ft = downcast_chain_ref::<FailedTargets>(&e).expect("carried out");
        assert!(!ft.rendered, "interactive defers to render_anyhow");
        assert_eq!(ft.failures().len(), 2);
        assert_eq!(ctx.pauses.get(), 0, "no pause on the failure path");

        // Non-interactive: no viewport, nothing repaints stderr — render here so
        // the boxes precede the end-of-run summary and the background drain.
        let ctx = FakeCtx::new(false);
        let rs = FakeRs::new(two_failures());
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize!(ctx, rs, res, _v => { Ok(()) }).unwrap_err();
        let ft = downcast_chain_ref::<FailedTargets>(&e).expect("carried out");
        assert!(
            ft.rendered,
            "already printed; render_anyhow must stay quiet"
        );
        assert_eq!(ft.failures().len(), 2);
        assert_eq!(ctx.pauses.get(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn finalize_prefers_failures_over_cancellation() -> anyhow::Result<()> {
        // A cancellation that arrives alongside recorded failures must still surface
        // the failures — the targets genuinely failed, and ctrl-c after the fact
        // does not make that less true.
        for interactive in [true, false] {
            let ctx = FakeCtx::new(interactive);
            let rs = FakeRs::new(two_failures());
            let res: anyhow::Result<u8> = Err(anyhow::Error::new(CancelledError));
            let e = finalize!(ctx, rs, res, _v => { Ok(()) }).unwrap_err();
            assert!(
                downcast_chain_ref::<FailedTargets>(&e).is_some(),
                "interactive={interactive}: failures outrank the cancellation"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn finalize_runs_the_success_body_paused_when_the_registry_is_clean() -> anyhow::Result<()>
    {
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(vec![]);
        let res: anyhow::Result<u8> = Ok(7);
        let mut got = 0;
        finalize!(ctx, rs, res, v => { got = v; Ok(()) }).unwrap();
        assert_eq!(got, 7, "the success value reaches the body");
        assert_eq!(
            ctx.pauses.get(),
            1,
            "inline output prints with the TUI paused"
        );
        Ok(())
    }

    #[tokio::test]
    async fn finalize_reports_a_clean_cancellation_without_running_the_body() -> anyhow::Result<()>
    {
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(vec![]);
        let res: anyhow::Result<u8> = Err(anyhow::Error::new(CancelledError));
        let mut ran = false;
        let e = finalize!(ctx, rs, res, _v => { ran = true; Ok(()) }).unwrap_err();
        assert_eq!(e.to_string(), "cancelled");
        assert!(!ran);
        Ok(())
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
