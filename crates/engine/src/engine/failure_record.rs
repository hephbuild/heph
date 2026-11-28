//! The one choke point a failure goes through to become a [`FailureRecord`]
//! (D7b): what every renderer, `--json` and the failure slot read in place of
//! the error chain.
//!
//! [`compact`] takes the culprit's error once, when the request records it,
//! and keeps only what the caps allow. The order is the contract (I7, E5):
//! every text is **scrubbed first** of the secrets the request handed the
//! culprit (its credentials' material; a resolved deferred value is not a
//! secret, U20), and only
//! then escaped and cut. Cutting first could leave a secret's prefix on the
//! near side of the cut, where no later scrub would recognise it.
//!
//! Nothing here reaches a hash (I1): the record is built from the error after
//! the run, and [`Scrub`] lives on the request, never on a def.

use crate::engine::failure_slot::{FailureSlot, RunStreamed};
use hcore::diagnostic::{FILE_CAP, HINT_CAP, bound_snippet, bound_text};
use hcore::failure::{
    CAUSE_CAP, CAUSES_CAP, ExitData, FailedIn, FailureRecord, LogRef, RECORD_CAP,
};
use hcore::hmemoizer::{MultiError, downcast_chain_ref};
use hdriver_support::redact::{Needles, Redactor};
use hmodel::htaddr::Addr;
use hplugin::diagnostic::{Diagnostic, Stage, StageFrame, cause_frames, find_diagnostic};
use hplugin::error::{
    Culprit, FailureLog, FrozenCheckError, MissingDep, ProcessFailed, RemoteProcessFailed,
    RevisionChangedError, TargetFailure,
};
use std::fmt;
use std::sync::Arc;

/// The secret values a failure's text is scrubbed of before it is recorded:
/// the credential material a run was handed, and nothing else: only what heph
/// knows for sure is a secret is scrubbed (U20). Request-scoped (it lives on the `RequestState`), never on a def, so
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

    /// `s` with every value replaced.
    pub(crate) fn apply(&self, s: &str) -> String {
        scrub_text(self.needles().as_ref(), s)
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
    scrub_line(&format!("{e:#}"), scrub)
}

/// `text` scrubbed of `scrub`, then control-escaped and within the record's
/// message cap: [`log_line`] for a text already formatted, such as an
/// event's `error` (I7).
pub(crate) fn scrub_line(text: &str, scrub: &Scrub) -> String {
    let scrubbed = scrub_text(scrub.needles().as_ref(), text);
    bound_text(&scrubbed, hcore::diagnostic::MESSAGE_CAP, &mut false)
}

/// `e`, recorded against `culprit` and reached through `entry`, compacted:
/// scrubbed of `scrub`, within every cap, with the typed facts a renderer
/// reads.
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
) -> FailureRecord {
    let needles = scrub.needles();
    let clean = |s: &str| scrub_text(needles.as_ref(), s);

    if let Some(tf) = downcast_chain_ref::<TargetFailure>(e) {
        let mut rec = (*tf.record).clone();
        rec.surface_top = true;
        return bound(scrubbed(rec, &clean));
    }

    let mut causes = Vec::new();
    let mut more_causes = 0;
    let found = find_diagnostic(e);
    let raised = found.is_some();
    let (exit, timed_out) = exec_facts(e);
    // A process that ended with no diagnostic of its driver's has its end
    // said as every renderer's header says it (`exit status 3`, `killed by
    // signal 9 (SIGKILL)`, `timed out after 1s (its timeout)`), not as the
    // chain's `process exited with status: …`. Only the engine's words are
    // replaced: a timeout, and a local child's status. A cdylib's failure
    // message is the plugin's own text (a Go test's `--- FAIL` lines), and
    // stays.
    let end =
        if !raised && (timed_out.is_some() || downcast_chain_ref::<ProcessFailed>(e).is_some()) {
            let mut ended = FailureRecord::new(Diagnostic::new(Stage::Run, String::new()));
            ended.exit.clone_from(&exit);
            ended.timed_out.clone_from(&timed_out);
            ended.process_end_message()
        } else {
            None
        };
    // The culprit declared a dependency on a target that does not exist.
    let missing_dep =
        downcast_chain_ref::<MissingDep>(e).filter(|m| culprit.target() == Some(&m.declarer));
    let frame = hplugin::diagnostic::innermost_stage_frame(e);
    let mut diagnostic = match found {
        Some(d) => d.clone(),
        None => {
            // A dependent blamed for an input that does not exist stopped
            // linking its inputs; the innermost frame is the input's
            // `get_def`, which would read as a parse failure.
            let stage = if missing_dep.is_some() {
                Stage::Link
            } else {
                frame.map(StageFrame::stage).unwrap_or(match culprit {
                    Culprit::File(_) => Stage::Eval,
                    Culprit::Target(_) => Stage::Run,
                })
            };
            let message = own_message(e, culprit, &mut causes, &mut more_causes, end.as_deref());
            Diagnostic::new(stage, message)
        }
    };
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
    rec.more_causes = more_causes;
    rec.boundary = frame.map(|f| f.boundary.as_str().to_owned());
    rec.missing_dep = missing_dep.map(|m| hcore::failure::MissingDepData {
        dep: m.dep.format(),
        declarer: m.declarer.format(),
        origin_id: m.origin_id.clone(),
    });

    (rec.exit, rec.timed_out) = (exit, timed_out);
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
        // A record names the first written paths and counts the rest, when
        // the slot's `fixes.txt` lists them all (I8, CQ-5). Without that
        // list every path stays: a restore hint naming fewer would leave
        // files behind.
        if let Some(hcore::events::FixesData::Written {
            paths,
            list: Some(_),
            paths_more,
            ..
        }) = rec.left_behind.as_mut().and_then(|l| l.fixes.as_mut())
            && paths.len() > hcore::events::FIXES_PATHS_CAP
        {
            *paths_more = count(paths.len() - hcore::events::FIXES_PATHS_CAP);
            paths.truncate(hcore::events::FIXES_PATHS_CAP);
        }
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
/// failed (D6.1c): a record holding the addr, the exit, the timeout and the
/// diagnostic as [`compact`] will record them (it is compact's, scrubbed of
/// `scrub`), [`FailedIn::Exec`]. Enough for the line a streamed run gets
/// (`tui::failure_report::streamed_exec_line`), which shows a driver's own
/// diagnostic and hints, since no box will. `None` when the run failed some
/// other way, or was stopped: a stop is no failure.
pub(crate) fn exec_failure(addr: &Addr, e: &anyhow::Error, scrub: &Scrub) -> Option<FailureRecord> {
    if hplugin::error::is_cancelled(e) {
        return None;
    }
    let (exit, timed_out) = exec_facts(e);
    if exit.is_none() && timed_out.is_none() {
        return None;
    }
    let full = compact(e, &Culprit::Target(addr.clone()), addr, scrub);
    let mut rec = FailureRecord::new(full.diagnostic);
    rec.exit = full.exit;
    rec.timed_out = full.timed_out;
    rec.failed_in = FailedIn::Exec;
    rec.entry = full.entry;
    rec.addr = full.addr;
    Some(rec)
}

/// `e`'s cause as one message, as [`cause_frames`] reads it, but with an
/// aggregate said as `{n} errors` and its members pushed onto `causes` (each
/// one's own cause frames, joined), in input order: only the first
/// [`CAUSES_CAP`] are formatted, and the rest are counted into `more` (CQ-6).
/// An aggregate of thousands is said in 16 formats, not thousands.
///
/// With `end`, the process-exit frame itself (a local [`ProcessFailed`] or
/// the engine's timeout) is said as `end`, once, and every other frame is
/// kept: what a driver or a wrapper said about the failure (a write-back
/// refused, the remedy a driver layers over the exit) stays beside it.
///
/// A frame that names `culprit` as its own subject, typed ([`RevisionChangedError`]),
/// is said without it: the record names the culprit already.
fn own_message(
    e: &anyhow::Error,
    culprit: &Culprit,
    causes: &mut Vec<String>,
    more: &mut u32,
    end: Option<&str>,
) -> String {
    let frames: Vec<(String, bool)> = hcore::hmemoizer::frames(e)
        .into_iter()
        .filter(|f| !hplugin::diagnostic::is_internal_frame(f))
        .map(|f| {
            let is_end = end.is_some()
                && (f.downcast_ref::<ProcessFailed>().is_some()
                    || f.downcast_ref::<crate::engine::run_timeout::TimedOutError>()
                        .is_some());
            let text = if let Some(multi) = f.downcast_ref::<MultiError>() {
                let room = CAUSES_CAP.saturating_sub(causes.len()).min(multi.0.len());
                causes.extend(
                    multi
                        .0
                        .iter()
                        .take(room)
                        .map(|m| cause_frames(m).join(": ")),
                );
                *more = more.saturating_add(count(multi.0.len() - room));
                format!("{} errors", multi.0.len())
            } else if let Some(changed) = f.downcast_ref::<RevisionChangedError>()
                && culprit.target() == Some(&changed.addr)
            {
                changed.what()
            } else {
                f.error().to_string()
            };
            (text, is_end)
        })
        .filter(|(s, is_end)| *is_end || !s.is_empty())
        .collect();
    let mut said_end = false;
    let mut parts = Vec::with_capacity(frames.len());
    for (i, (text, is_end)) in frames.iter().enumerate() {
        if *is_end && let Some(end) = end {
            // A second exit frame (a timeout over the killed child's
            // status) says nothing the first did not.
            if !said_end {
                parts.push(end);
                said_end = true;
            }
            continue;
        }
        // Cut against the next frame's own text, before any replacement,
        // as `own_parts` does.
        let own = match frames.get(i + 1) {
            Some((next, _)) => hcore::hmemoizer::own_part(text, next),
            None => text,
        };
        if !own.is_empty() {
            parts.push(own);
        }
    }
    parts.join(": ")
}

/// Every text `rec` holds, scrubbed. Runs before any cap ([`bound`]).
///
/// Every struct is destructured whole, so a new field does not compile
/// until it is either scrubbed here or named as holding no secret.
fn scrubbed(mut rec: FailureRecord, clean: &impl Fn(&str) -> String) -> FailureRecord {
    let opt = |s: &mut Option<String>| {
        if let Some(s) = s {
            *s = clean(s);
        }
    };
    let FailureRecord {
        // Addresses and workspace paths the engine resolved, and counts.
        addr: _,
        file: _,
        entry: _,
        path: _,
        took_down: _,
        missing_dep: _,
        boundary: _,
        more_causes: _,
        failed_in: _,
        timed_out: _,
        cancelled: _,
        surface_top: _,
        streamed: _,
        // Engine-made paths (the slot's log, `out/`, the sandbox) and the
        // write-back's workspace paths.
        left_behind: _,
        diagnostic,
        causes,
        frozen_diff,
        exit,
        log,
    } = &mut rec;
    let Diagnostic {
        stage: _,
        truncated: _,
        addr,
        message,
        location,
        snippet,
        hints,
        plugin,
    } = diagnostic;
    *message = clean(message);
    opt(addr);
    opt(snippet);
    opt(plugin);
    if let Some(hcore::diagnostic::SourceLoc {
        file,
        line: _,
        col: _,
    }) = location
    {
        *file = clean(file);
    }
    for hcore::diagnostic::Hint { kind: _, text } in hints {
        *text = clean(text);
    }
    for cause in causes {
        *cause = clean(cause);
    }
    opt(frozen_diff);
    if let Some(ExitData {
        code: _,
        signal: _,
        status,
    }) = exit
    {
        *status = clean(status);
    }
    if let Some(LogRef {
        // The slot's path and fill id: engine-made.
        path: _,
        fill_id: _,
        lines: _,
        memory_tail,
    }) = log
    {
        opt(memory_tail);
    }
    rec
}

/// `rec` within every cap, each cut marking the diagnostic `truncated`: the
/// diagnostic's own caps, the causes (the first [`CAUSES_CAP`], each within
/// [`CAUSE_CAP`]), the frozen diff as a snippet, the in-memory log tail (its
/// end kept), each identity field within [`FILE_CAP`] and the exit status
/// within [`HINT_CAP`], and then the whole within [`RECORD_CAP`] — dropping
/// causes from the end, then the snippet, the frozen diff and the hints,
/// then cutting the message, and last cutting the identity fields, least
/// telling first.
fn bound(mut rec: FailureRecord) -> FailureRecord {
    rec.diagnostic = rec.diagnostic.bounded();
    // Causes `compact` counted rather than formatted were cut already.
    let mut cut = rec.more_causes > 0;
    for field in identity(&mut rec) {
        *field = bound_text(field, FILE_CAP, &mut cut);
    }
    if let Some(exit) = &mut rec.exit {
        exit.status = bound_text(&exit.status, HINT_CAP, &mut cut);
    }
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
    // Only fields no per-field cap reaches together can still be over: the
    // identity fields, each within its own cap, and the exit status.
    let mut over = rec.text_len().saturating_sub(RECORD_CAP);
    if over > 0 {
        if let Some(exit) = &mut rec.exit {
            over = shed(&mut exit.status, over, &mut cut);
        }
        for field in identity(&mut rec) {
            over = shed(field, over, &mut cut);
        }
    }
    rec.diagnostic.truncated |= cut;
    rec
}

/// The record's identity texts, the least telling first: the order the
/// last step of [`bound`] cuts them in. The culprit's own address comes last.
fn identity(rec: &mut FailureRecord) -> impl Iterator<Item = &mut String> {
    let d = &mut rec.diagnostic;
    let (dep, declarer, origin) = match &mut rec.missing_dep {
        Some(m) => (
            Some(&mut m.dep),
            Some(&mut m.declarer),
            Some(&mut m.origin_id),
        ),
        None => (None, None, None),
    };
    [
        rec.boundary.as_mut(),
        origin,
        d.location.as_mut().map(|l| &mut l.file),
        rec.file.as_mut(),
        declarer,
        dep,
        Some(&mut rec.entry),
        d.addr.as_mut(),
        rec.addr.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// `s` shortened by up to `over` bytes, `…` marking the cut (`s` emptied
/// when what is left would not hold it); what is still over afterwards.
fn shed(s: &mut String, over: usize, cut: &mut bool) -> usize {
    if over == 0 || s.is_empty() {
        return over;
    }
    let before = s.len();
    let room = before.saturating_sub(over);
    if room < '…'.len_utf8() {
        s.clear();
        *cut = true;
    } else {
        *s = bound_text(s, room, cut);
    }
    over.saturating_sub(before.saturating_sub(s.len()))
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
        compact(e, &target("//p:t"), &addr("//p:t"), &Scrub::default())
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

    /// CQ-6: an aggregate of thousands formats its first [`CAUSES_CAP`]
    /// members only, and counts the rest.
    #[test]
    fn only_the_kept_causes_are_formatted() {
        #[derive(Debug)]
        struct Counted(Arc<std::sync::atomic::AtomicUsize>, usize);
        impl std::fmt::Display for Counted {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                write!(f, "member {}", self.1)
            }
        }
        impl std::error::Error for Counted {}

        let formats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = 5000;
        let e: anyhow::Error = MultiError(
            (0..n)
                .map(|i| anyhow::Error::new(Counted(Arc::clone(&formats), i)))
                .collect(),
        )
        .into();
        let rec = of(&e);
        assert_eq!(rec.causes.len(), CAUSES_CAP);
        assert_eq!(rec.causes.first().map(String::as_str), Some("member 0"));
        assert_eq!(rec.more_causes as usize, n - CAUSES_CAP);
        assert!(rec.diagnostic.truncated);
        let formatted = formats.load(std::sync::atomic::Ordering::Relaxed);
        // Each kept member may be formatted a few times (its frames, then
        // its own part); never once per member of the aggregate.
        assert!(
            formatted <= 4 * CAUSES_CAP,
            "{formatted} formats for {n} members"
        );
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
        // Identity fields, each within its own cap, fill the record.
        inner.entry = "e".repeat(6000);
        inner.file = Some("f".repeat(FILE_CAP));
        inner.diagnostic.location = Some(loc("l".repeat(FILE_CAP)));
        inner.missing_dep = Some(hcore::failure::MissingDepData {
            dep: "d".repeat(FILE_CAP),
            declarer: "r".repeat(FILE_CAP),
            origin_id: String::new(),
        });
        let tf = TargetFailure::new(addr("//p:t")).with_record(Arc::new(inner));
        let rec = of(&anyhow::Error::new(tf));
        assert!(rec.text_len() <= RECORD_CAP, "{} bytes", rec.text_len());
        assert_eq!(rec.diagnostic.snippet, None);
        assert!(rec.diagnostic.hints.is_empty());
        assert!(rec.diagnostic.message.ends_with('…'));
        assert!(rec.diagnostic.message.len() > 1000, "only cut what is over");
        assert!(rec.diagnostic.truncated);
        // The identity fields are within their caps, and not cut further.
        assert_eq!(rec.entry.len(), FILE_CAP);
        assert!(rec.entry.ends_with('…'));
        assert_eq!(rec.file, Some("f".repeat(FILE_CAP)));
        let dep = rec.missing_dep.as_ref().expect("missing_dep");
        assert_eq!(dep.dep, "d".repeat(FILE_CAP));
    }

    /// CQ NIT: identity fields no per-field cap holds together are cut
    /// after the message, least telling first and the culprit's address
    /// last, so `RECORD_CAP` holds whatever the record carries.
    #[test]
    fn record_total_cap_8k_cuts_identity_after_the_message() {
        let big = |c: &str| c.repeat(6000);
        let mut d = Diagnostic::new(Stage::Run, big("m")).with_addr(big("a"));
        d.location = Some(loc(big("l")));
        let mut inner = FailureRecord::new(d);
        inner.addr = Some(format!("//p:{}", big("t")));
        inner.file = Some(big("f"));
        inner.entry = big("e");
        inner.boundary = Some(big("b"));
        inner.exit = Some(ExitData {
            code: Some(1),
            signal: None,
            status: big("s"),
        });
        inner.missing_dep = Some(hcore::failure::MissingDepData {
            dep: big("d"),
            declarer: big("r"),
            origin_id: big("o"),
        });
        let tf = TargetFailure::new(addr("//p:t")).with_record(Arc::new(inner));
        let rec = of(&anyhow::Error::new(tf));
        assert!(rec.text_len() <= RECORD_CAP, "{} bytes", rec.text_len());
        assert!(rec.diagnostic.truncated);
        assert_eq!(rec.diagnostic.message, "…");
        assert_eq!(rec.exit.as_ref().map(|x| x.status.as_str()), Some(""));
        assert_eq!(rec.boundary.as_deref(), Some(""));
        // What is most telling is kept, within its own cap.
        let culprit = rec.addr.as_deref().expect("addr");
        assert!(culprit.starts_with("//p:t") && culprit.len() == FILE_CAP);
        assert_eq!(rec.entry.len(), FILE_CAP);
        assert_eq!(
            rec.diagnostic.addr.as_ref().map(String::len),
            Some(FILE_CAP)
        );
    }

    fn loc(file: String) -> hcore::diagnostic::SourceLoc {
        hcore::diagnostic::SourceLoc {
            file,
            line: 1,
            col: 1,
        }
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
        assert_eq!(
            rec.diagnostic.message,
            format!(
                "timed out after 10s (its timeout); {}",
                hcore::failure::ABANDONED
            )
        );
        let t = rec.timed_out.expect("timed out");
        assert_eq!((t.timeout_ms, t.abandoned), (10_000, true));
        assert_eq!(rec.failed_in, FailedIn::Exec);
    }

    /// RUN-14/15: a local child's end with no diagnostic is said as the box
    /// header says it. A driver's own diagnostic, and a cdylib's own
    /// message, are kept as they are.
    #[test]
    fn a_process_end_is_said_as_the_header_says_it() {
        let local = |exit: Exit| {
            anyhow::Error::new(ProcessFailed::new(exit.to_string(), Some(exit)))
                .context(hplugin::diagnostic::DriverCall("driver run"))
                .context(FailureSlot::default())
        };
        assert_eq!(
            of(&local(Exit::Code(3))).diagnostic.message,
            "exit status 3"
        );
        assert_eq!(
            of(&local(Exit::Signal(9))).diagnostic.message,
            "killed by signal 9 (SIGKILL)"
        );

        let raised = anyhow::Error::new(Diagnostic::new(Stage::Run, "go vet found 2 issues"))
            .context(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))));
        assert_eq!(of(&raised).diagnostic.message, "go vet found 2 issues");

        let remote = anyhow::Error::new(RemoteProcessFailed::new(
            "--- FAIL: TestX\nexit status: 1".into(),
            Exit::Code(1),
        ));
        let rec = of(&remote);
        assert_eq!(rec.diagnostic.message, "--- FAIL: TestX\nexit status: 1");
        assert_eq!(rec.exit.and_then(|x| x.code), Some(1));
    }

    /// Only the exit frame is reworded: what a driver layered over the exit
    /// (its remedy) and what the engine joined onto the run's error (a
    /// refused write-back) are kept beside the status, so the message is
    /// not the status alone and a renderer shows it.
    #[test]
    fn a_process_end_keeps_the_wrappers_context() {
        let e = anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))))
            .context("docker_build //app:img: select a builder with `builder = \"heph\"`")
            .context(hplugin::diagnostic::DriverCall("driver run"))
            .context(FailureSlot::default())
            .context("//fix:upper rewrites //fix:src in place, and it changed while it ran");
        let rec = of(&e);
        assert_eq!(
            rec.diagnostic.message,
            "//fix:upper rewrites //fix:src in place, and it changed while it ran: \
             docker_build //app:img: select a builder with `builder = \"heph\"`: exit status 1"
        );
        assert!(!rec.message_is_status(), "{rec:?}");
        assert_eq!(rec.exit.and_then(|x| x.code), Some(1));

        // A revision of the culprit's own that changed is said without its
        // address, which the record names already; another target's keeps it.
        let changed = |of: &str| {
            anyhow::Error::new(RevisionChangedError {
                addr: addr(of),
                hashin: "abc".into(),
            })
        };
        let own = of(&changed("//p:t")).diagnostic.message;
        assert!(own.starts_with("its cached output (revision abc)"), "{own}");
        let other = of(&changed("//q:u")).diagnostic.message;
        assert!(other.starts_with("//q:u: its cached output"), "{other}");

        // A timeout's own frame is reworded the same way; its context stays.
        let timeout = anyhow::Error::new(crate::engine::run_timeout::TimedOutError {
            addr: addr("//p:t"),
            timeout: hplugin::driver::Timeout::new(std::time::Duration::from_secs(1))
                .expect("timeout"),
            abandoned_after: None,
        })
        .context("its fixes were not written");
        assert_eq!(
            of(&timeout).diagnostic.message,
            "its fixes were not written: timed out after 1s (its timeout)"
        );
    }

    /// What a streamed run's wrapper is told carries a driver's own
    /// diagnostic and hints, scrubbed, as the record will (CQ-1): its line is
    /// all the run says. A plain exit's message is the status.
    #[test]
    fn exec_failure_keeps_a_raised_diagnostic() {
        let raised = anyhow::Error::new(
            Diagnostic::new(Stage::Run, "go vet found 2 issues in SECRET-VALUE-1")
                .with_hint(Hint::new(HintKind::Other, "run go vet")),
        )
        .context(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))));
        let scrub = Scrub::new(["SECRET-VALUE-1".to_owned()]);
        let told = exec_failure(&addr("//p:t"), &raised, &scrub).expect("a process failure");
        assert_eq!(
            told.diagnostic.message,
            "go vet found 2 issues in [redacted]"
        );
        assert_eq!(told.diagnostic.hints.len(), 1);
        assert!(!told.message_is_status());
        assert_eq!(told.addr.as_deref(), Some("//p:t"));
        assert_eq!(told.failed_in, FailedIn::Exec);

        let plain = anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))));
        let told = exec_failure(&addr("//p:t"), &plain, &scrub).expect("a process failure");
        assert!(told.message_is_status(), "{told:?}");
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
                paths_more: 0,
            })
        );
        assert_eq!(rec.exit.and_then(|x| x.code), Some(1));
    }

    /// CQ-5: with `fixes.txt` kept, the record names the first
    /// [`FIXES_PATHS_CAP`](hcore::events::FIXES_PATHS_CAP) paths and counts
    /// the rest; without it, every path stays for the restore hint.
    #[test]
    fn fixes_paths_capped_when_the_list_is_kept() {
        let cap = hcore::events::FIXES_PATHS_CAP;
        let paths: Vec<String> = (0..cap + 7).map(|i| format!("f{i:03}.go")).collect();
        for list in [Some(std::path::PathBuf::from("/slot/fixes.txt")), None] {
            let slot = FailureSlot {
                fixes: Some(FailureFixes::Written {
                    root: "/ws".into(),
                    paths: paths.clone(),
                    list: list.clone(),
                }),
                ..FailureSlot::default()
            };
            let e = anyhow::Error::new(ProcessFailed::new("exit status: 1", Some(Exit::Code(1))))
                .context(slot);
            let rec = of(&e);
            let Some(hcore::events::FixesData::Written {
                paths: got,
                paths_more,
                ..
            }) = rec.left_behind.and_then(|l| l.fixes)
            else {
                panic!("fixes written");
            };
            if list.is_some() {
                assert_eq!(got.len(), cap);
                assert_eq!(got.first().map(String::as_str), Some("f000.go"));
                assert_eq!(paths_more, 7);
            } else {
                assert_eq!(got.len(), cap + 7, "no list: every path stays");
                assert_eq!(paths_more, 0);
            }
        }
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
        );
        assert_eq!(leaks(&rec), Vec::<String>::new());
        assert!(rec.diagnostic.message.contains("[redacted]"));
        assert!(rec.causes.first().is_some_and(|c| c.contains("[redacted]")));
        assert!(rec.diagnostic.truncated);
    }

    /// FQ-3: a secret planted in every text field a record holds survives
    /// nowhere in it once compacted.
    #[test]
    fn scrub_reaches_every_text_field() {
        let secret = "PLANTED-SECRET-0123456789";
        let s = |what: &str| format!("{what} {secret} {what}");
        let mut d = Diagnostic::new(Stage::Run, s("message"));
        d.addr = Some(s("addr"));
        d.snippet = Some(s("snippet"));
        d.plugin = Some(s("plugin"));
        d.location = Some(hcore::diagnostic::SourceLoc {
            file: s("file"),
            line: 1,
            col: 1,
        });
        d = d.with_hint(Hint::new(HintKind::Other, s("hint")));
        let mut inner = FailureRecord::new(d);
        inner.causes = vec![s("cause")];
        inner.frozen_diff = Some(s("diff"));
        inner.exit = Some(ExitData {
            code: None,
            signal: None,
            status: s("status"),
        });
        inner.log = Some(LogRef {
            path: None,
            fill_id: None,
            lines: 1,
            memory_tail: Some(s("tail")),
        });
        // The fixture has teeth: every field holds the secret going in.
        let planted = format!("{inner:?}").matches(secret).count();
        assert_eq!(planted, 10, "{inner:?}");

        let tf = TargetFailure::new(addr("//p:t")).with_record(Arc::new(inner));
        let rec = compact(
            &anyhow::Error::new(tf),
            &target("//p:t"),
            &addr("//p:t"),
            &Scrub::new([secret.to_owned()]),
        );
        // `Debug` prints every field, `memory_tail` (never serialized)
        // included.
        let all = format!("{rec:?}");
        assert!(!all.contains(secret), "{all}");
        assert_eq!(all.matches("[redacted]").count(), planted, "{all}");
    }

    #[test]
    fn scrub_debug_never_prints_a_value() {
        let s = Scrub::new(["hunter2-hunter2".to_owned()]);
        assert_eq!(format!("{s:?}"), "Scrub(1 values)");
    }

    /// The stage: the diagnostic's, else the innermost stage frame, else the
    /// culprit's kind.
    #[test]
    fn stage_falls_back_from_frame_to_culprit() {
        let e = anyhow::anyhow!("boom");
        assert_eq!(of(&e).diagnostic.stage, Stage::Run);
        let file = Culprit::File(Arc::from("p/BUILD"));
        let rec = compact(&e, &file, &addr("//p:t"), &Scrub::default());
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
    }

    /// STAGE-09: a dependent blamed for an input that does not exist failed
    /// at link, although the innermost frame is the input's `get_def`; the
    /// input's own not-found (no `MissingDep` naming it) stays a parse.
    #[test]
    fn missing_dep_is_the_declarers_link_failure() {
        use hplugin::diagnostic::{Boundary, with_stage};
        let missing = || {
            with_stage(
                anyhow::Error::new(hplugin::error::TargetNotFoundError {
                    addr: addr("//d:missing"),
                }),
                Boundary::GetDef,
                &addr("//d:missing"),
            )
        };
        let e = with_stage(
            missing().context(MissingDep {
                dep: addr("//d:missing"),
                declarer: addr("//d:t"),
                origin_id: "deps".to_owned(),
            }),
            Boundary::GetDef,
            &addr("//d:t"),
        );
        let rec = compact(&e, &target("//d:t"), &addr("//d:t"), &Scrub::default());
        assert_eq!(rec.diagnostic.stage, Stage::Link);
        let rec = compact(
            &missing(),
            &target("//d:other"),
            &addr("//d:other"),
            &Scrub::default(),
        );
        assert_eq!(rec.diagnostic.stage, Stage::Parse);
    }
}
