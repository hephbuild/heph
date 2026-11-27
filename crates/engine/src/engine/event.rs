//! Build-progress event stream emitted by the engine core.
//!
//! Events are serde-serializable and transport-ready for a future client/server
//! process split. They carry the target address as a `String` (`//pkg:name`),
//! never the internal `Arc`-backed `Addr`. The server (engine) stamps every event
//! with a wall-clock timestamp at emit time (`at_unix_ms`), so elapsed times stay
//! correct even when the client and server are split across a channel.

use crate::engine::request_state::{RequestState, RequestStateData};
use std::future::Future;
use std::sync::Arc;

// Event types moved to `heph-core::events` (shared by the TUI + telemetry
// without an engine dep); re-exported so `engine::event::BuildEventKind` etc.
// keep resolving. `emit_scope` stays here — it needs `RequestState`.
pub use hcore::events::{BuildEvent, BuildEventKind, EventReceiver, EventSender, now_unix_ms};

/// Internal drop-guard so the `*End` event fires on early-return (`?`) **and** on
/// cancellation (the awaited future is dropped mid-flight). Once armed, the guard
/// emits exactly one end event when dropped.
///
/// Holds the shared [`RequestStateData`] (not just the event channel) so the end
/// event fans out to registered hooks too — an out-of-process hook (e.g. the GHA
/// status plugin) must see `ResultEnd`/`ExecuteEnd`, or every count it tallies
/// against paired scopes reads zero.
/// What a failing scope knows about its failure, beyond the flattened message.
///
/// Extracted once at the emit site by downcasting, so consumers never have to
/// re-derive it from prose. Before this existed the GHA reporter told a root
/// failure from collateral damage by searching the message for
/// `"dependency failed (root: …)"` — a string sniff of a `Display` impl, which
/// breaks the moment that wording changes.
#[derive(Debug, Default, Clone)]
pub struct ErrorDetail {
    /// `format!("{e:#}")` — the whole cause chain on one line.
    pub message: String,
    /// The root, when this target failed only because a dependency did.
    /// Never read off the chain ([`from_error`](Self::from_error) leaves it
    /// `None`): the result scope fills it from the failure classifier, the
    /// one rule that also decides what the run records as a root.
    pub upstream_of: Option<String>,
    /// The subprocess's exit status, spelled by `Exit`'s `Display` whenever
    /// the driver reported it structured (in-process or across the seam), so
    /// both spell it alike; else the driver's own text.
    pub exit_status: Option<String>,
    /// The code the subprocess exited with: `None` for a signal, or when the
    /// driver reported no structured status.
    pub exit_code: Option<i32>,
    /// The tail a recorded [`TargetFailure`](hplugin::error::TargetFailure)
    /// on the chain already read.
    pub log_tail: Option<hcore::events::LogTailData>,
    /// The log `execute` kept in the failure slot, not read yet: only
    /// `ResultEnd` reads its tail ([`result_log_tail`](Self::result_log_tail)),
    /// so the inner scopes, which report only the message, pay no IO for it.
    pub log: Option<hplugin::error::FailureLog>,
    /// The run outlasted the target's `timeout` (a
    /// [`TimedOutError`](crate::engine::run_timeout::TimedOutError) on the
    /// chain).
    pub timed_out: Option<hcore::events::TimedOutData>,
    /// A [`CancelledError`](hplugin::error::CancelledError) is on the chain.
    /// Not the whole answer to "was this a stop": a driver may report one as
    /// a bare `cancelled` of its own, which only the request's token tells
    /// apart ([`RequestState::end_is_cancelled`]).
    ///
    /// [`RequestState::end_is_cancelled`]: crate::engine::request_state::RequestState::end_is_cancelled
    pub cancelled: bool,
    /// A [`FailureSlot`](crate::engine::failure_slot::FailureSlot) is on the
    /// chain: `execute` already decided this run *failed* and kept it, so a
    /// cancel landing after that decision (fail-fast reaping another root)
    /// does not turn it into a stop.
    pub kept: bool,
    /// What the run left in its failure slot (the log's path, the sandbox's
    /// fate, `out/`, the write-back): `ResultEnd`'s `left_behind`, before it
    /// learns whether the log can be read. Paths and clones off the slot on
    /// the chain, no IO.
    pub left_behind: Option<hcore::events::LeftBehindData>,
    /// The run's output streamed to a terminal (the slot's, or the recorded
    /// failure's, on the chain): carried on `ResultEnd`'s log tail.
    pub streamed: bool,
}

impl ErrorDetail {
    /// Pull the structured detail out of an error chain.
    ///
    /// Every field is optional because every one of them is genuinely absent for
    /// some failures: a target can fail before it ever starts a process, and an
    /// internal engine error is neither upstream damage nor a subprocess.
    pub fn from_error(e: &anyhow::Error) -> Self {
        use hplugin::error::{ProcessFailed, TargetFailure};
        let slot =
            hcore::hmemoizer::downcast_chain_ref::<crate::engine::failure_slot::FailureSlot>(e);
        let mut d = Self {
            // Not `{e:#}`: a failure's artifacts ride the chain as an
            // empty-`Display` frame, which that would render as `: :`.
            message: hcore::hmemoizer::chain_message(e),
            cancelled: hplugin::error::is_cancelled(e),
            kept: slot.is_some(),
            streamed: slot.is_some_and(|s| s.streamed),
            left_behind: slot.map(|s| {
                hplugin::error::left_behind(
                    s.log.as_ref(),
                    s.sandbox.as_ref(),
                    s.out.as_deref(),
                    s.fixes.as_ref(),
                    None,
                )
            }),
            ..Self::default()
        };
        for cause in e.chain() {
            if d.log_tail.is_none()
                && let Some(t) = cause.downcast_ref::<TargetFailure>()
                && let Some(tail) = &t.log_tail
            {
                d.streamed |= t.streamed;
                d.log_tail = Some(hcore::events::LogTailData {
                    text: tail.text.clone(),
                    start_line: tail.start_line,
                    streamed: d.streamed,
                });
            }
        }
        // Through the memoizer's shared wrappers: a run's own failure reaches
        // `ResultEnd` from the `execute` memoizer, so its `ProcessFailed` and
        // the slot `execute` filled sit behind a `SharedAnyhow` a plain chain
        // walk stops at.
        // A cdylib driver's failure crosses the seam as `RemoteProcessFailed`,
        // whose status is structured. A structured status is spelled by
        // `Exit`'s one `Display` on both paths (std would add a signal's name
        // and `(core dumped)`, which the wire does not carry); only a driver
        // that reports none keeps its own text.
        let local = hcore::hmemoizer::downcast_chain_ref::<ProcessFailed>(e);
        let exit = local.and_then(|p| p.exit).or_else(|| {
            hcore::hmemoizer::downcast_chain_ref::<hplugin::error::RemoteProcessFailed>(e)
                .map(|r| r.exit)
        });
        d.exit_status = match exit {
            Some(x) => Some(x.to_string()),
            None => local.map(|p| p.status.clone()),
        };
        d.exit_code = match exit {
            Some(hcore::process::Exit::Code(c)) => Some(c),
            _ => None,
        };
        if d.log_tail.is_none() {
            d.log = slot.and_then(|s| s.log.clone());
        }
        // Through the memoizer's shared wrappers too: a recorded failure
        // reaches `ResultEnd` behind `TargetFailure`'s source.
        if let Some(t) = crate::engine::run_timeout::timed_out(e) {
            d.timed_out = Some(hcore::events::TimedOutData {
                timeout_ms: t.timeout.as_millis(),
                abandoned: t.abandoned_after.is_some(),
            });
        }
        d
    }

    /// Just the flattened message, for the scopes whose end events carry only
    /// that (`ExecuteEnd`, the cache spans). Structured detail is surfaced on
    /// `ResultEnd`, which is where a consumer reports a target's outcome; the
    /// inner spans would only duplicate it.
    pub fn into_message(self) -> String {
        self.message
    }

    /// The log tail `ResultEnd` carries: the recorded failure's, or else the
    /// last lines of the failure slot's log — one seek-based read bounded by
    /// the request's `--log-lines`, the same read the recorded failure makes.
    /// Kept whether or not the output streamed to a terminal (D6.1):
    /// `streamed` rides on it, and a renderer decides what to show.
    pub fn result_log_tail(
        log_tail: Option<hcore::events::LogTailData>,
        log: Option<&hplugin::error::FailureLog>,
        streamed: bool,
    ) -> Option<hcore::events::LogTailData> {
        let mut tail = log_tail.or_else(|| {
            let tail = log?.tail().ok()?;
            Some(hcore::events::LogTailData {
                text: tail.text,
                start_line: tail.start_line,
                streamed: false,
            })
        })?;
        tail.streamed |= streamed;
        Some(tail)
    }
}

/// Emits the scope's end event when it drops, built by `make_end`: held
/// in the scope's future, so the closure lives inline rather than boxed.
struct EndGuard<M: FnOnce(Option<ErrorDetail>) -> BuildEventKind> {
    data: Option<Arc<RequestStateData>>,
    make_end: Option<M>,
    error: Option<ErrorDetail>,
}

impl<M: FnOnce(Option<ErrorDetail>) -> BuildEventKind> Drop for EndGuard<M> {
    fn drop(&mut self) {
        if let (Some(data), Some(make_end)) = (self.data.take(), self.make_end.take()) {
            let kind = make_end(self.error.take());
            data.dispatch(BuildEvent {
                at_unix_ms: now_unix_ms(),
                kind,
            });
        }
    }
}

/// Emit `start`, run `fut`, then emit `make_end(error)` on
/// completion, early-return (`?`), or cancellation.
///
/// The end event is produced by an internal drop-guard armed before the await, so
/// it fires even if `fut` is cancelled mid-await. `at_unix_ms` is stamped on both
/// the start and end events. Call sites stay a single wrapping expression.
pub async fn emit_scope<T>(
    rs: &RequestState,
    start: BuildEventKind,
    make_end: impl FnOnce(Option<ErrorDetail>) -> BuildEventKind + Send + 'static,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    emit_scope_with(rs, start, make_end, |_| None::<std::future::Ready<()>>, fut).await
}

/// [`emit_scope`], awaiting what `on_error` returns for a failure before the
/// end event is built: for an end that carries something only an await can
/// learn, which `on_error` leaves where `make_end` reads it. The failure is
/// recorded first, so a scope cancelled during that await still ends as the
/// failure it was, without what the await would have added.
pub async fn emit_scope_with<T, F: Future<Output = ()>>(
    rs: &RequestState,
    start: BuildEventKind,
    make_end: impl FnOnce(Option<ErrorDetail>) -> BuildEventKind + Send + 'static,
    on_error: impl FnOnce(&anyhow::Error) -> Option<F>,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    rs.emit(start);
    let mut guard = EndGuard {
        data: Some(rs.data()),
        make_end: Some(make_end),
        error: None,
    };
    let out = fut.await; // guard still armed if this is cancelled mid-await
    if let Err(e) = &out {
        guard.error = Some(ErrorDetail::from_error(e));
        if let Some(more) = on_error(e) {
            more.await;
        }
    }
    out // guard drops here → emits *End
}
