//! A GitHub Actions hook: folds the engine's build-event stream into a status
//! tally and periodically rewrites the job's `$GITHUB_STEP_SUMMARY` markdown
//! (done/total, built, cached, failed, slow targets). Published out-of-process as
//! a cdylib (see `plugin-gha-cdylib`) and enabled via a config `plugins:` entry.
//!
//! The aggregation is intentionally self-contained (a small [`Tally`]) rather than
//! reusing the TUI's `BuildState`: the renderer's aggregator is coupled to ratatui
//! and lives in the (terminal) `tui` crate, and a hook only needs a handful of
//! counts plus the in-flight set for slow detection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hcore::events::{BuildEvent, BuildEventKind, now_unix_ms};
use hplugin::config::{Options, decode_opt, deny_unknown};
use hplugin::hook::Hook;

/// A target executing longer than this (ms) is surfaced in the "slow targets"
/// section of the summary.
const SLOW_THRESHOLD_MS: u64 = 10_000;

/// The folded build status. Only what the summary renders: matched/finished sets
/// for progress, cache-hit + built counts, the failure list, and the in-flight
/// execute start times for slow detection.
#[derive(Default)]
struct Tally {
    matched: BTreeSet<String>,
    matched_complete: bool,
    finished: BTreeSet<String>,
    built: usize,
    cache_hit: BTreeSet<String>,
    failed: Vec<(String, Option<String>)>,
    /// addr -> `ExecuteStart` timestamp; removed on `ExecuteEnd`. The remaining
    /// entries are the currently-running targets.
    running: BTreeMap<String, u64>,
}

impl Tally {
    fn apply(&mut self, ev: &BuildEvent) {
        match &ev.kind {
            BuildEventKind::Matched { addrs, complete } => {
                for a in addrs {
                    self.matched.insert(a.clone());
                }
                if *complete {
                    self.matched_complete = true;
                }
            }
            BuildEventKind::ResultEnd { addr, error } => {
                self.finished.insert(addr.clone());
                if let Some(e) = error {
                    self.failed.push((addr.clone(), Some(e.clone())));
                }
            }
            BuildEventKind::ExecuteStart { addr, .. } => {
                self.running.insert(addr.clone(), ev.at_unix_ms);
            }
            BuildEventKind::ExecuteEnd { addr, error } => {
                self.running.remove(addr);
                if error.is_none() {
                    self.built += 1;
                }
            }
            BuildEventKind::LocalCacheHit { addr } | BuildEventKind::RemoteCacheHit { addr } => {
                self.cache_hit.insert(addr.clone());
            }
            _ => {}
        }
    }

    /// `(done, total)` over the matched top-level set: done = matched targets that
    /// have finished. `total` is provisional until the matcher resolves.
    fn progress(&self) -> (usize, usize) {
        let done = self
            .matched
            .iter()
            .filter(|a| self.finished.contains(*a))
            .count();
        (done, self.matched.len())
    }

    fn cached_count(&self) -> usize {
        self.matched
            .iter()
            .filter(|a| self.cache_hit.contains(*a))
            .count()
    }

    /// Targets still executing past [`SLOW_THRESHOLD_MS`], slowest first.
    fn slow(&self, now_ms: u64) -> Vec<(String, u64)> {
        let mut slow: Vec<(String, u64)> = self
            .running
            .iter()
            .filter_map(|(addr, start)| {
                let elapsed = now_ms.saturating_sub(*start);
                (elapsed >= SLOW_THRESHOLD_MS).then(|| (addr.clone(), elapsed))
            })
            .collect();
        slow.sort_by(|a, b| b.1.cmp(&a.1));
        slow
    }

    /// Render the GitHub-Actions step-summary markdown for the current tally.
    fn render_markdown(&self, now_ms: u64) -> String {
        let (done, total) = self.progress();
        let total_str = if self.matched_complete {
            total.to_string()
        } else {
            format!("~{total}")
        };
        let mut out = String::new();
        out.push_str("## heph build\n\n");
        out.push_str(&format!(
            "**Targets:** {done} / {total_str} &nbsp;•&nbsp; **built:** {} &nbsp;•&nbsp; **cached:** {} &nbsp;•&nbsp; **failed:** {}\n",
            self.built,
            self.cached_count(),
            self.failed.len(),
        ));

        let slow = self.slow(now_ms);
        if !slow.is_empty() {
            out.push_str("\n### Slow targets\n\n| target | running for |\n| --- | --- |\n");
            for (addr, elapsed) in slow {
                out.push_str(&format!("| `{addr}` | {}s |\n", elapsed / 1000));
            }
        }

        if !self.failed.is_empty() {
            out.push_str("\n### Failed\n\n");
            for (addr, err) in &self.failed {
                match err {
                    Some(e) => out.push_str(&format!(
                        "- `{addr}` — {}\n",
                        e.lines().next().unwrap_or("")
                    )),
                    None => out.push_str(&format!("- `{addr}`\n")),
                }
            }
        }
        out
    }
}

struct Inner {
    tally: Mutex<Tally>,
    /// Where to write the summary; `None` disables writing (warned once at build).
    summary_path: Option<PathBuf>,
    /// Set by `on_close` so the periodic writer thread exits.
    stop: AtomicBool,
}

impl Inner {
    fn write_summary(&self) {
        let Some(path) = &self.summary_path else {
            return;
        };
        let markdown = {
            let tally = self.tally.lock().unwrap_or_else(|e| e.into_inner());
            tally.render_markdown(now_unix_ms())
        };
        // Atomic: write a temp sibling then rename, so a reader never sees a
        // half-written summary.
        let tmp = path.with_extension("heph-tmp");
        if std::fs::write(&tmp, markdown.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// The GitHub Actions build-status hook.
pub struct GhaHook {
    inner: Arc<Inner>,
}

impl GhaHook {
    /// Build from the plugin's `options:` map. Reads `refreshSecs` (default 30)
    /// and an optional `summaryPath` override; absent the override it targets
    /// `$GITHUB_STEP_SUMMARY`. Spawns a background thread that rewrites the
    /// summary every `refreshSecs` until [`Hook::on_close`].
    pub fn from_options(opts: &Options) -> anyhow::Result<Self> {
        deny_unknown("gha hook", opts, &["refreshSecs", "summaryPath"])?;
        let refresh_secs: u64 = decode_opt(opts, "gha hook", "refreshSecs")?
            .unwrap_or(30)
            .max(1);
        let summary_path = decode_opt::<String>(opts, "gha hook", "summaryPath")?
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("GITHUB_STEP_SUMMARY").map(PathBuf::from));
        if summary_path.is_none() {
            tracing::warn!(
                "gha hook: neither `summaryPath` option nor $GITHUB_STEP_SUMMARY set; \
                 no summary will be written"
            );
        }

        let inner = Arc::new(Inner {
            tally: Mutex::new(Tally::default()),
            summary_path,
            stop: AtomicBool::new(false),
        });

        // Periodic writer. A plain thread (no async runtime) keeps the hook free
        // of runtime entanglement; it exits once `stop` is set by `on_close`.
        let t = Arc::clone(&inner);
        std::thread::spawn(move || {
            while !t.stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_secs(refresh_secs));
                if t.stop.load(Ordering::Acquire) {
                    break;
                }
                t.write_summary();
            }
        });

        Ok(Self { inner })
    }
}

impl Hook for GhaHook {
    fn name(&self) -> String {
        "gha".to_string()
    }

    fn on_event(&self, ev: &BuildEvent) {
        self.inner
            .tally
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply(ev);
    }

    fn on_close(&self) {
        // Stop the periodic writer and flush the final summary synchronously, so
        // the completed status is written before the plugin acks the host.
        self.inner.stop.store(true, Ordering::Release);
        self.inner.write_summary();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
    }

    fn scripted() -> Tally {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into(), "//a:y".into(), "//a:z".into()],
                complete: true,
            },
        ));
        // x: built
        t.apply(&ev(
            10,
            BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: true,
            },
        ));
        t.apply(&ev(
            20,
            BuildEventKind::ExecuteEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        t.apply(&ev(
            20,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        // y: cache hit, finished
        t.apply(&ev(
            15,
            BuildEventKind::LocalCacheHit {
                addr: "//a:y".into(),
            },
        ));
        t.apply(&ev(
            15,
            BuildEventKind::ResultEnd {
                addr: "//a:y".into(),
                error: None,
            },
        ));
        // z: started long ago, still running (slow), then a separate failure
        t.apply(&ev(
            0,
            BuildEventKind::ExecuteStart {
                addr: "//a:z".into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        t.apply(&ev(
            30,
            BuildEventKind::ResultEnd {
                addr: "//a:w".into(),
                error: Some("boom".into()),
            },
        ));
        t
    }

    #[test]
    fn markdown_reports_counts_slow_and_failures() {
        let t = scripted();
        // now far enough past //a:z's start to mark it slow.
        let md = t.render_markdown(SLOW_THRESHOLD_MS + 5_000);

        // 2 of 3 matched finished (x, y); w finished but isn't in the matched set.
        assert!(md.contains("2 / 3"), "progress: {md}");
        assert!(md.contains("**built:** 1"), "built: {md}");
        assert!(md.contains("**cached:** 1"), "cached: {md}");
        assert!(md.contains("**failed:** 1"), "failed: {md}");
        // //a:z is still running past the threshold.
        assert!(md.contains("### Slow targets"), "slow section: {md}");
        assert!(md.contains("//a:z"), "slow target listed: {md}");
        // failure surfaced with its message.
        assert!(md.contains("### Failed"), "failed section: {md}");
        assert!(
            md.contains("//a:w") && md.contains("boom"),
            "failure detail: {md}"
        );
    }

    #[test]
    fn provisional_total_until_matcher_complete() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: false,
            },
        ));
        assert!(t.render_markdown(0).contains("~1"), "provisional total");
    }

    #[test]
    fn on_close_writes_final_summary_to_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.md");
        let opts: Options = [(
            "summaryPath".to_string(),
            serde_yaml::Value::String(path.to_string_lossy().into_owned()),
        )]
        .into_iter()
        .collect();
        let hook = GhaHook::from_options(&opts).expect("hook");
        hook.on_event(&ev(
            0,
            BuildEventKind::Matched {
                addrs: vec!["//a:x".into()],
                complete: true,
            },
        ));
        hook.on_event(&ev(
            5,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        hook.on_close();

        let written = std::fs::read_to_string(&path).expect("summary written");
        assert!(written.contains("1 / 1"), "final summary: {written}");
    }
}
