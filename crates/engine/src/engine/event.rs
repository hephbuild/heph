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
    pub upstream_of: Option<String>,
    /// The subprocess's exit status as the OS reported it.
    pub exit_status: Option<String>,
    pub log_tail: Option<hcore::events::LogTailData>,
}

impl ErrorDetail {
    /// Pull the structured detail out of an error chain.
    ///
    /// Every field is optional because every one of them is genuinely absent for
    /// some failures: a target can fail before it ever starts a process, and an
    /// internal engine error is neither upstream damage nor a subprocess.
    pub fn from_error(e: &anyhow::Error) -> Self {
        use hplugin::error::{ProcessFailed, TargetFailure, UpstreamFailed};
        let mut d = Self {
            message: format!("{e:#}"),
            ..Self::default()
        };
        for cause in e.chain() {
            if d.upstream_of.is_none()
                && let Some(u) = cause.downcast_ref::<UpstreamFailed>()
            {
                d.upstream_of = Some(u.root.format());
            }
            if d.exit_status.is_none()
                && let Some(p) = cause.downcast_ref::<ProcessFailed>()
            {
                d.exit_status = Some(p.status.clone());
            }
            if d.log_tail.is_none()
                && let Some(t) = cause.downcast_ref::<TargetFailure>()
                && let Some(tail) = &t.log_tail
            {
                d.log_tail = Some(hcore::events::LogTailData {
                    text: tail.text.clone(),
                    start_line: tail.start_line,
                });
            }
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
}

struct EndGuard {
    data: Option<Arc<RequestStateData>>,
    make_end: Option<Box<dyn FnOnce(Option<ErrorDetail>) -> BuildEventKind + Send>>,
    error: Option<ErrorDetail>,
}

impl Drop for EndGuard {
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
    rs.emit(start);
    let mut guard = EndGuard {
        data: Some(rs.data()),
        make_end: Some(Box::new(make_end)),
        error: None,
    };
    let out = fut.await; // guard still armed if this is cancelled mid-await
    if let Err(e) = &out {
        guard.error = Some(ErrorDetail::from_error(e));
    }
    out // guard drops here → emits *End
}
