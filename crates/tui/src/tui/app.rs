use std::sync::Arc;

use async_trait::async_trait;
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot};

use super::log_sink::LogSink;
use super::session::{HolderNames, SessionHeader};
use hcore::events::{BuildEvent, EventSender};
use hcore::pending::PendingCounter;

pub(crate) enum Control {
    Pause(oneshot::Sender<()>, PauseFor),
    Resume,
}

/// Why the TUI is stepping aside — which decides who owns Ctrl-C while it is
/// down. The renderer leaves raw mode to pause, so the terminal starts
/// generating SIGINT again the moment it does; this says what should happen to
/// that press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseFor {
    /// Something is *writing* to the cooked terminal — a diagnostic, a stdout
    /// flush, or a target running with the terminal attached. Nobody is typing
    /// at anything, so Ctrl-C still belongs to heph and must cancel the run.
    Output,
    /// The user is *typing* at something heph is hosting (`heph run --shell`).
    /// Every keystroke, Ctrl-C included, belongs to that session — heph must
    /// not read it as "cancel the build".
    Input,
}

impl PauseFor {
    /// Whether this pause hands keyboard input to something other than heph,
    /// and so must suppress the shutdown trigger for its duration.
    pub(crate) fn owns_input(self) -> bool {
        matches!(self, Self::Input)
    }
}

pub struct AppContext {
    sink: LogSink,
    control: Option<mpsc::UnboundedSender<Control>>,
    /// Build-event sender, plumbed into the request state the app builds so the
    /// engine's progress events reach the renderer's view. The backend owns the
    /// receiving end. `None` only in contexts with no renderer.
    events: Option<EventSender>,
    /// Background-work (sandbox cleanup) counter shared with the request state
    /// the app builds. The backend owns the other clone and polls it on exit so
    /// the renderer stays up until fire-and-forget cleanups have drained.
    bg_pending: PendingCounter,
    /// Asks the line backend which failures its view printed as they failed
    /// ([`failures_printed`](Self::failures_printed)). `None` on the
    /// interactive backend, whose view prints none.
    printed: Option<mpsc::UnboundedSender<PrintedQuery>>,
}

/// A [`AppContext::failures_printed`] query: the backend folds what is queued,
/// then answers with [`CIAppView::failures_printed`].
pub(crate) type PrintedQuery = oneshot::Sender<std::collections::HashSet<String>>;

impl AppContext {
    pub(crate) fn direct(
        sink: LogSink,
        events: Option<EventSender>,
        bg_pending: PendingCounter,
        printed: mpsc::UnboundedSender<PrintedQuery>,
    ) -> Self {
        Self {
            sink,
            control: None,
            events,
            bg_pending,
            printed: Some(printed),
        }
    }

    pub(crate) fn with_control(
        sink: LogSink,
        control: mpsc::UnboundedSender<Control>,
        events: Option<EventSender>,
        bg_pending: PendingCounter,
    ) -> Self {
        Self {
            sink,
            control: Some(control),
            events,
            bg_pending,
            printed: None,
        }
    }

    /// The addrs whose failure the line backend's view printed the moment it
    /// failed ([`CIAppView::failures_printed`]), answered once the view has
    /// folded every event sent before this call: a failure recorded by then
    /// had its `ResultEnd` sent first, so the answer covers it. What the end
    /// of a CI run leaves out of its failure boxes — only what was printed,
    /// never what was merely expected to be. Empty on the interactive backend
    /// and when the backend cannot answer, so nothing is left out.
    pub async fn failures_printed(&self) -> std::collections::HashSet<String> {
        let Some(tx) = &self.printed else {
            return std::collections::HashSet::new();
        };
        let (reply, answer) = oneshot::channel();
        if tx.send(reply).is_err() {
            return std::collections::HashSet::new();
        }
        answer.await.unwrap_or_default()
    }

    pub fn sink(&self) -> &LogSink {
        &self.sink
    }

    /// The session the run records into: the slot handed to
    /// [`run_app`](crate::tui::run_app) (the sink's, which `run_app` checks is
    /// the same one). Where a command records what it prints outside the
    /// event stream, like the failure report on the line backend.
    pub fn session(&self) -> &super::session::SessionSlot {
        self.sink.session()
    }

    /// The build-event sender to plumb into the request state, e.g.
    /// `engine.new_state_full(fail_fast, ctx.event_sender(), ctx.bg_pending())`.
    pub fn event_sender(&self) -> Option<EventSender> {
        self.events.clone()
    }

    /// The background-work counter to plumb into the request state alongside
    /// `event_sender`, so the engine's sandbox cleanups register against the
    /// counter the renderer watches on exit.
    pub fn bg_pending(&self) -> PendingCounter {
        Arc::clone(&self.bg_pending)
    }

    pub fn interactive(&self) -> bool {
        self.control.is_some()
    }

    /// Suspend the TUI for the lifetime of the returned guard. The renderer
    /// drains pending log lines, restores the terminal, and switches the
    /// log sink to direct passthrough. On guard drop the renderer resumes.
    /// In CI mode this is a no-op.
    ///
    /// Pauses as [`PauseFor::Output`] — Ctrl-C stays heph's. Use
    /// [`Pauser::pause_for`] to hand the keyboard over instead.
    pub async fn pause(&self) -> PauseGuard<'_> {
        pause_inner(&self.control, PauseFor::Output).await;
        PauseGuard { ctx: self }
    }

    /// Standalone pause handle, cloneable and `'static`. Suitable for
    /// background tasks that need to pause/resume the TUI without
    /// borrowing the `AppContext`.
    pub fn pauser(&self) -> Pauser {
        Pauser {
            control: self.control.clone(),
        }
    }
}

pub struct PauseGuard<'a> {
    ctx: &'a AppContext,
}

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        resume_inner(&self.ctx.control);
    }
}

#[derive(Clone)]
pub struct Pauser {
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Pauser {
    /// Pause as [`PauseFor::Output`] — heph keeps Ctrl-C.
    pub async fn pause(&self) -> OwnedPauseGuard {
        self.pause_for(PauseFor::Output).await
    }

    /// Pause, saying who owns the keyboard while the TUI is down. Only
    /// [`PauseFor::Input`] suppresses the shutdown trigger, and only a caller
    /// that is genuinely handing the user's keystrokes to something else
    /// (`--shell`) may ask for it: a paused TUI is in cooked mode, so anything
    /// else would silently eat the run's only Ctrl-C.
    pub async fn pause_for(&self, kind: PauseFor) -> OwnedPauseGuard {
        pause_inner(&self.control, kind).await;
        OwnedPauseGuard {
            control: self.control.clone(),
        }
    }
}

pub struct OwnedPauseGuard {
    control: Option<mpsc::UnboundedSender<Control>>,
}

impl Drop for OwnedPauseGuard {
    fn drop(&mut self) {
        resume_inner(&self.control);
    }
}

async fn pause_inner(control: &Option<mpsc::UnboundedSender<Control>>, kind: PauseFor) {
    if let Some(tx) = control {
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx.send(Control::Pause(ack_tx, kind)).is_ok() {
            drop(ack_rx.await);
        }
    }
}

fn resume_inner(control: &Option<mpsc::UnboundedSender<Control>>) {
    if let Some(tx) = control {
        drop(tx.send(Control::Resume));
    }
}

/// A key only a full screen binds (the target Log tab's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenKey {
    /// `f`: follow the log's end again.
    Follow,
    /// `g`, Home.
    Top,
    /// `G`, End: the end, following.
    Bottom,
    /// `w`: wrap long lines.
    Wrap,
    /// `n`: the next search match.
    Next,
    /// `N`: the previous one.
    Prev,
    PageUp,
    PageDown,
}

/// A decision key in the approval view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decide {
    /// `y`: approve the request on screen.
    Approve,
    /// `n`: reject it.
    Reject,
    /// `Y`: approve every request of its build, pending and later.
    ApproveAll,
    /// `N`: reject them all.
    RejectAll,
}

impl Decide {
    /// The decision `c` asks for, when it is a decision key.
    pub fn of(c: char) -> Option<Self> {
        Some(match c {
            'y' => Self::Approve,
            'n' => Self::Reject,
            'Y' => Self::ApproveAll,
            'N' => Self::RejectAll,
            _ => return None,
        })
    }
}

/// A mouse event on a full screen, at a cell of the last frame drawn
/// (column, row; 0-based). A drag never reaches the view: the backend
/// selects with it and copies the selection on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseInput {
    /// The left button went down and came back up there, with no drag in
    /// between. Delivered at the release.
    Click { col: u16, row: u16 },
    /// One wheel step, `down` (towards the end) or up.
    Wheel { col: u16, row: u16, down: bool },
}

/// Aggregates the engine's build-event stream and renders progress in the
/// interactive TUI. The renderer drives the terminal lifecycle and feeds events
/// in; *what* gets drawn is entirely the view's call.
/// [`crate::tui::TuiProgressView`] is the paved road; implement this directly
/// for bespoke UI.
pub trait TUIAppView: Send {
    /// Fold one build event into the view's state. The repaint happens later on
    /// the render tick, so this must not draw.
    fn apply(&mut self, ev: &BuildEvent);

    /// Number of pinned inline viewport rows to reserve, given the full terminal
    /// height. The paved-road view targets one third of the terminal.
    fn rows(&self, term_height: u16) -> u16;

    /// Scroll the body by `delta` rows (negative = up, positive = down). Called
    /// from the backend's key handler; the repaint happens on the next render
    /// tick. No-op by default for views without a scrollable body.
    fn scroll(&mut self, _delta: i32) {}

    /// Whether a full screen (the dashboard) is up rather than the inline
    /// rows. The backend draws it on the alternate screen with
    /// [`Self::render_full`] and keeps the inline rows as they were.
    /// `false` by default: a view with no full screen.
    fn full_screen(&self) -> bool {
        false
    }

    /// One frame of the full screen, exactly `rows` lines of at most `cols`
    /// columns. Called every render tick while [`Self::full_screen`].
    fn render_full(&mut self, _now_ms: u64, _cols: u16, _rows: u16) -> Vec<Line<'static>> {
        Vec::new()
    }

    /// A ctrl-c key at `now_ms`: whether it stops (see
    /// [`crate::tui::stop_arm`]). `finished` and `stopping` are the
    /// backend's: the build has ended, a stop is already under way. One
    /// press stops by default: a view with no full screen.
    fn ctrl_c(
        &mut self,
        _now_ms: u64,
        _finished: bool,
        _stopping: bool,
    ) -> crate::tui::stop_arm::CtrlC {
        crate::tui::stop_arm::CtrlC::Stop
    }

    /// Any other key: an armed stop is off. No-op by default.
    fn disarm_stop(&mut self) {}

    /// `--dashboard`: open the dashboard before the first frame and hold the
    /// finished run until `q`, on whichever screen is up. No-op by default,
    /// and for a command with no dashboard (the line only).
    fn pin_dashboard(&mut self) {}

    /// Enter (`log` false) or `l` (`log` true) on a full screen's selected
    /// row: open that target's screen. No-op by default.
    fn open_selected(&mut self, _log: bool) {}

    /// A full screen's own key (the target Log tab's `f g G w n N`, End,
    /// Home, Page Up/Down). No-op by default.
    fn screen_key(&mut self, _key: ScreenKey) {}

    /// A click or a wheel step on the full screen, matched against what the
    /// last [`Self::render_full`] drew. Everything a click does has a key;
    /// no click decides an approval. No-op by default.
    fn mouse(&mut self, _input: MouseInput) {}

    /// Cycle the body view (the `Tab` / `Shift+Tab` keys). `forward` advances to
    /// the next view, otherwise the previous. Called from the backend's key
    /// handler; the repaint happens on the next render tick. No-op by default for
    /// views with a single view.
    fn tab(&mut self, _forward: bool) {}

    /// Toggle the header counters between the matched-target set and every
    /// observed target (the `a` key). Called from the backend's key handler; the
    /// repaint happens on the next render tick. No-op by default for views without
    /// a scope toggle.
    fn toggle_scope(&mut self) {}

    /// Whether the view is showing its main (default) body rather than a tabbed
    /// list. The backend uses this to route `q`: on the main view `q` quits a
    /// held viewport, off it `q` returns to the main view. Default `true` (a view
    /// with no tabs is always "on main").
    fn is_on_main_view(&self) -> bool {
        true
    }

    /// Whether a `/` filter is currently applied (being typed or confirmed). The
    /// backend routes `Esc`: with a filter active it clears the filter, otherwise
    /// it returns to the main view. Default `false`.
    fn has_active_filter(&self) -> bool {
        false
    }

    /// Return to the main (default) body from a tabbed list view, resetting
    /// scroll and any active filter. Bound to `Esc`/`q` off the main view. No-op
    /// by default for views without tabs.
    fn back_to_main(&mut self) {}

    /// `q` on the main view while the run is still going. No-op by default: in
    /// a run of this process `q` does nothing and Ctrl-C cancels. A session
    /// viewer (`heph session attach`) detaches here, and the run it shows goes
    /// on — a viewer never cancels anything.
    fn quit_while_running(&mut self) {}

    /// Whether `q` leaves from every screen, not only the main view: a
    /// session viewer, where `q` detaches from the dashboard and the target
    /// screens too (I11). Default `false`: off the main view `q` goes back.
    fn quits_from_any_screen(&self) -> bool {
        false
    }

    /// Whether the view is currently capturing keystrokes into a search query
    /// (the `/` filter). While `true` the backend routes printable keys, Backspace,
    /// Enter and Esc to the search methods below instead of the normal shortcuts.
    /// Default `false` for views without search.
    fn is_searching(&self) -> bool {
        false
    }

    /// Enter search-input mode (the `/` key). No-op by default.
    fn search_start(&mut self) {}

    /// Append a typed character to the active search query. No-op by default.
    fn search_input(&mut self, _c: char) {}

    /// Delete the last character of the active search query. No-op by default.
    fn search_backspace(&mut self) {}

    /// Cancel search: leave input mode and clear the filter (the `Esc` key).
    /// No-op by default.
    fn search_cancel(&mut self) {}

    /// Confirm search: leave input mode but keep the filter applied so the body
    /// can be scrolled (the `Enter` key). No-op by default.
    fn search_confirm(&mut self) {}

    /// Open the approval view on the oldest pending request (Enter on the
    /// line's prompt row, `v` on a full screen). `false`: nothing pending,
    /// or a view that decides nothing (an attached one). Default `false`.
    fn approval_open(&mut self) -> bool {
        false
    }

    /// Whether the approval view is up: the only screen whose `y`/`n`/`Y`/`N`
    /// decide. Default `false`.
    fn in_approval_view(&self) -> bool {
        false
    }

    /// A decision key in the approval view, pressed at `now_ms`. Dropped
    /// while the request on screen is locked out. No-op by default.
    fn approval_decide(&mut self, _decide: Decide, _now_ms: u64) {}

    /// Move the view's state on to `now_ms` (the approval view's decision
    /// flash, a request that left the queue). Called each tick, before the
    /// backend checks [`Self::full_screen`]. No-op by default.
    fn advance(&mut self, _now_ms: u64) {}

    /// The lines for the pinned viewport, including the spinner row built from
    /// `spinner`. Called every render tick. `width` is the current terminal
    /// column count and `height` the reserved viewport row count, so the view can
    /// size borders, scroll long labels, and grow its body to fill the rows.
    fn render(&self, spinner: &str, now_ms: u64, width: u16, height: u16) -> Vec<Line<'static>>;

    /// Whether the viewport should stay open after the run finishes and wait for
    /// an explicit quit (`q` / Ctrl-C) instead of auto-exiting — e.g. the user
    /// navigated away from the main view and would lose what they are looking at
    /// on auto-tear-down. Default `false` (auto-exit).
    fn hold_after_finish(&self) -> bool {
        false
    }

    /// Mark the run as finished at `at_ms`, the instant its elapsed clock
    /// freezes at and the final summary measures against. Called when the
    /// viewport is held open (it then surfaces a "done — press q to quit"
    /// notice) and again at teardown, before [`Self::last_render`]. No-op by
    /// default.
    fn set_finished(&mut self, _at_ms: u64) {}

    /// The user stopped the run before it finished — a ctrl-c key that
    /// stopped it, or SIGINT/SIGTERM/SIGHUP — so the final summary says
    /// "stopped". Not called for fail-fast cancellation, which also cancels
    /// targets but is the build's own doing. Called before
    /// [`Self::last_render`]. No-op by default.
    fn set_stopped(&mut self) {}

    /// Whether [`Self::set_stopped`] was called (or, for a viewer, the run
    /// it follows recorded a stop): what `Finished` records.
    fn stopped(&self) -> bool {
        false
    }

    /// The approval prompt this view shows now, as a session records it.
    /// `None` by default (no approval gate).
    fn pending_approval(&self) -> Option<crate::tui::session::ApprovalPrompt> {
        None
    }

    /// The standing Y/N approval decision in force, as a session records it.
    /// `None` by default (no approval gate).
    fn approval_standing(&self) -> Option<crate::tui::session::ApprovalStanding> {
        None
    }

    /// After the live viewport is torn down, render a final build summary
    /// directly to the terminal (stderr) so it persists in scrollback. NOT
    /// routed through the log sink. No-op by default.
    fn last_render(&self) {}

    /// How a session viewer should title this run: which header it rebuilds
    /// and the label. Recorded once in the session's `meta.json`. Default: the
    /// build header with no label.
    fn session_header(&self) -> SessionHeader {
        SessionHeader::default()
    }

    /// How to name the session behind a lock holder's pid. Handed over once,
    /// by the backend, before the first event. Ignored by default.
    fn name_lock_holders(&mut self, _names: HolderNames) {}
}

/// Aggregates the engine's build-event stream and renders progress in non-TUI
/// (CI / piped) mode, through the log sink. [`crate::tui::CiProgressView`] is
/// the paved road; implement this directly for bespoke output.
pub trait CIAppView: Send {
    /// Called once before the run starts.
    fn begin(&self) {}

    /// Fold the event and optionally print a concise line for it.
    fn apply(&mut self, ev: &BuildEvent);

    /// Print the final summary once the event stream ends.
    fn finish(&self) {}

    /// See [`TUIAppView::set_stopped`]; called before [`Self::finish`].
    fn set_stopped(&mut self) {}

    /// See [`TUIAppView::session_header`].
    fn session_header(&self) -> SessionHeader {
        SessionHeader::default()
    }

    /// See [`TUIAppView::name_lock_holders`].
    fn name_lock_holders(&mut self, _names: HolderNames) {}

    /// The addrs whose failure this view has printed as it failed, with the
    /// rows the end-of-run box would have carried: the box leaves them out
    /// ([`AppContext::failures_printed`]). A view that prints none says so.
    fn failures_printed(&self) -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }
}

#[async_trait]
pub trait App: Send {
    type Output: Send;

    /// The interactive (TUI) view for this command's progress.
    type TuiView: TUIAppView;
    /// The non-TUI (CI / piped) view for this command's progress.
    type CiView: CIAppView;

    /// Build the interactive view. Use [`crate::tui::TuiProgressView`] for the
    /// paved road.
    fn tui_view(&self) -> Self::TuiView;

    /// Build the non-TUI view. Use [`crate::tui::CiProgressView`] for the paved
    /// road.
    fn ci_view(&self) -> Self::CiView;

    /// Run the business logic. May call `ctx.pause()` to suspend the TUI.
    async fn run(self, ctx: AppContext) -> anyhow::Result<Self::Output>;

    /// Whether running this app records a session (`~/.heph/sessions/<id>/`).
    /// True for every command; a session *viewer* says false, so watching a
    /// session never creates another one.
    fn records_session(&self) -> bool {
        true
    }

    /// How this app's view names a lock holder's pid, when the app knows
    /// better than the process's own session: a viewer names holders from the
    /// sessions directory it watches, excluding the session it attached to.
    /// `None`: the backend names them from the process's session slot.
    fn lock_holders(&self) -> Option<HolderNames> {
        None
    }
}
