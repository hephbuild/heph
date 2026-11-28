//! The compact record of one failure: what every renderer, `--json` and the
//! failure slot read, in place of the error chain it was made from.
//!
//! The engine builds it once, when it records a culprit, through one choke
//! point (`engine::failure_record::compact`) that scrubs secrets out of every
//! text and only then cuts it to the caps here. What the record keeps is
//! small and fixed (I8): no log text (a [`LogRef`] says where the log is), no
//! error chain, each text within its cap and the whole within
//! [`RECORD_CAP`].
//!
//! Lives in `hcore` because it rides the event wire (`ResultEnd`, as
//! [`LeftBehindData`] does) and `crates/tui` renders it with no engine
//! dependency.
//!
//! **Never hashed** (I1): no type here implements `Hash`, so nothing about a
//! failure reaches a def hash, `hashin` or a cache key. The `compile_fail`
//! doctest holding this is `failure_record_has_no_hash` in
//! `hplugin::diagnostic` (this package is named `core`, which a doctest here
//! cannot name).

use crate::diagnostic::Diagnostic;
use crate::events::{LeftBehindData, TimedOutData};
use serde::{Deserialize, Serialize};

/// The most bytes one cause of an aggregate failure keeps.
pub const CAUSE_CAP: usize = 512;
/// The most causes a record keeps, in input order; the rest are counted in
/// [`FailureRecord::more_causes`].
pub const CAUSES_CAP: usize = 16;
/// The most bytes of text a whole record holds ([`FailureRecord::text_len`]):
/// past the per-field caps, causes are dropped from the end, then the
/// snippet, until it fits. Also the cap on [`LogRef::memory_tail`].
pub const RECORD_CAP: usize = 8 * 1024;

/// One failure, compacted. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// The culprit target (`//pkg:name`); `None` for a file culprit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// The culprit file, workspace-relative, when the culprit is a BUILD or
    /// `.bzl` file that did not evaluate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The target the failure was reached through: the culprit itself, or for
    /// a file culprit, a target the file would have defined.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub entry: String,
    /// What is wrong, at which stage, where and what to do: the failure's own
    /// diagnostic when it raised one, else one made from its cause.
    pub diagnostic: Diagnostic,
    /// An aggregate failure's members, each as its own one-line cause: the
    /// first [`CAUSES_CAP`] in input order, each within [`CAUSE_CAP`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    /// How many causes were left out (`+N more`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more_causes: u32,
    /// The shortest path from a requested target to the culprit, inclusive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// Where in the target's life it failed.
    #[serde(default, skip_serializing_if = "FailedIn::is_before_run")]
    pub failed_in: FailedIn,
    /// The process's exit, when the failure is a process that exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitData>,
    /// The run outlasted its `timeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<TimedOutData>,
    /// A `--frozen` check's diff (generated output against the tree), capped
    /// like a snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_diff: Option<String>,
    /// A stop rather than a failure: a cancellation is on the chain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// The failure reached this request as another request's recorded
    /// failure, surfaced at its top (`surface_top`): its facts are that
    /// record's, carried over.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub surface_top: bool,
    /// What the failed run left behind: the slot's log path, the sandbox's
    /// fate, `out/`, the `in_place` write-back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_behind: Option<LeftBehindData>,
    /// Where the run's log is; never its text, but for [`LogRef::memory_tail`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogRef>,
    /// The run's output reached a terminal while it ran.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub streamed: bool,
    /// The targets that failed only because this one did: known once the
    /// request ends, so filled when it hands its failures out, never on the
    /// wire as the failure happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_down: Option<TookDownData>,
}

/// The targets one culprit took down: the first few by name, in the order
/// they failed, and how many in all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TookDownData {
    pub named: Vec<String>,
    pub total: u64,
}

impl FailureRecord {
    /// A record of `diagnostic` alone, every other fact absent.
    pub fn new(diagnostic: Diagnostic) -> Self {
        Self {
            addr: None,
            file: None,
            entry: String::new(),
            diagnostic,
            causes: Vec::new(),
            more_causes: 0,
            path: Vec::new(),
            failed_in: FailedIn::default(),
            exit: None,
            timed_out: None,
            frozen_diff: None,
            cancelled: false,
            surface_top: false,
            left_behind: None,
            log: None,
            streamed: false,
            took_down: None,
        }
    }

    /// The failure was already told on the terminal: its process failed
    /// while its output streamed there, and the wrapper that lent it the
    /// terminal printed its one line (`✗ //t · exit status 1 · heph log
    /// //t`, D6.1c) right after that output. Every renderer leaves its box
    /// out; `--json` still carries it. A failure after a streamed run (its
    /// outputs, the cache store) is not: it gets its box, without the log.
    pub fn told_streamed(&self) -> bool {
        self.streamed && self.failed_in == FailedIn::Exec && !self.cancelled
    }

    /// The bytes of text the record holds, as [`RECORD_CAP`] counts them:
    /// every text it would print about the failure. Not the path (its depth
    /// is the graph's, bounded where the request keeps it), not what was left
    /// behind (paths on disk), and not the log, whose in-memory tail has a
    /// cap of its own.
    pub fn text_len(&self) -> usize {
        let d = &self.diagnostic;
        let opt = |s: &Option<String>| s.as_ref().map_or(0, String::len);
        d.message.len()
            + opt(&d.addr)
            + opt(&d.snippet)
            + opt(&d.plugin)
            + d.location.as_ref().map_or(0, |l| l.file.len())
            + d.hints.iter().map(|h| h.text.len()).sum::<usize>()
            + self.causes.iter().map(String::len).sum::<usize>()
            + opt(&self.addr)
            + opt(&self.file)
            + self.entry.len()
            + self.exit.as_ref().map_or(0, |x| x.status.len())
            + opt(&self.frozen_diff)
    }

    /// How the failed process ended, in the words every renderer uses: its
    /// timeout (`timed out after 1s (its timeout)`), its exit (`exit status
    /// 3`) or its signal (`killed by signal 9 (SIGKILL)`). `None` when the
    /// failure is not a process that ended. A box's header, the streamed
    /// line, and the message of a process failure whose driver raised no
    /// diagnostic all say this, so no two of them can word it differently.
    pub fn process_end(&self) -> Option<String> {
        if let Some(t) = &self.timed_out {
            return Some(format!(
                "timed out after {} (its timeout)",
                crate::units::format_duration(std::time::Duration::from_millis(t.timeout_ms))
            ));
        }
        let exit = self.exit.as_ref()?;
        Some(match (exit.code, exit.signal) {
            (Some(code), _) => format!("exit status {code}"),
            (None, Some(sig)) => match signal_name(sig) {
                Some(name) => format!("killed by signal {sig} ({name})"),
                None => format!("killed by signal {sig}"),
            },
            (None, None) => exit.status.trim().to_owned(),
        })
    }
}

/// What a box adds under a timeout whose run was abandoned, and what the
/// message of such a failure ends with.
pub const ABANDONED: &str = "abandoned: the run did not stop once cancelled and heph stopped \
                             waiting for it, so its process may still be running";

/// The name of a signal a process commonly dies of.
pub fn signal_name(sig: i32) -> Option<&'static str> {
    Some(match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Where in a target's life a failure happened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedIn {
    /// Before the target's process ran, or with no process at all: a BUILD
    /// file, a spec, a parse, a link, a missing dep.
    #[default]
    BeforeRun,
    /// The process itself: it exited non-zero, was killed, or timed out.
    Exec,
    /// After the process ran: its outputs, the cache store, a `--frozen`
    /// check, the codegen write-back.
    AfterRun,
}

impl FailedIn {
    fn is_before_run(&self) -> bool {
        matches!(self, Self::BeforeRun)
    }
}

/// How a failed process exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitData {
    /// The code it exited with; `None` for a signal, or a status known only
    /// as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that killed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// The status as the driver spelled it (`exit status: 1`).
    #[serde(default)]
    pub status: String,
}

/// Where a failed run's log is: a reference, never the text (D7b).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRef {
    /// The failure slot's `log.txt`; `None` when no copy is kept on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The slot fill this record's log belongs to, so a reader can tell the
    /// log was since replaced by a newer failed run. Never hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_id: Option<String>,
    /// How many lines the log has; 0 when not counted.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub lines: u64,
    /// The scrubbed tail, for a target whose log may not be kept on disk (its
    /// outputs are credential material): within [`RECORD_CAP`], the end kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_tail: Option<String>,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{Hint, HintKind, SourceLoc, Stage};

    /// A sparse record (stage and message only) decodes, every absent fact
    /// at its default, and a full one survives a round trip unchanged. Absent
    /// facts are left off the wire.
    #[test]
    fn failure_record_serde_roundtrip_sparse() {
        let sparse: FailureRecord =
            serde_json::from_str(r#"{"diagnostic":{"stage":"run","message":"boom"}}"#)
                .expect("sparse decodes");
        assert_eq!(
            sparse,
            FailureRecord::new(Diagnostic::new(Stage::Run, "boom"))
        );
        assert_eq!(
            serde_json::to_string(&sparse).expect("encode"),
            r#"{"diagnostic":{"stage":"run","message":"boom"}}"#
        );

        let full = FailureRecord {
            addr: Some("//p:t".into()),
            entry: "//p:t".into(),
            causes: vec!["a".into()],
            more_causes: 3,
            path: vec!["//p:t".into()],
            failed_in: FailedIn::Exec,
            exit: Some(ExitData {
                code: None,
                signal: Some(9),
                status: "signal: 9".into(),
            }),
            timed_out: Some(TimedOutData {
                timeout_ms: 1000,
                abandoned: true,
            }),
            frozen_diff: Some("-a\n+b".into()),
            cancelled: true,
            surface_top: true,
            left_behind: Some(LeftBehindData {
                sandbox: Some("x (kept)".into()),
                kept: vec![crate::events::KeptGroup {
                    name: "report/".into(),
                    files: 3,
                }],
                kept_more: 2,
                ..LeftBehindData::default()
            }),
            log: Some(LogRef {
                path: Some("/h/failures/k/log.txt".into()),
                fill_id: Some("f1".into()),
                lines: 12,
                memory_tail: None,
            }),
            streamed: true,
            took_down: Some(TookDownData {
                named: vec!["//p:up".into()],
                total: 4,
            }),
            ..FailureRecord::new(
                Diagnostic::new(Stage::Parse, "bad")
                    .with_location(SourceLoc::checked("p/BUILD".into(), 4, 5).expect("loc"))
                    .with_hint(Hint::new(HintKind::DidYouMean, "deps"))
                    .with_plugin("go"),
            )
        };
        let json = serde_json::to_string(&full).expect("encode");
        assert!(json.contains(r#""stage":"parse""#), "{json}");
        assert!(json.contains(r#""kind":"did_you_mean""#), "{json}");
        assert!(json.contains(r#""failed_in":"exec""#), "{json}");
        let back: FailureRecord = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, full);
    }

    /// A newer peer's hint kind is `Other`. A location that climbs out of the
    /// workspace is refused at decode, as at the plugin seam, but only the
    /// location goes: the diagnostic around it is kept. So does one of the
    /// wrong shape.
    #[test]
    fn decode_is_lenient_on_hint_kind_and_strict_on_location() {
        let d: Diagnostic = serde_json::from_str(
            r#"{"stage":"eval","message":"m","hints":[{"kind":"from_the_future","text":"t"}]}"#,
        )
        .expect("decodes");
        assert_eq!(d.hints, [Hint::new(HintKind::Other, "t")]);
        for loc in [
            r#"{"file":"../x","line":1,"col":1}"#,
            r#"{"file":"/etc/passwd","line":1,"col":1}"#,
            r#"{"file":"a","line":"one","col":1}"#,
            r#""p/BUILD:1:1""#,
            "null",
        ] {
            let rec: FailureRecord = serde_json::from_str(&format!(
                r#"{{"diagnostic":{{"stage":"eval","message":"m","location":{loc}}},"entry":"//p:t"}}"#
            ))
            .unwrap_or_else(|e| panic!("{loc}: the record decodes: {e}"));
            assert_eq!(rec.diagnostic.location, None, "{loc}");
            assert_eq!(
                (rec.diagnostic.message.as_str(), rec.entry.as_str()),
                ("m", "//p:t")
            );
        }
        // A sound one is kept.
        let d: Diagnostic = serde_json::from_str(
            r#"{"stage":"eval","message":"m","location":{"file":"p/BUILD","line":2,"col":3}}"#,
        )
        .expect("decodes");
        assert_eq!(
            d.location,
            Some(SourceLoc::checked("p/BUILD".into(), 2, 3).expect("loc"))
        );
    }

    /// A stage this heph does not know (a newer heph's slot, a newer
    /// plugin's record) decodes as `run`, the rest of the record intact; a
    /// stage that is not a string still fails.
    #[test]
    fn unknown_stage_decodes() {
        let rec: FailureRecord = serde_json::from_str(
            r#"{"diagnostic":{"stage":"from_the_future","message":"boom"},"exit":{"code":3}}"#,
        )
        .expect("an unknown stage decodes");
        assert_eq!(rec.diagnostic.stage, Stage::Run);
        assert_eq!(rec.diagnostic.message, "boom");
        assert_eq!(rec.exit.and_then(|x| x.code), Some(3));
        assert!(
            serde_json::from_str::<Diagnostic>(r#"{"stage":7,"message":"m"}"#).is_err(),
            "not a stage name"
        );
    }

    /// How a process ended, in the one wording every renderer shares: a
    /// timeout wins over the exit it caused, a signal is named when it is a
    /// common one, a status known only as text is said as the driver spelled
    /// it, and a failure with no process says nothing.
    #[test]
    fn process_end_words_each_way_a_process_ends() {
        let exit = |code, signal, status: &str| {
            let mut r = FailureRecord::new(Diagnostic::new(Stage::Run, "m"));
            r.exit = Some(ExitData {
                code,
                signal,
                status: status.into(),
            });
            r
        };
        let end = |r: &FailureRecord| r.process_end();
        assert_eq!(
            end(&exit(Some(3), None, "exit status: 3")).as_deref(),
            Some("exit status 3")
        );
        assert_eq!(
            end(&exit(None, Some(9), "signal: 9")).as_deref(),
            Some("killed by signal 9 (SIGKILL)")
        );
        assert_eq!(
            end(&exit(None, Some(64), "signal: 64")).as_deref(),
            Some("killed by signal 64")
        );
        assert_eq!(
            end(&exit(None, None, " remote: lost \n")).as_deref(),
            Some("remote: lost")
        );
        let mut timed = exit(None, Some(9), "signal: 9");
        timed.timed_out = Some(TimedOutData {
            timeout_ms: 90_000,
            abandoned: false,
        });
        assert_eq!(
            end(&timed).as_deref(),
            Some("timed out after 1m30s (its timeout)")
        );
        assert_eq!(
            end(&FailureRecord::new(Diagnostic::new(Stage::Run, "m"))),
            None
        );
    }
}
