//! The one choke point a failure goes through to become a [`FailureRecord`]
//! (D7b): what every renderer, `--json` and the failure slot read in place of
//! the error chain.
//!
//! [`compact`] takes the culprit's error once, when the request records it,
//! and keeps only what the caps allow. The order is the contract (I7, E5):
//! every text is **scrubbed first** of the secrets the request handed the
//! culprit (its credentials' values, the deferred values it read), and only
//! then escaped and cut. Cutting first could leave a secret's prefix on the
//! near side of the cut, where no later scrub would recognise it.
//!
//! Nothing here reaches a hash (I1): the record is built from the error after
//! the run, and [`Scrub`] lives on the request, never on a def.

use crate::engine::failure_slot::{FailureSlot, RunStreamed};
use hcore::diagnostic::{bound_snippet, bound_text};
use hcore::failure::{
    CAUSE_CAP, CAUSES_CAP, ExitData, FailedIn, FailureRecord, LogRef, RECORD_CAP,
};
use hcore::hmemoizer::{MultiError, downcast_chain_ref};
use hdriver_support::redact::{Needles, Redactor};
use hmodel::htaddr::Addr;
use hplugin::diagnostic::{Diagnostic, Stage, StageFrame, cause_frames, find_diagnostic};
use hplugin::error::{
    Culprit, FailureLog, FrozenCheckError, ProcessFailed, RemoteProcessFailed, TargetFailure,
};
use std::fmt;
use std::sync::Arc;

/// The secret values a failure's text is scrubbed of before it is recorded:
/// the credential values a run was handed, and the deferred values a target
/// read. Request-scoped (it lives on the `RequestState`), never on a def, so
/// nothing about it reaches a hash.
///
/// Its `Debug` says how many values it holds, never what they are.
#[derive(Clone, Default)]
pub struct Scrub(Vec<String>);

impl Scrub {
    /// The values in `values`.
    #[cfg(test)]
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        Self(values.into_iter().collect())
    }

    /// Also scrub `values`.
    pub fn extend(&mut self, values: impl IntoIterator<Item = String>) {
        self.0.extend(values);
    }

    /// Also scrub what `other` scrubs.
    pub fn extend_from(&mut self, other: &Self) {
        self.0.extend(other.0.iter().cloned());
    }

    /// Whether there is nothing to scrub.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The scrubber over these values; `None` when none is long enough to be
    /// scrubbed ([`REDACT_MIN_LEN`](hplugin::driver::REDACT_MIN_LEN)).
    fn needles(&self) -> Option<Arc<Needles>> {
        Needles::new(&self.0)
    }
}

impl fmt::Debug for Scrub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Scrub({} values)", self.0.len())
    }
}

/// `s` with every scrubbed value replaced, through the same scrubber the run
/// log's tee uses (one scrubber, as `hdriver_support::redact` asks).
fn scrub_text(needles: Option<&Arc<Needles>>, s: &str) -> String {
    let Some(needles) = needles else {
        return s.to_owned();
    };
    let mut r = Redactor::new(Arc::clone(needles));
    let mut out = r.push(s.as_bytes());
    out.extend(r.finish());
    // A needle is a whole `str` and the marker is ASCII, so the bytes stay
    // UTF-8; lossy only as a guard.
    match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// What a `warn` or louder log line on a target's failure path prints for
/// `e` (P21): its chain scrubbed of `scrub`, then on one line (control
/// escaped) and within the record's message cap — the order [`compact`]
/// keeps. The raw chain is the caller's to log at `debug`, and only there:
/// a `warn` line reaches the same terminal the record does.
pub(crate) fn log_line(e: &anyhow::Error, scrub: &Scrub) -> String {
    let scrubbed = scrub_text(scrub.needles().as_ref(), &format!("{e:#}"));
    bound_text(&scrubbed, hcore::diagnostic::MESSAGE_CAP, &mut false)
}

/// `e`, recorded against `culprit` and reached through `entry`, compacted:
/// scrubbed of `scrub`, within every cap, with the typed facts a renderer
/// reads. `stage_override` names the stage when the caller knows it better
/// than the chain does.
///
/// A failure that reached this request as another request's recorded
/// [`TargetFailure`] (surfaced at that request's top) is that failure's own
/// record, marked `surface_top`, scrubbed and bounded again here: the facts
/// it carries are the inner run's, which this chain no longer holds.
///
/// `path` is left empty: the request fills it.
pub(crate) fn compact(
    e: &anyhow::Error,
    culprit: &Culprit,
    entry: &Addr,
    scrub: &Scrub,
    stage_override: Option<Stage>,
) -> FailureRecord {
    let needles = scrub.needles();
    let clean = |s: &str| scrub_text(needles.as_ref(), s);

    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        let mut rec = (*tf.record).clone();
        rec.surface_top = true;
        if let Some(stage) = stage_override {
            rec.diagnostic.stage = stage;
        }
        return bound(scrubbed(rec, &clean));
    }

    let mut causes = Vec::new();
    let mut diagnostic = match find_diagnostic(e) {
        Some(d) => d.clone(),
        None => {
            let stage = hplugin::diagnostic::innermost_stage_frame(e)
                .map(StageFrame::stage)
                .unwrap_or(match culprit {
                    Culprit::File(_) => Stage::Eval,
                    Culprit::Target(_) => Stage::Run,
                });
            Diagnostic::new(stage, own_message(e, &mut causes))
        }
    };
    if let Some(stage) = stage_override {
        diagnostic.stage = stage;
    }
    if diagnostic.addr.is_none()
        && let Culprit::Target(addr) = culprit
    {
        diagnostic.addr = Some(addr.format());
    }

    let mut rec = FailureRecord::new(diagnostic);
    rec.addr = culprit.target().map(Addr::format);
    rec.file = culprit.file().map(str::to_owned);
    rec.entry = entry.format();
    rec.causes = causes;

    (rec.exit, rec.timed_out) = exec_facts(e);
    let frozen = downcast_chain_ref::<FrozenCheckError>(e);
    rec.frozen_diff = frozen.map(|f| f.diff.clone());
    rec.cancelled = hplugin::error::is_cancelled(e);
    rec.streamed = RunStreamed::on(e);
    rec.failed_in = if rec.exit.is_some() || rec.timed_out.is_some() {
        FailedIn::Exec
    } else if frozen.is_some()
        || downcast_chain_ref::<RunStreamed>(e).is_some()
        || downcast_chain_ref::<FailureSlot>(e).is_some_and(|s| s.after_run)
        || downcast_chain_ref::<hplugin::error::AfterProcess>(e).is_some()
    {
        FailedIn::AfterRun
    } else {
        FailedIn::BeforeRun
    };

    if let Some(slot) = downcast_chain_ref::<FailureSlot>(e) {
        rec.left_behind = Some(hcore::events::LeftBehindData {
            kept: slot.kept.clone(),
            kept_more: slot.kept_more,
            ..hplugin::error::left_behind(
                slot.log.as_ref(),
                slot.sandbox.as_ref(),
                slot.out.as_deref(),
                slot.fixes.as_ref(),
                None,
            )
        });
        rec.log = slot.log.as_ref().map(|log| LogRef {
            path: log.path().map(|p| p.display().to_string()),
            fill_id: slot.fill_id.clone(),
            lines: slot.log_lines,
            memory_tail: log.memory_tail().map(|t| t.text.clone()),
        });
        // The slot already cut the in-memory tail to the record cap (E2):
        // `bound` sees it within the cap, so the cut is said here.
        if slot.log.as_ref().is_some_and(FailureLog::memory_cut) {
            rec.diagnostic.truncated = true;
        }
    }

    bound(scrubbed(rec, &clean))
}

/// How `e`'s process ended, when `e` is a process that failed: its exit
/// (local or remote) and its timeout. Both `None` for any other failure.
/// What makes a record [`FailedIn::Exec`].
fn exec_facts(e: &anyhow::Error) -> (Option<ExitData>, Option<hcore::events::TimedOutData>) {
    let local = downcast_chain_ref::<ProcessFailed>(e);
    let exit = local
        .and_then(|p| p.exit)
        .or_else(|| downcast_chain_ref::<RemoteProcessFailed>(e).map(|r| r.exit));
    let exit = match (exit, local) {
        (Some(x), _) => Some(ExitData {
            code: match x {
                hcore::process::Exit::Code(c) => Some(c),
                hcore::process::Exit::Signal(_) => None,
            },
            signal: match x {
                hcore::process::Exit::Signal(s) => Some(s),
                hcore::process::Exit::Code(_) => None,
            },
            status: x.to_string(),
        }),
        (None, Some(p)) => Some(ExitData {
            code: None,
            signal: None,
            status: p.status.clone(),
        }),
        (None, None) => None,
    };
    let timed_out = crate::engine::run_timeout::timed_out(e).map(|t| hcore::events::TimedOutData {
        timeout_ms: t.timeout.as_millis(),
        abandoned: t.abandoned_after.is_some(),
    });
    (exit, timed_out)
}

/// What a terminal wrapper is told of `addr`'s run the moment its process
/// failed (D6.1c): a record holding the addr, the exit and the timeout as
/// [`compact`] will record them, scrubbed of `scrub`, [`FailedIn::Exec`].
/// Enough for the one line a streamed run gets
/// (`tui::failure_report::streamed_exec_line`), nothing more. `None` when
/// the run failed some other way, or was stopped: a stop is no failure.
pub(crate) fn exec_failure(addr: &Addr, e: &anyhow::Error, scrub: &Scrub) -> Option<FailureRecord> {
    if hplugin::error::is_cancelled(e) {
        return None;
    }
    let (exit, timed_out) = exec_facts(e);
    if exit.is_none() && timed_out.is_none() {
        return None;
    }
    let needles = scrub.needles();
    let addr = addr.format();
    let mut rec = FailureRecord::new(Diagnostic::new(Stage::Run, String::new()));
    rec.exit = exit.map(|mut x| {
        x.status = bound_text(
            &scrub_text(needles.as_ref(), &x.status),
            hcore::diagnostic::MESSAGE_CAP,
            &mut false,
        );
        x
    });
    rec.timed_out = timed_out;
    rec.failed_in = FailedIn::Exec;
    rec.entry = addr.clone();
    rec.addr = Some(addr);
    Some(rec)
}

/// `e`'s cause as one message, as [`cause_frames`] reads it, but with an
/// aggregate said as `{n} errors` and each of its members pushed onto
/// `causes` (its own cause frames, joined), in input order.
fn own_message(e: &anyhow::Error, causes: &mut Vec<String>) -> String {
    let texts: Vec<String> = hcore::hmemoizer::frames(e)
        .into_iter()
        .filter(|f| f.downcast_ref::<StageFrame>().is_none())
        .map(|f| match f.downcast_ref::<MultiError>() {
            Some(multi) => {
                causes.extend(multi.0.iter().map(|m| cause_frames(m).join(": ")));
                format!("{} errors", multi.0.len())
            }
            None => f.error().to_string(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    hcore::hmemoizer::own_parts(texts).join(": ")
}

/// Every text `rec` holds, scrubbed. Runs before any cap ([`bound`]).
fn scrubbed(mut rec: FailureRecord, clean: &impl Fn(&str) -> String) -> FailureRecord {
    let opt = |s: &mut Option<String>| {
        if let Some(s) = s {
            *s = clean(s);
        }
    };
    let d = &mut rec.diagnostic;
    d.message = clean(&d.message);
    opt(&mut d.addr);
    opt(&mut d.snippet);
    opt(&mut d.plugin);
    if let Some(loc) = &mut d.location {
        loc.file = clean(&loc.file);
    }
    for hint in &mut d.hints {
        hint.text = clean(&hint.text);
    }
    for cause in &mut rec.causes {
        *cause = clean(cause);
    }
    opt(&mut rec.frozen_diff);
    if let Some(exit) = &mut rec.exit {
        exit.status = clean(&exit.status);
    }
    if let Some(log) = &mut rec.log {
        opt(&mut log.memory_tail);
    }
    rec
}

/// `rec` within every cap, each cut marking the diagnostic `truncated`: the
/// diagnostic's own caps, the causes (the first [`CAUSES_CAP`], each within
/// [`CAUSE_CAP`]), the frozen diff as a snippet, the in-memory log tail (its
/// end kept), and then the whole within [`RECORD_CAP`] — dropping causes
/// from the end, then the snippet, the frozen diff and the hints, and last
/// cutting the message.
fn bound(mut rec: FailureRecord) -> FailureRecord {
    rec.diagnostic = rec.diagnostic.bounded();
    let mut cut = false;
    if rec.causes.len() > CAUSES_CAP {
        let more = rec.causes.len() - CAUSES_CAP;
        rec.more_causes = rec.more_causes.saturating_add(count(more));
        rec.causes.truncate(CAUSES_CAP);
        cut = true;
    }
    for cause in &mut rec.causes {
        *cause = bound_text(cause, CAUSE_CAP, &mut cut);
    }
    if let Some(diff) = &mut rec.frozen_diff {
        *diff = bound_snippet(diff, &mut cut);
    }
    if let Some(tail) = rec.log.as_mut().and_then(|l| l.memory_tail.as_mut()) {
        cut |= keep_end(tail, RECORD_CAP);
    }

    while rec.text_len() > RECORD_CAP && rec.causes.pop().is_some() {
        rec.more_causes = rec.more_causes.saturating_add(1);
        cut = true;
    }
    if rec.text_len() > RECORD_CAP && rec.diagnostic.snippet.take().is_some() {
        cut = true;
    }
    if rec.text_len() > RECORD_CAP && rec.frozen_diff.take().is_some() {
        cut = true;
    }
    while rec.text_len() > RECORD_CAP && rec.diagnostic.hints.pop().is_some() {
        cut = true;
    }
    let len = rec.text_len();
    if len > RECORD_CAP {
        let message = &rec.diagnostic.message;
        let room = message.len().saturating_sub(len - RECORD_CAP);
        rec.diagnostic.message = bound_text(message, room, &mut cut);
    }
    rec.diagnostic.truncated |= cut;
    rec
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `s` cut to its last `cap` bytes, at a char boundary. True when it cut.
fn keep_end(s: &mut String, cap: usize) -> bool {
    if s.len() <= cap {
        return false;
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.drain(..start);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::diagnostic::{HINT_CAP, HINTS_CAP, Hint, HintKind, MESSAGE_CAP, SNIPPET_LINES_CAP};
    use hplugin::error::{CancelledError, Exit, FailureFixes, SandboxFate};

    fn addr(s: &str) -> Addr {
        hmodel::htaddr::parse_addr(s).expect("addr")
    }

    fn target(s: &str) -> Culprit {
        Culprit::Target(addr(s))
    }

    /// `e` compacted as the culprit `//p:t`'s own failure, nothing scrubbed.
    fn of(e: &anyhow::Error) -> FailureRecord {
        compact(e, &target("//p:t"), &addr("//p:t"), &Scrub::default(), None)
    }

    fn many(n: usize, text: impl Fn(usize) -> String) -> anyhow::Error {
        MultiError((0..n).map(|i| anyhow::anyhow!(text(i))).collect()).into()
    }

    #[test]
    fn message_cap_2k_at_char_boundary() {
        // Two-byte chars: the cap (less the `…`) falls inside one.
        let e = anyhow::anyhow!("é".repeat(1500));
        let rec = of(&e);
        let m = &rec.diagnostic.message;
        assert!(m.len() <= MESSAGE_CAP, "{} bytes", m.len());
        assert_eq!(*m, format!("{}…", "é".repeat(1022)));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn hint_count_cap_8() {
        let mut d = Diagnostic::new(Stage::Run, "boom");
        for i in 0..12 {
            d = d.with_hint(Hint::new(HintKind::Other, format!("hint {i}")));
        }
        let rec = of(&anyhow::Error::new(d));
        let hints: Vec<&str> = rec
            .diagnostic
            .hints
            .iter()
            .map(|h| h.text.as_str())
            .collect();
        assert_eq!(hints.len(), HINTS_CAP);
        assert_eq!(hints.first(), Some(&"hint 0"));
        assert_eq!(hints.last(), Some(&"+5 more"));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn hint_len_cap_256() {
        let d = Diagnostic::new(Stage::Run, "boom")
            .with_hint(Hint::new(HintKind::DidYouMean, "h".repeat(1000)));
        let rec = of(&anyhow::Error::new(d));
        let hint = &rec.diagnostic.hints.first().expect("hint").text;
        assert!(hint.len() <= HINT_CAP, "{} bytes", hint.len());
        assert!(hint.ends_with('…'));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn cause_cap_16_in_input_order() {
        let rec = of(&many(20, |i| format!("member {i}")));
        assert_eq!(rec.diagnostic.message, "20 errors");
        let want: Vec<String> = (0..CAUSES_CAP).map(|i| format!("member {i}")).collect();
        assert_eq!(rec.causes, want);
        assert_eq!(rec.more_causes, 4);
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn cause_len_cap_512() {
        let rec = of(&many(2, |i| {
            if i == 0 {
                "c".repeat(2000)
            } else {
                "short".into()
            }
        }));
        let first = rec.causes.first().expect("cause");
        assert!(first.len() <= CAUSE_CAP, "{} bytes", first.len());
        assert!(first.ends_with('…'));
        assert_eq!(rec.causes.get(1).map(String::as_str), Some("short"));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn snippet_cap_10_lines() {
        let snippet: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let d = Diagnostic::new(Stage::Eval, "bad").with_snippet(snippet.join("\n"));
        let rec = of(&anyhow::Error::new(d));
        let kept = rec.diagnostic.snippet.as_deref().expect("snippet");
        let lines: Vec<&str> = kept.lines().collect();
        assert_eq!(lines.len(), SNIPPET_LINES_CAP + 1, "{kept}");
        assert_eq!(lines.first(), Some(&"line 0"));
        assert_eq!(lines.last(), Some(&"…"));
        assert!(rec.diagnostic.truncated);
    }

    /// Past the per-field caps, a record over 8 KiB drops causes from the
    /// end, and keeps the snippet and hints while that is enough.
    #[test]
    fn record_total_cap_8k() {
        let mut d = Diagnostic::new(Stage::Run, "m".repeat(2000))
            .with_snippet(vec!["s".repeat(150); 10].join("\n"));
        for _ in 0..HINTS_CAP {
            d = d.with_hint(Hint::new(HintKind::Other, "h".repeat(200)));
        }
        let mut inner = FailureRecord::new(d);
        inner.causes = (0..CAUSES_CAP)
            .map(|i| format!("{i:03}{}", "c".repeat(497)))
            .collect();
        assert!(inner.text_len() > RECORD_CAP, "the fixture is over the cap");
        let tf = TargetFailure::new(addr("//p:t")).with_record(Arc::new(inner.clone()));
        let rec = of(&anyhow::Error::new(tf));
        assert!(rec.text_len() <= RECORD_CAP, "{} bytes", rec.text_len());
        assert!(rec.diagnostic.truncated);
        assert!(!rec.causes.is_empty() && rec.causes.len() < CAUSES_CAP);
        assert_eq!(
            rec.causes,
            inner.causes.get(..rec.causes.len()).expect("prefix")
        );
        assert_eq!(rec.more_causes as usize, CAUSES_CAP - rec.causes.len());
        assert_eq!(rec.diagnostic.snippet, inner.diagnostic.snippet);
        assert_eq!(rec.diagnostic.hints.len(), HINTS_CAP);
    }

    /// When causes alone cannot bring it under the cap, the snippet goes,
    /// then the hints, and last the message is cut.
    #[test]
    fn record_total_cap_8k_cuts_the_message_last() {
        let mut d = Diagnostic::new(Stage::Run, "m".repeat(2000))
            .with_snippet("snippet")
            .with_addr("a".repeat(1000))
            .with_plugin("p".repeat(200));
        for _ in 0..HINTS_CAP {
            d = d.with_hint(Hint::new(HintKind::Other, "h".repeat(200)));
        }
        let mut inner = FailureRecord::new(d);
        // Identity fields are not capped per field: here they fill the record.
        inner.entry = "e".repeat(6000);
        let tf = TargetFailure::new(addr("//p:t")).with_record(Arc::new(inner));
        let rec = of(&anyhow::Error::new(tf));
        assert!(rec.text_len() <= RECORD_CAP, "{} bytes", rec.text_len());
        assert_eq!(rec.diagnostic.snippet, None);
        assert!(rec.diagnostic.hints.is_empty());
        assert!(rec.diagnostic.message.ends_with('…'));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn typed_facts_survive_compaction_exit_signal() {
        let e = anyhow::Error::new(ProcessFailed::new("signal: 9", Some(Exit::Signal(9))))
            .context(FailureSlot::default());
        let rec = of(&e);
        let exit = rec.exit.expect("exit");
        assert_eq!((exit.code, exit.signal), (None, Some(9)));
        assert_eq!(exit.status, Exit::Signal(9).to_string());
        assert_eq!(rec.failed_in, FailedIn::Exec);
    }

    #[test]
    fn typed_facts_survive_compaction_timed_out() {
        let e = anyhow::Error::new(crate::engine::run_timeout::TimedOutError {
            addr: addr("//p:t"),
            timeout: hplugin::driver::Timeout::new(std::time::Duration::from_secs(10))
                .expect("timeout"),
            abandoned_after: Some(std::time::Duration::from_secs(10)),
        });
        let rec = of(&e);
        let t = rec.timed_out.expect("timed out");
        assert_eq!((t.timeout_ms, t.abandoned), (10_000, true));
        assert_eq!(rec.failed_in, FailedIn::Exec);
    }

    #[test]
    fn typed_facts_survive_compaction_frozen_diff() {
        let diff: Vec<String> = (0..30).map(|i| format!("+line {i}")).collect();
        let e = anyhow::Error::new(FrozenCheckError {
            addr: addr("//p:t"),
            diff: diff.join("\n"),
        });
        let rec = of(&e);
        let kept = rec.frozen_diff.expect("diff");
        assert!(kept.starts_with("+line 0\n"));
        assert_eq!(kept.lines().count(), SNIPPET_LINES_CAP + 1);
        assert_eq!(rec.failed_in, FailedIn::AfterRun);
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn typed_facts_survive_compaction_cancelled() {
        let rec = of(&anyhow::Error::new(CancelledError).context("building //p:t"));
        assert!(rec.cancelled);
        assert_eq!(rec.failed_in, FailedIn::BeforeRun);
    }

    #[test]
    fn typed_facts_survive_compaction_fixes() {
        let slot = FailureSlot {
            fixes: Some(FailureFixes::Written {
                root: "/ws".into(),
                paths: vec!["a.go".into()],
                list: None,
            }),
            ..FailureSlot::default()
        };
        let e = anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))))
            .context(slot);
        let rec = of(&e);
        let left = rec.left_behind.expect("left behind");
        assert_eq!(
            left.fixes,
            Some(hcore::events::FixesData::Written {
                root: "/ws".into(),
                paths: vec!["a.go".into()],
                list: None,
            })
        );
        assert_eq!(rec.exit.and_then(|x| x.code), Some(1));
    }

    #[test]
    fn typed_facts_survive_compaction_sandbox() {
        let slot = FailureSlot::default().with_sandbox(Some(SandboxFate::Kept("/sb/t".into())));
        let rec = of(&anyhow::anyhow!("boom").context(slot));
        let left = rec.left_behind.expect("left behind");
        assert_eq!(left.sandbox.as_deref(), Some("/sb/t (kept)"));
    }

    /// Another request's recorded failure keeps its own facts, marked as
    /// surfaced, and is scrubbed again with this request's values.
    #[test]
    fn typed_facts_survive_compaction_surface_top() {
        let secret = "tok-0123456789abcdef";
        let mut inner = FailureRecord::new(Diagnostic::new(
            Stage::Run,
            format!("auth {secret} refused"),
        ));
        inner.exit = Some(ExitData {
            code: Some(3),
            signal: None,
            status: "exit status: 3".into(),
        });
        inner.failed_in = FailedIn::Exec;
        let tf = TargetFailure::new(addr("//q:inner")).with_record(Arc::new(inner));
        let e = anyhow::Error::new(tf).context("calling //q:inner");
        let rec = compact(
            &e,
            &target("//p:t"),
            &addr("//p:t"),
            &Scrub::new([secret.to_owned()]),
            None,
        );
        assert!(rec.surface_top);
        assert_eq!(rec.exit.and_then(|x| x.code), Some(3));
        assert_eq!(rec.failed_in, FailedIn::Exec);
        assert_eq!(rec.diagnostic.message, "auth [redacted] refused");
    }

    /// The secret straddles the message cap and a cause's: cut first, its
    /// head would stay on the near side of the cut where no scrub sees a
    /// whole secret. Scrubbed first, nothing of it survives.
    #[test]
    fn scrub_before_truncate_secret_across_the_cut() {
        let secret = "SECRET-0123456789-abcdefghijklmnop";
        let message = format!(
            "{}{secret}{}",
            "x".repeat(MESSAGE_CAP - 16),
            "y".repeat(100)
        );
        let cause = format!("{}{secret}{}", "c".repeat(CAUSE_CAP - 16), "z".repeat(100));
        let e = || -> anyhow::Error {
            anyhow::Error::new(MultiError(vec![anyhow::anyhow!(cause.clone())]))
                .context(message.clone())
        };
        let leaks = |rec: &FailureRecord| -> Vec<String> {
            let texts = std::iter::once(&rec.diagnostic.message).chain(&rec.causes);
            texts
                .flat_map(|t| {
                    (8..=secret.len())
                        .filter_map(|n| secret.get(..n))
                        .filter(|p| t.contains(p))
                        .map(|p| format!("{p:?} in {} bytes", t.len()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };

        // The fixture has teeth: unscrubbed, a prefix survives each cut.
        let raw = of(&e());
        assert!(raw.diagnostic.message.len() <= MESSAGE_CAP);
        assert_eq!(
            leaks(&raw).len(),
            2 * (16 - '…'.len_utf8() - 8 + 1),
            "{:?}",
            leaks(&raw)
        );

        let rec = compact(
            &e(),
            &target("//p:t"),
            &addr("//p:t"),
            &Scrub::new([secret.to_owned()]),
            None,
        );
        assert_eq!(leaks(&rec), Vec::<String>::new());
        assert!(rec.diagnostic.message.contains("[redacted]"));
        assert!(rec.causes.first().is_some_and(|c| c.contains("[redacted]")));
        assert!(rec.diagnostic.truncated);
    }

    #[test]
    fn scrub_debug_never_prints_a_value() {
        let s = Scrub::new(["hunter2-hunter2".to_owned()]);
        assert_eq!(format!("{s:?}"), "Scrub(1 values)");
    }

    /// The stage: the caller's override, else the diagnostic's, else the
    /// innermost stage frame, else the culprit's kind.
    #[test]
    fn stage_falls_back_from_override_to_culprit() {
        let e = anyhow::anyhow!("boom");
        assert_eq!(of(&e).diagnostic.stage, Stage::Run);
        let file = Culprit::File(Arc::from("p/BUILD"));
        let rec = compact(&e, &file, &addr("//p:t"), &Scrub::default(), None);
        assert_eq!(rec.diagnostic.stage, Stage::Eval);
        assert_eq!((rec.addr, rec.file.as_deref()), (None, Some("p/BUILD")));
        let framed = hplugin::diagnostic::with_stage(
            anyhow::anyhow!("boom"),
            hplugin::diagnostic::Boundary::GetDef,
            &addr("//p:t"),
        );
        let rec = of(&framed);
        assert_eq!(rec.diagnostic.stage, Stage::Parse);
        assert_eq!(rec.diagnostic.message, "boom");
        let rec = compact(
            &framed,
            &target("//p:t"),
            &addr("//p:t"),
            &Scrub::default(),
            Some(Stage::Link),
        );
        assert_eq!(rec.diagnostic.stage, Stage::Link);
    }
}
