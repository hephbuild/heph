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
            // The cause as the user reads it: the diagnostic when one rides
            // the chain, else its frames with the engine's stage frames
            // (`execute //p:t`) and empty-`Display` frames (a failure's
            // artifacts) left out. `chain_message` is for `debug!`.
            message: hplugin::diagnostic::cause_frames(e).join(": "),
            cancelled: hplugin::error::is_cancelled(e),
            kept: slot.is_some(),
            streamed: crate::engine::failure_slot::RunStreamed::on(e),
            left_behind: slot.map(|s| hcore::events::LeftBehindData {
                // So a reader of an end with no record reads this fill of
                // the slot, never a newer run's (U26).
                log_fill_id: s.fill_id.clone(),
                ..hplugin::error::left_behind(
                    s.log.as_ref(),
                    s.sandbox.as_ref(),
                    s.out.as_deref(),
                    s.fixes.as_ref(),
                    None,
                )
            }),
            ..Self::default()
        };
        // Another request's recorded failure, surfaced at its top: its
        // record says whether its run streamed. The log itself is never
        // read here: `ResultEnd` carries the record's reference (P13).
        let surfaced = hcore::hmemoizer::downcast_chain_ref::<TargetFailure>(e);
        if let Some(t) = surfaced {
            d.streamed |= t.streamed;
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
        // Through the memoizer's shared wrappers too.
        if let Some(t) = crate::engine::run_timeout::timed_out(e) {
            d.timed_out = Some(hcore::events::TimedOutData {
                timeout_ms: t.timeout.as_millis(),
                abandoned: t.abandoned_after.is_some(),
            });
        }
        // A surfaced failure holds no chain (D7b): what the chain did not
        // say, its record does.
        if let Some(record) = surfaced.map(|t| &t.record) {
            if d.exit_status.is_none()
                && let Some(x) = &record.exit
            {
                d.exit_status = Some(x.status.clone());
                d.exit_code = x.code;
            }
            if d.timed_out.is_none() {
                d.timed_out.clone_from(&record.timed_out);
            }
        }
        d
    }

    /// [`from_error`](Self::from_error) for an event: its message scrubbed
    /// of what the request was handed to scrub, and within the message cap
    /// ([`RequestStateData::scrub_event_text`], I7). Every detail an event
    /// carries is built here: the event reaches `events.ndjson`, the session
    /// store and hook plugins.
    pub(crate) fn for_event(e: &anyhow::Error, data: &RequestStateData) -> Self {
        let mut d = Self::from_error(e);
        d.message = data.scrub_event_text(&d.message);
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
    emit_scope_with(
        rs,
        start,
        make_end,
        |e| (e, None::<std::future::Ready<()>>),
        fut,
    )
    .await
}

/// [`emit_scope`], handing a failure to `on_error` before the end event is
/// built. `on_error` returns the error the scope returns (the result scope
/// classifies it there, so its end knows whether it won the culprit's claim)
/// and, for an end that carries something only an await can learn, a future
/// that leaves it where `make_end` reads it. The failure's detail is taken
/// from the error as the body returned it, first, so a scope cancelled
/// during that await still ends as the failure it was, without what the
/// await would have added.
pub async fn emit_scope_with<T, F: Future<Output = ()>>(
    rs: &RequestState,
    start: BuildEventKind,
    make_end: impl FnOnce(Option<ErrorDetail>) -> BuildEventKind + Send + 'static,
    on_error: impl FnOnce(anyhow::Error) -> (anyhow::Error, Option<F>),
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    rs.emit(start);
    let mut guard = EndGuard {
        data: Some(rs.data()),
        make_end: Some(make_end),
        error: None,
    };
    // The guard stays armed if this is cancelled mid-await, and drops at
    // return → emits *End.
    match fut.await {
        Ok(v) => Ok(v),
        Err(e) => {
            guard.error = Some(ErrorDetail::for_event(&e, &rs.data));
            let (e, more) = on_error(e);
            if let Some(more) = more {
                more.await;
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::hook::Hook;
    use crate::engine::{Config, Engine};

    /// Each `ResultStart`/`ResultEnd` a hook saw, as `(start?, error)`.
    #[derive(Default)]
    struct Rec {
        seen: parking_lot::Mutex<Vec<(bool, Option<String>)>>,
    }

    impl Hook for Rec {
        fn name(&self) -> String {
            "rec".into()
        }
        fn on_event(&self, ev: &BuildEvent) {
            match &ev.kind {
                BuildEventKind::ResultStart { .. } => self.seen.lock().push((true, None)),
                BuildEventKind::ResultEnd { error, .. } => {
                    self.seen.lock().push((false, error.clone()));
                }
                _ => {}
            }
        }
        fn on_close(&self) {}
    }

    fn end(error: Option<ErrorDetail>) -> BuildEventKind {
        BuildEventKind::ResultEnd {
            failure: None,
            duplicate: false,
            left_behind: None,
            cancelled: false,
            addr: "//a:b".into(),
            error: error.map(ErrorDetail::into_message),
            upstream_of: None,
            upstream_file: None,
            path: Vec::new(),
            exit_status: None,
            exit_code: None,
            log_tail: None,
            timed_out: None,
        }
    }

    /// A scope dropped while `on_error`'s future is pending still pairs its
    /// Start with one End, and that End is the failure the body returned:
    /// the detail is taken before the await, so the cancel loses only what
    /// the await would have added.
    #[tokio::test]
    async fn emit_scope_with_cancelled_during_on_error_still_ends() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let _rt = crate::engine::test_rt_enter();
        let mut e = Engine::new(Config {
            parallelism: None,
            ..Config::for_tests(dir.path())
        })?;
        let rec = Arc::new(Rec::default());
        e.register_hook(Arc::clone(&rec) as Arc<dyn Hook>)?;
        let engine = Arc::new(e);
        let state = engine.new_state_with_events(false, None);

        let awaiting = tokio::sync::Notify::new();
        let scope = emit_scope_with(
            &state,
            BuildEventKind::ResultStart {
                addr: "//a:b".into(),
            },
            end,
            |e| {
                let more = async {
                    awaiting.notify_one();
                    std::future::pending::<()>().await;
                };
                (e, Some(more))
            },
            async { Err::<(), _>(anyhow::anyhow!("boom")) },
        );
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                _ = scope => false,
                () = awaiting.notified() => true,
            }
        })
        .await?;
        assert!(cancelled, "the scope ended before on_error's await");
        // `scope` was dropped when the select returned: the cancel.

        assert_eq!(
            rec.seen.lock().clone(),
            [(true, None), (false, Some("boom".to_string()))],
            "one Start, one End carrying the body's failure"
        );
        Ok(())
    }

    /// The event's message is the cause as the user reads it: none of the
    /// engine's stage frames (`execute //p:t`, `run`), and no empty frame
    /// rendered as `: :`; the context a driver added stays.
    #[test]
    fn error_detail_message_leaves_out_stage_frames() {
        use hplugin::diagnostic::{Boundary, with_stage};
        let a = hmodel::htaddr::parse_addr("//p:t").expect("addr");
        let e = anyhow::anyhow!("exit status: 1").context("driver run");
        let e = with_stage(with_stage(e, Boundary::Run, &a), Boundary::Execute, &a);
        assert_eq!(
            ErrorDetail::from_error(&e).message,
            "driver run: exit status: 1"
        );
        assert!(
            hcore::hmemoizer::chain_message(&e).contains("execute //p:t"),
            "the debug chain keeps them"
        );
    }
}
