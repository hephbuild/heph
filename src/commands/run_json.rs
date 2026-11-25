//! `heph run --json`: one entry per matched target, as each one finishes.
//!
//! The entries are folded from the request's own event stream, which the run
//! already produces for the progress display: a matched target's entry is
//! written at its `ResultEnd`. Nothing here exists without `--json`. The run
//! hands the request a channel of its own only then, and this fold is the
//! only reader of it; a plain run builds no fold, keeps no per-target state
//! and forwards nothing (I4).
//!
//! The list is closed only when it is whole (D10): the matcher finished, the
//! walk skipped nothing, the run was not cancelled, and every matched target
//! has its entry. Fail-fast stopping the walk, Ctrl-C (exit 130) or a gap
//! leaves it without its `]`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use hcore::events::{BuildEvent, BuildEventKind, CacheOutcomeKind, EventReceiver, EventSender};
use serde::Serialize;

use crate::engine::request_state::RequestState;
use crate::engine::{Engine, OutputMatcher, ResultOptions};
use crate::htaddr::Addr;

/// How a matched target ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Its result came from a cache; nothing ran.
    Cached,
    /// It ran and succeeded.
    Ran,
    /// It failed: its own run, or resolving it.
    Failed,
    /// It did not run because a dependency failed (`blocked_by`).
    Blocked,
    /// The run was stopped (Ctrl-C) while it was pending.
    Cancelled,
}

/// Where a target's result was looked up, and what came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTier {
    /// The local cache had it.
    Local,
    /// A remote cache had it (`remote` names which).
    Remote,
    /// No cache had it, so it ran.
    Miss,
}

/// What a failed run left in its failure slot, as `heph inspect failure`
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunFailure {
    /// The failed run's full process log; null when it kept none.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub log: Option<PathBuf>,
    /// Its `failure_out` matches; null when none matched.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub out: Option<PathBuf>,
    /// The list of files an `in_place` write-back changed; null when none.
    #[serde(serialize_with = "output::ser::opt_path")]
    pub fixes: Option<PathBuf>,
}

/// `heph run --json`: one element per matched target, in the order they
/// finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, output::Record)]
pub struct RunEntry {
    /// The target's address.
    pub addr: String,
    /// How it ended: cached, ran, failed, blocked or cancelled.
    pub outcome: Outcome,
    /// The cache lookup: local, remote or miss; null when no cache was
    /// consulted (caching off, `--force`, or it failed before the lookup).
    pub cache: Option<CacheTier>,
    /// The name of the remote cache a `remote` hit came from; null otherwise.
    pub remote: Option<String>,
    /// From the start of its resolution to its end, in milliseconds.
    pub duration_ms: u64,
    /// Its process's exit code: 0 when it ran and succeeded, null when
    /// nothing ran or a signal ended it.
    pub exit_code: Option<i32>,
    /// It failed because its run outlasted its `timeout`.
    pub timed_out: bool,
    /// Why it failed, on one line (the whole cause chain); null on success.
    pub error: Option<String>,
    /// For `blocked`: the failed dependency at the root of it.
    pub blocked_by: Option<String>,
    /// What its failed run left behind (as `heph inspect failure`); null
    /// unless it failed running.
    pub failure: Option<RunFailure>,
    /// Its output files, the set `--list-out` prints (narrowed by
    /// `--output`). Fetches the outputs of a remote hit.
    #[record(opt_in)]
    #[serde(serialize_with = "opt_paths")]
    pub outputs: Option<Vec<PathBuf>>,
    /// Its input hash.
    #[record(built)]
    pub hashin: Option<String>,
}

fn opt_paths<S: serde::Serializer>(ps: &Option<Vec<PathBuf>>, s: S) -> Result<S::Ok, S::Error> {
    match ps {
        Some(ps) => output::ser::paths(ps, s),
        None => s.serialize_none(),
    }
}


/// What the fold knows of one target while it resolves.
#[derive(Debug, Default)]
struct Live {
    start_ms: Option<u64>,
    ran: bool,
    cache: Option<CacheTier>,
    remote: Option<String>,
}

/// The request's events, folded into one entry per matched target.
///
/// A matched target is usually announced (`Matched`) before it ends, but a
/// provider can build a selected target as a dependency before the walk
/// reports it: such an entry waits in `early` until its `Matched`, and
/// `early` is dropped once the matched set is complete.
#[derive(Debug, Default)]
struct Fold {
    live: HashMap<String, Live>,
    /// Matched and not ended yet.
    pending: HashSet<String>,
    /// Ended before being matched (only while the set is incomplete).
    early: HashMap<String, RunEntry>,
    /// Entries already handed out.
    done: HashSet<String>,
    matched_complete: bool,
}

impl Fold {
    /// Whether events about `addr` can still matter.
    fn tracks(&self, addr: &str) -> bool {
        !self.done.contains(addr) && (!self.matched_complete || self.pending.contains(addr))
    }

    /// Fold one event; the entries it completes, in order.
    fn on_event(&mut self, ev: &BuildEvent) -> Vec<RunEntry> {
        match &ev.kind {
            BuildEventKind::Matched { addrs, complete } => {
                let mut ready = Vec::new();
                for addr in addrs {
                    if self.done.contains(addr) {
                        continue;
                    }
                    match self.early.remove(addr) {
                        Some(entry) => {
                            self.done.insert(addr.clone());
                            ready.push(entry);
                        }
                        None => {
                            self.pending.insert(addr.clone());
                        }
                    }
                }
                if *complete {
                    self.matched_complete = true;
                    self.early = HashMap::new();
                    let pending = &self.pending;
                    self.live.retain(|addr, _| pending.contains(addr));
                }
                ready
            }
            BuildEventKind::ResultStart { addr } if self.tracks(addr) => {
                let live = self.live.entry(addr.clone()).or_default();
                live.start_ms.get_or_insert(ev.at_unix_ms);
                Vec::new()
            }
            BuildEventKind::ExecuteStart { addr, .. } if self.tracks(addr) => {
                self.live.entry(addr.clone()).or_default().ran = true;
                Vec::new()
            }
            BuildEventKind::CacheOutcome { addr, outcome, .. } if self.tracks(addr) => {
                self.live.entry(addr.clone()).or_default().cache = match outcome {
                    CacheOutcomeKind::LocalHit => Some(CacheTier::Local),
                    CacheOutcomeKind::RemoteHit => Some(CacheTier::Remote),
                    CacheOutcomeKind::Miss => Some(CacheTier::Miss),
                    CacheOutcomeKind::Unknown => None,
                };
                Vec::new()
            }
            // The hit's blobs were gone: it is rebuilt, so it was a miss.
            BuildEventKind::CacheHitRebuilt { addr, .. } if self.tracks(addr) => {
                let live = self.live.entry(addr.clone()).or_default();
                live.cache = Some(CacheTier::Miss);
                live.remote = None;
                Vec::new()
            }
            BuildEventKind::RemoteCacheHit { addr, cache } if self.tracks(addr) => {
                self.live.entry(addr.clone()).or_default().remote = cache.clone();
                Vec::new()
            }
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_code,
                timed_out,
                cancelled,
                ..
            } if self.tracks(addr) => {
                let live = self.live.remove(addr).unwrap_or_default();
                let outcome = match (error, upstream_of) {
                    (Some(_), _) if *cancelled => Outcome::Cancelled,
                    (Some(_), Some(_)) => Outcome::Blocked,
                    (Some(_), None) => Outcome::Failed,
                    (None, _) if live.ran => Outcome::Ran,
                    (None, _) => Outcome::Cached,
                };
                let exit_code = match outcome {
                    Outcome::Ran => Some(0),
                    Outcome::Failed => *exit_code,
                    _ => None,
                };
                let entry = RunEntry {
                    addr: addr.clone(),
                    outcome,
                    remote: (live.cache == Some(CacheTier::Remote))
                        .then_some(live.remote)
                        .flatten(),
                    cache: live.cache,
                    duration_ms: live
                        .start_ms
                        .map_or(0, |start| ev.at_unix_ms.saturating_sub(start)),
                    exit_code,
                    timed_out: timed_out.is_some(),
                    error: error.clone(),
                    blocked_by: (outcome == Outcome::Blocked)
                        .then(|| upstream_of.clone())
                        .flatten(),
                    failure: None,
                    outputs: None,
                    hashin: None,
                };
                if self.pending.remove(addr) {
                    self.done.insert(addr.clone());
                    vec![entry]
                } else {
                    // Not matched (yet): a dependency, or a selected target
                    // the walk has not reported. `tracks` held, so the set is
                    // still incomplete.
                    self.early.entry(addr.clone()).or_insert(entry);
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Every matched target has its entry, and the matched set is final.
    fn whole(&self) -> bool {
        self.matched_complete && self.pending.is_empty()
    }
}

/// What the fold needs to fill the fields an event does not carry.
pub struct Sources<'a> {
    pub engine: &'a Arc<Engine>,
    pub rs: &'a Arc<RequestState>,
    pub out: &'a output::JsonArgs<RunEntry>,
    /// What the run asked of each matched target's outputs: `outputs` reads
    /// the same set.
    pub outputs: &'a OutputMatcher,
    pub opts: &'a ResultOptions,
}

impl Sources<'_> {
    /// The fields of `entry` that read something beyond the event, each only
    /// when it is printed (I4).
    async fn complete(&self, entry: &mut RunEntry) -> anyhow::Result<()> {
        let failed = entry.outcome == Outcome::Failed;
        let wants_outputs = !failed
            && entry.outcome != Outcome::Blocked
            && entry.outcome != Outcome::Cancelled
            && self.out.wants(RunEntry::OUTPUTS);
        let wants_hashin = self.out.wants(RunEntry::HASHIN);
        if !(failed && self.out.wants(RunEntry::FAILURE)) && !wants_outputs && !wants_hashin {
            return Ok(());
        }
        let addr = crate::htaddr::parse_addr(&entry.addr)
            .with_context(|| format!("parsing {} back", entry.addr))?;
        if failed && self.out.wants(RunEntry::FAILURE) {
            entry.failure = failure_of(self.engine, &addr)?;
        }
        if wants_outputs {
            entry.outputs = Some(self.output_paths(&addr).await?);
        }
        if wants_hashin {
            // Memoized: the run already hashed it. Null when it could not be
            // (a failed resolution, a failed dependency).
            entry.hashin = Arc::clone(self.engine)
                .meta(Arc::clone(self.rs), &addr)
                .await
                .ok()
                .map(|m| m.hashin);
        }
        Ok(())
    }

    /// The paths `--list-out` prints for `addr`. The run resolved it with the
    /// same outputs, so this is its memoized result.
    async fn output_paths(&self, addr: &Addr) -> anyhow::Result<Vec<PathBuf>> {
        let result = Arc::clone(self.engine)
            .result_addr(Arc::clone(self.rs), addr, self.outputs.clone(), self.opts)
            .await
            .with_context(|| format!("reading the outputs of {}", addr.format()))?;
        let mut paths = Vec::new();
        for a in &result.artifacts {
            for e in a.walk()? {
                paths.push(e?.path);
            }
        }
        Ok(paths)
    }
}

/// The failure slot of `addr`, as `heph inspect failure` reads it.
fn failure_of(engine: &Engine, addr: &Addr) -> anyhow::Result<Option<RunFailure>> {
    let view = crate::commands::inspect::failure::failure_view(engine.failure_slots(), addr)?;
    Ok(view.map(|v| RunFailure {
        log: v.log,
        out: v.out,
        fixes: v.fixes,
    }))
}

/// How the fold ended.
pub struct Collected<T> {
    /// The run's own result.
    pub res: anyhow::Result<T>,
    /// The events still to come (background work after the run): the caller
    /// forwards them, or drops them.
    pub events: EventReceiver,
    /// stdout was closed: the request was cancelled, and the command exits 0.
    pub stopped: bool,
    /// Every matched target has its entry and the matched set is final.
    pub whole: bool,
    /// Writing to stdout failed (not a closed pipe).
    pub write_error: Option<anyhow::Error>,
}

/// Run `run` while folding `events` (the request's own stream) into `list`,
/// forwarding each event, untouched, to `forward` (the progress display).
///
/// Every event emitted before `run` returned is folded: the channel is
/// drained once it has. A closed stdout cancels the request; the run then
/// winds down and the fold keeps forwarding.
pub async fn collect<T, W: Write>(
    run: impl Future<Output = anyhow::Result<T>>,
    mut events: EventReceiver,
    forward: Option<EventSender>,
    src: &Sources<'_>,
    list: &mut output::ListWriter<'_, RunEntry, W>,
) -> Collected<T> {
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let run = async move {
        let res = run.await;
        // The receiver lives until the fold has drained, so this cannot
        // fail; there is nothing to do if it did.
        if done_tx.send(()).is_err() {
            tracing::debug!("run --json: the fold ended before the run");
        }
        res
    };
    let mut fold = Fold::default();
    let mut stopped = false;
    let mut write_error = None;
    let folding = async {
        let mut finished = false;
        loop {
            let ev = if finished {
                match events.try_recv() {
                    Ok(ev) => ev,
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    biased;
                    ev = events.recv() => match ev {
                        Some(ev) => ev,
                        None => break,
                    },
                    _ = &mut done_rx => {
                        finished = true;
                        continue;
                    }
                }
            };
            if !stopped && write_error.is_none() {
                for mut entry in fold.on_event(&ev) {
                    let pushed = match src.complete(&mut entry).await {
                        Ok(()) => list.push(&entry).and_then(|flow| {
                            // Flushed per entry: a reader sees each target as
                            // it finishes, not when the buffer fills.
                            Ok(if flow == output::Flow::Stop {
                                flow
                            } else {
                                list.flush()?
                            })
                        }),
                        Err(e) => Err(e),
                    };
                    match pushed {
                        Ok(output::Flow::Continue) => {}
                        Ok(output::Flow::Stop) => {
                            stopped = true;
                            src.rs.ctoken().cancel();
                            break;
                        }
                        Err(e) => {
                            write_error = Some(e);
                            src.rs.ctoken().cancel();
                            break;
                        }
                    }
                }
            }
            if let Some(tx) = &forward {
                // A display that went away only stops being fed.
                drop(tx.send(ev));
            }
        }
    };
    let (res, ()) = tokio::join!(run, folding);
    Collected {
        res,
        events,
        stopped,
        whole: fold.whole(),
        write_error,
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

    fn matched(addrs: &[&str], complete: bool) -> BuildEventKind {
        BuildEventKind::Matched {
            addrs: addrs.iter().map(|a| (*a).to_string()).collect(),
            complete,
        }
    }

    fn start(addr: &str) -> BuildEventKind {
        BuildEventKind::ResultStart {
            addr: addr.to_string(),
        }
    }

    fn end(addr: &str, error: Option<&str>, upstream_of: Option<&str>) -> BuildEventKind {
        BuildEventKind::ResultEnd {
            addr: addr.to_string(),
            error: error.map(str::to_string),
            upstream_of: upstream_of.map(str::to_string),
            exit_status: error.map(|_| "exit status: 3".to_string()),
            exit_code: error.map(|_| 3),
            log_tail: None,
            timed_out: None,
            cancelled: false,
            left_behind: None,
        }
    }

    fn outcome(addr: &str, outcome: CacheOutcomeKind) -> BuildEventKind {
        BuildEventKind::CacheOutcome {
            addr: addr.to_string(),
            outcome,
            reason: None,
            stale_host_path: None,
        }
    }

    fn execute(addr: &str) -> BuildEventKind {
        BuildEventKind::ExecuteStart {
            addr: addr.to_string(),
            driver: "bash".to_string(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
            output_groups: Vec::new(),
            log_not_streamed: false,
        }
    }

    fn fold(events: Vec<BuildEvent>) -> (Vec<RunEntry>, bool) {
        let mut f = Fold::default();
        let entries = events.iter().flat_map(|e| f.on_event(e)).collect();
        (entries, f.whole())
    }

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&RunEntry {
            addr: "//p:t".to_string(),
            outcome: Outcome::Failed,
            cache: Some(CacheTier::Remote),
            remote: Some("r2".to_string()),
            duration_ms: 3,
            exit_code: Some(1),
            timed_out: true,
            error: Some("boom".to_string()),
            blocked_by: Some("//p:d".to_string()),
            failure: Some(RunFailure {
                log: Some(PathBuf::from("/h/log.txt")),
                out: None,
                fixes: None,
            }),
            outputs: Some(vec![PathBuf::from("o.txt")]),
            hashin: Some("h".to_string()),
        })
        .unwrap();
    }

    /// Each outcome, the cache tier and remote name, the duration, and the
    /// entry order: as each matched target ends, never a dependency.
    #[test]
    fn outcomes_from_events() {
        let (entries, whole) = fold(vec![
            ev(
                0,
                matched(&["//p:hit", "//p:ran", "//p:bad", "//p:blk"], true),
            ),
            ev(1, start("//p:dep")),
            ev(2, end("//p:dep", None, None)),
            ev(3, start("//p:hit")),
            ev(
                4,
                BuildEventKind::RemoteCacheHit {
                    addr: "//p:hit".to_string(),
                    cache: Some("r2".to_string()),
                },
            ),
            ev(4, outcome("//p:hit", CacheOutcomeKind::RemoteHit)),
            ev(10, end("//p:hit", None, None)),
            ev(11, start("//p:ran")),
            ev(12, outcome("//p:ran", CacheOutcomeKind::Miss)),
            ev(12, execute("//p:ran")),
            ev(20, end("//p:ran", None, None)),
            ev(21, start("//p:bad")),
            ev(22, execute("//p:bad")),
            ev(30, end("//p:bad", Some("process exited"), None)),
            ev(31, start("//p:blk")),
            ev(
                32,
                end("//p:blk", Some("dependency failed"), Some("//p:bad")),
            ),
        ]);
        assert!(whole);
        let got: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.addr.as_str(),
                    e.outcome,
                    e.cache,
                    e.remote.as_deref(),
                    e.duration_ms,
                    e.exit_code,
                    e.blocked_by.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "//p:hit",
                    Outcome::Cached,
                    Some(CacheTier::Remote),
                    Some("r2"),
                    7,
                    None,
                    None
                ),
                (
                    "//p:ran",
                    Outcome::Ran,
                    Some(CacheTier::Miss),
                    None,
                    9,
                    Some(0),
                    None
                ),
                ("//p:bad", Outcome::Failed, None, None, 9, Some(3), None),
                (
                    "//p:blk",
                    Outcome::Blocked,
                    None,
                    None,
                    1,
                    None,
                    Some("//p:bad")
                ),
            ]
        );
    }

    /// A selected target built as a dependency before the walk reports it
    /// still gets its entry, once, when it is matched.
    #[test]
    fn ended_before_matched() {
        let (entries, whole) = fold(vec![
            ev(0, matched(&[], false)),
            ev(1, start("//p:a")),
            ev(2, outcome("//p:a", CacheOutcomeKind::LocalHit)),
            ev(3, end("//p:a", None, None)),
            ev(4, start("//p:a")),
            ev(5, end("//p:a", None, None)),
            ev(6, matched(&["//p:a"], false)),
            ev(7, matched(&[], true)),
        ]);
        assert!(whole);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].outcome, Outcome::Cached);
        assert_eq!(entries[0].cache, Some(CacheTier::Local));
    }

    /// The list is whole only when the matched set is final and every member
    /// ended: a walk fail-fast stopped, or a target that never ended, is not.
    #[test]
    fn whole_needs_every_matched_target() {
        let (_, whole) = fold(vec![ev(0, matched(&["//p:a"], false))]);
        assert!(!whole, "the set is not final");
        let (_, whole) = fold(vec![ev(0, matched(&["//p:a", "//p:b"], true))]);
        assert!(!whole, "no target ended");
        let (entries, whole) = fold(vec![ev(0, matched(&[], true))]);
        assert!(whole && entries.is_empty(), "zero matches is a whole `[]`");
    }

    /// A stop is `cancelled`, a rebuilt hit a miss, a timeout says so.
    #[test]
    fn cancelled_rebuilt_and_timed_out() {
        let mut stop = end("//p:c", Some("cancelled"), None);
        if let BuildEventKind::ResultEnd { cancelled, .. } = &mut stop {
            *cancelled = true;
        }
        let mut late = end("//p:t", Some("timed out"), None);
        if let BuildEventKind::ResultEnd { timed_out, .. } = &mut late {
            *timed_out = Some(hcore::events::TimedOutData {
                timeout_ms: 10,
                abandoned: false,
            });
        }
        let (entries, _) = fold(vec![
            ev(0, matched(&["//p:c", "//p:r", "//p:t"], true)),
            ev(1, stop),
            ev(2, outcome("//p:r", CacheOutcomeKind::LocalHit)),
            ev(
                3,
                BuildEventKind::CacheHitRebuilt {
                    addr: "//p:r".to_string(),
                    was: hcore::events::CacheHitKind::Local,
                },
            ),
            ev(4, execute("//p:r")),
            ev(5, end("//p:r", None, None)),
            ev(6, late),
        ]);
        assert_eq!(entries[0].outcome, Outcome::Cancelled);
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(
            (entries[1].outcome, entries[1].cache),
            (Outcome::Ran, Some(CacheTier::Miss))
        );
        assert_eq!(entries[2].outcome, Outcome::Failed);
        assert!(entries[2].timed_out);
    }
}
