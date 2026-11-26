use crate::tui::session::SessionSlot;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use std::io::Write;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::failure_gc::FailureGcStats;
use crate::engine::{Engine, GcStats};
use crate::tui::{self, App, AppContext, LogSink};

#[derive(clap::Args, Clone)]
pub struct GcArgs {
    /// Cap the total size of persistent scratch caches, e.g. `50GiB`. Least
    /// recently used caches are dropped whole until the store fits.
    ///
    /// Whole caches, never partial trims: heph cannot know which of a foreign
    /// tool's entries are hot, and guessing would degrade a cache while claiming
    /// to manage it. The next build repopulates what it actually needs.
    #[arg(long = "scratch-max-size", value_name = "SIZE")]
    pub scratch_max_size: Option<String>,
    /// Drop scratch caches untouched for longer than this many days.
    #[arg(long = "scratch-max-age-days", value_name = "DAYS")]
    pub scratch_max_age_days: Option<u64>,
    /// Drop a target's last-failure log (`<home>/failures/`) older than this
    /// many days. `0` drops every failure log. Logs of targets that no longer
    /// exist are dropped whatever their age.
    #[arg(
        long = "failure-max-age-days",
        value_name = "DAYS",
        default_value_t = 7
    )]
    pub failure_max_age_days: u64,
    #[command(flatten)]
    pub out: output::JsonArgs<GcView>,
}

/// `heph tool gc --json`: what the sweep removed.
#[derive(Debug, Default, serde::Serialize, output::Record)]
pub struct GcView {
    /// Targets whose spec no longer resolves; all their revisions were removed.
    orphan_targets_removed: usize,
    /// Cache revisions deleted (orphan drops and history trims).
    revisions_removed: usize,
    /// Cache revisions kept on live targets.
    revisions_kept: usize,
    /// Bytes freed by the deleted revisions.
    bytes_removed: u64,
    /// Targets that could not be processed; each is logged.
    errored: usize,
    /// Rows pruned from the filesystem-walk cache.
    fswalk_rows_removed: usize,
    /// Staged read-only input entries no surviving revision references.
    stage_entries_removed: usize,
    /// Entrypoint invocation sandboxes a killed heph left behind.
    invoke_sandboxes_removed: usize,
    /// Run-log directories of hephs that ended without removing them.
    run_log_dirs_removed: usize,
    /// Last-failure slots removed (`<home>/failures/`).
    failure_logs_removed: usize,
    /// Bytes those failure slots held.
    failure_log_bytes_removed: u64,
    /// Scratch caches dropped by the size and age budgets; `null` when the
    /// sweep failed (logged, never fatal).
    scratch_caches_removed: Option<usize>,
    /// Bytes those scratch caches held; `null` when the sweep failed.
    scratch_bytes_removed: Option<u64>,
}

impl GcView {
    fn new(stats: &GcStats, failures: &FailureGcStats, scratch: Option<(usize, u64)>) -> Self {
        Self {
            orphan_targets_removed: stats.orphan_targets_removed,
            revisions_removed: stats.revisions_removed,
            revisions_kept: stats.revisions_kept,
            bytes_removed: stats.bytes_removed,
            errored: stats.errored,
            fswalk_rows_removed: stats.fswalk_rows_removed,
            stage_entries_removed: stats.stage_entries_removed,
            invoke_sandboxes_removed: stats.invoke_sandboxes_removed,
            run_log_dirs_removed: stats.run_log_dirs_removed,
            failure_logs_removed: failures.removed,
            failure_log_bytes_removed: failures.bytes_removed,
            scratch_caches_removed: scratch.map(|(n, _)| n),
            scratch_bytes_removed: scratch.map(|(_, bytes)| bytes),
        }
    }
}

struct GcApp {
    engine: Arc<Engine>,
    out: output::JsonArgs<GcView>,
    fail_fast: bool,
    /// Cap for the whole scratch store; `None` leaves it unbounded.
    scratch_max_bytes: Option<u64>,
    /// Drop a scratch cache untouched for longer than this. `None` never ages
    /// one out.
    scratch_max_age: Option<std::time::Duration>,
    /// Drop a failure log older than this; zero drops every one.
    failure_max_age: std::time::Duration,
}

#[async_trait]
impl App for GcApp {
    type Output = ();
    type TuiView = tui::TuiProgressView;
    type CiView = tui::GcCiView;

    fn tui_view(&self) -> Self::TuiView {
        tui::TuiProgressView::with_header(Box::new(tui::GcHeader::new("GC")))
    }

    fn ci_view(&self) -> Self::CiView {
        tui::GcCiView::new("GC")
    }

    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        // GC resolves every cached target via `get_spec`, which may run
        // providers (filesystem walks, Starlark) and record failures in `rs`.
        let rs = self
            .engine
            .new_state_with_events(self.fail_fast, ctx.event_sender());
        let res = self.engine.clone().gc_all(rs.clone()).await;

        // Last-failure logs: nothing but this sweep removes the slot of a
        // target that is never built again. Never fatal; the progress view
        // appends its count to the summary.
        let failures = self
            .engine
            .clone()
            .gc_failure_slots(&rs, self.failure_max_age)
            .await;
        tracing::debug!(?failures, "swept failure slots");

        // Scratch caches are swept here too, because nothing else bounds them:
        // they are keyed by a declaration rather than by an input hash, so there
        // is no `hashin` to age out and no `cache.history` to trim against. A
        // sweep that only reclaimed target revisions would leave the one part of
        // the store that grows without limit.
        //
        // Reported separately and never fatal — reclaiming scratch is an
        // optimization, and failing a GC because one directory would not be
        // removed would be a worse outcome than leaving it.
        let scratch = match self
            .engine
            .scratch_sweep(self.scratch_max_bytes, self.scratch_max_age)
        {
            Ok(swept) => Some(swept),
            Err(err) => {
                tracing::warn!(error = %err, "sweeping scratch caches");
                None
            }
        };
        if let Some((n, freed)) = scratch.filter(|&(n, _)| n > 0 && !self.out.enabled()) {
            writeln!(
                std::io::stdout(),
                "Swept {n} scratch cache(s), freed {freed} bytes."
            )
            .context("writing the scratch sweep")?;
        }
        // Plugin checksum stamps are keyed by the plugin's path, so a plugin
        // that was removed or moved leaves its stamp behind. Tiny, but nothing
        // else ever removes one. Never fatal, for the same reason as above.
        match crate::engine::sweep_checksum_stamps() {
            Ok(removed) => tracing::debug!(removed, "swept stale plugin checksum stamps"),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "sweeping plugin checksum stamps")
            }
        }
        // The progress view renders the sweep summary (TUI final line / CI finish);
        // no extra print here, which would duplicate it. Under the output flags
        // the record is the summary on stdout; the view's line stays on stderr.
        let out = self.out;
        let cancelled = || rs.ctoken().is_cancelled();
        crate::commands::errors::finalize(&ctx, &rs, res, None, |stats| {
            if !out.enabled() {
                return Ok(());
            }
            out.record(&GcView::new(&stats, &failures, scratch), &cancelled)
        })
        .await
    }
}

pub fn execute(
    args: &GcArgs,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: &GlobalOptions,
) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, session, global.clone()))?
}

async fn execute_async(
    _args: GcArgs,
    sink: LogSink,
    session: &Arc<SessionSlot>,
    global: GlobalOptions,
) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine(&global, session)?;
    // Under the output flags stdout carries the record alone (I1).
    let interactive = tui::should_use_tui(global.no_tui) && !_args.out.enabled();
    let app = GcApp {
        engine,
        out: _args.out.clone(),
        fail_fast: global.fail_fast,
        scratch_max_bytes: _args
            .scratch_max_size
            .as_deref()
            .map(hcore::units::parse_size)
            .transpose()?,
        scratch_max_age: _args
            .scratch_max_age_days
            .map(|d| std::time::Duration::from_secs(d * 24 * 60 * 60)),
        failure_max_age: std::time::Duration::from_secs(
            _args.failure_max_age_days.saturating_mul(24 * 60 * 60),
        ),
    };
    tui::run_app(app, sink, session, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_keys_subset_of_fields() -> anyhow::Result<()> {
        let view = GcView::new(
            &GcStats::default(),
            &FailureGcStats::default(),
            Some((2, 1024)),
        );
        assert_eq!(view.scratch_bytes_removed, Some(1024));
        output::check_keys(&view)
    }
}
