//! What an attached view takes from a recording besides build events.
//!
//! `heph session attach` draws the run's own [`TuiProgressView`], in its
//! `Attached` role. Build events reach it exactly as they reached the run's
//! view. The rest of what the run's terminal showed was recorded as view
//! records (`Label`, `Approval`, `Finished`), and [`SessionFeed::route`] is
//! the one place that turns a record into what the viewer does with it: fold
//! an event, update the shared feed the view reads, or print a line now or
//! after the viewer's own teardown.
//!
//! [`TuiProgressView`]: crate::tui::TuiProgressView

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hcore::events::BuildEvent;

use hcore::events::BuildEventKind;

use super::format::{
    ApprovalPrompt, ApprovalStanding, RunLogRecord, SessionRecord, SessionRecordKind,
};

/// The recorded view state an attached [`TuiProgressView`] reads while it
/// renders. Cloned between the app that routes records and the view.
///
/// [`TuiProgressView`]: crate::tui::TuiProgressView
#[derive(Debug, Clone, Default)]
pub struct SessionFeed(Arc<Mutex<FeedState>>);

#[derive(Debug, Default)]
struct FeedState {
    /// The latest `Label`: `None` until the run changed it (then the view's
    /// own label, from `meta.json`, stands).
    label: Option<String>,
    approval: Option<ApprovalPrompt>,
    /// The standing Y/N decision, from the press until the build ends.
    standing: Option<ApprovalStanding>,
    /// `Finished`'s time: the clock the run's final summary used.
    finished_at_ms: Option<u64>,
    /// `Finished` said the user stopped the run.
    stopped: bool,
    /// `End`'s time: the fallback clock when no `Finished` was recorded (a
    /// line-backend run, or a heph from before `Finished`).
    ended_at_ms: Option<u64>,
    /// Each addr's newest `RunLog`, for the viewer's log readers: the same
    /// records `heph log` finds the run's live file through, read off the
    /// stream the viewer follows anyway instead of rescanning the session's
    /// `events.ndjson` per lookup. One entry per target that ran.
    run_logs: std::collections::HashMap<String, FeedRunLog>,
    /// Each running addr's `ExecuteStart` time, until its `ExecuteEnd`: a
    /// run that has written nothing yet is "started", not absent. One entry
    /// per target running now.
    started: std::collections::HashMap<String, u64>,
    /// Addrs running on a driver that does not stream its log, with their
    /// `ExecuteStart`'s time, until the run's first byte (its `RunLog`) or
    /// its `ExecuteEnd`: `run_logs.rs`'s rule, off the same stream.
    not_streamed: std::collections::HashMap<String, u64>,
}

/// An addr's newest `RunLog` in the followed session.
#[derive(Debug, Clone)]
pub struct FeedRunLog {
    pub record: RunLogRecord,
    /// When the record was written.
    pub at_unix_ms: u64,
    /// An `ExecuteEnd` for the addr followed the record.
    pub ended: bool,
}

/// What to do with one record.
#[derive(Debug, Clone)]
pub enum Routed {
    /// Fold it into the view.
    Event(BuildEvent),
    /// Print it above the viewport now, as the run did. Verbatim: the caller
    /// escapes it.
    Print(String),
    /// Print it once the viewer's own TUI is torn down: the run printed it
    /// after its teardown, below its final summary.
    AfterTeardown(String),
    /// Nothing to print: taken into the feed, or not shown by a TUI.
    Nothing,
}

impl SessionFeed {
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Route one record, updating the feed with what it says about the view.
    pub fn route(&self, record: SessionRecord) -> Routed {
        let at = record.at_unix_ms;
        if let Some(kind) = record.kind {
            match &kind {
                BuildEventKind::ExecuteStart {
                    addr,
                    log_not_streamed,
                    ..
                } => {
                    let mut s = self.lock();
                    s.started.insert(addr.clone(), at);
                    if *log_not_streamed {
                        s.not_streamed.insert(addr.clone(), at);
                    }
                }
                // The run whose log was announced is over (`run_logs.rs`'s
                // rule).
                BuildEventKind::ExecuteEnd { addr, .. } => {
                    let mut s = self.lock();
                    s.started.remove(addr);
                    s.not_streamed.remove(addr);
                    if let Some(run) = s.run_logs.get_mut(addr) {
                        run.ended = true;
                    }
                }
                _ => {}
            }
            return Routed::Event(BuildEvent {
                at_unix_ms: at,
                kind,
            });
        }
        let Some(kind) = record.session else {
            return Routed::Nothing;
        };
        let mut s = self.lock();
        let text = match kind {
            SessionRecordKind::Log { mut msg, .. } => {
                // The recorder dropped the line's own newline.
                msg.push('\n');
                msg
            }
            SessionRecordKind::Output { text } => text,
            SessionRecordKind::Label { label } => {
                s.label = Some(label);
                return Routed::Nothing;
            }
            SessionRecordKind::Approval { prompt, standing } => {
                s.approval = prompt.map(|p| *p);
                s.standing = standing;
                return Routed::Nothing;
            }
            SessionRecordKind::Finished { stopped } => {
                s.finished_at_ms = Some(at);
                s.stopped = stopped;
                // Torn down: no prompt is on screen any more, and the build
                // a standing decision covered is over.
                s.approval = None;
                s.standing = None;
                return Routed::Nothing;
            }
            SessionRecordKind::End { .. } => {
                s.ended_at_ms = Some(at);
                return Routed::Nothing;
            }
            // Where a run's log is, for a log reader; nothing a TUI prints.
            SessionRecordKind::RunLog(run) => {
                // A path that could leave `<home>/runs/` is never kept.
                if run.relative_file().is_some() {
                    s.not_streamed.remove(&run.addr);
                    s.run_logs.insert(
                        run.addr.clone(),
                        FeedRunLog {
                            record: *run,
                            at_unix_ms: at,
                            ended: false,
                        },
                    );
                }
                return Routed::Nothing;
            }
            SessionRecordKind::Truncated
            | SessionRecordKind::InflightWritten
            | SessionRecordKind::Unknown => return Routed::Nothing,
        };
        if text.is_empty() {
            Routed::Nothing
        } else if s.finished_at_ms.is_some() {
            Routed::AfterTeardown(text)
        } else {
            Routed::Print(text)
        }
    }

    /// The run's label, once it changed from the one `meta.json` recorded.
    pub fn label(&self) -> Option<String> {
        self.lock().label.clone()
    }

    /// The approval prompt the run shows now.
    pub fn approval(&self) -> Option<ApprovalPrompt> {
        self.lock().approval.clone()
    }

    /// The standing Y/N decision in force, for the indicator.
    pub fn approval_standing(&self) -> Option<ApprovalStanding> {
        self.lock().standing
    }

    /// `addr`'s newest announced run log, as far as the stream has been read.
    pub fn run_log(&self, addr: &str) -> Option<FeedRunLog> {
        self.lock().run_logs.get(addr).cloned()
    }

    /// When `addr`'s run going on started, as far as the stream has been
    /// read; `None` once it ended.
    pub fn started_at(&self, addr: &str) -> Option<u64> {
        self.lock().started.get(addr).copied()
    }

    /// When `addr`'s current run started, if it runs on a driver that does
    /// not stream its log and no byte of it has arrived yet.
    pub fn not_streamed(&self, addr: &str) -> Option<u64> {
        self.lock().not_streamed.get(addr).copied()
    }

    /// The instant the run's final summary was measured against: its
    /// `Finished`, else its `End`. `None` while the run goes on.
    pub fn finished_at_ms(&self) -> Option<u64> {
        let s = self.lock();
        s.finished_at_ms.or(s.ended_at_ms)
    }

    /// Whether the run's `Finished` recorded a user stop.
    pub fn stopped(&self) -> bool {
        self.lock().stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(at: u64, msg: &str) -> SessionRecord {
        SessionRecord::session(
            at,
            SessionRecordKind::Log {
                level: "INFO".into(),
                msg: msg.into(),
                seq: 0,
            },
        )
    }

    fn printed(r: &Routed) -> Option<(&'static str, &str)> {
        match r {
            Routed::Print(t) => Some(("now", t.as_str())),
            Routed::AfterTeardown(t) => Some(("after", t.as_str())),
            Routed::Event(_) | Routed::Nothing => None,
        }
    }

    /// What the run printed before its teardown prints now; what it printed
    /// after (a late log line, the failure report) waits for the viewer's own
    /// teardown, in order. `Finished` clears the prompt and fixes the clock,
    /// and a later `End` does not move it.
    #[test]
    fn routes_by_teardown_order() {
        let feed = SessionFeed::default();
        let prompt = ApprovalPrompt {
            queue: vec![crate::tui::session::ApprovalQueued {
                seq: 1,
                addr: "//a:gated".into(),
                ..Default::default()
            }],
        };
        let label = SessionRecordKind::Label {
            label: "Invoking //a:b".into(),
        };
        let approval = SessionRecordKind::Approval {
            prompt: Some(Box::new(prompt.clone())),
            standing: Some(ApprovalStanding::Approve),
        };
        let routed: Vec<Routed> = [
            log(2, "before"),
            SessionRecord::session(3, label),
            SessionRecord::session(4, approval),
        ]
        .into_iter()
        .map(|r| feed.route(r))
        .collect();
        assert_eq!(
            routed.iter().filter_map(printed).collect::<Vec<_>>(),
            [("now", "before\n")]
        );
        assert_eq!(feed.label().as_deref(), Some("Invoking //a:b"));
        assert_eq!(feed.approval(), Some(prompt));
        assert_eq!(feed.approval_standing(), Some(ApprovalStanding::Approve));
        assert_eq!(feed.finished_at_ms(), None, "still running");

        let end = SessionRecordKind::End {
            exit_code: Some(1),
            panicked: false,
        };
        let output = SessionRecordKind::Output {
            text: "× target failed\n".into(),
        };
        let routed: Vec<Routed> = [
            SessionRecord::session(10, SessionRecordKind::Finished { stopped: true }),
            log(11, "late"),
            SessionRecord::session(12, output),
            SessionRecord::session(13, end),
        ]
        .into_iter()
        .map(|r| feed.route(r))
        .collect();
        assert_eq!(
            routed.iter().filter_map(printed).collect::<Vec<_>>(),
            [("after", "late\n"), ("after", "× target failed\n")]
        );
        assert_eq!(feed.approval(), None, "torn down: no prompt on screen");
        assert_eq!(feed.approval_standing(), None, "the build is over");
        assert_eq!(feed.finished_at_ms(), Some(10), "Finished, not End");
        assert!(feed.stopped(), "the run's stop, as Finished recorded it");
    }

    /// A run with no `Finished` (the line backend, or an older heph) is
    /// measured at its `End`, and everything it printed prints now.
    #[test]
    fn end_is_the_clock_without_finished() {
        let feed = SessionFeed::default();
        assert!(matches!(feed.route(log(1, "x")), Routed::Print(_)));
        let end = SessionRecordKind::End {
            exit_code: Some(0),
            panicked: false,
        };
        assert!(matches!(
            feed.route(SessionRecord::session(9, end)),
            Routed::Nothing
        ));
        assert_eq!(feed.finished_at_ms(), Some(9));
    }
}
