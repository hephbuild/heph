//! `heph run --json`: one entry per matched target, as each one finishes,
//! plus one per culprit outside the selection.
//!
//! The entries are folded from the request's own event stream, which the run
//! already produces for the progress display: a matched target's entry is
//! written at its `ResultEnd`. Nothing here exists without `--json`. The run
//! hands the request a channel of its own only then, and this fold is the
//! only reader of it; a plain run builds no fold, keeps no per-target state
//! and forwards nothing (I4).
//!
//! Output contract 2 (D5): every culprit has a record, `selected: false` when
//! the selection did not name it, and a BUILD file that did not evaluate is
//! one with `addr: null` and `file` set. A skipped target names its culprit
//! in `root` (a target) or `root_file` (a file), never both, with the full
//! `path` to it and the count of `more` culprits it reaches: those come from
//! the request-end pass over the dependency graph, so skipped records are
//! written once the run returns, after every culprit. `blocked_by` and the
//! `blocked` outcome are gone; `docs/JSON_OUTPUT.md`, "Migrating from
//! contract 1", is what a contract 1 reader changes.
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
use hplugin::error::{Culprit, TargetFailure};
use serde::Serialize;

use crate::engine::request_state::RequestState;
use crate::engine::{Engine, OutputMatcher, ResultOptions};
use crate::htaddr::Addr;
use crate::tui::text::{Redact, WorkspaceRel};

/// How a matched target ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, output::Shape)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Its result came from a cache; nothing ran.
    Cached,
    /// It ran and succeeded.
    Ran,
    /// It failed: its own run, or resolving it.
    Failed,
    /// It did not run because a culprit it depends on failed (`root` or
    /// `root_file`).
    Skipped,
    /// The run was stopped (Ctrl-C) while it was pending.
    Cancelled,
}

/// Where a target's result was looked up, and what came of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, output::Shape)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, output::Shape)]
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

/// `heph run --json`: one element per matched target and per culprit, in
/// the order they finish; a culprit before every record naming it as `root`
/// or `root_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, output::Record)]
pub struct RunEntry {
    /// The target's address; null for a BUILD file that did not evaluate
    /// (`file` names it). Read `.addr // .file`.
    pub addr: Option<String>,
    /// The workspace-relative BUILD or `.bzl` file that did not evaluate;
    /// null on every other record.
    pub file: Option<String>,
    /// How it ended: cached, ran, failed, skipped or cancelled.
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
    /// Why it failed, on one line; null on success and on `skipped`. Its
    /// wording is not contract.
    pub error: Option<String>,
    /// The culprit when it is a target: the record itself for a failed
    /// culprit, the nearest culprit for `skipped`. Always an address; null
    /// when the culprit is a file (`root_file`), and for ran, cached and
    /// cancelled.
    pub root: Option<String>,
    /// The culprit when it is a BUILD or `.bzl` file that did not evaluate:
    /// the file record's own `file`, or a skipped target's nearest culprit.
    /// Always a workspace-relative path; at most one of `root` and
    /// `root_file` is set, the pair mirroring `addr` and `file`.
    pub root_file: Option<String>,
    /// On `skipped`: how many culprits other than its own it reaches; null
    /// elsewhere.
    pub more: Option<u64>,
    /// On a failed culprit: a shortest path from a requested target to it,
    /// the one known when it failed (it can differ between runs in rare
    /// cases). On `skipped`: from this target to where its culprit was
    /// recorded, over the final graph. Inclusive and never elided; null
    /// elsewhere.
    pub path: Option<Vec<String>>,
    /// The selection named it. A culprit outside the selection is written
    /// too, with false; a file record is never selected.
    pub selected: bool,
    /// What its failed run left behind (as `heph inspect failure`); null
    /// unless it failed running.
    pub failure: Option<RunFailure>,
    /// Its output files, the set `--list-out` prints (narrowed by
    /// `--output`). Selecting it costs what `--list-out` does: a remote
    /// hit's outputs are downloaded, which the default fields never do.
    /// Neither a spec, a def nor a hash is added, so its tier is `free`, and
    /// the download is why it is not a default.
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

/// What the fold asks the request about a culprit: its recorded failure.
trait Culprits {
    fn failure(&self, c: &Culprit) -> Option<Arc<TargetFailure>>;
}

impl Culprits for RequestState {
    fn failure(&self, c: &Culprit) -> Option<Arc<TargetFailure>> {
        self.get_culprit_failure(c)
    }
}

/// An entry that ended before the matched set said whether it was selected.
#[derive(Debug)]
enum Early {
    /// Written when matched; at the end of matching, written with
    /// `selected: false` when it is a culprit (`root` set), else dropped.
    /// `seq` keeps the order they ended in.
    Record { seq: u64, entry: RunEntry },
    /// Held when matched, dropped otherwise.
    Skipped(Held),
}

/// A selected target's `skipped` record, waiting for the request-end pass
/// that gives it `root` or `root_file`, `path` and `more`.
#[derive(Debug)]
struct Held {
    entry: RunEntry,
    /// What its end named: the culprit target, or the address that led into
    /// `file`.
    of: String,
    file: Option<String>,
}

/// The request's events, folded into one entry per matched target and per
/// culprit.
///
/// A matched target is usually announced (`Matched`) before it ends, but a
/// provider can build a selected target as a dependency before the walk
/// reports it: such an entry waits in `early` until its `Matched`. Once the
/// matched set is complete, what is left there is a dependency: a culprit
/// among them is written with `selected: false`, and the rest is dropped.
#[derive(Debug, Default)]
struct Fold {
    /// Every target between its `ResultStart` and its `ResultEnd`: bounded
    /// by the targets in flight. A culprit's duration and cache come from it
    /// whether or not it was selected.
    live: HashMap<String, Live>,
    /// Matched and not ended yet.
    pending: HashSet<String>,
    /// Ended before being matched (only while the set is incomplete).
    early: HashMap<String, Early>,
    /// The order `early` entries came in.
    seq: u64,
    /// Addresses whose entry is handed out or held.
    done: HashSet<String>,
    /// Files whose record is handed out: one per file.
    files: HashSet<String>,
    /// Skipped records, written at request end.
    held: Vec<Held>,
    matched_complete: bool,
    /// A target ended `cancelled`: the run stopped it before it finished (a
    /// cancel, or `--fail-fast` after a failure), so the list is not whole
    /// however many records it has (RUN-24, JSON_OUTPUT.md).
    cut: bool,
    /// What a culprit's `error` line is scrubbed with, as the failure box
    /// is.
    redact: Redact,
    rel: WorkspaceRel,
}

/// A record for `addr` with nothing in it yet.
fn blank(addr: Option<String>, outcome: Outcome) -> RunEntry {
    RunEntry {
        addr,
        file: None,
        outcome,
        cache: None,
        remote: None,
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        error: None,
        root: None,
        root_file: None,
        more: None,
        path: None,
        selected: false,
        failure: None,
        outputs: None,
        hashin: None,
    }
}

/// `s` on one line: each line trimmed, the lines joined by a space.
fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl Fold {
    fn new(redact: Redact, rel: WorkspaceRel) -> Self {
        Self {
            redact,
            rel,
            ..Self::default()
        }
    }

    /// `text` as a record's `error`: one line, scrubbed as the box is.
    fn line(&self, text: &str) -> String {
        crate::tui::text::message(&one_line(text), &self.redact, &self.rel).into_owned()
    }

    /// A culprit's `error`: its recorded cause (the frames the box prints,
    /// without the engine's boundary frames), else what the event said.
    fn culprit_line(&self, f: Option<&TargetFailure>, event: Option<&str>) -> Option<String> {
        let frames = f.map(|f| hplugin::diagnostic::cause_frames(&f.source).join(": "));
        match frames.as_deref() {
            Some(text) if !text.is_empty() => Some(self.line(text)),
            _ => event.map(|e| self.line(e)),
        }
    }

    /// Fold one event; the entries it completes, in order.
    fn on_event(&mut self, ev: &BuildEvent, culprits: &dyn Culprits) -> Vec<RunEntry> {
        match &ev.kind {
            BuildEventKind::Matched { addrs, complete } => {
                let mut ready = Vec::new();
                for addr in addrs {
                    if self.done.contains(addr) {
                        continue;
                    }
                    match self.early.remove(addr) {
                        Some(Early::Record { mut entry, .. }) => {
                            self.done.insert(addr.clone());
                            entry.selected = true;
                            ready.push(entry);
                        }
                        Some(Early::Skipped(mut held)) => {
                            self.done.insert(addr.clone());
                            held.entry.selected = true;
                            self.held.push(held);
                        }
                        None => {
                            self.pending.insert(addr.clone());
                        }
                    }
                }
                if *complete {
                    self.matched_complete = true;
                    ready.extend(self.early_culprits());
                }
                ready
            }
            BuildEventKind::ResultStart { addr } => {
                let live = self.live.entry(addr.clone()).or_default();
                live.start_ms.get_or_insert(ev.at_unix_ms);
                Vec::new()
            }
            BuildEventKind::ExecuteStart { addr, .. } => {
                self.live.entry(addr.clone()).or_default().ran = true;
                Vec::new()
            }
            BuildEventKind::CacheOutcome { addr, outcome, .. } => {
                self.live.entry(addr.clone()).or_default().cache = match outcome {
                    CacheOutcomeKind::LocalHit => Some(CacheTier::Local),
                    CacheOutcomeKind::RemoteHit => Some(CacheTier::Remote),
                    CacheOutcomeKind::Miss => Some(CacheTier::Miss),
                    CacheOutcomeKind::Unknown => None,
                };
                Vec::new()
            }
            // The hit's blobs were gone: it is rebuilt, so it was a miss.
            BuildEventKind::CacheHitRebuilt { addr, .. } => {
                let live = self.live.entry(addr.clone()).or_default();
                live.cache = Some(CacheTier::Miss);
                live.remote = None;
                Vec::new()
            }
            BuildEventKind::RemoteCacheHit { addr, cache } => {
                self.live.entry(addr.clone()).or_default().remote = cache.clone();
                Vec::new()
            }
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                upstream_file,
                path,
                exit_code,
                timed_out,
                cancelled,
                duplicate,
                ..
            } => {
                let live = self.live.remove(addr).unwrap_or_default();
                if self.done.contains(addr) {
                    return Vec::new();
                }
                // A duplicate end closes a row another end (or a file's
                // record) already reported. One naming a file is a target of
                // that broken file the file's record was not entered
                // through: skipped because of its own file.
                if *duplicate {
                    if let Some(file) = upstream_file {
                        let held = Held {
                            entry: blank(Some(addr.clone()), Outcome::Skipped),
                            of: addr.clone(),
                            file: Some(file.clone()),
                        };
                        self.hold(addr, held);
                    }
                    return Vec::new();
                }
                let outcome = match (error, upstream_of) {
                    (Some(_), _) if *cancelled => Outcome::Cancelled,
                    (Some(_), Some(_)) => Outcome::Skipped,
                    (Some(_), None) => Outcome::Failed,
                    (None, _) if live.ran => Outcome::Ran,
                    (None, _) => Outcome::Cached,
                };
                self.cut |= outcome == Outcome::Cancelled;
                // Past the matched set, only a failure can still be a record:
                // a culprit's.
                if self.matched_complete
                    && outcome != Outcome::Failed
                    && !self.pending.contains(addr)
                {
                    return Vec::new();
                }
                let mut entry = blank(Some(addr.clone()), outcome);
                entry.remote = (live.cache == Some(CacheTier::Remote))
                    .then_some(live.remote)
                    .flatten();
                entry.cache = live.cache;
                entry.duration_ms = live
                    .start_ms
                    .map_or(0, |start| ev.at_unix_ms.saturating_sub(start));
                entry.exit_code = match outcome {
                    Outcome::Ran => Some(0),
                    Outcome::Failed => *exit_code,
                    _ => None,
                };
                entry.timed_out = timed_out.is_some();
                match (outcome, upstream_of) {
                    (Outcome::Skipped, Some(of)) => {
                        let held = Held {
                            entry,
                            of: of.clone(),
                            file: upstream_file.clone(),
                        };
                        self.hold(addr, held);
                        Vec::new()
                    }
                    (Outcome::Failed, _) => {
                        let failure = crate::htaddr::parse_addr(addr)
                            .ok()
                            .and_then(|a| culprits.failure(&Culprit::Target(a)));
                        if let Some(f) = &failure {
                            entry.root = Some(addr.clone());
                            entry.path = Some(if path.is_empty() {
                                f.path.iter().map(Addr::format).collect()
                            } else {
                                path.clone()
                            });
                        }
                        entry.error = self.culprit_line(failure.as_deref(), error.as_deref());
                        self.place(addr, entry)
                    }
                    _ => {
                        entry.error = error.as_deref().map(|e| self.line(e));
                        self.place(addr, entry)
                    }
                }
            }
            // A BUILD file that did not evaluate: its own record, once, and
            // the address that led into it is skipped because of it.
            BuildEventKind::FileFailed {
                file,
                entry,
                error,
                path,
            } => {
                let mut ready = Vec::new();
                if self.files.insert(file.clone()) {
                    let failure = culprits.failure(&Culprit::File(Arc::from(file.as_str())));
                    let mut rec = blank(None, Outcome::Failed);
                    rec.file = Some(file.clone());
                    rec.root_file = Some(file.clone());
                    rec.path = Some(match (&failure, path.is_empty()) {
                        (Some(f), true) => f.path.iter().map(Addr::format).collect(),
                        _ => path.clone(),
                    });
                    rec.error = self.culprit_line(failure.as_deref(), Some(error));
                    ready.push(rec);
                }
                if !self.done.contains(entry) {
                    let held = Held {
                        entry: blank(Some(entry.clone()), Outcome::Skipped),
                        of: entry.clone(),
                        file: Some(file.clone()),
                    };
                    self.hold(entry, held);
                }
                ready
            }
            _ => Vec::new(),
        }
    }

    /// Hand out `entry` if its target was matched, keep it while the matched
    /// set is incomplete, else hand it out as an unselected culprit or drop
    /// it.
    fn place(&mut self, addr: &str, mut entry: RunEntry) -> Vec<RunEntry> {
        if self.pending.remove(addr) {
            self.done.insert(addr.to_owned());
            entry.selected = true;
            vec![entry]
        } else if !self.matched_complete {
            // Not matched (yet): a dependency, or a selected target the walk
            // has not reported. Its first end is kept, unless a later one
            // makes it a culprit: a target that built and was then blamed
            // must still get its culprit record, which only `root` earns.
            self.seq += 1;
            let seq = self.seq;
            match self.early.entry(addr.to_owned()) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Early::Record { seq, entry });
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let earlier_no_culprit = matches!(
                        slot.get(),
                        Early::Record { entry: earlier, .. } if earlier.root.is_none()
                    );
                    if earlier_no_culprit && entry.root.is_some() {
                        slot.insert(Early::Record { seq, entry });
                    }
                }
            }
            Vec::new()
        } else if entry.root.is_some() {
            self.done.insert(addr.to_owned());
            vec![entry]
        } else {
            Vec::new()
        }
    }

    /// Hold a skipped record for `addr` if it was matched, keep it while the
    /// matched set is incomplete, else drop it: a skipped dependency has no
    /// record.
    fn hold(&mut self, addr: &str, mut held: Held) {
        if self.pending.remove(addr) {
            self.done.insert(addr.to_owned());
            held.entry.selected = true;
            self.held.push(held);
        } else if !self.matched_complete {
            self.early
                .entry(addr.to_owned())
                .or_insert(Early::Skipped(held));
        }
    }

    /// Empty `early`: its culprits, in the order they ended, with
    /// `selected: false`; the rest are dependencies and are dropped.
    fn early_culprits(&mut self) -> Vec<RunEntry> {
        let mut culprits: Vec<(u64, String, RunEntry)> = std::mem::take(&mut self.early)
            .into_iter()
            .filter_map(|(addr, e)| match e {
                Early::Record { seq, entry } if entry.root.is_some() => Some((seq, addr, entry)),
                Early::Record { .. } | Early::Skipped(_) => None,
            })
            .collect();
        culprits.sort_unstable_by_key(|(seq, ..)| *seq);
        culprits
            .into_iter()
            .map(|(_, addr, entry)| {
                self.done.insert(addr);
                entry
            })
            .collect()
    }

    /// The entries left once the run returned: the culprits still waiting
    /// on a matched set that never completed, then every held skipped
    /// record, filled from one request-end pass over the graph. Each culprit
    /// was written before this, so a culprit precedes its skipped records.
    fn finish(&mut self, rs: &RequestState) -> Vec<RunEntry> {
        let mut ready = if self.matched_complete {
            Vec::new()
        } else {
            self.early_culprits()
        };
        if self.held.is_empty() {
            return ready;
        }
        let held = std::mem::take(&mut self.held);
        let addrs: Vec<Option<Addr>> = held
            .iter()
            .map(|h| {
                h.entry
                    .addr
                    .as_deref()
                    .and_then(|a| crate::htaddr::parse_addr(a).ok())
            })
            .collect();
        // `skipped_report` reads each culprit, never its path: no path pass.
        let failures = rs.failures_unpathed();
        let report = rs.skipped_report(failures.iter().map(|f| &**f), addrs.iter().flatten());
        for (h, addr) in held.into_iter().zip(&addrs) {
            let mut entry = h.entry;
            match addr.as_ref().and_then(|a| report.get(a)) {
                Some(rec) => {
                    match rec.root {
                        Culprit::Target(a) => entry.root = Some(a.format()),
                        Culprit::File(f) => entry.root_file = Some(f.to_string()),
                    }
                    entry.path = Some(rec.path.iter().map(Addr::format).collect());
                    entry.more = Some(u64::try_from(rec.more).unwrap_or(u64::MAX));
                }
                // Not in the graph the pass walks: what the end named.
                None => {
                    let me = entry.addr.clone().unwrap_or_default();
                    entry.path = Some(if me == h.of {
                        vec![me]
                    } else {
                        vec![me, h.of.clone()]
                    });
                    match h.file {
                        Some(file) => entry.root_file = Some(file),
                        None => entry.root = Some(h.of),
                    }
                    entry.more = Some(0);
                }
            }
            ready.push(entry);
        }
        ready
    }

    /// Every matched target has its entry, the matched set is final, and
    /// none of them was stopped.
    fn whole(&self) -> bool {
        self.matched_complete && self.pending.is_empty() && !self.cut
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
        // A file's record has no target to read anything of (its failure
        // slot is PR 4's).
        let Some(addr) = &entry.addr else {
            return Ok(());
        };
        let failed = entry.outcome == Outcome::Failed;
        let wants_outputs = matches!(entry.outcome, Outcome::Ran | Outcome::Cached)
            && self.out.wants(RunEntry::OUTPUTS);
        let wants_hashin = self.out.wants(RunEntry::HASHIN);
        if !(failed && self.out.wants(RunEntry::FAILURE)) && !wants_outputs && !wants_hashin {
            return Ok(());
        }
        let addr =
            crate::htaddr::parse_addr(addr).with_context(|| format!("parsing {addr} back"))?;
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
        let listing = || format!("listing the outputs of {}", addr.format());
        let mut paths = Vec::new();
        for a in &result.artifacts {
            for e in a.walk().with_context(listing)? {
                paths.push(e.with_context(listing)?.path);
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
    /// Writing to stdout failed (not a closed pipe). The request was
    /// cancelled: there is nowhere left to write.
    pub write_error: Option<anyhow::Error>,
    /// The first entry whose fields could not be read (its `outputs`, say).
    /// That entry is not written, the list is not closed, and the build is
    /// not cancelled: the command fails with it once the build's own
    /// failures are rendered.
    pub field_error: Option<anyhow::Error>,
}

impl<T> Collected<T> {
    /// Whether the list is whole and may be closed (D10): every matched
    /// target has its entry, the walk skipped nothing (`gaps`), the run was
    /// not cancelled, and every entry was written. Otherwise it is left
    /// without its `]`.
    pub fn may_close(&self, gaps: &crate::engine::Gaps) -> bool {
        self.whole
            && gaps.is_empty()
            && !self.stopped
            && self.write_error.is_none()
            && self.field_error.is_none()
            && !self
                .res
                .as_ref()
                .err()
                .is_some_and(crate::commands::errors::is_cancelled)
    }
}

/// The command's result once `finalized` (what `finalize` made of the
/// build, its failures already rendered) is known.
///
/// A closed stdout (`stopped`) is exit 0 whatever the build did: the reader
/// has what it wanted. Otherwise the document's own error (a write error,
/// else a field error) is the command's, unless the build failed for itself
/// (failed targets, an incomplete selection): that failure keeps its exit and
/// the document's error is logged. A cancellation, which a write error
/// causes, is not such a failure.
pub fn exit(
    stopped: bool,
    write_error: Option<anyhow::Error>,
    field_error: Option<anyhow::Error>,
    finalized: anyhow::Result<()>,
) -> anyhow::Result<()> {
    if stopped {
        return Ok(());
    }
    let doc = match (write_error, field_error) {
        (Some(w), Some(f)) => {
            tracing::error!(error = %format!("{f:#}"), "reading a --json entry's fields");
            Some(w)
        }
        (w, f) => w.or(f),
    };
    let Some(doc) = doc else {
        return finalized;
    };
    match finalized {
        Ok(()) => Err(doc),
        Err(f)
            if f.downcast_ref::<crate::commands::errors::FailedTargets>()
                .is_some()
                || f.downcast_ref::<crate::commands::errors::IncompleteSelection>()
                    .is_some() =>
        {
            tracing::error!(error = %format!("{doc:#}"), "writing the --json document");
            Err(f)
        }
        Err(f) => {
            // Not a stop (`finish_exit`'s plain `cancelled` included): a
            // genuine error the document's would otherwise hide.
            if !crate::commands::errors::is_cancelled(&f) && f.to_string() != "cancelled" {
                tracing::error!(error = %format!("{f:#}"), "the run failed");
            }
            Err(doc)
        }
    }
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
    let mut fold = Fold::new(
        Redact::heph_home(&src.engine.home),
        src.rs
            .workspace_root()
            .as_deref()
            .map(WorkspaceRel::workspace)
            .unwrap_or_default(),
    );
    let mut w = Written::default();
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
            if w.open() {
                w.write(fold.on_event(&ev, &**src.rs), src, list).await;
            }
            if let Some(tx) = &forward {
                // A display that went away only stops being fed.
                drop(tx.send(ev));
            }
        }
        // Every event the run emitted is folded: what only the request's
        // end can say (the skipped records' root, path and more).
        if w.open() {
            w.write(fold.finish(src.rs), src, list).await;
        }
    };
    let (res, ()) = tokio::join!(run, folding);
    Collected {
        res,
        events,
        stopped: w.stopped,
        whole: fold.whole(),
        write_error: w.write_error,
        field_error: w.field_error,
    }
}

/// What writing the entries has come to.
#[derive(Default)]
struct Written {
    stopped: bool,
    write_error: Option<anyhow::Error>,
    field_error: Option<anyhow::Error>,
}

impl Written {
    /// Whether more entries may be written: stdout is open and took every
    /// write.
    fn open(&self) -> bool {
        !self.stopped && self.write_error.is_none()
    }

    /// Complete and write each of `entries`, until stdout closes or fails.
    async fn write<W: Write>(
        &mut self,
        entries: Vec<RunEntry>,
        src: &Sources<'_>,
        list: &mut output::ListWriter<'_, RunEntry, W>,
    ) {
        for mut entry in entries {
            // An entry whose fields cannot be read is not written, and the
            // list stays open (D10); the build goes on.
            if let Err(e) = src.complete(&mut entry).await {
                if self.field_error.is_none() {
                    self.field_error = Some(e);
                }
                continue;
            }
            let pushed = list.push(&entry).and_then(|flow| {
                // Flushed per entry: a reader sees each target as it
                // finishes, not when the buffer fills.
                Ok(if flow == output::Flow::Stop {
                    flow
                } else {
                    list.flush()?
                })
            });
            match pushed {
                Ok(output::Flow::Continue) => {}
                Ok(output::Flow::Stop) => {
                    self.stopped = true;
                    src.rs.ctoken().cancel();
                    return;
                }
                Err(e) => {
                    self.write_error = Some(e);
                    src.rs.ctoken().cancel();
                    return;
                }
            }
        }
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
            duplicate: false,
            upstream_file: None,
            path: Vec::new(),
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

    /// The culprits a test's request recorded.
    #[derive(Default)]
    struct Known(Vec<Arc<TargetFailure>>);

    impl Known {
        fn target(mut self, a: &str, e: &str) -> Self {
            let addr = crate::htaddr::parse_addr(a).expect("addr");
            self.0.push(Arc::new(TargetFailure::new(
                addr,
                None,
                anyhow::anyhow!("{e}"),
            )));
            self
        }
    }

    impl Culprits for Known {
        fn failure(&self, c: &Culprit) -> Option<Arc<TargetFailure>> {
            self.0.iter().find(|f| f.culprit() == *c).cloned()
        }
    }

    fn fold_with(events: Vec<BuildEvent>, known: &Known) -> (Vec<RunEntry>, Fold) {
        let mut f = Fold::default();
        let entries = events.iter().flat_map(|e| f.on_event(e, known)).collect();
        (entries, f)
    }

    fn fold(events: Vec<BuildEvent>) -> (Vec<RunEntry>, bool) {
        let (entries, f) = fold_with(events, &Known::default());
        (entries, f.whole())
    }

    fn with_path(mut end: BuildEventKind, p: &[&str]) -> BuildEventKind {
        if let BuildEventKind::ResultEnd { path, .. } = &mut end {
            *path = p.iter().map(|a| (*a).to_string()).collect();
        }
        end
    }

    fn file_failed(file: &str, entry: &str, path: &[&str]) -> BuildEventKind {
        BuildEventKind::FileFailed {
            file: file.to_string(),
            entry: entry.to_string(),
            error: "d/BUILD:3:1: fail: boom\nTraceback (most recent call last):".to_string(),
            path: path.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    fn addrs(entries: &[RunEntry]) -> Vec<(Option<&str>, Outcome, bool)> {
        entries
            .iter()
            .map(|e| (e.addr.as_deref(), e.outcome, e.selected))
            .collect()
    }

    #[test]
    fn record_keys_subset_of_fields() {
        output::check_keys(&RunEntry {
            addr: Some("//p:t".to_string()),
            file: Some("p/BUILD".to_string()),
            outcome: Outcome::Failed,
            cache: Some(CacheTier::Remote),
            remote: Some("r2".to_string()),
            duration_ms: 3,
            exit_code: Some(1),
            timed_out: true,
            error: Some("boom".to_string()),
            root: Some("//p:d".to_string()),
            root_file: Some("p/BUILD".to_string()),
            more: Some(1),
            path: Some(vec!["//p:t".to_string()]),
            selected: true,
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
    /// entry order: as each matched target ends, never a dependency. A
    /// culprit is its own root, with its path; a skipped target is held for
    /// the request-end pass.
    #[test]
    fn outcomes_from_events() {
        let known = Known::default().target("//p:bad", "process exited");
        let (entries, f) = fold_with(
            vec![
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
                ev(
                    30,
                    with_path(end("//p:bad", Some("process exited"), None), &["//p:bad"]),
                ),
                ev(31, start("//p:blk")),
                ev(
                    32,
                    end("//p:blk", Some("dependency failed"), Some("//p:bad")),
                ),
            ],
            &known,
        );
        assert!(f.whole());
        let got: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.addr.as_deref().expect("a target"),
                    e.outcome,
                    e.cache,
                    e.remote.as_deref(),
                    e.duration_ms,
                    e.exit_code,
                    e.root.as_deref(),
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
                (
                    "//p:bad",
                    Outcome::Failed,
                    None,
                    None,
                    9,
                    Some(3),
                    Some("//p:bad")
                ),
            ]
        );
        assert!(entries.iter().all(|e| e.selected), "{entries:?}");
        let bad = &entries[2];
        assert_eq!(bad.path.as_deref(), Some(&["//p:bad".to_string()][..]));
        assert_eq!(bad.more, None, "more is a skipped record's");
        assert_eq!(bad.error.as_deref(), Some("process exited"));
        assert!(entries[0].root.is_none() && entries[0].path.is_none());
        let held: Vec<_> = f.held.iter().map(|h| h.entry.addr.as_deref()).collect();
        assert_eq!(held, [Some("//p:blk")], "skipped: held for the request end");
        assert_eq!(f.held[0].entry.duration_ms, 1);
        assert!(f.held[0].entry.error.is_none(), "{:?}", f.held[0].entry);
    }

    /// A culprit outside the selection is written once matching completes,
    /// in the order the culprits ended, with `selected: false`; a failed or
    /// skipped dependency that is no culprit is dropped. After the set is
    /// complete a culprit is written at its end.
    #[test]
    fn unselected_culprits_after_matching() {
        let known = Known::default()
            .target("//p:c1", "c1 broke")
            .target("//p:c2", "c2 broke")
            .target("//p:c3", "c3 broke");
        let (entries, f) = fold_with(
            vec![
                ev(0, matched(&["//p:top"], false)),
                ev(1, start("//p:c2")),
                ev(2, end("//p:c2", Some("c2 broke"), None)),
                ev(3, start("//p:c1")),
                ev(4, end("//p:c1", Some("c1 broke"), None)),
                // A cycle: failed, but nobody's culprit.
                ev(5, end("//p:cyc", Some("cycle"), None)),
                ev(6, end("//p:mid", Some("dependency failed"), Some("//p:c1"))),
                ev(7, end("//p:okdep", None, None)),
                ev(8, matched(&[], true)),
                ev(9, end("//p:c3", Some("c3 broke"), None)),
                ev(10, end("//p:late", Some("cycle"), None)),
                ev(
                    11,
                    end("//p:top", Some("dependency failed"), Some("//p:c1")),
                ),
            ],
            &known,
        );
        assert_eq!(
            addrs(&entries),
            [
                (Some("//p:c2"), Outcome::Failed, false),
                (Some("//p:c1"), Outcome::Failed, false),
                (Some("//p:c3"), Outcome::Failed, false),
            ]
        );
        assert_eq!(entries[1].duration_ms, 1, "its own live state");
        assert_eq!(entries[1].root.as_deref(), Some("//p:c1"));
        assert!(f.whole());
        assert_eq!(f.held.len(), 1, "only the selected skipped target");
        assert!(f.held[0].entry.selected);
    }

    /// A BUILD file that did not evaluate: one record, `addr` and `root`
    /// null, `file` and `root_file` the file, never selected, however many
    /// targets it took down; the requested target that led into it is held
    /// as skipped.
    #[test]
    fn file_failed_one_record_and_holds_its_entry() {
        let dup = |a: &str, file: Option<&str>| {
            let mut dup = end(a, Some("eval"), None);
            if let BuildEventKind::ResultEnd {
                duplicate,
                upstream_file,
                ..
            } = &mut dup
            {
                *duplicate = true;
                *upstream_file = file.map(str::to_string);
            }
            dup
        };
        let (entries, f) = fold_with(
            vec![
                ev(0, matched(&["//d:t1", "//d:t3"], true)),
                ev(1, file_failed("d/BUILD", "//d:t1", &["//d:t1"])),
                ev(2, file_failed("d/BUILD", "//d:t2", &["//d:t2"])),
                ev(3, dup("//d:t1", Some("d/BUILD"))),
                // Another requested target of the same file: its duplicate
                // end names the file, so it is skipped because of it.
                ev(4, dup("//d:t3", Some("d/BUILD"))),
            ],
            &Known::default(),
        );
        assert_eq!(entries.len(), 1, "{entries:?}");
        let rec = &entries[0];
        assert_eq!(rec.addr, None);
        assert_eq!(rec.file.as_deref(), Some("d/BUILD"));
        assert_eq!(rec.root_file.as_deref(), Some("d/BUILD"));
        assert_eq!(rec.root, None, "a file is never an address");
        assert_eq!(rec.path.as_deref(), Some(&["//d:t1".to_string()][..]));
        assert!(!rec.selected);
        assert_eq!(
            rec.error.as_deref(),
            Some("d/BUILD:3:1: fail: boom Traceback (most recent call last):"),
            "one line"
        );
        assert!(f.whole());
        let held: Vec<_> = f
            .held
            .iter()
            .map(|h| (h.entry.addr.as_deref(), h.of.as_str(), h.file.as_deref()))
            .collect();
        assert_eq!(
            held,
            [
                (Some("//d:t1"), "//d:t1", Some("d/BUILD")),
                (Some("//d:t3"), "//d:t3", Some("d/BUILD"))
            ]
        );
    }

    /// A culprit's `error` is its recorded cause on one line, not the
    /// event's flattened chain, with control bytes escaped.
    #[test]
    fn culprit_error_is_its_cause_on_one_line() {
        let addr = crate::htaddr::parse_addr("//p:bad").expect("addr");
        let e = anyhow::anyhow!("line one\n  line two\u{1b}[31m").context("outer");
        let known = Known(vec![Arc::new(TargetFailure::new(addr, None, e))]);
        let (entries, _) = fold_with(
            vec![
                ev(0, matched(&["//p:bad"], true)),
                ev(
                    1,
                    end(
                        "//p:bad",
                        Some("get_def: //p:bad: outer: line one\n  line two"),
                        None,
                    ),
                ),
            ],
            &known,
        );
        let error = entries[0].error.as_deref().expect("an error");
        assert!(
            !error.contains('\n') && !error.contains('\u{1b}'),
            "{error:?}"
        );
        assert!(error.starts_with("outer: line one line two"), "{error:?}");
        assert!(!error.contains("get_def"), "{error:?}");
    }

    /// The request-end pass: held records get `root`, `path` and `more`;
    /// one outside the graph falls back to what its end named. With no
    /// skipped record there is no pass.
    #[tokio::test]
    async fn finish_fills_held_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine =
            Arc::new(Engine::new(crate::engine::Config::for_tests(dir.path())).expect("engine"));
        let rs = engine.new_state_with_events(false, None);
        let (_, mut f) = fold_with(
            vec![
                ev(0, matched(&["//p:a", "//d:t"], true)),
                ev(1, end("//p:a", Some("dependency failed"), Some("//p:c"))),
                ev(2, file_failed("d/BUILD", "//d:t", &["//d:t"])),
            ],
            &Known::default(),
        );
        let entries = f.finish(&rs);
        assert_eq!(rs.report_passes(), 1);
        let got: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.addr.as_deref(),
                    e.outcome,
                    e.root.as_deref(),
                    e.root_file.as_deref(),
                    e.path.clone(),
                    e.more,
                    e.selected,
                )
            })
            .collect();
        let s = |v: &[&str]| Some(v.iter().map(|a| (*a).to_string()).collect::<Vec<_>>());
        assert_eq!(
            got,
            [
                (
                    Some("//p:a"),
                    Outcome::Skipped,
                    Some("//p:c"),
                    None,
                    s(&["//p:a", "//p:c"]),
                    Some(0),
                    true
                ),
                (
                    Some("//d:t"),
                    Outcome::Skipped,
                    None,
                    Some("d/BUILD"),
                    s(&["//d:t"]),
                    Some(0),
                    true
                ),
            ]
        );
        let rs = engine.new_state_with_events(false, None);
        assert!(Fold::default().finish(&rs).is_empty());
        assert_eq!(rs.report_passes(), 0, "a green run pays for no pass");
    }

    /// The request-end pass over a real graph: a skipped target whose
    /// nearest culprit is a target names it in `root` alone, one whose
    /// culprit is a file names it in `root_file` alone.
    #[tokio::test]
    async fn finish_splits_root_and_root_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine =
            Arc::new(Engine::new(crate::engine::Config::for_tests(dir.path())).expect("engine"));
        let rs = engine.new_state_with_events(false, None);
        let a = |s: &str| crate::htaddr::parse_addr(s).expect("addr");
        rs.add_dep_for_test(&a("//a:x"), &a("//c:y")).expect("edge");
        rs.add_dep_for_test(&a("//b:z"), &a("//d:t")).expect("edge");
        rs.record_failure(
            Culprit::Target(a("//c:y")),
            Arc::new(TargetFailure::new(
                a("//c:y"),
                None,
                anyhow::anyhow!("boom"),
            )),
        );
        let mut file = TargetFailure::new(a("//d:t"), None, anyhow::anyhow!("eval"));
        file.file = Some(Arc::from("d/BUILD"));
        rs.record_failure(Culprit::File(Arc::from("d/BUILD")), Arc::new(file));
        let mut to_file = end("//b:z", Some("dependency failed"), Some("//d:t"));
        if let BuildEventKind::ResultEnd { upstream_file, .. } = &mut to_file {
            *upstream_file = Some("d/BUILD".to_string());
        }
        let (_, mut f) = fold_with(
            vec![
                ev(0, matched(&["//a:x", "//b:z"], true)),
                ev(1, end("//a:x", Some("dependency failed"), Some("//c:y"))),
                ev(2, to_file),
            ],
            &Known::default(),
        );
        let got: Vec<_> = f
            .finish(&rs)
            .into_iter()
            .map(|e| (e.addr, e.outcome, e.root, e.root_file, e.path))
            .collect();
        let o = |s: &str| Some(s.to_string());
        let p = |v: &[&str]| Some(v.iter().map(|a| (*a).to_string()).collect::<Vec<_>>());
        assert_eq!(
            got,
            [
                (
                    o("//a:x"),
                    Outcome::Skipped,
                    o("//c:y"),
                    None,
                    p(&["//a:x", "//c:y"])
                ),
                (
                    o("//b:z"),
                    Outcome::Skipped,
                    None,
                    o("d/BUILD"),
                    p(&["//b:z", "//d:t"])
                ),
            ]
        );
    }

    /// `finish` hands each held record out once and keeps none: the held
    /// set is empty after it, so a second `finish` writes nothing and runs
    /// no second pass.
    #[tokio::test]
    async fn finish_releases_held_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine =
            Arc::new(Engine::new(crate::engine::Config::for_tests(dir.path())).expect("engine"));
        let rs = engine.new_state_with_events(false, None);
        let a = |s: &str| crate::htaddr::parse_addr(s).expect("addr");
        rs.add_dep_for_test(&a("//p:a"), &a("//p:c")).expect("edge");
        rs.add_dep_for_test(&a("//p:b"), &a("//p:c")).expect("edge");
        rs.record_failure(
            Culprit::Target(a("//p:c")),
            Arc::new(TargetFailure::new(
                a("//p:c"),
                None,
                anyhow::anyhow!("boom"),
            )),
        );
        let (_, mut f) = fold_with(
            vec![
                ev(0, matched(&["//p:a", "//p:b"], true)),
                ev(1, end("//p:a", Some("dependency failed"), Some("//p:c"))),
                ev(2, end("//p:b", Some("dependency failed"), Some("//p:c"))),
            ],
            &Known::default(),
        );
        assert_eq!(f.held.len(), 2);
        assert_eq!(f.finish(&rs).len(), 2);
        assert!(f.held.is_empty(), "{:?}", f.held.len());
        assert!(f.finish(&rs).is_empty(), "each record once");
        assert_eq!(rs.report_passes(), 1);
        // The culprits it hands `skipped_report` are read without a path
        // pass: `finish` never reads a failure's own path.
        assert_eq!(rs.path_passes(), 0);
    }

    /// FQ-4: a pure chain of 5000 selected targets, all skipped under the
    /// one culprit at its foot. Every node is asked for its record, and each
    /// record carries the whole path down: the output is quadratic in the
    /// chain's depth, 12,507,500 path entries for 5000 records, made by one
    /// pass over the graph.
    #[tokio::test]
    async fn finish_pure_chain_output_size() {
        const N: usize = 5_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let engine =
            Arc::new(Engine::new(crate::engine::Config::for_tests(dir.path())).expect("engine"));
        let rs = engine.new_state_with_events(false, None);
        let a = |s: &str| crate::htaddr::parse_addr(s).expect("addr");
        let names: Vec<String> = (0..N).map(|i| format!("//p:n{i:04}")).collect();
        let chain: Vec<Addr> = names.iter().map(|n| a(n)).collect();
        let c = a("//p:c");
        for w in chain.windows(2) {
            if let [from, to] = w {
                rs.add_dep_for_test(from, to).expect("edge");
            }
        }
        rs.add_dep_for_test(&chain[N - 1], &c).expect("edge");
        rs.mark_requested(&chain[0]);
        rs.record_failure(
            Culprit::Target(c.clone()),
            Arc::new(TargetFailure::new(c, None, anyhow::anyhow!("boom"))),
        );
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let events = std::iter::once(ev(0, matched(&refs, true)))
            .chain(
                refs.iter()
                    .map(|n| ev(1, end(n, Some("dependency failed"), Some("//p:c")))),
            )
            .collect();
        let (_, mut f) = fold_with(events, &Known::default());
        let entries = f.finish(&rs);
        assert_eq!(rs.report_passes(), 1);
        assert_eq!(entries.len(), N, "one record per skipped target");
        let paths: Vec<usize> = entries
            .iter()
            .map(|e| e.path.as_ref().map_or(0, Vec::len))
            .collect();
        // `n_i`'s path is `n_i … n_4999, c`: N - i + 1 entries.
        assert_eq!(paths.first(), Some(&(N + 1)));
        assert_eq!(paths.last(), Some(&2));
        let total: usize = paths.iter().sum();
        assert_eq!(total, N * (N + 1) / 2 + N);
        assert_eq!(total, 12_507_500);
    }

    /// A dependency that built, then was blamed when it later ended failed,
    /// all before the matched set completed: its culprit record is written
    /// at the end of matching, not lost behind its first, successful end.
    #[test]
    fn early_culprit_after_an_ok_end() {
        let known = Known::default().target("//p:c", "c broke");
        let (entries, f) = fold_with(
            vec![
                ev(0, matched(&[], false)),
                ev(1, start("//p:c")),
                ev(2, end("//p:c", None, None)),
                ev(3, start("//p:c")),
                ev(4, end("//p:c", Some("c broke"), None)),
                ev(5, matched(&["//p:top"], false)),
                ev(6, end("//p:top", Some("dependency failed"), Some("//p:c"))),
                ev(7, matched(&[], true)),
            ],
            &known,
        );
        assert_eq!(
            addrs(&entries),
            [(Some("//p:c"), Outcome::Failed, false)],
            "{entries:?}"
        );
        assert_eq!(entries[0].root.as_deref(), Some("//p:c"));
        assert_eq!(f.held.len(), 1, "the selected target, skipped");
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
        let (entries, whole) = fold(vec![
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
        assert!(!whole, "a stopped target leaves the list open (RUN-24)");
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(
            (entries[1].outcome, entries[1].cache),
            (Outcome::Ran, Some(CacheTier::Miss))
        );
        assert_eq!(entries[2].outcome, Outcome::Failed);
        assert!(entries[2].timed_out);
    }

    fn collected(whole: bool, cancelled: bool) -> Collected<()> {
        let (_tx, events) = tokio::sync::mpsc::unbounded_channel();
        Collected {
            res: if cancelled {
                Err(anyhow::Error::new(crate::engine::error::CancelledError).context("result"))
            } else {
                Ok(())
            },
            events,
            stopped: false,
            whole,
            write_error: None,
            field_error: None,
        }
    }

    /// D10: the list is closed only when every matched target has its
    /// entry, nothing was skipped and nothing stopped the run, and every
    /// entry was written.
    #[test]
    fn may_close_table() {
        let clean = crate::engine::Gaps::new("//...");
        let skipped = crate::engine::Gaps::new("//...");
        skipped.record(
            crate::engine::discovery::Stage::List,
            "go",
            "//a".into(),
            &anyhow::anyhow!("boom"),
        );
        for whole in [false, true] {
            for gaps in [&clean, &skipped] {
                for cancelled in [false, true] {
                    let want = whole && gaps.is_empty() && !cancelled;
                    let got = collected(whole, cancelled).may_close(gaps);
                    assert_eq!(
                        got,
                        want,
                        "whole={whole} gaps={} {cancelled}",
                        gaps.is_empty()
                    );
                }
            }
        }
        let mut c = collected(true, false);
        c.stopped = true;
        assert!(!c.may_close(&clean), "stdout closed");
        let mut c = collected(true, false);
        c.write_error = Some(anyhow::anyhow!("EIO"));
        assert!(!c.may_close(&clean), "a write error");
        let mut c = collected(true, false);
        c.field_error = Some(anyhow::anyhow!("no outputs"));
        assert!(!c.may_close(&clean), "an entry left unwritten");
    }

    /// The exit once `finalize` ran: a closed stdout is 0; the document's
    /// error is the command's unless the build failed for itself.
    #[test]
    fn exit_table() {
        let doc = || Some(anyhow::anyhow!("doc"));
        assert!(exit(true, doc(), doc(), Err(anyhow::anyhow!("cancelled"))).is_ok());
        assert!(exit(false, None, None, Ok(())).is_ok());
        let e = exit(false, None, doc(), Ok(())).expect_err("a field error fails");
        assert_eq!(e.to_string(), "doc");
        let e = exit(false, doc(), None, Err(anyhow::anyhow!("cancelled")))
            .expect_err("a write error fails");
        assert_eq!(e.to_string(), "doc", "not the cancellation it caused");
        let failed = crate::commands::errors::FailedTargets::deferred(Vec::new()).into_error();
        let e = exit(false, None, doc(), Err(failed)).expect_err("the build failed");
        assert!(
            e.downcast_ref::<crate::commands::errors::FailedTargets>()
                .is_some(),
            "the build's failure keeps its exit: {e:#}"
        );
    }

    /// Writes its first chunk, then fails every write with `kind`.
    struct FailsAfterFirst {
        kind: std::io::ErrorKind,
        wrote: bool,
    }

    impl Write for FailsAfterFirst {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.wrote {
                return Err(self.kind.into());
            }
            self.wrote = true;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `collect` over `events` (a whole run of them), the list written to
    /// `w` under `flags`. Also whether the request was cancelled.
    async fn collect_over<W: Write>(
        flags: &[&str],
        events: Vec<BuildEventKind>,
        w: W,
    ) -> (Collected<()>, bool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine =
            Arc::new(Engine::new(crate::engine::Config::for_tests(dir.path())).expect("engine"));
        let rs = engine.new_state_with_events(false, None);
        let out = output::JsonArgs::<RunEntry>::try_parse_from(flags).expect("flags");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for kind in events {
            tx.send(ev(0, kind)).expect("send");
        }
        drop(tx);
        let opts = ResultOptions::default();
        let src = Sources {
            engine: &engine,
            rs: &rs,
            out: &out,
            outputs: &OutputMatcher::None,
            opts: &opts,
        };
        let mut list = out
            .printer_to(w, output::Style::Compact)
            .start_list()
            .expect("start the list");
        let c = collect(async { Ok(()) }, rx, None, &src, &mut list).await;
        (c, rs.ctoken().is_cancelled())
    }

    fn two_ran() -> Vec<BuildEventKind> {
        vec![
            matched(&["//p:a", "//p:b"], true),
            execute("//p:a"),
            end("//p:a", None, None),
            execute("//p:b"),
            end("//p:b", None, None),
        ]
    }

    /// A write that fails (not a closed pipe) is the command's error and
    /// cancels the request: there is nowhere left to write.
    #[tokio::test]
    async fn collect_failing_writer() {
        let w = FailsAfterFirst {
            kind: std::io::ErrorKind::Other,
            wrote: false,
        };
        let (c, cancelled) = collect_over(&["--json"], two_ran(), w).await;
        assert!(c.write_error.is_some(), "a write error");
        assert!(!c.stopped);
        assert!(cancelled, "the request is cancelled");
        assert!(!c.may_close(&crate::engine::Gaps::new("t")));
    }

    /// A closed stdout stops the document and cancels the request; no error.
    #[tokio::test]
    async fn collect_closed_stdout() {
        let w = FailsAfterFirst {
            kind: std::io::ErrorKind::BrokenPipe,
            wrote: false,
        };
        let (c, cancelled) = collect_over(&["--json"], two_ran(), w).await;
        assert!(c.stopped, "stdout closed");
        assert!(c.write_error.is_none(), "{:?}", c.write_error);
        assert!(cancelled, "the request is cancelled");
    }

    /// An entry whose fields cannot be read is left out and recorded; the
    /// build is not cancelled, later entries are written, the list stays
    /// open.
    #[tokio::test]
    async fn collect_field_error_keeps_building() {
        let mut buf = Vec::new();
        let events = vec![
            matched(&["not an addr", "//p:b"], true),
            end("not an addr", None, None),
            end("//p:b", None, None),
        ];
        let (c, cancelled) = collect_over(&["--fields", "+hashin"], events, &mut buf).await;
        assert!(c.field_error.is_some(), "the unreadable entry's error");
        assert!(c.write_error.is_none(), "{:?}", c.write_error);
        assert!(!cancelled, "the build goes on");
        assert!(c.whole);
        assert!(!c.may_close(&crate::engine::Gaps::new("t")), "left open");
        let text = String::from_utf8(buf).expect("utf-8");
        assert!(text.contains("//p:b"), "{text}");
        assert!(!text.contains("not an addr"), "{text}");
    }

    /// A skipped record is written once the run returned, after its
    /// culprit's.
    #[tokio::test]
    async fn collect_writes_skipped_at_request_end() {
        let mut buf = Vec::new();
        let events = vec![
            matched(&["//p:a", "//p:b"], true),
            end("//p:a", Some("dependency failed"), Some("//p:b")),
            end("//p:b", Some("boom"), None),
        ];
        let (c, _) = collect_over(&["--json"], events, &mut buf).await;
        assert!(c.whole);
        let text = String::from_utf8(buf).expect("utf-8");
        let (b, a) = (text.find("//p:b\""), text.find("\"//p:a\""));
        assert!(b.is_some() && b < a, "{text}");
        assert!(text.contains("\"skipped\""), "{text}");
    }
}
