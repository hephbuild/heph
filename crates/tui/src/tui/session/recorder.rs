//! The recorder: one OS thread per session that writes what the process does.
//!
//! Everything a build task does to feed it is one `std::sync::mpsc` send (I1):
//! the channel is unbounded, the thread does all the IO, and a recording failure
//! only ever stops the recording. The thread folds its own [`BuildState`], so
//! the `status` row does not depend on which view the command drew.
//!
//! It owns the one write connection on `session.db` ([`store::Writer`]):
//!
//! - Records go into one transaction per flush tick, committed 250ms after
//!   the first uncommitted record and at exit, with the explorer rows they
//!   complete.
//! - The `status` row is upserted inside the open transaction, at most once a
//!   second and only when something changed.
//! - Caps (D15): everything up to [`FULL_CAP`] bytes of records; past it a
//!   `Truncated` record, then only the [`is_critical`] kinds; at the
//!   [`CEILING`] (records plus explorer rows) a second `Truncated`, derivation
//!   stops at it, and only `End` follows.
//!
//! Shutdown (design amendment A8): `End` is appended, the final `status`
//! (carrying the exit) upserted, the session row's end and `sealed=1` written
//! in the last transaction, the file sealed ([`store::seal`], bounded), and
//! only then is the lock released — so "lock free and no `End`" means exactly
//! "crashed". The caller waits for that with a timeout, never a bare `join`.
//!
//! Failure (A9): the open transaction is rolled back, then the "stopped" latch
//! is set *before* the single `warn!`, because that `warn!` reaches the log
//! tap, which would otherwise feed the recorder that is failing. Then the
//! thread drops its receiver, so every later send fails at once and the
//! backlog is freed. The thread must not panic: the process's panic hook
//! leaves raw mode under a live TUI.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use hcore::events::{BuildEvent, BuildEventKind, EventReceiver, now_unix_ms};
use rusqlite::Connection;
use tokio::sync::mpsc::error::TryRecvError;

use super::cleanup::{self, Retention};
use super::format::{
    ApprovalPrompt, ApprovalStanding, SessionEnd, SessionRecord, SessionRecordKind, SessionSnapshot,
};
use super::holder::{HolderNames, event_holder_pid};
use super::id::SessionId;
use super::store;
use crate::tui::app::TUIAppView;
use crate::tui::progress::BuildState;

/// Bytes of record bodies recorded in full (D15). Past it a `Truncated`
/// record marks the point, and only the [`is_critical`] kinds are kept: `Log`,
/// `Output` and progress-only events are what make a recording large.
pub const FULL_CAP: u64 = 256 * 1024 * 1024;

/// Bytes of record bodies plus explorer rows past which the recording stops
/// (D15): a second `Truncated`, derivation stops at the same record (I4), and
/// only `End` is written after it.
pub const CEILING: u64 = 1024 * 1024 * 1024;

/// Messages queued and not yet taken by the recorder past which it stops: a
/// disk slower than the event rate must not grow the process without bound.
pub const MAX_PENDING: usize = 500_000;

/// Delay from the first uncommitted record to its commit.
const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Bytes of records and explorer rows past which the open transaction is
/// committed without waiting for [`FLUSH_EVERY`]. The exiting process waits
/// for the last commit, so this bounds what it writes then: a fast build's
/// whole run fits in one flush tick, and its single transaction (26k records,
/// 5 MiB) took 8 ms to commit at exit. It also keeps a transaction inside
/// SQLite's page cache, which a larger one spills mid-transaction.
pub(crate) const COMMIT_BYTES: u64 = 512 * 1024;
/// Minimum gap between two `status` upserts.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// How long the exiting process waits for `End` to be written and the
/// recording sealed.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(3);
/// What the seal leaves of [`FINISH_TIMEOUT`] for the answer to reach the
/// finisher, so it is not cut short while the seal still runs.
const FINISH_MARGIN: Duration = Duration::from_millis(250);
/// The most the owner's seal waits on another connection to the file: the
/// PR 2 gate bounds a seal with a viewer attached at 500 ms. A follower closes
/// within a [`super::source::FOLLOW_TICK`] of `End`, well inside it; a
/// connection that stays open (a `sqlite3` shell) cuts the seal short, and the
/// next cleanup seals the recording.
pub(crate) const SEAL_BUDGET: Duration = Duration::from_millis(400);

/// What the recorder thread is sent.
#[derive(Debug)]
enum Msg {
    Event(BuildEvent),
    Log {
        at_unix_ms: u64,
        seq: u64,
        level: &'static str,
        msg: String,
    },
    /// Flush everything received so far, then answer whether it reached the
    /// file. The channel is FIFO, so every record sent before this is ahead.
    Flush {
        ack: mpsc::SyncSender<bool>,
    },
    InflightWritten {
        at_unix_ms: u64,
    },
    /// A view record (`Label`, `Approval`, `Finished`, `Output`), or a
    /// `RunLog`: a session record the build itself did not emit.
    View {
        at_unix_ms: u64,
        kind: SessionRecordKind,
    },
    End {
        at_unix_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
        /// When the finisher stops waiting: the seal is bounded under it.
        deadline: Instant,
        ack: mpsc::SyncSender<()>,
    },
}

/// State shared between the recorder thread and every sender.
#[derive(Debug)]
struct Shared {
    /// The failure latch. Once set, nothing is sent and the thread exits.
    stopped: AtomicBool,
    /// Messages sent and not yet received.
    pending: AtomicUsize,
    /// [`MAX_PENDING`], or a test's smaller cap.
    max_pending: usize,
    /// The owner finished the session: every later send is dropped, silently.
    finished: AtomicBool,
    /// The owner's `flock`, held until `End` is on disk. Here rather than on the
    /// thread so a recorder that stopped early still holds the session alive
    /// until the process finishes it.
    lock: Mutex<LockState>,
    /// Signalled when the thread exits.
    exited: Condvar,
}

/// The lock, and who has had their say about it. Short of a committed `End`
/// (the thread then frees it itself), the lock goes only once both the owner
/// has finished the session and the thread has exited: a thread still alive
/// may hold its write connection, whose close checkpoints and deletes the
/// `-wal` — a viewer that saw the lock free before that could copy a
/// half-checkpointed `session.db` with its `-wal` gone.
#[derive(Debug)]
struct LockState {
    file: Option<File>,
    /// `finish` ran (whatever came of its `End`).
    finished: bool,
    /// The thread exited, its write connection and registration closed.
    exited: bool,
}

impl Shared {
    /// Latch the failure; whether this call latched it.
    fn latch(&self) -> bool {
        !self.stopped.swap(true, Ordering::AcqRel)
    }

    /// Latch the failure, then report it — in that order (A9).
    fn stop(&self, why: &str, error: Option<&io::Error>) {
        if self.latch() {
            match error {
                Some(error) => tracing::warn!(%error, "{why}; this run's session stops recording"),
                None => tracing::warn!("{why}; this run's session stops recording"),
            }
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn lock_state(&self) -> MutexGuard<'_, LockState> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Free the lock now: `End` is committed, so no viewer copies the file.
    fn release_lock(&self) {
        drop(self.lock_state().file.take());
    }

    /// The owner's say: the session is finished. The lock goes now if the
    /// thread has exited, after waiting for that until `deadline`; otherwise
    /// the thread's exit frees it ([`ReleaseOnExit`]).
    fn finish_lock(&self, deadline: Instant) {
        let mut state = self.lock_state();
        state.finished = true;
        while !state.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            state = self
                .exited
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(state.file.take());
    }
}

/// The thread's say, when the record loop exits however it exits (a stop, a
/// panic), after its write connection and registration are closed: the lock
/// goes now if the owner has already finished the session. A finisher that
/// timed out, or found the recording stopped, left the lock to the thread; a
/// long-lived process's session would otherwise keep looking live until that
/// process exits. The session is over either way, and with no `End` on disk a
/// viewer reads it as lost, which is the truth.
///
/// If the owner has not finished yet, the lock stays: `finish` releases it.
#[derive(Debug)]
struct ReleaseOnExit(Arc<Shared>);

impl Drop for ReleaseOnExit {
    fn drop(&mut self) {
        let mut state = self.0.lock_state();
        state.exited = true;
        if state.finished {
            drop(state.file.take());
        }
        drop(state);
        self.0.exited.notify_all();
    }
}

const FELL_BEHIND: &str = "the session recorder fell more than 500000 records behind";

/// A recording stop latched by a send made inside the log sink's lock, where
/// a `warn!` would re-enter the sink and deadlock. Its holder reports it once
/// it has let go of the lock.
#[derive(Debug)]
#[must_use = "a latched stop is reported once, by its holder"]
pub struct StopNotice(&'static str);

impl StopNotice {
    /// The one `warn!` for this stop.
    pub fn report(self) {
        tracing::warn!("{}; this run's session stops recording", self.0);
    }
}

/// Why [`SessionTap::flush`] did not get the records to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushFailed {
    /// The recording stopped, or the session finished.
    Stopped,
    /// The recorder did not answer in time.
    TimedOut,
}

impl std::fmt::Display for FlushFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "the session's recording stopped",
            Self::TimedOut => "the session recorder did not flush in time",
        })
    }
}

/// A handle that feeds the recorder. Cheap to clone; every send is one channel
/// push and two atomics, and never blocks.
#[derive(Debug, Clone)]
pub struct SessionTap {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
}

impl SessionTap {
    /// Send `msg`; whether it was queued, and a stop this send latched, which
    /// the caller reports (see [`StopNotice`]).
    fn try_send(&self, msg: Msg) -> (bool, Option<StopNotice>) {
        if self.shared.is_stopped() || self.shared.finished.load(Ordering::Acquire) {
            return (false, None);
        }
        if self.shared.pending.fetch_add(1, Ordering::Relaxed) >= self.shared.max_pending {
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            let notice = self.shared.latch().then_some(StopNotice(FELL_BEHIND));
            return (false, notice);
        }
        if self.tx.send(msg).is_err() {
            // The thread is gone (stopped, or finished): nothing to undo but the
            // count.
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            return (false, None);
        }
        (true, None)
    }

    fn send(&self, msg: Msg) {
        if let (_, Some(notice)) = self.try_send(msg) {
            notice.report();
        }
    }

    /// Record a build event, already folded into the view.
    pub fn event(&self, ev: BuildEvent) {
        self.send(Msg::Event(ev));
    }

    /// Record line `seq` of the log sink, a line the terminal showed. Called
    /// inside the sink's lock, so it never logs: a stop it latched comes back
    /// for the caller to report once it has let go.
    #[must_use = "a latched stop must be reported outside the sink's lock"]
    pub fn log(&self, seq: u64, level: &'static str, msg: String) -> Option<StopNotice> {
        self.try_send(Msg::Log {
            at_unix_ms: now_unix_ms(),
            seq,
            level,
            msg,
        })
        .1
    }

    /// Wait, at most `timeout`, until every record sent before this call is
    /// committed to `session.db`. Fails at once when the recording stopped or the
    /// session finished: a caller must not wait on a recorder that is gone.
    pub fn flush(&self, timeout: Duration) -> Result<(), FlushFailed> {
        let (ack, acked) = mpsc::sync_channel(1);
        match self.try_send(Msg::Flush { ack }) {
            (true, _) => {}
            (false, notice) => {
                if let Some(notice) = notice {
                    notice.report();
                }
                return Err(FlushFailed::Stopped);
            }
        }
        match acked.recv_timeout(timeout) {
            Ok(true) => Ok(()),
            // `false`: the flush failed and stopped the recording. Disconnected:
            // the thread stopped before it got there.
            Ok(false) | Err(RecvTimeoutError::Disconnected) => Err(FlushFailed::Stopped),
            Err(RecvTimeoutError::Timeout) => Err(FlushFailed::TimedOut),
        }
    }

    /// Record what the terminal showed besides build events and log lines,
    /// so a viewer draws the same UI: a label change, the approval prompt,
    /// the teardown instant, a failure report. `at_unix_ms` is the instant
    /// it stands for (a `Finished`'s is the clock the summary used).
    pub fn view(&self, at_unix_ms: u64, kind: SessionRecordKind) {
        self.send(Msg::View { at_unix_ms, kind });
    }

    /// Record that a run's live log was created, past the cap too.
    pub fn run_log(&self, record: hcore::session_record::RunLogRecord) {
        self.send(Msg::View {
            at_unix_ms: now_unix_ms(),
            kind: SessionRecordKind::RunLog(Box::new(record)),
        });
    }

    /// Record that `inflight.txt` was just rewritten.
    pub fn inflight_written(&self) {
        self.send(Msg::InflightWritten {
            at_unix_ms: now_unix_ms(),
        });
    }

    /// Whether the recording stopped on a failure.
    pub fn is_stopped(&self) -> bool {
        self.shared.is_stopped()
    }

    /// Messages sent and not yet received by the thread.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::Relaxed)
    }
}

/// The build-event receiver a backend consumes, with the session tap on its
/// receive side (A1): each event is handed to the recorder right after the view
/// folded it, by move, so what is recorded is exactly what was folded.
///
/// It also resolves lock holders for the view: an event carrying a holder pid
/// not yet claimed waits, parked here, while [`HolderNames::resolve_blocking`]
/// scans the sessions directory on a thread of its own, and is folded once the
/// answer is in the shared cache. So the view names the session on the very
/// event that first carries the pid, never does IO itself, and the reactor
/// never runs the scan. Each pid is scanned at most once (it is claimed before
/// its scan starts, and the answer is cached).
///
/// The wait is bounded by [`HOLDER_SCAN_WAIT`]: past it the event is folded
/// showing the pid, and the scan is left to finish on its own. It runs on a
/// detached thread rather than the blocking pool, because a runtime's shutdown
/// waits for its blocking tasks, and a hung `~/.heph` (NFS) would hold up the
/// exit. The pid stays claimed, so it is not rescanned on every later event;
/// a late answer still lands in the cache and names the pid from then on.
#[derive(Debug)]
pub struct TappedReceiver {
    rx: EventReceiver,
    tap: Option<SessionTap>,
    holders: HolderNames,
    /// An event taken from `rx` whose holder is being resolved. Kept here,
    /// not in the `recv` future, so a `recv` dropped by a `select!` loses
    /// nothing: the next call picks it up first.
    parked: Option<Parked>,
    /// Holder pids already recorded as a `LockHolder` record (D6): each is
    /// recorded once, before the first event that carries it.
    recorded_holders: HashSet<u32>,
}

/// How long the receiver holds an event for its holder's scan before folding
/// it with the pid alone.
pub(crate) const HOLDER_SCAN_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug)]
struct Parked {
    ev: BuildEvent,
    /// Completes when the scan has stored its answer (or its thread is gone).
    /// `None` when the thread could not be started.
    resolving: Option<tokio::sync::oneshot::Receiver<()>>,
    /// When to stop waiting, fixed at parking so a dropped `recv` does not
    /// restart the clock.
    deadline: tokio::time::Instant,
}

/// Scan for `pid`'s session on a detached thread; the receiver completes once
/// the answer is cached.
fn spawn_scan(holders: HolderNames, pid: u32) -> Option<tokio::sync::oneshot::Receiver<()>> {
    let (done, resolving) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("heph-holder-scan".to_owned())
        .spawn(move || {
            holders.resolve_blocking(pid);
            if done.send(()).is_err() {
                // The receiver gave up waiting: the answer is in the cache.
                tracing::trace!(pid, "a lock holder's session scan finished late");
            }
        });
    match spawned {
        Ok(_detached) => Some(resolving),
        Err(error) => {
            // The pid stays claimed and unnamed: the notice shows the pid.
            tracing::debug!(%error, "naming a lock holder's session");
            None
        }
    }
}

impl TappedReceiver {
    pub fn new(rx: EventReceiver, tap: Option<SessionTap>, holders: HolderNames) -> Self {
        Self {
            rx,
            tap,
            holders,
            parked: None,
            recorded_holders: HashSet::new(),
        }
    }

    /// Await the next event, fold it with `apply`, record it. `None` once every
    /// sender is gone. Cancel-safe: an event is either still in the channel,
    /// parked here while its holder resolves, or folded and recorded in the
    /// poll that takes it out of the parking. A parked event waits at most
    /// [`HOLDER_SCAN_WAIT`] from when it was parked.
    pub async fn recv(&mut self, apply: impl FnOnce(&BuildEvent)) -> Option<()> {
        if self.parked.is_none() {
            let ev = self.rx.recv().await?;
            match event_holder_pid(&ev).filter(|pid| self.holders.claim(*pid)) {
                None => {
                    self.fold(ev, apply);
                    return Some(());
                }
                Some(pid) => {
                    self.parked = Some(Parked {
                        ev,
                        resolving: spawn_scan(self.holders.clone(), pid),
                        deadline: tokio::time::Instant::now() + HOLDER_SCAN_WAIT,
                    });
                }
            }
        }
        if let Some(Parked {
            resolving: Some(resolving),
            deadline,
            ..
        }) = self.parked.as_mut()
        {
            // A scan past its wait, or whose thread died, leaves the pid
            // unnamed: the notice falls back to the pid.
            if tokio::time::timeout_at(*deadline, resolving).await.is_err() {
                tracing::debug!("a lock holder's session scan is slow; showing the pid");
            }
        }
        if let Some(parked) = self.parked.take() {
            self.fold(parked.ev, apply);
        }
        Some(())
    }

    /// [`Self::recv`] without waiting. A parked event is folded first, named
    /// only if its scan already finished: this drain runs at teardown, where
    /// waiting on a scan would hold up the exit.
    pub fn try_recv(&mut self, apply: impl FnOnce(&BuildEvent)) -> Result<(), TryRecvError> {
        if let Some(parked) = self.parked.take() {
            self.fold(parked.ev, apply);
            return Ok(());
        }
        let ev = self.rx.try_recv()?;
        self.fold(ev, apply);
        Ok(())
    }

    /// [`Self::try_recv`] until nothing is queued: every event already sent
    /// is folded, none is waited for.
    pub fn drain(&mut self, mut apply: impl FnMut(&BuildEvent)) {
        while self.try_recv(&mut apply).is_ok() {}
    }

    /// Await the next event and only record it — for a backend that has stopped
    /// folding (the line backend, after its summary) but whose process still
    /// emits.
    pub async fn recv_record_only(&mut self) -> Option<()> {
        let ev = match self.parked.take() {
            Some(parked) => parked.ev,
            None => self.rx.recv().await?,
        };
        if let Some(tap) = &self.tap {
            tap.event(ev);
        }
        Some(())
    }

    fn fold(&mut self, ev: BuildEvent, apply: impl FnOnce(&BuildEvent)) {
        apply(&ev);
        if let Some(tap) = &self.tap {
            // D6: the run names the holder, once per pid, ahead of the event
            // that first carries it, so a viewer (here or on another machine)
            // names it from the recording and never scans for pids.
            if let Some(pid) = event_holder_pid(&ev)
                && self.recorded_holders.insert(pid)
            {
                tap.view(
                    ev.at_unix_ms,
                    SessionRecordKind::LockHolder {
                        pid,
                        session: self.holders.get(pid).map(SessionId::encode),
                    },
                );
            }
            tap.event(ev);
        }
    }
}

/// What the run's view showed besides build events and log lines, recorded
/// when it changes so `heph session attach` draws it too: the label and the
/// approval prompt, then the teardown instant. The interactive backend reads
/// the view once per tick; a record goes out only on a change.
#[derive(Debug)]
pub(crate) struct ViewTap {
    tap: SessionTap,
    /// The label last recorded (the session row holds the first).
    label: String,
    approval: Option<ApprovalPrompt>,
    standing: Option<ApprovalStanding>,
}

impl ViewTap {
    pub(crate) fn new(tap: SessionTap, view: &impl TUIAppView) -> Self {
        Self {
            tap,
            label: view.session_header().label,
            approval: None,
            standing: None,
        }
    }

    /// Record the label or approval prompt if either changed.
    pub(crate) fn observe(&mut self, view: &impl TUIAppView) {
        let label = view.session_header().label;
        if label != self.label {
            self.tap.view(
                now_unix_ms(),
                SessionRecordKind::Label {
                    label: label.clone(),
                },
            );
            self.label = label;
        }
        let approval = view.pending_approval();
        let standing = view.approval_standing();
        if approval != self.approval || standing != self.standing {
            self.tap.view(
                now_unix_ms(),
                SessionRecordKind::Approval {
                    prompt: approval.clone().map(Box::new),
                    standing,
                },
            );
            self.approval = approval;
            self.standing = standing;
        }
    }

    /// The TUI was torn down, its summary measured at `at_ms`: record what
    /// changed since the last tick, then `Finished`.
    pub(crate) fn finished(&mut self, view: &impl TUIAppView, at_ms: u64) {
        self.observe(view);
        self.tap.view(
            at_ms,
            SessionRecordKind::Finished {
                stopped: view.stopped(),
            },
        );
    }
}

/// A bug in the recorder, for the test that proves one does not fail the app.
/// Compiled out of every non-test build.
#[cfg(test)]
fn injected_panic() {
    panic!("injected recorder panic");
}

#[cfg(not(test))]
fn injected_panic() {}

/// The kinds kept past [`FULL_CAP`] (D15): every record an explorer row is
/// derived from (so a re-derivation stays exact, I4), and what a viewer needs
/// at the end of a truncated session — `End`, the view records (`Label`,
/// `Approval`, `Finished`), `RunLog` (the only way a follower finds the log
/// of a run that starts after the cap) and `InflightWritten`. `Log`, `Output`
/// and progress-only events are dropped. Exhaustive on purpose: a new kind
/// must say which side it is on.
pub(crate) fn is_critical(record: &SessionRecord) -> bool {
    use BuildEventKind as K;
    if let Some(kind) = &record.kind {
        return match kind {
            K::RequestConfig { .. }
            | K::ResultStart { .. }
            | K::ResultEnd { .. }
            | K::ExecuteStart { .. }
            | K::LocalCacheHit { .. }
            | K::LocalCacheMiss { .. }
            | K::RemoteCacheHit { .. }
            | K::RemoteCacheMiss { .. }
            | K::CacheOutcome { .. }
            | K::CacheHitRebuilt { .. }
            | K::ResultLockWaitStart { .. }
            | K::ResultLockWaitEnd { .. }
            | K::AdmissionWaitStart { .. }
            | K::AdmissionWaitEnd { .. }
            | K::ScratchLockWaitStart { .. }
            | K::ScratchLockWaitEnd { .. }
            | K::ScratchPrepareStart { .. }
            | K::ScratchPrepareEnd { .. }
            | K::PoolUsage { .. }
            | K::CodegenExcluded { .. } => true,
            K::Matched { .. }
            | K::SelectionIncomplete { .. }
            | K::ResultOutputs { .. }
            | K::ExecuteEnd { .. }
            | K::LocalCacheWriteStart { .. }
            | K::LocalCacheWriteEnd { .. }
            | K::RemoteCacheReadStart { .. }
            | K::RemoteCacheReadEnd { .. }
            | K::RemoteCacheWriteStart { .. }
            | K::RemoteCacheWriteEnd { .. }
            | K::GcTargetSwept { .. }
            | K::GcFailureLogsSwept { .. }
            | K::GcCacheDb { .. }
            | K::CodegenImported { .. }
            | K::Unknown => false,
        };
    }
    match &record.session {
        Some(
            SessionRecordKind::End { .. }
            | SessionRecordKind::Truncated
            | SessionRecordKind::InflightWritten
            | SessionRecordKind::Label { .. }
            | SessionRecordKind::Approval { .. }
            | SessionRecordKind::Finished { .. }
            | SessionRecordKind::RunLog(_)
            | SessionRecordKind::LockHolder { .. },
        ) => true,
        Some(
            SessionRecordKind::Log { .. }
            | SessionRecordKind::Output { .. }
            | SessionRecordKind::Unknown,
        )
        | None => false,
    }
}

/// A test's stand-in for a write that hangs: every record write waits until
/// the flag is set.
pub(crate) type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;

/// Block until `gate` opens.
fn wait_for(gate: &Gate) {
    let (open, opened) = &**gate;
    let mut open = open.lock().unwrap_or_else(PoisonError::into_inner);
    while !*open {
        open = opened.wait(open).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Test-only fault injection. Always default outside tests.
#[derive(Debug, Default, Clone)]
pub(crate) struct Faults {
    /// Panic on the first event, as a bug in the recorder would.
    pub(crate) panic_on_event: bool,
    /// Fail to take the session's lock, as an NFS `ENOLCK` would.
    pub(crate) fail_lock: bool,
    /// Fail every record write, as a full disk would.
    pub(crate) fail_writes: bool,
    /// Hold every record write until the gate opens, as a hung disk would.
    pub(crate) gate: Option<Gate>,
    /// Hold the start-of-session cleanup until the gate opens, as a slow
    /// seal of a large lost recording would.
    pub(crate) cleanup_gate: Option<Gate>,
    /// [`FULL_CAP`] in its place, so a test reaches it with a few records.
    pub(crate) full_cap: Option<u64>,
    /// [`CEILING`] in its place.
    pub(crate) ceiling: Option<u64>,
    /// [`MAX_PENDING`] in its place, so a test falls behind with a few sends.
    pub(crate) max_pending: Option<usize>,
    /// [`FLUSH_EVERY`] and [`STATUS_EVERY`] in their place, so a test tells
    /// a commit by size from one by time.
    pub(crate) ticks: Option<Duration>,
    /// [`COMMIT_BYTES`] in its place.
    pub(crate) commit_bytes: Option<u64>,
    /// [`cleanup::INTERVAL`] in its place: `Duration::ZERO` cleans at every
    /// start, for a test that needs a second start's cleanup.
    pub(crate) cleanup_interval: Option<Duration>,
    /// Refuse WAL mode when creating the recording, as some filesystems do.
    pub(crate) refuse_wal: bool,
}

/// What the recorder thread starts with.
pub(crate) struct RecorderStart {
    /// The sessions root, cleaned on its own thread while recording starts.
    pub(crate) root: std::path::PathBuf,
    pub(crate) lock: File,
    /// The write connection on the session's freshly created `session.db`.
    pub(crate) conn: Connection,
    /// `conn`'s registration as open read-write ([`store::rw_open`]), held
    /// until the connection is closed.
    pub(crate) rw: store::RwOpen,
    pub(crate) session_id: SessionId,
    pub(crate) retention: Retention,
    /// A private-sessions root to sweep of every ended session too.
    pub(crate) private_root: Option<std::path::PathBuf>,
    pub(crate) faults: Faults,
}

/// Whether this process claimed `root`'s cleanup ([`cleanup::claim`]).
fn claimed(root: &std::path::Path, interval: Duration) -> bool {
    match cleanup::claim(root, interval) {
        Ok(claimed) => claimed,
        // A private root nobody has used yet: nothing to clean.
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::debug!(root = %root.display(), error = %e, "cannot claim the session cleanup");
            false
        }
    }
}

/// The start-of-session cleanup of `root`, then of `private_root`, each when
/// this process claims it: at most once an `interval` per root.
fn clean_ended(
    root: &std::path::Path,
    retention: Retention,
    private_root: Option<&std::path::Path>,
    interval: Duration,
) {
    // Another process claimed this root within the interval, and so the
    // private one beside it.
    if !claimed(root, interval) {
        return;
    }
    match cleanup::cleanup(root, SystemTime::now(), retention) {
        Ok(report) if report != cleanup::CleanupReport::default() => {
            tracing::debug!(
                removed = report.removed.len(),
                sealed = report.sealed.len(),
                tombs = report.tombs,
                tmp = report.tmp,
                "cleaned ended sessions"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "cannot clean ended sessions"),
    }
    // A killed `HEPH_NO_SESSION` run's directory: nobody else reads it, so it
    // goes whatever its age. Missing is normal.
    if let Some(private) = private_root
        && claimed(private, interval)
        && let Err(e) = cleanup::cleanup(private, SystemTime::now(), Retention::PRIVATE)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(error = %e, "cannot clean ended private sessions");
    }
}

/// The owner's handle on a running recorder.
#[derive(Debug)]
pub struct Recorder {
    tap: SessionTap,
}

impl Recorder {
    /// Spawn the recorder thread. The caller has created the directory, taken
    /// the lock and created `session.db`.
    pub(crate) fn spawn(start: RecorderStart) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            max_pending: start.faults.max_pending.unwrap_or(MAX_PENDING),
            finished: AtomicBool::new(false),
            lock: Mutex::new(LockState {
                file: Some(start.lock),
                finished: false,
                exited: false,
            }),
            exited: Condvar::new(),
        });
        // The thread logs through whatever subscriber the spawner had — the
        // global one in the binary, a test's own in a test.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread_shared = Arc::clone(&shared);
        let RecorderStart {
            root,
            conn,
            rw,
            session_id,
            retention,
            private_root,
            faults,
            ..
        } = start;
        // Cleanup on its own thread: sealing another session's recording can
        // wait on that file's viewers, and the record loop must not (I1). It
        // is detached; an exit mid-cleanup leaves what the next one finishes.
        // Started only when the root's stamp says it is due: within the
        // interval, a start costs this one `stat` and no thread.
        let interval = faults.cleanup_interval.unwrap_or(cleanup::INTERVAL);
        if cleanup::due(&root, interval) {
            let cleanup_gate = faults.cleanup_gate.clone();
            let cleanup_dispatch = dispatch.clone();
            let spawned = std::thread::Builder::new()
                .name("heph-session-cleanup".to_owned())
                .spawn(move || {
                    let _dispatch = tracing::dispatcher::set_default(&cleanup_dispatch);
                    if let Some(gate) = &cleanup_gate {
                        wait_for(gate);
                    }
                    clean_ended(&root, retention, private_root.as_deref(), interval);
                });
            if let Err(e) = spawned {
                tracing::debug!(error = %e, "cannot start the session cleanup");
            }
        }
        // One value, so the connection drops before its registration even if
        // the thread never starts (a tuple drops in order).
        let write = (conn, rw);
        std::thread::Builder::new()
            .name("heph-session-recorder".to_owned())
            .spawn(move || {
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                // Declared before the thread state, so it drops after the
                // write connection and its registration.
                let _exit = ReleaseOnExit(Arc::clone(&thread_shared));
                let (conn, rw) = write;
                let mut thread = RecorderThread {
                    writer: Some(store::Writer::new(conn, session_id)),
                    _rw: rw,
                    record_bytes: 0,
                    full_cap: faults.full_cap.unwrap_or(FULL_CAP),
                    ceiling: faults.ceiling.unwrap_or(CEILING),
                    phase: Phase::Full,
                    uncommitted_since: None,
                    uncommitted_bytes: 0,
                    flush_every: faults.ticks.unwrap_or(FLUSH_EVERY),
                    status_every: faults.ticks.unwrap_or(STATUS_EVERY),
                    commit_bytes: faults.commit_bytes.unwrap_or(COMMIT_BYTES),
                    state: BuildState::new(),
                    end: None,
                    dirty: true,
                    last_status: None,
                    line: String::new(),
                    shared: thread_shared,
                    faults,
                };
                thread.run(&rx);
            })?;
        Ok(Self {
            tap: SessionTap { tx, shared },
        })
    }

    /// A tap feeding this recorder.
    pub fn tap(&self) -> SessionTap {
        self.tap.clone()
    }

    /// Write `End`, seal the recording and release the lock, waiting at most
    /// `timeout` for the recorder to say it did; the seal is bounded under
    /// it. Idempotent: only the first call does anything.
    ///
    /// If the recorder stopped, or its `End` failed, the lock is released with
    /// no `End` on disk — a viewer reads that as a lost session, which is the
    /// truth about its records — but only once the thread has exited, its
    /// write connection closed: this waits for that within `timeout`. If the
    /// thread does not answer or exit in time, the lock stays held: the thread
    /// releases it once its `End` and seal are done, or as it exits.
    pub fn finish(&self, exit_code: Option<i32>, panicked: bool, timeout: Duration) {
        let shared = &self.tap.shared;
        // From here every tap drops its sends: nothing may follow `End`.
        if shared.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let deadline = Instant::now() + timeout;
        if !shared.is_stopped() {
            let (ack, acked) = mpsc::sync_channel(1);
            // Counted like every other message: the thread uncounts each one
            // it receives, `End` included.
            shared.pending.fetch_add(1, Ordering::Relaxed);
            let sent = self
                .tap
                .tx
                .send(Msg::End {
                    at_unix_ms: now_unix_ms(),
                    exit_code,
                    panicked,
                    deadline,
                    ack,
                })
                .is_ok();
            if !sent {
                shared.pending.fetch_sub(1, Ordering::Relaxed);
            }
            if sent {
                match acked.recv_timeout(timeout) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                    // The thread still holds the write connection and may yet
                    // commit `End` and seal: the lock stays with it (I2, I3).
                    // Its own release after the seal, or its exit, frees it.
                    Err(RecvTimeoutError::Timeout) => {
                        tracing::debug!(
                            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                            "session recorder did not finish in time"
                        );
                    }
                }
            }
        }
        shared.finish_lock(deadline);
    }
}

/// Where the recording is against its caps (D15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Under [`FULL_CAP`]: every record is kept.
    Full,
    /// Past [`FULL_CAP`]: only the [`is_critical`] kinds.
    Critical,
    /// Past the [`CEILING`]: only `End`.
    Stopped,
}

struct RecorderThread {
    /// `None` once `End` is written and the recording sealed.
    writer: Option<store::Writer>,
    /// The write connection's registration: declared after `writer`, so it
    /// drops after the connection however the thread ends.
    _rw: store::RwOpen,
    /// Bytes of record bodies inserted, toward the caps.
    record_bytes: u64,
    full_cap: u64,
    ceiling: u64,
    phase: Phase,
    /// When the oldest uncommitted record was inserted.
    uncommitted_since: Option<Instant>,
    /// Bytes of records and explorer rows in the open transaction.
    uncommitted_bytes: u64,
    flush_every: Duration,
    status_every: Duration,
    commit_bytes: u64,
    state: BuildState,
    end: Option<SessionEnd>,
    /// The state changed since the `status` row was last upserted.
    dirty: bool,
    last_status: Option<Instant>,
    /// Reused serialization buffer.
    line: String,
    shared: Arc<Shared>,
    faults: Faults,
}

/// Whether the loop goes on.
enum Flow {
    Continue,
    Stop,
}

impl RecorderThread {
    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        loop {
            if self.shared.is_stopped() {
                return;
            }
            if let Flow::Stop = self.tick() {
                return;
            }
            let msg = match self.next_deadline() {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx
                    .recv()
                    .map_err(|_disconnected| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(msg) => {
                    self.shared.pending.fetch_sub(1, Ordering::Relaxed);
                    if let Flow::Stop = self.handle(msg, rx) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every tap dropped without an `End`: commit what there is and
                // stop. The lock stays with whoever finishes the session.
                Err(RecvTimeoutError::Disconnected) => {
                    if let Err(e) = self.upsert_status() {
                        tracing::debug!(error = %e, "cannot write the session's last status");
                    }
                    if let Err(e) = self.commit() {
                        tracing::debug!(error = %e, "cannot commit the session's last records");
                    }
                    return;
                }
            }
        }
    }

    /// The next instant something is due: a commit or a status upsert.
    fn next_deadline(&self) -> Option<Instant> {
        let flush = self.uncommitted_since.map(|t| t + self.flush_every);
        let status = self.dirty.then(|| {
            self.last_status
                .map_or_else(Instant::now, |t| t + self.status_every)
        });
        match (flush, status) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Do whatever is due.
    fn tick(&mut self) -> Flow {
        let now = Instant::now();
        let commit_due = self
            .uncommitted_since
            .is_some_and(|t| now >= t + self.flush_every);
        let status_due = self.dirty
            && self
                .last_status
                .is_none_or(|t| now >= t + self.status_every);
        if status_due && let Err(e) = self.upsert_status() {
            return self.fail("cannot write the session's status", &e);
        }
        if (commit_due || status_due)
            && let Err(e) = self.commit()
        {
            return self.fail("cannot write the session's records", &e);
        }
        Flow::Continue
    }

    fn handle(&mut self, msg: Msg, rx: &mpsc::Receiver<Msg>) -> Flow {
        let written = match msg {
            Msg::Event(ev) => {
                if self.faults.panic_on_event {
                    injected_panic();
                }
                self.state.apply(&ev);
                self.dirty = true;
                self.record(&SessionRecord {
                    at_unix_ms: ev.at_unix_ms,
                    kind: Some(ev.kind),
                    session: None,
                })
            }
            Msg::Log {
                at_unix_ms,
                seq,
                level,
                msg,
            } => self.record(&SessionRecord::session(
                at_unix_ms,
                SessionRecordKind::Log {
                    level: level.to_owned(),
                    msg,
                    seq,
                },
            )),
            Msg::Flush { ack } => {
                let committed = self.commit();
                // The asker may have timed out and gone.
                ack.send(committed.is_ok()).unwrap_or_default();
                committed
            }
            Msg::InflightWritten { at_unix_ms } => self
                .record(&SessionRecord::session(
                    at_unix_ms,
                    SessionRecordKind::InflightWritten,
                ))
                .and_then(|()| self.commit()),
            Msg::View { at_unix_ms, kind } => {
                self.record(&SessionRecord::session(at_unix_ms, kind))
            }
            Msg::End {
                at_unix_ms,
                exit_code,
                panicked,
                deadline,
                ack,
            } => {
                // `End` landed: the lock is already free. It failed: the
                // write connection is still open, so the lock goes as this
                // thread exits, once `finish` has had its say.
                self.end_session(rx, at_unix_ms, exit_code, panicked, deadline);
                // The finisher may have timed out and gone; nothing to tell it.
                ack.send(()).unwrap_or_default();
                return Flow::Stop;
            }
        };
        match written.and_then(|()| self.commit_if_full()) {
            Ok(()) => Flow::Continue,
            Err(e) => self.fail("cannot write the session's events", &e),
        }
    }

    /// Everything queued before `End`, then `End`, the final status and the
    /// session row's end in the last transaction, the seal, and the lock — in
    /// that order.
    fn end_session(
        &mut self,
        rx: &mpsc::Receiver<Msg>,
        at_unix_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
        deadline: Instant,
    ) {
        while let Ok(msg) = rx.try_recv() {
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            // A second `End` cannot be queued: `finish` sends once.
            if let Flow::Stop = self.handle(msg, rx) {
                return;
            }
        }
        let end = SessionRecord::session(
            at_unix_ms,
            SessionRecordKind::End {
                exit_code,
                panicked,
            },
        );
        if let Err(e) = self.record(&end) {
            self.fail("cannot write the session's end", &e);
            return;
        }
        self.end = Some(SessionEnd {
            at_unix_ms,
            exit_code,
            panicked,
        });
        self.dirty = true;
        if let Err(e) = self.upsert_status() {
            self.fail("cannot write the session's status", &e);
            return;
        }
        let Some(writer) = self.writer.take() else {
            return;
        };
        let conn = match writer.finish(at_unix_ms, exit_code, panicked) {
            Ok(conn) => conn,
            Err(e) => {
                self.fail("cannot write the session's end", &io::Error::other(e));
                return;
            }
        };
        // `End` is committed: whatever the seal does, the lock may go. A seal
        // cut short leaves the file in WAL mode for the next cleanup.
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(FINISH_MARGIN)
            .min(SEAL_BUDGET);
        match store::seal(conn, budget) {
            Ok(store::Sealed::Done) => {}
            Ok(store::Sealed::CutShort) => {
                tracing::debug!("a reader kept the session's recording from being sealed");
            }
            Err(e) => tracing::debug!(error = %e, "cannot seal the session's recording"),
        }
        self.shared.release_lock();
    }

    /// Roll back the open transaction, then latch and report the failure.
    fn fail(&mut self, why: &str, error: &io::Error) -> Flow {
        if let Some(writer) = self.writer.as_mut() {
            writer.rollback();
        }
        self.shared.stop(why, Some(error));
        Flow::Stop
    }

    /// Record one message under the caps (D15):
    ///
    /// - [`Phase::Full`]: kept. The record that would cross [`FULL_CAP`] is
    ///   preceded by a `Truncated`, and kept only if [`is_critical`].
    /// - [`Phase::Critical`]: kept only if [`is_critical`].
    /// - A record that would take records plus explorer rows past the
    ///   [`CEILING`] is replaced by a second `Truncated`, at which derivation
    ///   stops (I4); [`Phase::Stopped`] keeps only `End`.
    fn record(&mut self, record: &SessionRecord) -> io::Result<()> {
        let is_end = matches!(record.session, Some(SessionRecordKind::End { .. }));
        match self.phase {
            Phase::Stopped if !is_end => return Ok(()),
            Phase::Critical if !is_critical(record) => return Ok(()),
            _ => {}
        }
        self.encode(record)?;
        if self.phase == Phase::Full
            && self.record_bytes.saturating_add(self.line_len()) > self.full_cap
        {
            self.truncate(Phase::Critical)?;
            if !is_critical(record) {
                return Ok(());
            }
        }
        if !is_end && self.phase != Phase::Stopped {
            let explorer = self.writer.as_ref().map_or(0, |w| w.explorer_bytes);
            let total = self
                .record_bytes
                .saturating_add(self.line_len())
                .saturating_add(explorer);
            if total > self.ceiling {
                return self.truncate(Phase::Stopped);
            }
        }
        self.insert(record)
    }

    /// Insert a `Truncated` and enter `next`, leaving the line buffer as it
    /// was. Into [`Phase::Stopped`], derivation stops first, so this
    /// `Truncated` is the session row's `truncated_seq` and no derivation,
    /// live or rebuilt, applies it.
    fn truncate(&mut self, next: Phase) -> io::Result<()> {
        let pending = std::mem::take(&mut self.line);
        let inserted = self.insert_truncated(next);
        self.line = pending;
        inserted
    }

    fn insert_truncated(&mut self, next: Phase) -> io::Result<()> {
        if next == Phase::Stopped
            && let Some(writer) = self.writer.as_mut()
        {
            writer
                .stop_deriving(writer.next_seq())
                .map_err(io::Error::other)?;
        }
        self.phase = next;
        self.dirty = true;
        let truncated = SessionRecord::session(now_unix_ms(), SessionRecordKind::Truncated);
        self.encode(&truncated)?;
        self.insert(&truncated)
    }

    /// Serialize `record` into the line buffer, reusing its allocation.
    fn encode(&mut self, record: &SessionRecord) -> io::Result<()> {
        let mut buf = std::mem::take(&mut self.line).into_bytes();
        buf.clear();
        serde_json::to_writer(&mut buf, record).map_err(io::Error::other)?;
        self.line = String::from_utf8(buf).map_err(io::Error::other)?;
        Ok(())
    }

    fn line_len(&self) -> u64 {
        u64::try_from(self.line.len()).unwrap_or(u64::MAX)
    }

    /// Insert `record`, already encoded in the line buffer, into the open
    /// transaction.
    fn insert(&mut self, record: &SessionRecord) -> io::Result<()> {
        self.injected_write_faults()?;
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let explorer = writer.explorer_bytes;
        writer
            .insert(record, &self.line)
            .map_err(io::Error::other)?;
        let explorer_added = writer.explorer_bytes.saturating_sub(explorer);
        self.uncommitted_bytes = self
            .uncommitted_bytes
            .saturating_add(self.line_len())
            .saturating_add(explorer_added);
        self.record_bytes = self.record_bytes.saturating_add(self.line_len());
        if self.uncommitted_since.is_none() {
            self.uncommitted_since = Some(Instant::now());
        }
        Ok(())
    }

    /// A full disk or a hung one, for tests. No-op outside them.
    fn injected_write_faults(&self) -> io::Result<()> {
        if let Some(gate) = &self.faults.gate {
            wait_for(gate);
        }
        if self.faults.fail_writes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "no space left on device (injected)",
            ));
        }
        Ok(())
    }

    /// Commit the open transaction: records and status reach readers.
    fn commit(&mut self) -> io::Result<()> {
        self.uncommitted_since = None;
        self.uncommitted_bytes = 0;
        match self.writer.as_mut() {
            Some(writer) => writer.commit().map_err(io::Error::other),
            None => Ok(()),
        }
    }

    /// Commit now if the open transaction holds [`COMMIT_BYTES`] or more.
    fn commit_if_full(&mut self) -> io::Result<()> {
        if self.uncommitted_bytes < self.commit_bytes {
            return Ok(());
        }
        self.commit()
    }

    /// Upsert the `status` row inside the open transaction.
    fn upsert_status(&mut self) -> io::Result<()> {
        let mut snapshot: SessionSnapshot =
            self.state.snapshot_capped(super::format::STATUS_LIST_CAP);
        snapshot.truncated = self.phase != Phase::Full;
        snapshot.end = self.end.clone();
        let json = serde_json::to_string(&snapshot).map_err(io::Error::other)?;
        if let Some(writer) = self.writer.as_mut() {
            writer
                .upsert_status(now_unix_ms(), &json)
                .map_err(io::Error::other)?;
        }
        self.dirty = false;
        self.last_status = Some(Instant::now());
        Ok(())
    }
}

/// Write `bytes` to `dir/name` through a `0600` temp file renamed over it, so
/// a reader sees the old file or the new one, never half of one
/// (`inflight.txt`).
pub fn write_bytes_atomically(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    io::Write::write_all(tmp.as_file_mut(), bytes)?;
    tmp.persist(dir.join(name)).map_err(|e| e.error)?;
    Ok(())
}
