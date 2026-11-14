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

use super::format::{ApprovalPrompt, SessionRecord, SessionRecordKind};

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
    /// `Finished`'s time: the clock the run's final summary used.
    finished_at_ms: Option<u64>,
    /// `End`'s time: the fallback clock when no `Finished` was recorded (a
    /// line-backend run, or a heph from before `Finished`).
    ended_at_ms: Option<u64>,
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
            SessionRecordKind::Approval { prompt } => {
                s.approval = prompt.map(|p| *p);
                return Routed::Nothing;
            }
            SessionRecordKind::Finished => {
                s.finished_at_ms = Some(at);
                // Torn down: no prompt is on screen any more.
                s.approval = None;
                return Routed::Nothing;
            }
            SessionRecordKind::End { .. } => {
                s.ended_at_ms = Some(at);
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

    /// The instant the run's final summary was measured against: its
    /// `Finished`, else its `End`. `None` while the run goes on.
    pub fn finished_at_ms(&self) -> Option<u64> {
        let s = self.lock();
        s.finished_at_ms.or(s.ended_at_ms)
    }
}
