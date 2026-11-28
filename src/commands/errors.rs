//! End-of-execution error rendering. Failing targets are collected in the
//! per-request registry as [`TargetFailure`]s, each carrying its compacted
//! record, and every box is that record drawn by [`render_box`]: in line mode
//! for the CI box here, in report mode after the TUI ([`ReportContext`]).

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::style::Stylize;

use crate::engine::error::{CancelledError, FrozenCheckError, TargetFailure};
use crate::engine::request_state::FailFastStopped;
use crate::hmemoizer::{chain_frames, downcast_chain_ref};
use crate::tui::color::stderr_color_enabled as color_enabled;
use crate::tui::failure_report::{RenderCtx, RenderMode, render_box};
use crate::tui::session::SessionSlot;
use crate::tui::text::{Redact, WorkspaceRel, message};

mod report;
pub use report::ReportContext;

/// Render each recorded target failure's box, line mode, separated by a blank
/// line. Pure, so the exact bytes the user sees are assertable without
/// capturing stderr.
pub fn render_failures_to_string(failures: &[Arc<TargetFailure>], color: bool) -> String {
    render_failures(
        failures.iter(),
        &RenderCtx {
            color,
            ..RenderCtx::default()
        },
    )
}

/// Each failure's record drawn by [`render_box`] under `ctx`, a blank line
/// between two boxes. A process that failed while its output streamed has
/// no box: its one line said it then (D6.1c).
fn render_failures<'a>(
    failures: impl Iterator<Item = &'a Arc<TargetFailure>>,
    ctx: &RenderCtx,
) -> String {
    let mut out = String::new();
    for (i, f) in failures.filter(|f| !f.record.told_streamed()).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&render_box(&f.record, ctx));
    }
    out
}

/// Render the "incomplete selection" block: what the selector walk skipped,
/// grouped by stage and provider, and what to do about it. Pure, like
/// [`render_failures_to_string`]. `after_failures` points the reader at the
/// failure boxes printed above it, which is only true when there are some.
/// ```text
/// ! selection incomplete: 412 skipped while matching `label(nix) && //...`
///   412 targets could not be loaded (provider go)
///       //svc/api:build   go list: exit status 1
///       ... 411 more
///   Targets that matched were still processed; the skipped ones were not, and
///   may also have matched. Fix the cause and re-run, or pass --fail-fast to
///   stop at the first one with its full error.
/// ```
///
/// The stage is said in words (`could not be loaded`), not by its key
/// (`spec`): the key is the machine contract and travels on the
/// `SelectionIncomplete` event.
///
/// A provider whose package listing failed is its own sentence (STAGE-05),
/// `provider go could not list packages: <cause>`, and is not counted in the
/// header: it lost every package of that provider, not one scope the walk
/// skipped. With nothing else skipped the header has no count.
fn render_incomplete(
    report: &crate::engine::discovery::GapReport,
    after_failures: bool,
    color: bool,
) -> String {
    use crate::engine::discovery::Stage;
    let bang = if color {
        format!("{}", "!".yellow())
    } else {
        "!".to_string()
    };
    let mut out = format!("{bang} {}\n", incomplete_headline(report));
    for group in &report.groups {
        if group.stage == Stage::Packages {
            let who = if group.provider.is_empty() {
                "a provider".to_owned()
            } else {
                format!("provider {}", group.provider)
            };
            for example in &group.examples {
                let under = if example.scope == "all packages" {
                    String::new()
                } else {
                    format!(" under {}", example.scope)
                };
                out.push_str(&format!(
                    "  {who} could not list packages{under}: {}\n",
                    example.cause
                ));
            }
            let more = group.count.saturating_sub(group.examples.len());
            if more > 0 {
                out.push_str(&format!("      ... {more} more\n"));
            }
            continue;
        }
        let provider = if group.provider.is_empty() {
            String::new()
        } else {
            format!(" (provider {})", group.provider)
        };
        out.push_str(&format!(
            "  {} {} {}{provider}\n",
            group.count,
            group.stage.noun(group.count),
            group.stage.phrase(),
        ));
        let width = group
            .examples
            .iter()
            .map(|e| e.scope.chars().count())
            .max()
            .unwrap_or(0);
        for example in &group.examples {
            let scope = if color {
                format!("{:<width$}", example.scope).dim().to_string()
            } else {
                format!("{:<width$}", example.scope)
            };
            out.push_str(&format!("      {scope}   {}\n", example.cause));
        }
        let more = group.count.saturating_sub(group.examples.len());
        if more > 0 {
            out.push_str(&format!("      ... {more} more\n"));
        }
    }
    if after_failures {
        out.push_str("  Target failures above may be the cause.\n");
    }
    out.push_str(if report.nothing_matched {
        "  No target matched, so nothing ran.\n"
    } else {
        "  Targets that matched were still processed; the skipped ones were not, and may also \
         have matched.\n"
    });
    out.push_str(
        "  Fix the cause and re-run, or pass --fail-fast to stop at the first one with its full \
         error.\n",
    );
    out
}

/// The block's first line, and [`IncompleteSelection`]'s text: `selection
/// incomplete: 3 skipped while matching `//...``, the count leaving out
/// failed package listings (STAGE-05), and gone when nothing else was
/// skipped.
fn incomplete_headline(report: &crate::engine::discovery::GapReport) -> String {
    let listings: usize = report
        .groups
        .iter()
        .filter(|g| g.stage == crate::engine::discovery::Stage::Packages)
        .map(|g| g.count)
        .sum();
    match report.skipped.saturating_sub(listings) {
        0 => format!("selection incomplete while matching `{}`", report.selector),
        n => format!(
            "selection incomplete: {n} skipped while matching `{}`",
            report.selector
        ),
    }
}

/// What a keep-going walk skipped, read once at the end of a command. `None`
/// when nothing was skipped, or when the command was cancelled: a Ctrl-C cuts
/// the selection short on purpose, and exits as cancelled rather than as
/// incomplete. [`finalize`] announces it on the event stream.
pub fn incomplete_selection(
    gaps: &crate::engine::Gaps,
    cancelled: bool,
) -> Option<crate::engine::discovery::GapReport> {
    (!gaps.is_empty() && !cancelled).then(|| gaps.report())
}

/// A command whose selector walk skipped part of the workspace. It acted on
/// everything that resolved, and exits non-zero with the "incomplete
/// selection" block — the same rule [`require_non_empty`] applies to an empty
/// selection: a CI job that skipped part of what it was asked to cover must not
/// go green.
///
/// Built like [`FailedTargets`]: interactive mode defers the render to
/// [`render_anyhow`], after the viewport is torn down; otherwise it prints at
/// the failure site.
#[derive(Debug)]
pub struct IncompleteSelection {
    report: crate::engine::discovery::GapReport,
    /// A genuine error the command also hit, rendered above the block.
    cause: Option<anyhow::Error>,
    rendered: bool,
}

impl IncompleteSelection {
    pub fn new(
        report: crate::engine::discovery::GapReport,
        cause: Option<anyhow::Error>,
        interactive: bool,
        rel: &WorkspaceRel,
    ) -> Self {
        let mut this = Self {
            report,
            cause,
            rendered: false,
        };
        if !interactive {
            use std::io::Write as _;
            // A failed write to the terminal is not actionable; the exit code
            // still carries the outcome out.
            drop(write!(
                io::stderr(),
                "{}",
                this.render_to_string(color_enabled(), rel)
            ));
            this.rendered = true;
        }
        this
    }

    /// The block, with the command's own error above it, as the user reads
    /// it. Pure.
    pub fn render_to_string(&self, color: bool, rel: &WorkspaceRel) -> String {
        let mut out = self
            .cause
            .as_ref()
            .and_then(|c| render_anyhow_in(c, color, rel))
            .unwrap_or_default();
        out.push_str(&render_incomplete(&self.report, false, color));
        out
    }

    pub fn into_error(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }
}

impl std::fmt::Display for IncompleteSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&incomplete_headline(&self.report))
    }
}

impl std::error::Error for IncompleteSelection {
    /// The genuine error the command also hit, so `chain()` walks into it.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_ref()
            .map(|c| c.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// For a command that does not end in [`finalize`]: `Err` with the
/// "incomplete selection" block when the walk skipped anything, rendered by
/// [`render_anyhow`] on the way out.
pub fn require_complete_selection(gaps: &crate::engine::Gaps) -> anyhow::Result<()> {
    if gaps.is_empty() {
        return Ok(());
    }
    Err(IncompleteSelection {
        report: gaps.report(),
        cause: None,
        rendered: false,
    }
    .into_error())
}

/// True when the error chain carries a Ctrl-C cancellation. A cancellation is
/// never recorded as a [`TargetFailure`]; it aborts the build but is not a target
/// fault, so commands surface it separately from the failure registry.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    hplugin::error::is_cancelled(e)
}

/// Fold a keep-going batch into the single `res` that [`finalize`] expects.
///
/// `Engine::result` reports keep-going failures INSIDE its `Ok`
/// (`BatchResult::errors`); they are normally duplicated into the failure
/// registry, which `finalize` renders. This fold must still exist: an error
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

/// [`require_non_empty`], unless the selector walk skipped something. Then an
/// empty match is answered by what was skipped — the "incomplete selection"
/// block `finalize` prints — not by "check your selector", which would send
/// the user after a typo that is not there.
pub fn require_non_empty_unless_incomplete(
    results: Vec<Arc<crate::engine::EResult>>,
    gaps: &crate::engine::Gaps,
) -> anyhow::Result<Vec<Arc<crate::engine::EResult>>> {
    if gaps.is_empty() {
        require_non_empty(results)
    } else {
        if results.is_empty() {
            gaps.note_nothing_matched();
        }
        Ok(results)
    }
}

/// What [`finalize`] asks of the app context: [`AppContext`](crate::tui::AppContext)
/// in a command, a fake in the tests, so the branch it takes can be driven
/// without a terminal.
pub(crate) trait FinalizeCtx {
    fn interactive(&self) -> bool;
    fn session(&self) -> &SessionSlot;
    /// The failures the line backend printed as they failed.
    async fn failures_printed(&self) -> HashSet<String>;
    /// Pause the TUI until the returned guard drops ([`crate::tui::paused!`]).
    async fn pause(&self) -> impl Sized;
}

impl FinalizeCtx for crate::tui::AppContext {
    fn interactive(&self) -> bool {
        crate::tui::AppContext::interactive(self)
    }
    fn session(&self) -> &SessionSlot {
        crate::tui::AppContext::session(self)
    }
    async fn failures_printed(&self) -> HashSet<String> {
        crate::tui::AppContext::failures_printed(self).await
    }
    async fn pause(&self) -> impl Sized {
        crate::tui::AppContext::pause(self).await
    }
}

/// What [`finalize`] asks of the request: its failure registry and events.
/// [`RequestState`](crate::engine::request_state::RequestState) in a command,
/// a fake in the tests, so the branch it takes can be driven without an engine.
pub(crate) trait FinalizeRequest {
    async fn take_failures_with_logs_where(
        &self,
        wanted: impl Fn(&TargetFailure) -> bool,
    ) -> Vec<Arc<TargetFailure>>;
    fn emit(&self, kind: crate::engine::event::BuildEventKind);
    /// The most log lines a box shows (`--log-lines`).
    fn log_tail_lines(&self) -> usize;
    fn was_cancelled(&self, addr: &str) -> bool;
    fn heph_home(&self) -> Option<std::path::PathBuf>;
    fn workspace_root(&self) -> Option<std::path::PathBuf>;
    fn fail_fast_stopped(&self) -> Option<FailFastStopped>;
}

impl FinalizeRequest for Arc<crate::engine::request_state::RequestState> {
    async fn take_failures_with_logs_where(
        &self,
        wanted: impl Fn(&TargetFailure) -> bool,
    ) -> Vec<Arc<TargetFailure>> {
        crate::engine::request_state::RequestState::take_failures_with_logs_where(self, wanted)
            .await
    }
    fn emit(&self, kind: crate::engine::event::BuildEventKind) {
        crate::engine::request_state::RequestState::emit(self, kind)
    }
    fn log_tail_lines(&self) -> usize {
        crate::engine::request_state::RequestState::log_tail_lines(self)
    }
    fn was_cancelled(&self, addr: &str) -> bool {
        crate::engine::request_state::RequestState::was_cancelled(self, addr)
    }
    fn heph_home(&self) -> Option<std::path::PathBuf> {
        crate::engine::request_state::RequestState::heph_home(self)
    }
    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        crate::engine::request_state::RequestState::workspace_root(self)
    }
    fn fail_fast_stopped(&self) -> Option<FailFastStopped> {
        crate::engine::request_state::RequestState::fail_fast_stopped(self)
    }
}

/// Paved-road command finalizer. The single construct every command ends with.
///
/// One place owns the awkward truth that a command's *outcome* lives in two
/// channels: the per-request failure registry (rich, deduped `TargetFailure`s,
/// recorded when a provider/driver runs a target) and the returned `res`. Given
/// `ctx`, the request state `rs`, an engine `res`, and `body` (the success
/// output), it resolves them (pausing the TUI only for the success/error output
/// that prints inline):
///
/// - registry non-empty → exit with [`FailedTargets`]; interactive mode defers the
///   render to `render_anyhow` (after the viewport is torn down), non-interactive
///   mode renders here. The returned `res` is a collateral marker, dropped;
/// - registry empty, `res` Ok → run `body` on the value, then exit `Ok` (or with
///   the body's error);
/// - registry empty, `res` Err → a cancellation exits `cancelled`; any other error
///   is a genuine non-registry failure propagated to `render_anyhow`.
///
/// Commands whose outcome isn't a single `Result` (e.g. `run`'s fail-fast
/// batch) fold it into one `res` before calling — a cancellation among batch
/// errors becomes `Err(CancelledError)`. A command that prints incrementally
/// and needs no end-of-run output passes `|_| Ok(())`.
///
/// One more outcome applies to a command whose walk ran
/// [`Discovery::KeepGoing`](crate::engine::Discovery) and passes its sink as
/// `gaps`: anything skipped — and the command not cancelled — emits one
/// `SelectionIncomplete` event, renders the "incomplete selection" block after
/// any failure boxes (and after a genuine error, the body's included), and
/// exits non-zero even when the success body ran.
pub(crate) async fn finalize<T>(
    ctx: &impl FinalizeCtx,
    rs: &impl FinalizeRequest,
    res: anyhow::Result<T>,
    gaps: Option<&Arc<crate::engine::Gaps>>,
    body: impl FnOnce(T) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let cancelled = res.as_ref().err().is_some_and(is_cancelled);
    let incomplete = take_failure_outcome(ctx, rs, gaps, cancelled).await?;
    let printed: anyhow::Result<()> = crate::tui::paused!(ctx, {
        match res {
            Ok(val) => body(val),
            // A cancellation is surfaced by `finish_exit`; any other error
            // is a genuine non-registry failure → `render_anyhow`.
            Err(e) if !cancelled => Err(e),
            Err(_) => Ok(()),
        }
    });
    match incomplete {
        None => {
            printed?;
            finish_exit(cancelled)
        }
        // What matched was processed; the run still fails for what was not.
        Some(report) => Err(IncompleteSelection::new(
            report,
            printed.err(),
            ctx.interactive(),
            &ctx.session().workspace_rel(),
        )
        .into_error()),
    }
}

/// The part of [`finalize`] that does not depend on the command's success
/// value: announce what `gaps` skipped, then take the request's failures.
/// `Err` is the command's error when the registry holds any failure
/// ([`FailedTargets`]); `Ok` carries the incomplete selection, if any, for the
/// success path.
async fn take_failure_outcome(
    ctx: &impl FinalizeCtx,
    rs: &impl FinalizeRequest,
    gaps: Option<&Arc<crate::engine::Gaps>>,
    cancelled: bool,
) -> anyhow::Result<Option<crate::engine::discovery::GapReport>> {
    let incomplete = gaps.and_then(|g| incomplete_selection(g, cancelled));
    if let Some(report) = &incomplete {
        // Once per command, so a hook learns the run was incomplete even
        // when it never sees the terminal.
        rs.emit(report.to_event());
    }
    // The failures the line backend printed as they failed, box rows
    // included: their boxes are left out below. Asked before the
    // failures are taken, so the answer covers every one of them; empty
    // on the interactive backend, whose report is unchanged.
    let printed_live = ctx.failures_printed().await;
    // Each saying whether `heph log <addr>` would find its log: the
    // report and the CI box point at it on the same answer. One printed
    // live carried that answer then and is not asked again — but it is
    // still taken, so the exit code and telemetry count it.
    // A failure told in its one line after a streamed run gets no box, so
    // no `log:` row to ask about either.
    let failures = rs
        .take_failures_with_logs_where(|f| {
            !f.record.told_streamed() && !printed_live.contains(&f.addr.format())
        })
        .await;
    if failures.is_empty() {
        return Ok(incomplete);
    }
    // The caller's `res` is a collateral marker here — dropped either way.
    if ctx.interactive() {
        // Do NOT render here: mid-run the TUI is only *paused*, so the
        // resume re-anchor and the final viewport collapse would wipe the
        // boxes off screen. Carry the failures out; `render_anyhow` in
        // `main` prints them once the viewport is fully torn down.
        // As the report: everything it reads from disk is read
        // here, never at the render (two-mode TUI I7).
        let home = rs.heph_home();
        let report = ReportContext::gather(
            &failures,
            |addr| rs.was_cancelled(addr),
            home.as_deref(),
            rs.log_tail_lines(),
        )
        .with_workspace(rs.workspace_root().as_deref())
        .with_stopped(rs.fail_fast_stopped());
        Err(FailedTargets::deferred(failures)
            .with_incomplete(incomplete)
            .with_report(report)
            .into_error())
    } else {
        // No viewport, so nothing ever repaints stderr and there is no
        // reason to defer. Print at the failure site: deferring to `main`
        // would push the diagnostics behind the backend's end-of-run
        // summary *and* its unbounded background-upload drain, invert the
        // errors-then-verdict order every other build tool prints, and lose
        // them entirely if a second ctrl-c aborts during that drain.
        // (Interactive has no such choice — teardown must come first, so it
        // necessarily prints verdict-then-errors.)
        Err(FailedTargets::reported_to_stderr(
            failures,
            incomplete,
            &printed_live,
            ctx.session(),
            BoxRequest {
                root: rs.workspace_root(),
                stopped: rs.fail_fast_stopped(),
                log_lines: rs.log_tail_lines(),
            },
        )
        .into_error())
    }
}

/// Map the cancelled flag to an exit result for the no-failures path. Registry
/// failures never reach here — they exit as [`FailedTargets`]. A
/// [`CancelledError`], so `main` reads it as a cancel: exit 130.
pub(crate) fn finish_exit(cancelled: bool) -> anyhow::Result<()> {
    if cancelled {
        return Err(CancelledError.into());
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
    /// What the selector walk skipped, rendered after the boxes. A failed target
    /// is often why a candidate could not be resolved, so the block points back
    /// at them.
    incomplete: Option<crate::engine::discovery::GapReport>,
    /// Already printed at the failure site; `render_anyhow` must not print twice.
    rendered: bool,
    /// The interactive report, gathered where the failures were collected:
    /// when set, it is printed instead of the boxes.
    report: Option<ReportContext>,
    /// What the boxes are drawn with (line mode, never wrapped); its colour
    /// is decided at the render.
    boxes: RenderCtx,
    /// What `--fail-fast` stopped, listed after the boxes.
    stopped: Option<FailFastStopped>,
}

/// What the CI box reads from its request beyond the failures, read where
/// the failures are collected.
#[derive(Debug)]
pub struct BoxRequest {
    /// The workspace: what heph's text names paths relative to, and where
    /// an excerpt's file is read from.
    pub root: Option<PathBuf>,
    /// What `--fail-fast` stopped once a target failed.
    pub stopped: Option<FailFastStopped>,
    /// The most log lines a box shows (`--log-lines`).
    pub log_lines: usize,
}

impl Default for BoxRequest {
    fn default() -> Self {
        Self {
            root: None,
            stopped: None,
            log_lines: RenderCtx::default().log_lines,
        }
    }
}

impl BoxRequest {
    /// The CI box's context: line mode, never wrapped (a CI log), paths
    /// under the workspace named relative to it. Reads the filesystem
    /// ([`WorkspaceRel::workspace`]): build it once, before the render.
    fn render_ctx(&self) -> RenderCtx {
        RenderCtx {
            rel: self
                .root
                .as_deref()
                .map(WorkspaceRel::workspace)
                .unwrap_or_default(),
            redact: Redact::default(),
            source_root: self.root.clone(),
            width: None,
            color: false,
            log_lines: self.log_lines,
            mode: RenderMode::Line,
        }
    }
}

/// The `stopped by --fail-fast` lines (RUN-24), after a blank line, when
/// fail-fast stopped anything.
fn push_stopped(out: &mut String, stopped: Option<&FailFastStopped>) {
    if let Some(s) = stopped {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&crate::tui::failure_report::fail_fast_lines(
            &s.running, s.queued,
        ));
    }
}

impl FailedTargets {
    /// Failures not yet printed — [`render_anyhow`] renders them.
    pub fn deferred(failures: Vec<Arc<TargetFailure>>) -> Self {
        Self {
            failures,
            incomplete: None,
            rendered: false,
            report: None,
            boxes: RenderCtx::default(),
            stopped: None,
        }
    }

    /// Print `report` (one block per root) instead of the boxes: what an
    /// interactive run shows once its TUI is gone.
    pub fn with_report(mut self, report: ReportContext) -> Self {
        self.report = Some(report);
        self
    }

    /// Attach the walk's skips to a [`deferred`](Self::deferred) set.
    pub fn with_incomplete(
        mut self,
        incomplete: Option<crate::engine::discovery::GapReport>,
    ) -> Self {
        self.incomplete = incomplete;
        self
    }

    /// Everything this error prints: the boxes, then any incomplete-selection
    /// block.
    fn render_to_string(&self, color: bool) -> String {
        self.render_except(&HashSet::new(), color)
    }

    /// [`render_to_string`](Self::render_to_string), leaving out the box of
    /// each failure in `printed_live`: the line backend printed its block,
    /// tail and box rows included, the moment it failed, and the box would
    /// only say it again. The failure stays in [`failures`](Self::failures)
    /// all the same, so the exit code and telemetry still count it.
    fn render_except(&self, printed_live: &HashSet<String>, color: bool) -> String {
        let mut out = match &self.report {
            Some(report) => report.render(color),
            None => render_failures(
                self.failures
                    .iter()
                    .filter(|f| !printed_live.contains(&f.addr.format())),
                &RenderCtx {
                    color,
                    ..self.boxes.clone()
                },
            ),
        };
        if self.report.is_none() {
            push_stopped(&mut out, self.stopped.as_ref());
        }
        if let Some(report) = &self.incomplete {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&render_incomplete(report, true, color));
        }
        out
    }

    /// Print the failures to `out` and record that it happened, so
    /// [`render_anyhow`] stays quiet later. A failure in `printed_live` gets
    /// no box: its live block was printed (tracked by the line backend's view,
    /// [`AppContext::failures_printed`](crate::tui::app::AppContext::failures_printed),
    /// never assumed), and it ends with the box's rows.
    ///
    /// Printing lives *in* the constructor on purpose: `rendered` is a promise
    /// about what is already on the terminal, and a caller that sets it without
    /// printing produces a non-zero exit with no diagnostics at all — the exact
    /// bug this module exists to prevent, and one no test of the marker can see.
    pub fn reported(
        failures: Vec<Arc<TargetFailure>>,
        incomplete: Option<crate::engine::discovery::GapReport>,
        printed_live: &HashSet<String>,
        out: &mut dyn io::Write,
        color: bool,
    ) -> Self {
        Self::reported_in(
            failures,
            incomplete,
            printed_live,
            out,
            color,
            BoxRequest::default(),
        )
    }

    /// [`FailedTargets::reported`], with what `req` read from the request:
    /// the cause lines name paths under its workspace relative to it (D2),
    /// and the boxes are followed by what `--fail-fast` stopped.
    pub fn reported_in(
        failures: Vec<Arc<TargetFailure>>,
        incomplete: Option<crate::engine::discovery::GapReport>,
        printed_live: &HashSet<String>,
        out: &mut dyn io::Write,
        color: bool,
        req: BoxRequest,
    ) -> Self {
        let this = Self {
            failures,
            incomplete,
            rendered: true,
            report: None,
            boxes: req.render_ctx(),
            stopped: req.stopped,
        };
        // A failed write to the terminal is not actionable, and the exit code still
        // has to carry the failure out.
        drop(write!(out, "{}", this.render_except(printed_live, color)));
        this
    }

    /// [`FailedTargets::reported`] against the real stderr — what `finalize` uses.
    /// What it printed is recorded in `session`, so an attached viewer prints
    /// the report too.
    pub fn reported_to_stderr(
        failures: Vec<Arc<TargetFailure>>,
        incomplete: Option<crate::engine::discovery::GapReport>,
        printed_live: &HashSet<String>,
        session: &SessionSlot,
        req: BoxRequest,
    ) -> Self {
        let mut out = Vec::new();
        let this = Self::reported_in(
            failures,
            incomplete,
            printed_live,
            &mut out,
            color_enabled(),
            req,
        );
        // Every failure printed live leaves nothing to print.
        if !out.is_empty() {
            // Best-effort, like the write in `reported`: the exit code carries
            // the failure out either way.
            drop(io::Write::write_all(&mut io::stderr(), &out));
            session.record_output(&String::from_utf8_lossy(&out));
        }
        this
    }

    pub fn failures(&self) -> &[Arc<TargetFailure>] {
        &self.failures
    }

    /// Build the `anyhow::Error` that carries these failures out of `finalize`.
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
/// [`TargetFailure`] is in the chain its record's box is drawn (line mode);
/// otherwise the error's cause chain is printed as `×` top / `╰─▶` rest. `false` means
/// nothing renderable was found and the caller should fall back to its own log; a
/// claimed error may still print nothing, when it was already reported at the
/// failure site. What it printed is recorded in `session`.
pub fn render_anyhow(e: &anyhow::Error, session: &SessionSlot) -> bool {
    match render_anyhow_in(e, color_enabled(), &session.workspace_rel()) {
        Some(out) => {
            eprint!("{out}");
            // An attached viewer prints the report below its summary too.
            session.record_output(&out);
            true
        }
        None => false,
    }
}

/// A bare frozen-check failure's record: its target, its diff, and the
/// header saying the generated output differs.
fn frozen_record(fc: &FrozenCheckError) -> hcore::failure::FailureRecord {
    let addr = fc.addr.format();
    let mut rec = hcore::failure::FailureRecord::new(
        hcore::diagnostic::Diagnostic::new(hcore::diagnostic::Stage::Run, "frozen check failed")
            .with_addr(addr.clone()),
    );
    rec.addr = Some(addr.clone());
    rec.entry = addr;
    rec.frozen_diff = Some(fc.diff.clone());
    rec
}

/// The bytes [`render_anyhow`] would write, or `None` when the error carries
/// nothing renderable and the caller should fall back to its own logging. Pure
/// and colour-explicit so each arm is assertable without capturing stderr.
/// Every line is heph's text: paths under the workspace are named relative
/// to it by `rel` (the session's, empty before bootstrap resolved one), and
/// control bytes are shown escaped (D2).
///
/// `Some("")` is a deliberate case: [`FailedTargets::reported`] already
/// printed at the failure site, so the error is handled but adds no output.
fn render_anyhow_in(e: &anyhow::Error, color: bool, rel: &WorkspaceRel) -> Option<String> {
    // Registry failures from `finalize`. Interactive runs deferred their render
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
        return Some(ft.render_to_string(color));
    }
    if let Some(inc) = downcast_chain_ref::<IncompleteSelection>(e) {
        if inc.rendered {
            return Some(String::new());
        }
        return Some(inc.render_to_string(color, rel));
    }
    let ctx = RenderCtx {
        color,
        rel: rel.clone(),
        ..RenderCtx::default()
    };
    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        // Told already, in its one line after the output (D6.1c).
        if tf.record.told_streamed() {
            return Some(String::new());
        }
        return Some(render_box(&tf.record, &ctx));
    }
    // A frozen-check failure that arrived outside a `TargetFailure` still gets
    // its diff framed, so CI output is legible.
    if let Some(fc) = downcast_chain_ref::<FrozenCheckError>(e) {
        return Some(render_box(&frozen_record(fc), &ctx));
    }
    let frames = chain_frames(e);
    let top = frames.first()?;
    // heph's message: workspace paths relative, controls shown escaped,
    // never obeyed (D2).
    let redact = Redact::default();
    let mut out = format!("× {}\n", message(top, &redact, rel));
    let rest = frames.get(1..).unwrap_or(&[]);
    if !rest.is_empty() {
        out.push_str(&format!(
            "╰─▶ {}\n",
            message(&rest.join(": "), &redact, rel)
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::error::FailureRecord;
    use anyhow::Context as _;
    use hcore::diagnostic::{Diagnostic, Stage};
    use hcore::events::{ExitData, FailedIn, FixesData, LeftBehindData, LogRef};
    use std::path::Path;
    use std::sync::Arc;

    /// [`render_anyhow_in`] before any workspace is known.
    fn render_anyhow_to_string(e: &anyhow::Error, color: bool) -> Option<String> {
        render_anyhow_in(e, color, &WorkspaceRel::default())
    }

    /// Q4: what reaches `main` is heph's text, so it names paths under the
    /// session's workspace relative to it once bootstrap set one, and shows
    /// a control byte escaped either way — the generic chain and a target's
    /// box alike.
    #[test]
    fn render_anyhow_relative_and_escaped() {
        let ws = tempfile::tempdir().unwrap();
        let slot = SessionSlot::new();
        slot.set_workspace(ws.path(), ws.path(), &ws.path().join(".heph"));
        let build = ws.path().join("a/BUILD");
        let e = anyhow::anyhow!("denied\x1b[2J").context(format!("reading {}", build.display()));
        assert_eq!(
            render_anyhow_in(&e, false, &slot.workspace_rel()).as_deref(),
            Some("× reading a/BUILD\n╰─▶ denied\\x1b[2J\n")
        );
        // No workspace resolved: nothing rewritten, still escaped.
        assert_eq!(
            render_anyhow_in(&e, false, &SessionSlot::new().workspace_rel()),
            Some(format!(
                "× reading {}\n╰─▶ denied\\x1b[2J\n",
                build.display()
            ))
        );

        let f = failure(
            "//p:t",
            rec(
                "//p:t",
                &format!("reading {}: denied\x1b[2J", build.display()),
            ),
        );
        let e = anyhow::Error::new(f.as_ref().clone());
        assert_eq!(
            render_anyhow_in(&e, false, &slot.workspace_rel()).as_deref(),
            Some("✗ //p:t\n  reading a/BUILD: denied\\x1b[2J\n")
        );
    }

    /// A failure of `addr` carrying `rec` as its record: what the CI box
    /// draws.
    fn failure(addr: &str, rec: FailureRecord) -> Arc<TargetFailure> {
        Arc::new(
            TargetFailure::new(crate::htaddr::parse_addr(addr).expect("addr"))
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

    /// A record of a process that exited 1, its log kept in a slot under
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
            code: Some(1),
            signal: None,
            status: "exit status: 1".into(),
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

    #[test]
    fn is_cancelled_detects_cancellation_in_chain() {
        use crate::engine::error::CancelledError;
        let cancelled = anyhow::Error::new(CancelledError).context("running //pkg:a");
        assert!(is_cancelled(&cancelled));
        let other = anyhow::anyhow!("boom").context("running //pkg:a");
        assert!(!is_cancelled(&other));
    }

    /// An error that writes its source into its own text, as object_store's do.
    #[derive(Debug)]
    struct Embeds(&'static str, anyhow::Error);

    impl std::fmt::Display for Embeds {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.0, self.1)
        }
    }

    impl std::error::Error for Embeds {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.1.as_ref())
        }
    }

    /// The CLI's own renderers write a cause its wrapper already repeats once,
    /// as `chain_message` does.
    #[test]
    fn a_cause_its_wrapper_repeats_is_rendered_once() {
        let embedded = || Embeds("Generic GCS error", anyhow::anyhow!("token refused"));
        let e = anyhow::Error::new(embedded()).context("open remote object k");
        let out = render_anyhow_to_string(&e, false).expect("rendered");
        assert_eq!(
            out,
            "× open remote object k\n╰─▶ Generic GCS error: token refused\n"
        );

        let a = crate::htaddr::parse_addr("//p:a").unwrap();
        let e = hplugin::diagnostic::with_stage(
            anyhow::Error::new(embedded()),
            hplugin::diagnostic::Boundary::Execute,
            &a,
        );
        assert_eq!(
            hplugin::diagnostic::cause_frames(&e).join(": "),
            "Generic GCS error: token refused"
        );
    }

    #[test]
    fn finish_exit_maps_cancelled_to_exit() {
        // Cancellation (no registry failures) → `cancelled`.
        let e = finish_exit(true).unwrap_err();
        assert_eq!(e.to_string(), "cancelled");
        assert!(is_cancelled(&e), "a CancelledError, so main exits 130");

        // Not cancelled → Ok.
        assert!(finish_exit(false).is_ok());
    }

    /// Two registry failures, wrapped in a context frame the way `finalize`'s
    /// caller chain wraps them.
    fn two_failures() -> Vec<Arc<TargetFailure>> {
        vec![
            failure("//simple_fail:d1", rec("//simple_fail:d1", "boom a")),
            failure("//simple_fail:d2", rec("//simple_fail:d2", "boom b")),
        ]
    }

    #[test]
    fn render_anyhow_reproduces_every_deferred_failure_box() {
        // Regression: in interactive TUI mode the registry failures used to be
        // printed mid-run inside a `paused!` block, where the resume re-anchor +
        // final viewport collapse wiped them. `finalize` now carries them out and
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
            "✗ //simple_fail:d1\n  boom a\n\n✗ //simple_fail:d2\n  boom b\n"
        );
    }

    /// U3: a culprit three deep names how the request reached it, under its
    /// cause; one that was itself requested gets no row.
    #[test]
    fn failure_box_shows_reached_by() {
        let mut deep = rec("//p:t", "boom");
        deep.entry = "//p:root".to_owned();
        deep.path = ["//p:root", "//p:mid", "//p:t"].map(str::to_owned).into();
        assert_eq!(
            render_failures_to_string(&[failure("//p:t", deep)], false),
            "✗ //p:t\n  boom\n  reached by: //p:root → //p:mid → //p:t\n"
        );

        assert_eq!(
            render_failures_to_string(&[failure("//p:t", rec("//p:t", "boom"))], false),
            "✗ //p:t\n  boom\n"
        );
    }

    #[test]
    fn render_anyhow_stays_quiet_for_failures_rendered_at_the_site() {
        // Non-interactive runs print the boxes at the failure site so they land
        // ahead of the summary and the background drain. The error still travels up
        // for the exit code, and `render_anyhow` must claim it — but print nothing,
        // or every failing CI run shows its diagnostics twice.
        let mut sink: Vec<u8> = Vec::new();
        let e = FailedTargets::reported(two_failures(), None, &HashSet::new(), &mut sink, false)
            .into_error();
        // The constructor is what printed them — the only way to make the `rendered`
        // promise — so the flag cannot drift from what is actually on the terminal.
        assert_eq!(
            String::from_utf8(sink).expect("utf8"),
            render_failures_to_string(&two_failures(), false),
            "the boxes go out at the failure site, in full"
        );
        assert_eq!(render_anyhow_to_string(&e, false), Some(String::new()));
        assert!(
            render_anyhow(&e, &SessionSlot::new()),
            "still counts as handled"
        );
    }

    #[test]
    fn empty_failed_targets_is_not_claimed_as_handled() {
        // `finalize` guards on a non-empty registry, so this is latent — but an
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

    /// `finalize` takes the app context and the request state through
    /// [`FinalizeCtx`] and [`FinalizeRequest`], so the branch it takes can be
    /// driven without an engine or a terminal.
    struct FakeCtx {
        interactive: bool,
        pauses: std::cell::Cell<usize>,
        session: crate::tui::session::SessionSlot,
        printed_live: HashSet<String>,
    }

    struct FakeGuard;

    impl FakeCtx {
        fn new(interactive: bool) -> Self {
            Self {
                interactive,
                pauses: std::cell::Cell::new(0),
                session: crate::tui::session::SessionSlot::new(),
                printed_live: HashSet::new(),
            }
        }
        /// A line backend that printed the live blocks of `addrs`.
        fn printed_live(mut self, addrs: &[&str]) -> Self {
            self.printed_live = addrs.iter().map(|a| (*a).to_owned()).collect();
            self
        }
    }

    impl FinalizeCtx for FakeCtx {
        fn interactive(&self) -> bool {
            self.interactive
        }
        async fn failures_printed(&self) -> HashSet<String> {
            self.printed_live.clone()
        }
        fn session(&self) -> &crate::tui::session::SessionSlot {
            &self.session
        }
        async fn pause(&self) -> impl Sized {
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

    /// All-cancelled collapses to the cancellation `finalize` recognises…
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

    struct FakeRs(
        std::cell::RefCell<Vec<Arc<TargetFailure>>>,
        std::cell::RefCell<Vec<crate::engine::event::BuildEventKind>>,
        /// The addrs whose failure was a stop.
        Vec<&'static str>,
    );

    impl FakeRs {
        fn new(failures: Vec<Arc<TargetFailure>>) -> Self {
            Self(
                std::cell::RefCell::new(failures),
                std::cell::RefCell::new(Vec::new()),
                Vec::new(),
            )
        }
    }

    impl FinalizeRequest for FakeRs {
        /// No engine to ask: every failure as recorded, wanted or not, as
        /// the real one returns them once the engine is gone.
        async fn take_failures_with_logs_where(
            &self,
            _wanted: impl Fn(&TargetFailure) -> bool,
        ) -> Vec<Arc<TargetFailure>> {
            std::mem::take(&mut *self.0.borrow_mut())
        }
        fn emit(&self, kind: crate::engine::event::BuildEventKind) {
            self.1.borrow_mut().push(kind);
        }
        fn log_tail_lines(&self) -> usize {
            crate::engine::Engine::DEFAULT_LOG_TAIL_LINES
        }
        fn was_cancelled(&self, addr: &str) -> bool {
            self.2.contains(&addr)
        }
        fn heph_home(&self) -> Option<std::path::PathBuf> {
            None
        }
        fn workspace_root(&self) -> Option<std::path::PathBuf> {
            None
        }
        fn fail_fast_stopped(&self) -> Option<FailFastStopped> {
            None
        }
    }

    /// A sink holding what one walk skipped: 7 go specs and one probe.
    fn some_gaps() -> Arc<crate::engine::Gaps> {
        use crate::engine::discovery::Stage;
        let gaps = crate::engine::Gaps::new("label(nix) && //...");
        for i in 0..7 {
            gaps.record(
                Stage::Spec,
                "go",
                format!("//svc/api:t{i}"),
                &anyhow::anyhow!("go list: exit status 1"),
            );
        }
        gaps.record(
            Stage::Probe,
            "",
            "//broken".to_string(),
            &anyhow::anyhow!("BUILD: syntax error"),
        );
        gaps
    }

    #[test]
    fn incomplete_selection_groups_by_stage_and_provider_and_caps_examples() {
        let report = some_gaps().report();
        assert_eq!(
            render_incomplete(&report, false, false),
            "! selection incomplete: 8 skipped while matching `label(nix) && //...`\n\
             \x20 1 package could not be read\n\
             \x20     //broken   BUILD: syntax error\n\
             \x20 7 targets could not be loaded (provider go)\n\
             \x20     //svc/api:t0   go list: exit status 1\n\
             \x20     //svc/api:t1   go list: exit status 1\n\
             \x20     //svc/api:t2   go list: exit status 1\n\
             \x20     //svc/api:t3   go list: exit status 1\n\
             \x20     //svc/api:t4   go list: exit status 1\n\
             \x20     ... 2 more\n\
             \x20 Targets that matched were still processed; the skipped ones were not, and may \
             also have matched.\n\
             \x20 Fix the cause and re-run, or pass --fail-fast to stop at the first one with its \
             full error.\n"
        );
    }

    /// STAGE-05: a failed package listing is a sentence of its own, named
    /// for its provider, and the header counts only the other skips.
    #[test]
    fn package_listing_failure_is_its_own_sentence_and_not_counted() {
        use crate::engine::discovery::Stage;
        let gaps = crate::engine::Gaps::new("//...");
        gaps.record(
            Stage::Packages,
            "go",
            "all packages".to_string(),
            &anyhow::anyhow!("go env: not found"),
        );
        gaps.record(
            Stage::Packages,
            "",
            "//vendor".to_string(),
            &anyhow::anyhow!("permission denied"),
        );
        let only_listings = gaps.report();
        assert_eq!(
            render_incomplete(&only_listings, false, false)
                .lines()
                .take(3)
                .collect::<Vec<_>>(),
            [
                "! selection incomplete while matching `//...`",
                "  a provider could not list packages under //vendor: permission denied",
                "  provider go could not list packages: go env: not found",
            ]
        );
        gaps.record(
            Stage::Spec,
            "go",
            "//svc:t".to_string(),
            &anyhow::anyhow!("go list: exit status 1"),
        );
        let err = IncompleteSelection {
            report: gaps.report(),
            cause: None,
            rendered: false,
        };
        assert_eq!(
            err.to_string(),
            "selection incomplete: 1 skipped while matching `//...`"
        );
    }

    #[tokio::test]
    async fn incomplete_selection_fails_the_command_when_every_target_passed() -> anyhow::Result<()>
    {
        for interactive in [true, false] {
            let ctx = FakeCtx::new(interactive);
            let rs = FakeRs::new(vec![]);
            let gaps = some_gaps();
            let res: anyhow::Result<u8> = Ok(1);
            let e = finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(()))
                .await
                .unwrap_err();
            let inc = downcast_chain_ref::<IncompleteSelection>(&e).expect("incomplete");
            assert_eq!(inc.rendered, !interactive);
            assert_eq!(inc.report.skipped, 8);
            assert_eq!(
                rs.1.borrow().len(),
                1,
                "one SelectionIncomplete event per command"
            );
            if interactive {
                let out = render_anyhow_to_string(&e, false).expect("claimed");
                assert!(out.starts_with("! selection incomplete"), "{out}");
            }
        }
        Ok(())
    }

    /// A body that fails on an incomplete selection does not end the command
    /// on its own error: the skips still fail it, as [`IncompleteSelection`],
    /// with the body's error as the cause rendered above the block.
    #[tokio::test]
    async fn incomplete_selection_wraps_a_body_error() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(vec![]);
        let gaps = some_gaps();
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize(&ctx, &rs, res, Some(&gaps), |_| {
            anyhow::bail!("writing --copy-out")
        })
        .await
        .unwrap_err();
        let inc = downcast_chain_ref::<IncompleteSelection>(&e).expect("incomplete");
        assert_eq!(inc.report.skipped, 8);
        let cause = inc.cause.as_ref().expect("the body's error is the cause");
        assert_eq!(cause.to_string(), "writing --copy-out");
        assert_eq!(ctx.pauses.get(), 1, "the body ran with the TUI paused");
        assert_eq!(rs.1.borrow().len(), 1, "one SelectionIncomplete event");
        let out = render_anyhow_to_string(&e, false).expect("claimed");
        let cause_at = out.find("writing --copy-out").expect("cause rendered");
        let block_at = out.find("! selection incomplete").expect("block rendered");
        assert!(
            cause_at < block_at,
            "the cause renders above the block: {out}"
        );
        Ok(())
    }

    #[test]
    fn empty_match_with_skips_reports_the_skips() {
        let empty = crate::engine::Gaps::new("//...");
        let Err(err) = require_non_empty_unless_incomplete(vec![], &empty) else {
            panic!("an empty run with nothing skipped must fail");
        };
        assert!(err.to_string().contains("no targets matched"), "{err}");
        let gaps = some_gaps();
        assert!(
            !gaps.report().nothing_matched,
            "unset until the command says"
        );
        assert!(
            require_non_empty_unless_incomplete(vec![], &gaps).is_ok(),
            "the skips are the answer; `finalize` reports them"
        );
        // STAGE-04: and the block they are reported in says nothing ran.
        let report = gaps.report();
        assert!(report.nothing_matched);
        let block = render_incomplete(&report, false, false);
        assert!(
            block.contains("\n  No target matched, so nothing ran.\n  Fix the cause"),
            "{block}"
        );
        assert!(!block.contains("still processed"), "{block}");
    }

    /// I8: after a Ctrl-C stop with two root failures seen, the interactive
    /// report prints and lists both; a target the stop ended is no block, and
    /// nothing says it stopped.
    #[tokio::test]
    async fn report_after_ctrl_c_lists_every_root() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(true);
        let mut failures = two_failures();
        failures.push(Arc::new(TargetFailure::new(crate::htaddr::parse_addr(
            "//simple_fail:stopped",
        )?)));
        let mut rs = FakeRs::new(failures);
        rs.2.push("//simple_fail:stopped");
        let res: anyhow::Result<()> = Err(anyhow::Error::new(CancelledError));
        let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
            .await
            .unwrap_err();
        let out = render_anyhow_to_string(&e, false).expect("claimed");
        // The report draws each failure's record; these were never compacted,
        // so their records name only the target.
        assert!(out.contains("\n✗ //simple_fail:d1\n"), "{out}");
        assert!(out.contains("\n✗ //simple_fail:d2\n"), "{out}");
        assert!(!out.contains("//simple_fail:stopped"), "{out}");
        assert!(!out.contains("stopped"), "{out}");
        Ok(())
    }

    /// A stop with no root failure: the report has no block and no line.
    #[tokio::test]
    async fn report_after_ctrl_c_without_a_root_is_empty() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(true);
        let failures = vec![Arc::new(TargetFailure::new(crate::htaddr::parse_addr(
            "//simple_fail:stopped",
        )?))];
        let mut rs = FakeRs::new(failures);
        rs.2.push("//simple_fail:stopped");
        let res: anyhow::Result<()> = Err(anyhow::Error::new(CancelledError));
        let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
            .await
            .unwrap_err();
        let out = render_anyhow_to_string(&e, false).unwrap_or_default();
        assert!(!out.contains("✗"), "{out}");
        assert!(!out.contains("stopped"), "{out}");
        Ok(())
    }

    #[tokio::test]
    async fn points_at_failures_only_when_one_is_registered() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(two_failures());
        let gaps = some_gaps();
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(()))
            .await
            .unwrap_err();
        let out = render_anyhow_to_string(&e, false).expect("claimed");
        // Interactive: the report's blocks, not the boxes.
        let boxes = out.find("\n✗ ").expect("report blocks first");
        let block = out.find("! selection incomplete").expect("then the block");
        assert!(boxes < block, "{out}");
        // One block per failure: the report alone, no boxes besides.
        assert_eq!(out.matches("✗ ").count(), 2, "{out}");
        assert!(
            out.contains("Target failures above may be the cause."),
            "{out}"
        );

        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(vec![]);
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(()))
            .await
            .unwrap_err();
        let out = render_anyhow_to_string(&e, false).expect("claimed");
        assert!(!out.contains("Target failures above"), "{out}");
        Ok(())
    }

    /// A genuine error alongside skips: the error renders first, then the
    /// block, and neither hides the other.
    #[tokio::test]
    async fn a_genuine_error_with_skips_renders_both() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(true);
        let rs = FakeRs::new(vec![]);
        let gaps = some_gaps();
        let res: anyhow::Result<u8> = Err(anyhow::anyhow!("boom"));
        let e = finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(()))
            .await
            .unwrap_err();
        let out = render_anyhow_to_string(&e, false).expect("claimed");
        let boom = out.find("boom").expect("the error");
        let block = out.find("! selection incomplete").expect("the block");
        assert!(boom < block, "{out}");
        Ok(())
    }

    /// Non-interactive, the boxes and the block print together at the failure
    /// site, boxes first, with the pointer back at them.
    #[test]
    fn reported_failures_print_the_block_after_the_boxes() {
        let mut sink: Vec<u8> = Vec::new();
        let ft = FailedTargets::reported(
            two_failures(),
            Some(some_gaps().report()),
            &HashSet::new(),
            &mut sink,
            false,
        );
        let out = String::from_utf8(sink).expect("utf8");
        let boxes = out.find("✗ ").expect("boxes");
        let block = out.find("! selection incomplete").expect("block");
        assert!(boxes < block, "{out}");
        assert!(
            out.contains("Target failures above may be the cause."),
            "{out}"
        );

        // Every failure printed live: the block alone, still pointing up at
        // them, with no blank line standing in for the boxes.
        let mut sink: Vec<u8> = Vec::new();
        drop(FailedTargets::reported(
            two_failures(),
            Some(some_gaps().report()),
            &["//simple_fail:d1", "//simple_fail:d2"]
                .map(str::to_owned)
                .into(),
            &mut sink,
            false,
        ));
        let out = String::from_utf8(sink).expect("utf8");
        assert!(out.starts_with("! selection incomplete"), "{out}");
        assert!(!out.contains("✗ "), "{out}");
        assert!(
            out.contains("Target failures above may be the cause."),
            "{out}"
        );
        let e = ft.into_error();
        assert_eq!(
            render_anyhow_to_string(&e, false).as_deref(),
            Some(""),
            "already printed; render_anyhow must stay quiet"
        );
    }

    /// Ctrl-C cuts a selection short on purpose: the command exits as
    /// cancelled, not as incomplete, and says nothing about the skips.
    #[tokio::test]
    async fn a_cancelled_command_with_skips_exits_as_cancelled() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(false);
        let rs = FakeRs::new(vec![]);
        let gaps = some_gaps();
        let res: anyhow::Result<u8> = Err(anyhow::Error::new(CancelledError));
        let e = finalize(&ctx, &rs, res, Some(&gaps), |_| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "cancelled");
        assert!(rs.1.borrow().is_empty());
        Ok(())
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
        let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
            .await
            .unwrap_err();
        let ft = downcast_chain_ref::<FailedTargets>(&e).expect("carried out");
        assert!(!ft.rendered, "interactive defers to render_anyhow");
        assert_eq!(ft.failures().len(), 2);
        assert_eq!(ctx.pauses.get(), 0, "no pause on the failure path");

        // Non-interactive: no viewport, nothing repaints stderr — render here so
        // the boxes precede the end-of-run summary and the background drain.
        let ctx = FakeCtx::new(false);
        let rs = FakeRs::new(two_failures());
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
            .await
            .unwrap_err();
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
            let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
                .await
                .unwrap_err();
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
        finalize(&ctx, &rs, res, None, |v| {
            got = v;
            Ok(())
        })
        .await
        .unwrap();
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
        let e = finalize(&ctx, &rs, res, None, |_| {
            ran = true;
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), "cancelled");
        assert!(!ran);
        Ok(())
    }

    /// I8: the CI box gains `log: heph log <addr>` as its very last line,
    /// after the fixes, exactly where a `full log:` line is printed; a
    /// failure with no kept log (its tail in memory only) gets none.
    #[test]
    fn ci_report_names_heph_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = exited(dir.path(), "//web:lint", &["line"]);
        let path = r
            .left_behind
            .as_ref()
            .and_then(|l| l.log_path.clone())
            .expect("log path");
        if let Some(l) = r.left_behind.as_mut() {
            l.fixes = Some(FixesData::NotWritten {
                reason: "killed by signal 9".to_owned(),
            });
        }
        let rendered = render_failures_to_string(&[failure("//web:lint", r.clone())], false);
        assert!(
            rendered.ends_with(&format!(
                "  full log: {path}\n  fixes not written: killed by signal 9\n  log: heph log //web:lint\n"
            )),
            "{rendered}"
        );

        // The engine's answer decides: only the last line moves.
        if let Some(l) = r.left_behind.as_mut() {
            l.log_readable = Some(false);
        }
        let none = render_failures_to_string(&[failure("//web:lint", r)], false);
        assert_eq!(
            none,
            rendered.replace("  log: heph log //web:lint\n", "  log: none kept\n"),
            "{none}"
        );
    }

    /// The end of a non-interactive run prints no box for a failure whose
    /// live block the line backend printed — the block carried its tail and
    /// its rows — and the full box, tail framed, for one it did not; both
    /// stay in the error for the exit code.
    #[test]
    fn ci_end_leaves_out_a_failure_printed_live() {
        let dir = tempfile::tempdir().unwrap();
        let failed = |addr: &str| failure(addr, exited(dir.path(), addr, &["first", "last words"]));
        let (live, not_live) = (failed("//web:lint"), failed("//web:test"));

        let mut sink: Vec<u8> = Vec::new();
        let ft = FailedTargets::reported(
            vec![Arc::clone(&live), Arc::clone(&not_live)],
            None,
            &HashSet::from(["//web:lint".to_owned()]),
            &mut sink,
            false,
        );
        assert_eq!(
            String::from_utf8(sink).expect("utf8"),
            render_failures_to_string(&[not_live], false),
        );
        assert_eq!(ft.failures().len(), 2, "the exit code counts both");

        let boxed = render_failures_to_string(&[live], false);
        assert!(boxed.contains("│ last words"), "{boxed}");
        assert!(boxed.contains("  log: heph log //web:lint\n"), "{boxed}");
    }

    /// `finalize` on the line backend asks which failures were printed live
    /// before taking them: a failure printed live is taken all the same, so
    /// the run still exits non-zero for it.
    #[tokio::test]
    async fn finalize_counts_a_failure_printed_live() -> anyhow::Result<()> {
        let ctx = FakeCtx::new(false).printed_live(&["//simple_fail:d1", "//simple_fail:d2"]);
        let rs = FakeRs::new(two_failures());
        let res: anyhow::Result<u8> = Ok(1);
        let e = finalize(&ctx, &rs, res, None, |_| Ok(()))
            .await
            .unwrap_err();
        let ft = downcast_chain_ref::<FailedTargets>(&e).expect("carried out");
        assert!(ft.rendered);
        assert_eq!(ft.failures().len(), 2);
        assert!(downcast_chain_ref::<TargetFailure>(&e).is_some(), "{e:#}");
        Ok(())
    }

    /// The CI box: the cause line is heph's message, so it names workspace
    /// paths relative and shows a control byte escaped; the log box is the
    /// tool's output and keeps both as printed (D2). The interactive report
    /// does the same through `message` (`report.rs`).
    #[test]
    fn relpath_in_cause_not_in_log_box() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path().to_path_buf();
        let build = root.join("a/BUILD");
        let said = format!("{}:1: tool said so", build.display());
        // Failed in the run, its log kept, but not an exit: the message is
        // the box's cause line.
        let mut r = exited(ws.path(), "//p:t", &[&said]);
        r.exit = None;
        r.diagnostic.message = format!("reading {}: denied\x1b[2J", build.display());
        let mut sink: Vec<u8> = Vec::new();
        drop(FailedTargets::reported_in(
            vec![failure("//p:t", r)],
            None,
            &HashSet::new(),
            &mut sink,
            false,
            BoxRequest {
                root: Some(root),
                ..BoxRequest::default()
            },
        ));
        let rendered = String::from_utf8(sink).expect("utf8");
        assert!(
            rendered.contains("\n  reading a/BUILD: denied\\x1b[2J\n"),
            "{rendered}"
        );
        assert!(rendered.contains(&format!("│ {said}\n")), "{rendered}");
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
    }

    /// A process that exited: the exit in the header, its log's lines
    /// numbered under the message.
    #[test]
    fn renders_target_failure_with_log_box() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = vec!["stuff"; 8];
        lines.push("not gucci");
        let r = exited(dir.path(), "//simple_fail:d1", &lines);
        let log = r
            .left_behind
            .as_ref()
            .and_then(|l| l.log_path.clone())
            .expect("log path");
        let rendered = render_failures_to_string(&[failure("//simple_fail:d1", r)], false);
        let expected = format!(
            "\
✗ //simple_fail:d1 · exit status 1
    ╭─[log · 9 lines]
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
  full log: {log}
  log: heph log //simple_fail:d1
"
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renders_failure_outputs_and_fixes() {
        const SLOT: &str = "/ws/.heph/failures/abc";
        let box_of = |outputs: Option<&str>, fixes: FixesData| {
            let mut r = rec("//web:lint", "boom");
            r.left_behind = Some(LeftBehindData {
                outputs: outputs.map(str::to_owned),
                fixes: Some(fixes),
                ..LeftBehindData::default()
            });
            render_failures_to_string(&[failure("//web:lint", r)], false)
        };
        let written = |paths: &[String], list: bool| FixesData::Written {
            root: "/my ws".to_owned(),
            paths: paths.to_vec(),
            list: list.then(|| format!("{SLOT}/fixes.txt")),
        };

        // Few fixes: every path, and a restore hint naming each one. It runs
        // git in the (quoted) workspace root, since the paths are
        // workspace-relative, and reads them literally: `[id]` is a glob to
        // git, which would also revert `web/i.ts`.
        let few = ["web/[id].ts".to_owned(), "web/my file.ts".to_owned()];
        let expected = "\
✗ //web:lint
  boom
  outputs: /ws/.heph/failures/abc/out
  fixes written, target still failed (re-run to verify; codegen = \"in_place_on_success\" opts out):
    web/[id].ts
    web/my file.ts
  restore with (takes the index version; uncommitted edits to these files are lost):
    git -C '/my ws' --literal-pathspecs checkout -- 'web/[id].ts' 'web/my file.ts'
";
        assert_eq!(
            box_of(Some(&format!("{SLOT}/out")), written(&few, true)),
            expected
        );

        // Twelve fixes: ten named, the rest counted, and the restore reads the
        // full list rather than the ten the box shows.
        let paths: Vec<String> = (0..12).map(|i| format!("web/f{i:02}.ts")).collect();
        let rendered = box_of(None, written(&paths, true));
        let mut expected = String::from(
            "✗ //web:lint\n  boom\n  fixes written, target still failed \
             (re-run to verify; codegen = \"in_place_on_success\" opts out):\n",
        );
        for p in &paths[..10] {
            expected.push_str(&format!("    {p}\n"));
        }
        expected.push_str(
            "    +2 more (full list: /ws/.heph/failures/abc/fixes.txt)\n  restore with (takes \
             the index version; uncommitted edits to these files are lost):\n    git -C '/my ws' \
             --literal-pathspecs checkout --pathspec-from-file=/ws/.heph/failures/abc/fixes.txt\n",
        );
        assert_eq!(rendered, expected);
        assert!(!rendered.contains("outputs:"), "{rendered}");

        // Twelve fixes and no list kept: the hint still names all twelve.
        let rendered = box_of(None, written(&paths, false));
        assert!(rendered.contains("    +2 more\n"), "{rendered}");
        assert!(
            rendered.ends_with(&format!(
                "    git -C '/my ws' --literal-pathspecs checkout -- {}\n",
                paths.join(" ")
            )),
            "{rendered}"
        );

        // A write-back that matched the tree already says nothing.
        assert_eq!(box_of(None, written(&[], false)), "✗ //web:lint\n  boom\n");

        // Nothing written: why, and no restore hint.
        let rendered = box_of(
            None,
            FixesData::NotWritten {
                reason: "killed by signal 9".to_owned(),
            },
        );
        assert!(
            rendered.ends_with("  fixes not written: killed by signal 9\n"),
            "{rendered}"
        );
        assert!(!rendered.contains("restore"), "{rendered}");
    }

    /// A failure before any run: its header and its message, nothing kept.
    #[test]
    fn renders_target_failure_without_log() {
        let out = render_failures_to_string(
            &[failure(
                "//pkg:a",
                rec("//pkg:a", "target not found: //pkg:a"),
            )],
            false,
        );
        assert_eq!(out, "✗ //pkg:a\n  target not found: //pkg:a\n");
    }

    /// A frozen check's record draws its diff under the message, not the
    /// raw multi-line error.
    #[test]
    fn renders_frozen_check_failure_with_diff_box() {
        let mut r = rec("//gen:proto", "generated output differs from tree");
        r.frozen_diff = Some("--- tree\n+++ generated\n-old line\n+new line\n".to_owned());
        let rendered = render_failures_to_string(&[failure("//gen:proto", r)], false);
        assert_eq!(
            rendered,
            "\
✗ //gen:proto · generated output differs from tree
  ╭─[diff]
  │ --- tree
  │ +++ generated
  │ -old line
  │ +new line
  ╰────
"
        );
    }

    /// A frozen-check error that reached `main` outside a `TargetFailure`
    /// is drawn as a box of its own, diff and all.
    #[test]
    fn renders_bare_frozen_check_error() {
        let e = anyhow::Error::new(FrozenCheckError {
            addr: crate::htaddr::parse_addr("//gen:proto").unwrap(),
            diff: "-old\n+new\n".to_owned(),
        });
        assert_eq!(
            render_anyhow_to_string(&e, false).as_deref(),
            Some(
                "✗ //gen:proto · generated output differs from tree\n  ╭─[diff]\n  │ -old\n  │ +new\n  ╰────\n"
            )
        );
    }

    /// The colour decided at the render reaches the box.
    #[test]
    fn color_reaches_the_box() {
        let failures = [failure("//pkg:a", rec("//pkg:a", "boom"))];
        assert!(render_failures_to_string(&failures, true).contains("\x1b["));
        assert!(!render_failures_to_string(&failures, false).contains('\x1b'));
    }
}
