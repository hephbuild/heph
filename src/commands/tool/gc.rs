use std::sync::Arc;

use async_trait::async_trait;

use crate::commands::GlobalOptions;
use crate::commands::bootstrap;
use crate::engine::Engine;
use crate::tui::{self, App, AppContext, LogSink};
use anyhow::Context as _;

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
}

/// Parse a human size (`50GiB`, `500MB`, `1024`) into bytes.
///
/// Accepts the binary units people actually type and treats the `B`-less forms
/// as the same thing, because `50G` meaning something different from `50GiB`
/// would be a trap and nobody wants decimal gigabytes for a disk cap.
fn parse_size(s: &str) -> anyhow::Result<u64> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits);
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s:?} does not start with a number"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024u64 * 1024 * 1024 * 1024,
        other => anyhow::bail!("unknown size unit {other:?} in {s:?}; try 50GiB, 500MiB, 1024"),
    };
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("size {s:?} overflows"))
}

struct GcApp {
    engine: Arc<Engine>,
    fail_fast: bool,
    /// Cap for the whole scratch store; `None` leaves it unbounded.
    scratch_max_bytes: Option<u64>,
    /// Drop a scratch cache untouched for longer than this. `None` never ages
    /// one out.
    scratch_max_age: Option<std::time::Duration>,
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

        // Scratch caches are swept here too, because nothing else bounds them:
        // they are keyed by a declaration rather than by an input hash, so there
        // is no `hashin` to age out and no `cache.history` to trim against. A
        // sweep that only reclaimed target revisions would leave the one part of
        // the store that grows without limit.
        //
        // Reported separately and never fatal — reclaiming scratch is an
        // optimization, and failing a GC because one directory would not be
        // removed would be a worse outcome than leaving it.
        match self
            .engine
            .scratch_sweep(self.scratch_max_bytes, self.scratch_max_age)
        {
            Ok((0, _)) => {}
            Ok((n, freed)) => println!("Swept {n} scratch cache(s), freed {freed} bytes."),
            Err(err) => tracing::warn!(error = %err, "sweeping scratch caches"),
        }
        // The progress view renders the sweep summary (TUI final line / CI finish);
        // no extra print here, which would duplicate it.
        crate::commands::errors::finalize!(ctx, rs, res, _stats => { Ok(()) })
    }
}

pub fn execute(args: &GcArgs, sink: LogSink, global: &GlobalOptions) -> anyhow::Result<()> {
    bootstrap::block_on(execute_async(args.clone(), sink, global.clone()))?
}

async fn execute_async(_args: GcArgs, sink: LogSink, global: GlobalOptions) -> anyhow::Result<()> {
    let (engine, shutdown) = bootstrap::new_engine()?;
    let app = GcApp {
        engine,
        fail_fast: global.fail_fast,
        scratch_max_bytes: _args
            .scratch_max_size
            .as_deref()
            .map(parse_size)
            .transpose()?,
        scratch_max_age: _args
            .scratch_max_age_days
            .map(|d| std::time::Duration::from_secs(d * 24 * 60 * 60)),
    };
    let interactive = tui::should_use_tui(global.no_tui);
    tui::run_app(app, sink, interactive, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_the_units_people_type() {
        assert_eq!(parse_size("1024").expect("plain"), 1024);
        assert_eq!(parse_size("1KiB").expect("kib"), 1024);
        // Binary throughout: `50GB` meaning something different from `50GiB`
        // would be a trap for a disk cap.
        assert_eq!(
            parse_size("2GB").expect("gb"),
            parse_size("2GiB").expect("gib")
        );
        assert_eq!(parse_size("50GiB").expect("gib"), 50 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 8 MiB ").expect("spaces"), 8 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_nonsense_with_a_usable_message() {
        let err = parse_size("lots").expect_err("not a number");
        assert!(format!("{err:#}").contains("number"));
        let err = parse_size("5 parsecs").expect_err("bad unit");
        let msg = format!("{err:#}");
        assert!(msg.contains("parsecs"), "{msg}");
        assert!(msg.contains("50GiB"), "must suggest a valid form: {msg}");
    }

    #[test]
    fn parse_size_does_not_overflow_silently() {
        assert!(parse_size("99999999999999999999TiB").is_err());
    }
}
